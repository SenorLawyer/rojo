-- Local Studio MCP control, available even when no Rojo server is connected.
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local Settings = require(script.Parent.Settings)
local Version = require(script.Parent.Version)
local Config = require(script.Parent.Config)
local PatchSet = require(script.Parent.PatchSet)
local StudioBridge = require(script.Parent.StudioBridge)

local AgentControl = {}
local version = Version.display(Config.version)
local settingTypes = {
	openScriptsExternally = "boolean",
	twoWaySync = "boolean",
	autoReconnect = "boolean",
	showNotifications = "boolean",
	enableSyncFallback = "boolean",
	syncReminderPolling = "boolean",
	checkForUpdates = "boolean",
	checkForPrereleases = "boolean",
	autoConnectPlaytestServer = "boolean",
	playSounds = "boolean",
	typecheckingEnabled = "boolean",
	timingLogsEnabled = "boolean",
	guardScriptWipes = "boolean",
	autoBackupBeforeSync = "boolean",
	syncReminderMode = { "None", "Notify", "Fullscreen" },
	confirmationBehavior = { "Never", "Initial", "Always", "Large Changes", "Unlisted PlaceId" },
	logLevel = { "Trace", "Debug", "Info", "Warn", "Error" },
	largeChangesConfirmationThreshold = "number",
}

function AgentControl.validateSetting(name, value)
	local kind = settingTypes[name]
	assert(kind, "Unsupported setting; inspect capabilities")
	if type(kind) == "table" then
		assert(table.find(kind, value), "Invalid setting choice")
	else
		assert(type(value) == kind, "Invalid setting type")
	end
	if kind == "number" then
		assert(value % 1 == 0 and value >= 1 and value <= 1000000, "Threshold must be an integer from 1 to 1000000")
	end
end

-- How strict each confirmation behaviour is. The three conditional
-- behaviours confirm in different situations, so none of them is stricter
-- than another and switching between them counts as loosening.
local confirmationStrictness = {
	Never = 0,
	Initial = 1,
	["Large Changes"] = 1,
	["Unlisted PlaceId"] = 1,
	Always = 2,
}

local CONFIRMATION_SETTING_HINT =
	"Change it in Rojo settings, or allow agent control to change sync confirmation there."

-- Any plugin, the command bar or a Studio MCP tool can invoke the controller,
-- so anything that lets changes reach the place or the file system without
-- the user's review must respect their saved agent-confirmation preference.
function AgentControl.checkConfirmationChange(name, value)
	if Settings:get("agentControlCanChangeConfirmation") then
		return
	end
	if name == "confirmationBehavior" then
		local current = Settings:get("confirmationBehavior")
		assert(
			value == current or (confirmationStrictness[value] or 0) > (confirmationStrictness[current] or 0),
			"Agent control can only make sync confirmation stricter. " .. CONFIRMATION_SETTING_HINT
		)
	elseif name == "largeChangesConfirmationThreshold" then
		assert(
			value <= Settings:get("largeChangesConfirmationThreshold"),
			"Agent control can only lower the confirmation threshold. " .. CONFIRMATION_SETTING_HINT
		)
	elseif name == "twoWaySync" then
		-- Two-way sync writes Studio edits to the project's files.
		assert(value == false, "Agent control cannot turn on two-way sync. " .. CONFIRMATION_SETTING_HINT)
	elseif name == "guardScriptWipes" then
		-- Without the guard, an empty file on disk empties the script in Studio.
		assert(value == true, "Agent control cannot turn off the script wipe guard. " .. CONFIRMATION_SETTING_HINT)
	elseif name == "autoBackupBeforeSync" then
		-- Without it, Studio's copies of what a resync overwrites are lost.
		assert(value == true, "Agent control cannot turn off backups before resyncing. " .. CONFIRMATION_SETTING_HINT)
	end
end

local CONTROLLER_PROTOCOL_VERSION = 1

-- Only controllers mounted by a Rojo plugin carry these attributes, so other
-- instances in the folder cannot block syncing by accident. This is not a
-- security boundary: any plugin can create a BindableFunction with the same
-- attributes, mark it SyncActive to stop this copy from syncing, or pose as
-- a Rojo controller to callers that pick one from the folder. Roblox does not
-- let a script read another instance's OnInvoke, so there is no cheap way to
-- confirm that a sibling really belongs to Rojo.
local function isRojoController(instance)
	return instance:IsA("BindableFunction")
		and type(instance:GetAttribute("PluginVersion")) == "string"
		and instance:GetAttribute("ProtocolVersion") == CONTROLLER_PROTOCOL_VERSION
end

AgentControl.isRojoController = isRojoController

function AgentControl.tryClaim(app)
	local controller = app.agentController
	if controller == nil or controller.Parent == nil then
		-- Anything with plugin security can delete the shared folder. Mount
		-- again rather than leave this plugin unable to sync until reload.
		if controller then
			controller:Destroy()
		end
		controller = AgentControl.mount(app)
		app.agentController = controller
	end
	for _, other in controller.Parent:GetChildren() do
		if other ~= controller and isRojoController(other) and other:GetAttribute("SyncActive") then
			return false
		end
	end
	-- Claim without yielding so UI, auto-connect and MCP all share one owner.
	controller:SetAttribute("SyncActive", true)
	return true
end

function AgentControl.state(app)
	local session = app.serveSession
	local host, port = app:getHostAndPort()
	return {
		protocolVersion = 1,
		pluginVersion = version,
		placeId = game.PlaceId,
		status = app.state.appStatus,
		error = if app.state.appStatus == "Error" then tostring(app.state.errorMessage) else nil,
		host = host,
		port = port,
		confirmation = app.agentConfirmation,
		syncStatus = if session then session:getStatus() else "Disconnected",
		serverVersion = if session then session.__serverVersion else nil,
		phase = if session then session.__phase else nil,
		phaseSeconds = if session and session.__phaseStartedAt then os.clock() - session.__phaseStartedAt else nil,
		requests = if session then table.clone(session.__apiContext.__requestTimings) else {},
		bridge = if session then session:getBridgeStatus() else { available = false },
	}
end

-- Agent control is on by default, so the first change it makes in a session
-- is announced where the user can see it.
local warnedMutation = false

local function announceMutation(action)
	if warnedMutation then
		return
	end
	warnedMutation = true
	-- Not Log.warn, which a lowered log level would hide.
	warn(
		string.format(
			"[Rojo] Agent control performed '%s' for another plugin, the command bar or a Studio MCP tool. This is only reported once per session. Turn off agent control in Rojo settings if you did not expect this.",
			action
		)
	)
end

-- Lets tests see the warning again.
function AgentControl._resetMutationWarning()
	warnedMutation = false
end

function AgentControl.handle(app, request)
	assert(Settings:get("agentControlEnabled"), "Agent control is turned off in Rojo settings")
	assert(type(request) == "table", "Expected a request table")
	local action = request.action
	if action == "status" then
		return AgentControl.state(app)
	elseif action == "capabilities" then
		return {
			protocolVersion = 1,
			pluginVersion = version,
			actions = {
				"status",
				"capabilities",
				"connect",
				"disconnect",
				"confirm",
				"settings.get",
				"settings.set",
				"bridge.retry",
			},
			settings = table.clone(settingTypes),
			versionActivation = "Install a versioned local plugin, reload Studio, then select its exact controller. Legacy plugins cannot be enabled or disabled through this API.",
		}
	elseif action == "settings.get" then
		local values = {}
		for name in settingTypes do
			values[name] = Settings:get(name)
		end
		return values
	end
	assert(RunService:IsEdit(), "Plugin controls require Studio Edit mode")
	assert(request.expectedPlaceId == game.PlaceId, "expectedPlaceId must match the selected Studio place")
	assert(request.expectedVersion == version, "expectedVersion must match this plugin controller")
	if
		action == "connect"
		or action == "disconnect"
		or action == "confirm"
		or action == "settings.set"
		or action == "bridge.retry"
	then
		announceMutation(action)
	end
	if action == "connect" then
		assert(not app.serveSession, "Disconnect this controller before changing endpoints")
		assert(
			request.host == "127.0.0.1" or request.host == "localhost" or request.host == "[::1]",
			"Use an explicit loopback host"
		)
		assert(
			type(request.port) == "number" and request.port % 1 == 0 and request.port >= 1 and request.port <= 65535,
			"Use an integer port from 1 to 65535"
		)
		app.setHost(request.host)
		app.setPort(tostring(request.port))
		app:startSession()
	elseif action == "disconnect" then
		app:endSession()
	elseif action == "confirm" then
		assert(
			app.agentConfirmation and request.confirmationId == app.agentConfirmation.id,
			"Read status and use the current confirmationId"
		)
		-- Accepting applies the server's changes and rejecting writes Studio's
		-- version to the files, so both skip the user's review. Abort is safe.
		assert(
			request.decision == "Abort" or Settings:get("agentControlCanChangeConfirmation"),
			"Agent control can only abort a sync confirmation. Confirm it in Studio, or allow agent control to change sync confirmation in Rojo settings."
		)
		-- Reject is only offered by the confirmation UI when two-way sync is on.
		local canReject = Settings:get("twoWaySync")
		assert(
			request.decision == "Accept" or request.decision == "Abort" or (canReject and request.decision == "Reject"),
			if canReject then "decision must be Accept, Abort or Reject" else "decision must be Accept or Abort"
		)
		app.agentConfirmation = nil
		app.confirmationBindable:Fire(request.decision)
	elseif action == "settings.set" then
		assert(not app.serveSession, "Disconnect before changing settings so they apply to the next connection")
		AgentControl.validateSetting(request.name, request.value)
		AgentControl.checkConfirmationChange(request.name, request.value)
		Settings:set(request.name, request.value)
	elseif action == "bridge.retry" then
		assert(app.serveSession, "Connect sync before retrying the bridge")
		-- An explicit retry starts the bridge even when automatic enabling is off.
		StudioBridge.manualStart = true
		local ok, err = pcall(app.serveSession.restartStudioBridge, app.serveSession)
		StudioBridge.manualStart = false
		assert(ok, err)
	else
		error("Unknown action; inspect capabilities")
	end
	return AgentControl.state(app)
end

function AgentControl.confirmation(patch, projectName)
	return {
		id = HttpService:GenerateGUID(false),
		projectName = projectName,
		changedInstances = PatchSet.countInstances(patch),
	}
end

function AgentControl.mount(app)
	local coreGui = game:GetService("CoreGui")
	local folder = coreGui:FindFirstChild("RojoAgentControls")
	if not folder then
		folder = Instance.new("Folder")
		folder.Name = "RojoAgentControls"
		folder.Parent = coreGui
	end
	local controller = Instance.new("BindableFunction")
	controller.Name = version .. "-" .. HttpService:GenerateGUID(false)
	controller:SetAttribute("PluginVersion", version)
	controller:SetAttribute("ProtocolVersion", 1)
	controller.OnInvoke = function(request)
		local ok, result = pcall(AgentControl.handle, app, request)
		return if ok then { ok = true, value = result } else { ok = false, error = tostring(result) }
	end
	controller.Parent = folder
	return controller
end

return AgentControl
