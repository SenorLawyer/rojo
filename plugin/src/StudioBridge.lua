local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local LogService = game:GetService("LogService")
local Selection = game:GetService("Selection")
local SerializationService = game:GetService("SerializationService")
-- Missing on some Studio builds. Without it the bridge cannot move models,
-- so it stays off instead of stopping the whole plugin from loading.
local hasEncodingService, EncodingService = pcall(game.GetService, game, "EncodingService")
if not hasEncodingService then
	EncodingService = nil
end
local ChangeHistoryService = game:GetService("ChangeHistoryService")

local Packages = script.Parent.Parent.Packages
local Promise = require(Packages.Promise)
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)
local ReflectionDatabase = require(Packages.RbxDom.database)
local Settings = require(script.Parent.Settings)
local Config = require(script.Parent.Config)
local Version = require(script.Parent.Version)
local SpecRunner = require(script.Parent.SpecRunner)
local StreamRequest = require(Packages.Http.StreamRequest)
local StreamBudget = require(Packages.Http.StreamBudget)

local MAX_MODEL_BYTES = 5 * 1024 * 1024
local MAX_BODY_BYTES = 8 * 1024 * 1024
-- The stream stops buffering a response past this, a little above
-- MAX_BODY_BYTES so an oversized body is still reported as such.
local MAX_STREAM_BYTES = 12 * 1024 * 1024
-- The server holds every /api/studio request for up to 15 seconds while Git
-- holds its locks (GIT_WAIT_LIMIT in src/web/studio.rs), and then holds a
-- poll for up to 2 more seconds. 30 covers both with room for Studio's own
-- HTTP queue, so ordinary Git activity does not look like a dead server.
local REQUEST_TIMEOUT = 30
-- A request that times out or fails without a client error status is sent
-- again, up to this many attempts in a row, before the bridge stops. A poll
-- that took the whole timeout usually outlived its session on the server, in
-- which case the retry gets 404 and the bridge stops; the retries mainly help
-- with transport errors and 5xx responses. The one exception is the first
-- poll after Run mode, which registers the session again instead.
local MAX_REQUEST_ATTEMPTS = 3
-- IDs are held strongly so Roblox cannot collect an Instance proxy while the
-- instance still exists. The oldest IDs are forgotten beyond this bound.
local MAX_TRACKED_INSTANCES = 20_000
local MAX_LOG_BYTES = 4096
local MAX_LOG_ENTRIES = 200
local MAX_SANITIZE_DEPTH = 64
local REPLACEMENT_CHARACTER = utf8.char(0xFFFD)

local StudioBridge = {}
StudioBridge.__index = StudioBridge
-- One request at a time, on the stream slot the budget holds back for the
-- bridge, so agent traffic never waits behind sync or delays it.
-- Replaced by tests.
StudioBridge.bridgeStream = StreamRequest.new(nil, nil, StreamBudget.Priority.Bridge)

--[[
	Sends one bridge request and returns its response. Replaced by tests.

	Requests go over an HTTP stream, not RequestAsync. Once a plugin uses
	RequestAsync for requests that the server holds open, as a poll is,
	Studio delays that plugin's later RequestAsync calls more and more until
	it reloads, so a polling bridge fell behind until its session expired.
	Streams are not affected and can be closed when the bridge stops. The
	bodies are JSON, which never contains the NUL bytes a raw stream cuts
	at. Only when no stream slot is free, which needs another Rojo session in
	the same plugin, does a request fall back to RequestAsync.

	`onCancel` registers a function that closes the stream if the caller
	gives up.
]]
function StudioBridge.requestAsync(options, onCancel)
	local pending = StudioBridge.bridgeStream(options, {
		timeout = REQUEST_TIMEOUT,
		queueTimeout = REQUEST_TIMEOUT,
		maxBytes = MAX_STREAM_BYTES,
	})
	if onCancel then
		onCancel(function()
			pending:cancel()
		end)
	end
	local ok, response = pending:await()
	if not ok then
		error(response, 0)
	end
	if response == StreamRequest.BudgetExhausted then
		Log.debug("No stream slot is free for the Studio bridge; using RequestAsync")
		return StudioBridge.fallbackRequestAsync(options)
	end
	return response
end

-- Sends a bridge request with RequestAsync when no stream slot is free.
-- Replaced by tests.
function StudioBridge.fallbackRequestAsync(options)
	return HttpService:RequestAsync(options)
end

-- How the test runner's insert and removal are kept in undo history. See
-- recordRunnerChange.
StudioBridge.runnerHistoryOperation = Enum.FinishRecordingOperation.Append

-- Copied into the spec runner. Replaced by tests.
StudioBridge.specRunnerModule = script.Parent.SpecRunner

-- While Studio is in Run mode the worker cannot poll, and the server expires a
-- session that has not been heard from in 30 seconds. A run therefore checks
-- in this often (seconds), which also keeps the session listed. Replaced by
-- tests.
StudioBridge.heartbeatInterval = 5

-- How long past its expected duration a run is given before the runner is
-- removed and the heartbeats stop (seconds). Replaced by tests.
StudioBridge.runModeGrace = REQUEST_TIMEOUT

-- A spec run starts only with at least this many seconds of its request's
-- deadline left beyond its own timeout, for Studio to start and leave Run
-- mode. The server allows 30 (SPEC_RUN_MARGIN in src/web/studio.rs), so a
-- run that waited behind another one may have too little left.
local SPEC_START_MARGIN = 10

--[[
	Starts Run mode and waits for the runner to call EndTest. Replaced by tests.
]]
function StudioBridge.executeRunMode(args)
	local service = game:GetService("StudioTestService")
	local found, executeRunMode = pcall(function()
		return service.ExecuteRunModeAsync
	end)
	assert(found and executeRunMode, "StudioTestService.ExecuteRunModeAsync is unavailable in this Studio version")
	return executeRunMode(service, args)
end

-- AgentControl sets this around an explicit bridge.retry so a manual start is
-- allowed even when automatic enabling is turned off in settings.
StudioBridge.manualStart = false

-- Seconds to wait before resending a failed request, multiplied by the
-- attempt number. Replaced by tests.
StudioBridge.retryDelay = 0.5

-- Replaced by tests.
StudioBridge.encodingAvailable = EncodingService ~= nil
StudioBridge.maxTrackedInstances = MAX_TRACKED_INSTANCES

-- Servers this plugin load has already told the user about, so the notice
-- that the bridge is active prints once per server rather than per reconnect.
local announcedServers = {}

-- Cut on a codepoint boundary so truncating cannot split a multibyte character.
function StudioBridge.truncateUtf8(value, maxBytes)
	if #value <= maxBytes then
		return value
	end
	local cut = maxBytes
	while cut > 0 do
		local nextByte = string.byte(value, cut + 1)
		if nextByte < 0x80 or nextByte >= 0xC0 then
			break
		end
		cut -= 1
	end
	return string.sub(value, 1, cut)
end

function StudioBridge.toValidUtf8(value)
	if not utf8.len(value) then
		local parts = {}
		local position = 1
		while position <= #value do
			local length, invalid = utf8.len(value, position)
			if length then
				table.insert(parts, string.sub(value, position))
				break
			end
			table.insert(parts, string.sub(value, position, invalid - 1))
			table.insert(parts, REPLACEMENT_CHARACTER)
			-- Luau's utf8.len rejects encoded UTF-16 surrogates. Replace the
			-- whole three-byte sequence with one character, not one per byte.
			if string.find(value, "^\237[\160-\191][\128-\191]", invalid) then
				position = invalid + 3
			else
				position = invalid + 1
			end
		end
		value = table.concat(parts)
	end
	-- In case utf8.len accepts encoded UTF-16 surrogates, which JSON text
	-- cannot carry, replace any that remain.
	return (string.gsub(value, "\237[\160-\191][\128-\191]", REPLACEMENT_CHARACTER))
end

-- JSONEncode throws or emits invalid JSON for non-finite numbers, invalid
-- UTF-8 and Roblox datatypes. Values read from a place can contain any of
-- these, so every outgoing body is converted to plain JSON data first.
function StudioBridge.sanitize(value, depth)
	depth = depth or 0
	local kind = type(value)
	if kind == "number" then
		if value ~= value then
			return { type = "nonFiniteNumber", value = "nan" }
		elseif value == math.huge then
			return { type = "nonFiniteNumber", value = "inf" }
		elseif value == -math.huge then
			return { type = "nonFiniteNumber", value = "-inf" }
		end
		return value
	elseif kind == "string" then
		return StudioBridge.toValidUtf8(value)
	elseif kind == "boolean" or kind == "nil" then
		return value
	elseif kind == "table" then
		assert(depth < MAX_SANITIZE_DEPTH, "Studio bridge value is nested too deeply")
		local result = {}
		for key, child in value do
			if type(key) ~= "number" then
				key = StudioBridge.toValidUtf8(tostring(key))
			end
			result[key] = StudioBridge.sanitize(child, depth + 1)
		end
		return result
	end
	return { type = typeof(value), display = StudioBridge.toValidUtf8(tostring(value)) }
end

-- Paths are '/'-joined names starting at `game`. A '/' or '\' inside a name is
-- escaped with '\' so every instance has a path that `resolve` accepts.
local function escapeName(name)
	return (string.gsub(name, "[\\/]", "\\%0"))
end

local function splitPath(reference)
	local parts = {}
	local current = {}
	local index = 1
	while index <= #reference do
		local character = string.sub(reference, index, index)
		if character == "\\" and index < #reference then
			index += 1
			table.insert(current, string.sub(reference, index, index))
		elseif character == "/" then
			table.insert(parts, table.concat(current))
			current = {}
		else
			table.insert(current, character)
		end
		index += 1
	end
	table.insert(parts, table.concat(current))
	return parts
end

function StudioBridge.pathOf(instance)
	local segments = {}
	local current = instance
	while current and current ~= game do
		table.insert(segments, 1, current.Name)
		current = current.Parent
	end
	local escaped = { "game" }
	for _, name in segments do
		table.insert(escaped, escapeName(name))
	end
	table.insert(segments, 1, "game")
	return table.concat(escaped, "/"), segments
end

-- `sync`, when given, connects the bridge to the sync session it runs beside:
-- `getSummary()` for the health summary sent with every poll,
-- `getSourceHashes()` for `rojo studio verify` and `applyResync(patch)` for
-- `rojo studio verify --fix`.
function StudioBridge.new(apiContext, pairing, onFailure, sync)
	assert(
		pairing.protocolVersion == 1 and type(pairing.pluginToken) == "string" and type(pairing.identity) == "table",
		"Unsupported Studio bridge pairing"
	)
	assert(
		apiContext.__baseUrl:match("^http://localhost:%d+$")
			or apiContext.__baseUrl:match("^http://127%.0%.0%.1:%d+$")
			or apiContext.__baseUrl:match("^http://%[::1%]:%d+$"),
		"Studio control requires a loopback Rojo address"
	)
	local self = setmetatable({
		apiContext = apiContext,
		pairing = pairing,
		onFailure = onFailure,
		sync = sync,
		sessionId = HttpService:GenerateGUID(false),
		stopped = false,
		transports = {},
		testRunners = {},
		logs = {},
		-- Numbers every captured message so `rojo studio logs --follow` can ask
		-- for what is new since its last read. It keeps counting when the
		-- buffer is trimmed or cleared, so a follower can tell what it missed.
		logSequence = 0,
	}, StudioBridge)
	self:__resetInstances()
	return self
end

function StudioBridge:__resetInstances()
	if self.instanceConnections then
		for _, connection in self.instanceConnections do
			connection:Disconnect()
		end
	end
	self.instances = {}
	self.ids = {}
	self.instanceConnections = {}
	-- A queue with explicit ends. `#` is unreliable once the front entries
	-- are cleared, which would stop eviction and let IDs grow without bound.
	self.instanceOrder = {}
	self.instanceOrderHead = 1
	self.instanceOrderTail = 0
end

function StudioBridge:__forget(id)
	local instance = self.instances[id]
	if instance then
		self.ids[instance] = nil
	end
	self.instances[id] = nil
	local connection = self.instanceConnections[id]
	if connection then
		connection:Disconnect()
		self.instanceConnections[id] = nil
	end
end

function StudioBridge:__track(instance)
	local id = HttpService:GenerateGUID(false)
	self.ids[instance] = id
	self.instances[id] = instance
	self.instanceConnections[id] = instance.Destroying:Connect(function()
		self:__forget(id)
	end)
	self.instanceOrderTail += 1
	self.instanceOrder[self.instanceOrderTail] = id
	while self.instanceOrderTail - self.instanceOrderHead + 1 > StudioBridge.maxTrackedInstances do
		local oldest = self.instanceOrder[self.instanceOrderHead]
		self.instanceOrder[self.instanceOrderHead] = nil
		self.instanceOrderHead += 1
		self:__forget(oldest)
	end
	return id
end

function StudioBridge:describe(instance)
	local id = if instance == game then "game" else self.ids[instance]
	if not id then
		id = self:__track(instance)
	end
	local path, segments = StudioBridge.pathOf(instance)
	return {
		id = id,
		name = instance.Name,
		className = instance.ClassName,
		-- `path` and `pathSegments` are both accepted by `resolve`. `fullName`
		-- keeps Roblox's dotted display form.
		path = path,
		pathSegments = segments,
		fullName = instance:GetFullName(),
	}
end

function StudioBridge:resolve(reference)
	local parts
	if type(reference) == "table" then
		assert(#reference >= 1 and #reference <= 512, "An instance path needs 1 to 512 names")
		for _, name in reference do
			assert(type(name) == "string" and #name <= 2048, "Instance path names must be strings")
		end
		parts = reference
	else
		assert(
			type(reference) == "string" and #reference <= 2048,
			"An explicit instance ID or game/... path is required"
		)
		if reference == "game" then
			return game
		end
		local known = self.instances[reference]
		if known and known:IsDescendantOf(game) then
			return known
		end
		parts = splitPath(reference)
	end
	assert(parts[1] == "game", "Unknown instance ID. Inspect the tree again for current IDs.")
	local current = game
	for index = 2, #parts do
		local found
		for _, child in current:GetChildren() do
			if child.Name == parts[index] then
				assert(not found, "Ambiguous path: use the exact instance ID from tree inspection")
				found = child
			end
		end
		assert(found, "Instance path no longer exists: " .. table.concat(parts, "/"))
		current = found
	end
	return current
end

function StudioBridge:capabilities()
	return {
		status = { available = true },
		tree = { available = true, maxInstances = 2000, maxDepth = 10 },
		properties = { available = true, maxProperties = 64 },
		selection = { available = true },
		logs = {
			available = true,
			maxEntries = MAX_LOG_ENTRIES,
			follow = true,
			scope = "Messages since this bridge session connected",
		},
		export = { available = true, format = "Native SerializationService model bytes", maxBytes = MAX_MODEL_BYTES },
		import = {
			available = RunService:IsEdit(),
			reason = "Validated model insertion is available in Edit mode only; scripts require opt-in and remain disabled",
		},
		execute = { available = false, reason = "An unyielding Luau program cannot be interrupted safely by a plugin" },
		placeSave = {
			available = false,
			reason = "SerializationService serializes model instances and rejects services; no whole-place save or publish is performed",
		},
		playControl = {
			available = false,
			reason = "Cross-mode test lifecycle and cancellation are not enabled by this bridge; use Studio controls",
		},
		test = {
			available = RunService:IsEdit(),
			mode = "Run",
			maxSimulationSeconds = 5,
			reason = "StudioTestService.ExecuteRunModeAsync runs existing game scripts with a controlled EndTest runner",
		},
		spec = {
			available = RunService:IsEdit(),
			mode = "Run",
			frameworks = { "testez", "jest" },
			maxTimeoutSeconds = SpecRunner.MAX_TIMEOUT_SECONDS,
			maxFailures = SpecRunner.MAX_FAILURES,
			reason = "Runs the place's *.spec modules in Run mode with a fixed runner and returns a bounded report",
		},
		sources = {
			available = self.sync ~= nil,
			reason = "Hashes of the scripts Rojo syncs, compared with the server's files by rojo studio verify",
		},
		resync = {
			available = self.sync ~= nil,
			reason = "Sets the source of scripts that rojo studio verify --fix found different from their files",
		},
	}
end

-- Registered once and compared on a retried registration, so it holds only
-- what cannot change during the bridge session. Sync health changes all the
-- time and goes with each poll instead (see `syncSummary`).
function StudioBridge:status()
	return {
		sessionId = self.sessionId,
		placeId = game.PlaceId,
		placeName = game.Name,
		pluginVersion = Version.display(Config.version),
		mode = if RunService:IsEdit() then "Edit" else "Play",
		capabilities = self:capabilities(),
	}
end

function StudioBridge:syncSummary()
	if not self.sync then
		return nil
	end
	local ok, summary = pcall(self.sync.getSummary)
	if ok then
		return summary
	end
	Log.debug("Could not read the sync summary for the Studio bridge: {}", summary)
	return nil
end

local function encodeBody(value)
	local body = HttpService:JSONEncode(StudioBridge.sanitize(value))
	assert(#body <= MAX_BODY_BYTES, "Studio bridge request exceeds 8MiB")
	return body
end

-- `failure`, when passed, receives the response's `status` if the server
-- answered, so callers can tell a server refusal from a lost request.
function StudioBridge:request(route, value, failure)
	value.identity = self.pairing.identity
	value.sessionId = self.sessionId
	local encoded, body = pcall(encodeBody, value)
	if not encoded and route == "result" then
		-- A result that cannot be sent must still answer its request, otherwise
		-- the same query would stop the bridge every time it is retried.
		value.result = { ok = false, error = "Studio could not encode this result: " .. tostring(body) }
		encoded, body = pcall(encodeBody, value)
	end
	assert(encoded, body)
	local token = {}
	local request = Promise.new(function(resolve, _reject, onCancel)
		-- Cancelling this promise closes its thread, so the transport has to be
		-- cancelled here or the request would stay tracked in `transports`.
		onCancel(function()
			token.cancelled = true
			if token.transport then
				token.transport:cancel()
			end
		end)
		local response = self:__requestAsync({
			Url = self.apiContext.__baseUrl .. "/api/studio/" .. route,
			Method = "POST",
			Headers = { ["Content-Type"] = "application/json", ["x-rojo-studio-token"] = self.pairing.pluginToken },
			Body = body,
		}, token)
		if failure then
			failure.status = response.StatusCode
		end
		assert(#response.Body <= MAX_BODY_BYTES, "Studio bridge response exceeds 8MiB")
		if route == "result" and response.StatusCode == 410 then
			-- A yielding command may finish after its caller cancels or times out.
			-- Discard that result without terminating this session or normal sync.
			-- A transport that reports the status without the server's body
			-- marks it with BodyFromServer = false, and that body is not read.
			local reason = "Request expired or was cancelled"
			if response.BodyFromServer ~= false then
				local decoded, discarded = pcall(HttpService.JSONDecode, HttpService, response.Body)
				if decoded and type(discarded) == "table" and type(discarded.error) == "string" then
					reason = discarded.error
				end
			end
			resolve({ discarded = true, reason = reason })
			return
		end
		assert(response.Success, "Studio bridge request failed: " .. response.StatusCode .. " " .. response.Body)
		resolve(HttpService:JSONDecode(response.Body))
	end):timeout(
		REQUEST_TIMEOUT,
		"Studio bridge " .. route .. " request timed out after " .. REQUEST_TIMEOUT .. " seconds"
	)
	self.pendingRequest = request
	return request:finally(function()
		if self.pendingRequest == request then
			self.pendingRequest = nil
		end
	end)
end

--[[
	Sends a request, sending it again when it timed out, failed in transport
	or got a 5xx status, because the server can hold a request while Git is
	busy. Client error statuses such as 401, 404 and 409 mean the server
	refused this session, so they are not retried. Returns the response and
	the number of attempts it took.

	Resending is safe for every route. Register accepts the same description
	twice, a poll delivers each command at most once, so a lost poll response
	is never answered twice, and a result for a request the server already
	settled is answered with 410, not applied again.

	`lastFailure`, when passed, receives the `status` of the last attempt, so
	a caller can tell which refusal it was.
]]
function StudioBridge:__requestWithRetry(route, value, lastFailure)
	local attempt = 0
	while true do
		attempt += 1
		local failure = {}
		local ok, response = self:request(route, value, failure):await()
		if lastFailure then
			lastFailure.status = failure.status
		end
		if ok then
			return response, attempt
		end
		local transient = failure.status == nil or failure.status >= 500
		if self.stopped or not transient or attempt >= MAX_REQUEST_ATTEMPTS then
			error(response, 0)
		end
		Log.debug("Studio bridge {} request failed ({}); retrying", route, tostring(response))
		task.wait(StudioBridge.retryDelay * attempt)
		if self.stopped then
			error(response, 0)
		end
	end
end

function StudioBridge:__requestAsync(options, token)
	local request = Promise.new(function(resolve, reject, onCancel)
		-- Cancelling a promise closes its executor thread, so the request runs
		-- on its own thread rather than being closed while Studio sends it,
		-- and cancelling closes its stream instead.
		task.spawn(function()
			local ok, response = pcall(StudioBridge.requestAsync, options, onCancel)
			if ok then
				resolve(response)
			else
				reject(response)
			end
		end)
	end)
	-- Every in-flight transport is tracked so stop() can release all of them.
	self.transports[request] = true
	-- The failure is reported through `await` below. The chained promise
	-- would otherwise warn about an unhandled rejection for every failure.
	request
		:finally(function()
			self.transports[request] = nil
		end)
		:catch(function() end)
	if token then
		token.transport = request
		if token.cancelled then
			request:cancel()
		end
	end
	local ok, response = request:await()
	assert(ok, tostring(response))
	return response
end

-- Services an import may target, by class. Others, such as CoreGui,
-- CorePackages and the plugin GUI and debug services, hold Studio's and
-- plugins' own instances, including Rojo's agent controllers, which an agent
-- must not be able to replace or shadow.
local IMPORTABLE_SERVICES = {
	Workspace = true,
	Lighting = true,
	MaterialService = true,
	ReplicatedFirst = true,
	ReplicatedStorage = true,
	ServerScriptService = true,
	ServerStorage = true,
	StarterGui = true,
	StarterPack = true,
	StarterPlayer = true,
	Teams = true,
	SoundService = true,
	Chat = true,
	TextChatService = true,
	LocalizationService = true,
	TestService = true,
}

function StudioBridge.checkImportParent(parent)
	assert(parent ~= game, "Import beneath an explicit model/folder/service, not DataModel")
	local service = parent
	while service.Parent ~= game do
		service = service.Parent
		assert(service ~= nil, "Import parent must be inside one of the place's services")
	end
	assert(
		IMPORTABLE_SERVICES[service.ClassName] == true and table.find(game:GetChildren(), service) ~= nil,
		"Import cannot target "
			.. service.ClassName
			.. ". Import into a service such as Workspace or ReplicatedStorage."
	)
end

function StudioBridge:ensureActive(command)
	assert(
		not self.stopped and DateTime.now().UnixTimestampMillis < command.expiresAt,
		"Studio request expired or was cancelled"
	)
end

-- Inserting and removing the runner are each appended to the latest undo
-- step instead of committed as steps of their own. As separate steps,
-- undoing the removal would bring the runner back into the place. Appended,
-- both land in the same step, where undoing or redoing it applies the
-- insert and the removal together and leaves no runner.
function StudioBridge.recordRunnerChange(name, callback)
	local recording = ChangeHistoryService:TryBeginRecording(name)
	local ok, err = pcall(callback)
	if recording then
		ChangeHistoryService:FinishRecording(
			recording,
			if ok then StudioBridge.runnerHistoryOperation else Enum.FinishRecordingOperation.Cancel
		)
	end
	return ok, err
end

--[[
	Runs `runner`, a Script the caller prepared, in Run mode and removes it
	afterwards. `seconds` is how long the run is expected to take at most.
	Returns whether Run mode succeeded, what the runner passed to EndTest and
	the elapsed time.
]]
function StudioBridge:__runInRunMode(command, runner, args, seconds)
	local function removeRunner()
		self.testRunners[runner] = nil
		if runner.Parent then
			StudioBridge.recordRunnerChange("Rojo: Remove Studio test runner", function()
				runner:Destroy()
			end)
		else
			runner:Destroy()
		end
	end

	self.testRunners[runner] = true
	-- If Studio never returns from the run, remove the runner anyway so it is
	-- not saved or published with the place.
	local grace = seconds + StudioBridge.runModeGrace
	local watchdog = task.delay(grace, removeRunner)
	local heartbeatsEnd = os.clock() + grace
	local running = true
	task.spawn(function()
		while true do
			task.wait(StudioBridge.heartbeatInterval)
			-- Past the watchdog's time the run is presumed hung, and its
			-- heartbeats would only keep a session that cannot answer listed.
			if not running or self.stopped or os.clock() >= heartbeatsEnd then
				return
			end
			-- Failures are left to the command's own result. A cancelled or
			-- expired request answers 410 here, but Run mode cannot be stopped
			-- from the plugin, so the run continues and its result is
			-- discarded. The heartbeats still go on: they keep the session
			-- registered, so the bridge can poll again once Run mode ends.
			self:request("validate", { requestId = command.requestId }):await()
		end
	end)
	local parented, parentError = StudioBridge.recordRunnerChange("Rojo: Insert Studio test runner", function()
		runner.Parent = game:GetService("ServerScriptService")
	end)
	local started = os.clock()
	local ok, result = false, parentError
	if parented then
		ok, result = pcall(StudioBridge.executeRunMode, args)
	end
	running = false
	if coroutine.status(watchdog) == "suspended" then
		task.cancel(watchdog)
	end
	removeRunner()
	return ok, result, os.clock() - started
end

function StudioBridge:__runTest(command, seconds)
	local runner = Instance.new("Script")
	runner.Name = "__RojoBridgeTest_" .. self.sessionId
	runner.Source = [[local service = game:GetService("StudioTestService")
local args = service:GetTestArgs()
task.wait(args.simulationSeconds)
service:EndTest({ completed = true, simulationSeconds = args.simulationSeconds, requestId = args.requestId })]]
	return self:__runInRunMode(command, runner, { simulationSeconds = seconds, requestId = command.requestId }, seconds)
end

-- The spec runner's fixed source. It only reads options from GetTestArgs and
-- runs the SpecRunner module copied beside it, so a caller passes data, never
-- code. Any error still ends the run with a report, because Run mode would
-- otherwise continue until someone stops it in Studio.
StudioBridge.SPEC_RUNNER_SOURCE = [[local service = game:GetService("StudioTestService")
local ok, report = pcall(function()
	return require(script:WaitForChild("SpecRunner")).runWithTimeout(service:GetTestArgs())
end)
service:EndTest(if ok then report else { framework = "testez", status = "error", error = tostring(report) })]]

function StudioBridge:__runSpecs(command, options)
	local runner = Instance.new("Script")
	runner.Name = "__RojoBridgeSpec_" .. self.sessionId
	runner.Source = StudioBridge.SPEC_RUNNER_SOURCE
	local module = StudioBridge.specRunnerModule:Clone()
	module.Name = "SpecRunner"
	module.Parent = runner
	local ok, report, elapsed = self:__runInRunMode(command, runner, options, options.timeoutSeconds)
	if ok and type(report) ~= "table" then
		ok, report = false, "The spec runner ended Run mode without a report"
	end
	return ok, report, elapsed
end

function StudioBridge:__recordLog(message, kind)
	self.logSequence += 1
	table.insert(self.logs, {
		sequence = self.logSequence,
		message = StudioBridge.truncateUtf8(message, MAX_LOG_BYTES),
		kind = kind,
		timestamp = DateTime.now().UnixTimestampMillis,
	})
	if #self.logs > MAX_LOG_ENTRIES then
		table.remove(self.logs, 1)
	end
end

--[[
	Chooses the entries a `logs` command returns.

	Without `after`, returns the newest `limit` entries as a list, which is
	what CLIs from before follow mode expect. With `after`, returns the oldest
	`limit` entries whose sequence is greater, along with:

	- `latestSequence`: the sequence of the newest message captured so far.
	- `nextAfter`: the `after` to send next. It is `latestSequence` unless
	  `more` is true, in which case more entries are waiting.
	- `dropped`: how many entries after `after` were trimmed from the buffer,
	  or cleared when the bridge stopped, before they could be read.
	- `restarted`: true when `after` was ahead of `latestSequence`, so the
	  entries start over from the oldest one still kept.
]]
function StudioBridge.selectLogs(logs, latestSequence, args)
	local limit = math.clamp(tonumber(args.limit) or 100, 1, MAX_LOG_ENTRIES)
	local after = tonumber(args.after)
	if after == nil then
		local result = {}
		for index = math.max(1, #logs - limit + 1), #logs do
			table.insert(result, logs[index])
		end
		return result
	end

	after = math.max(0, math.floor(after))
	local restarted = after > latestSequence
	if restarted then
		after = 0
	end

	local oldest = if logs[1] then logs[1].sequence else latestSequence + 1
	local entries = {}
	local more = false
	for _, entry in logs do
		if entry.sequence > after then
			if #entries >= limit then
				more = true
				break
			end
			table.insert(entries, entry)
		end
	end

	return {
		entries = entries,
		latestSequence = latestSequence,
		nextAfter = if more then entries[#entries].sequence else latestSequence,
		dropped = math.max(0, oldest - after - 1),
		more = more,
		restarted = restarted,
	}
end

function StudioBridge:execute(command)
	self:ensureActive(command)
	local args = command.args or {}
	if command.action == "status" then
		local status = self:status()
		status.sync = self:syncSummary()
		return status
	elseif command.action == "sources" then
		assert(self.sync, "This bridge is not attached to a sync session")
		return self.sync.getSourceHashes()
	elseif command.action == "resync" then
		assert(self.sync, "This bridge is not attached to a sync session")
		return self.sync.applyResync(args.patch, args.expect, args.includeHeld == true, command.expiresAt)
	elseif command.action == "selection" then
		local result = {}
		for index, instance in Selection:Get() do
			if index > 2000 then
				break
			end
			table.insert(result, self:describe(instance))
		end
		return result
	elseif command.action == "logs" then
		return StudioBridge.selectLogs(self.logs, self.logSequence, args)
	elseif command.action == "tree" then
		local limit = math.clamp(tonumber(args.limit) or 500, 1, 2000)
		local maxDepth = math.clamp(tonumber(args.depth) or 3, 0, 10)
		local queue = { { instance = self:resolve(args.instance or "game"), depth = 0 } }
		local nodes = {}
		local cursor = 1
		-- Queue one node past the limit so `truncated` means something was left out.
		while cursor <= #queue and #nodes < limit do
			local current = queue[cursor]
			cursor += 1
			local description = self:describe(current.instance)
			description.depth = current.depth
			description.parentId = current.parentId
			table.insert(nodes, description)
			if current.depth < maxDepth then
				for _, child in current.instance:GetChildren() do
					if #queue > limit then
						break
					end
					table.insert(queue, { instance = child, depth = current.depth + 1, parentId = description.id })
				end
			end
		end
		return { nodes = nodes, limit = limit, depth = maxDepth, truncated = cursor <= #queue }
	elseif command.action == "properties" then
		local instance = self:resolve(args.instance)
		assert(
			type(args.properties) == "table" and #args.properties <= 64,
			"Request at most 64 explicit property names"
		)
		local values = {}
		for _, property in args.properties do
			assert(type(property) == "string" and #property <= 128, "Invalid property name")
			local ok, value = pcall(function()
				return instance[property]
			end)
			if not ok then
				values[property] = { error = tostring(value) }
			elseif typeof(value) == "Instance" then
				values[property] = { type = "Instance", value = self:describe(value) }
			elseif value == nil then
				values[property] = { type = "nil" }
			else
				local descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, property)
				local encoded, result = false, nil
				if descriptor then
					encoded, result = RbxDom.EncodedValue.encode(value, descriptor.dataType)
				end
				if encoded then
					values[property] = { type = typeof(value), value = result }
				elseif type(value) == "string" or type(value) == "boolean" or type(value) == "number" then
					values[property] = { type = typeof(value), value = value }
				else
					values[property] = { type = typeof(value), display = tostring(value) }
				end
			end
		end
		return { instance = self:describe(instance), properties = values }
	elseif command.action == "test" then
		assert(RunService:IsEdit(), "Native tests must start in Edit mode")
		local seconds = tonumber(args.seconds) or 1
		assert(seconds >= 1 and seconds <= 5 and seconds % 1 == 0, "Native test duration must be 1-5 seconds")
		self:request("validate", { requestId = command.requestId }):expect()
		self:ensureActive(command)
		local ok, result, elapsed = self:__runTest(command, seconds)
		self:ensureActive(command)
		assert(ok, tostring(result))
		return { mode = "Run", result = result, elapsedSeconds = elapsed, isEdit = RunService:IsEdit() }
	elseif command.action == "spec" then
		assert(RunService:IsEdit(), "Specs must start in Edit mode")
		local options = SpecRunner.normalizeOptions(args)
		-- Resolve the roots and the framework now, so a wrong path or a
		-- missing TestEZ fails without starting Run mode.
		SpecRunner.prepare(options, game)
		self:request("validate", { requestId = command.requestId }):expect()
		self:ensureActive(command)
		-- A spec that waited behind another run may have too little of its
		-- deadline left. Starting it anyway would run the place's scripts for a
		-- result the server has already given up on.
		local remaining = (command.expiresAt - DateTime.now().UnixTimestampMillis) / 1000
		assert(
			remaining >= options.timeoutSeconds + SPEC_START_MARGIN,
			string.format(
				"Only %d seconds were left of this request's deadline when Studio could start it, "
					.. "too few for a %d-second spec run. Studio was probably busy with another run; run the specs again.",
				math.max(0, math.floor(remaining)),
				options.timeoutSeconds
			)
		)
		local ok, report, elapsed = self:__runSpecs(command, options)
		self:ensureActive(command)
		assert(ok, tostring(report))
		return { mode = "Run", report = report, elapsedSeconds = elapsed, isEdit = RunService:IsEdit() }
	elseif command.action == "export" then
		local instance = self:resolve(args.instance)
		assert(
			instance ~= game and instance.Parent ~= game,
			"Services and DataModel cannot be exported as native models"
		)
		assert(#instance:GetDescendants() <= 10_000, "Export exceeds 10,000 instances")
		local model = SerializationService:SerializeInstancesAsync({ instance })
		self:ensureActive(command)
		assert(buffer.len(model) <= MAX_MODEL_BYTES, "Export exceeds 5MiB")
		return {
			instance = self:describe(instance),
			model = buffer.tostring(EncodingService:Base64Encode(model)),
			bytes = buffer.len(model),
			format = "rbxm",
		}
	elseif command.action == "import" then
		assert(RunService:IsEdit(), "Native model import is restricted to Edit mode")
		assert(type(args.model) == "string" and #args.model <= 7 * 1024 * 1024, "Invalid or oversized model")
		local parent = self:resolve(args.parent)
		StudioBridge.checkImportParent(parent)
		local model = EncodingService:Base64Decode(buffer.fromstring(args.model))
		assert(buffer.len(model) <= MAX_MODEL_BYTES, "Import exceeds 5MiB")
		local objects = SerializationService:DeserializeInstancesAsync(model)
		local ok, result = pcall(function()
			self:ensureActive(command)
			self:request("validate", { requestId = command.requestId }):expect()
			self:ensureActive(command)
			assert(#objects > 0, "Import contains no instances")
			local count = 0
			for _, object in objects do
				local all = object:GetDescendants()
				table.insert(all, object)
				for _, child in all do
					count += 1
					assert(count <= 10_000, "Import exceeds 10,000 instances")
					assert(not child:IsA("ServiceProvider"), "Import cannot contain services or DataModel")
					local class = ReflectionDatabase.Classes[child.ClassName]
					assert(
						class and not table.find(class.Tags, "Service"),
						"Import cannot contain services or unknown classes"
					)
					if child:IsA("LuaSourceContainer") then
						assert(
							args.allowScripts == true,
							"Import contains script source. Explicit allowScripts is required."
						)
						if child:IsA("BaseScript") then
							child.Disabled = true
						end
					end
				end
			end
			local descriptions = {}
			local recording = ChangeHistoryService:TryBeginRecording("Rojo: Studio model import")
			local parented, parentError = pcall(function()
				for _, object in objects do
					object.Parent = parent
					table.insert(descriptions, self:describe(object))
				end
			end)
			if recording then
				ChangeHistoryService:FinishRecording(
					recording,
					if parented then Enum.FinishRecordingOperation.Commit else Enum.FinishRecordingOperation.Cancel
				)
			end
			assert(parented, tostring(parentError))
			return {
				instances = descriptions,
				count = count,
				persistence = "Studio session only. Export or save through Studio to preserve these instances.",
			}
		end)
		if not ok then
			for _, object in objects do
				object:Destroy()
			end
			error(result)
		end
		return result
	end
	error("Unsupported Studio action. Inspect capabilities for the precise availability contract.")
end

-- The bridge lets local agents read and change the place, so the user is told
-- once per server that it is running and how to keep it off.
function StudioBridge:__announce()
	local identity = self.pairing.identity
	local key = tostring(identity.serverSession) .. ":" .. tostring(identity.digest)
	if announcedServers[key] then
		return false
	end
	announcedServers[key] = true
	print(
		"[Rojo] The agent Studio bridge is active for this server, so local agents can read and change this place."
			.. " Turning off 'Auto Enable Studio Bridge' in Rojo's settings stops it from starting on its own;"
			.. " a local agent can still start it with an explicit bridge retry."
	)
	return true
end

function StudioBridge:start()
	if not StudioBridge.encodingAvailable then
		self.stopped = true
		self.onFailure("Studio bridge is unavailable because this Studio has no EncodingService.")
		return
	end
	if not StudioBridge.manualStart and not Settings:get("studioBridgeAutoEnable") then
		self.stopped = true
		-- The user chose this, so the session reports it without a warning.
		self.onFailure(
			"Studio bridge automatic enabling is turned off in Rojo settings. Start it with an explicit bridge retry.",
			true
		)
		return
	end
	self.logConnection = LogService.MessageOut:Connect(function(message, kind)
		if self.stopped then
			return
		end
		self:__recordLog(message, kind.Name)
	end)
	self.worker = task.spawn(function()
		local ok, err = pcall(function()
			local description = self:status()
			self:__requestWithRetry("register", { description = description })
			self:__announce()
			-- Set after a command that ran in Run mode, until the next poll
			-- succeeds. See below.
			local afterRunMode = false
			while not self.stopped do
				local failure = {}
				-- Each poll carries the sync session's current health, so agents
				-- can list every Studio's sync state without a command per session.
				local polled, response =
					pcall(self.__requestWithRetry, self, "poll", { sync = self:syncSummary() }, failure)
				if not polled then
					-- Studio cannot poll in Run mode, and if its heartbeats did not
					-- reach the server for long enough the session expired, which
					-- the next poll reports as 404. Registering the same session
					-- again with the same description is safe: the server adds it
					-- back, or refreshes it if it was still there. Only once, so a
					-- server that keeps refusing still stops the bridge. A session
					-- evicted by newer registrations during Run mode also comes
					-- back this way, evicting the oldest one in turn.
					if afterRunMode and failure.status == 404 and not self.stopped then
						afterRunMode = false
						Log.warn("Rojo Studio bridge session expired during Run mode; registering it again")
						self:__requestWithRetry("register", { description = description })
						continue
					end
					error(response, 0)
				end
				afterRunMode = false
				for _, command in response.commands do
					if self.stopped then
						break
					end
					if command.action == "test" or command.action == "spec" then
						afterRunMode = true
					end
					local success, value = pcall(self.execute, self, command)
					if self.stopped then
						if success and command.action == "import" then
							Log.warn(
								"Rojo Studio bridge stopped after applying an import, before its result was sent. The agent will see a failure, but the imported instances are in the place; undo 'Rojo: Studio model import' to remove them."
							)
						end
						break
					end
					local delivered, attempts = self:__requestWithRetry("result", {
						requestId = command.requestId,
						result = if success
							then { ok = true, value = value }
							else { ok = false, error = tostring(value) },
					})
					if success and command.action == "import" and delivered.discarded then
						if attempts > 1 then
							-- An earlier attempt may have reached the server, which
							-- answers a repeated result with 410 as well.
							Log.warn(
								"Rojo Studio bridge applied an import but could not confirm that its agent received the result ({}). If the agent saw a failure, the imported instances are still in the place; undo 'Rojo: Studio model import' to remove them.",
								delivered.reason
							)
						else
							-- The import changed the place even though its caller already
							-- gave up and was told it failed.
							Log.warn(
								"Rojo Studio bridge applied an import after its agent request expired ({}). The agent saw a failure, but the imported instances are in the place; undo 'Rojo: Studio model import' to remove them.",
								delivered.reason
							)
						end
					end
				end
			end
		end)
		if not ok and not self.stopped then
			self:stop()
			self.onFailure("Studio bridge disconnected: " .. tostring(err))
		end
	end)
end

function StudioBridge:stop()
	if self.stopped then
		return
	end
	self.stopped = true
	for transport in table.clone(self.transports) do
		transport:cancel()
	end
	self.transports = {}
	if self.logConnection then
		self.logConnection:Disconnect()
	end
	if self.pendingRequest then
		self.pendingRequest:cancel()
		self.pendingRequest = nil
	end
	for runner in table.clone(self.testRunners) do
		runner:Destroy()
	end
	self.testRunners = {}
	-- Server-side expiry also clears sessions if Studio closes before this best-effort message arrives.
	self:request("disconnect", {}):catch(function() end)
	self:__resetInstances()
	self.logs = {}
end

return StudioBridge
