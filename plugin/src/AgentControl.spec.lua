return function()
	local AgentControl = require(script.Parent.AgentControl)
	local PatchSet = require(script.Parent.PatchSet)
	local App = require(script.Parent.App)({}).component

	local function fakeController(parent)
		local controller = Instance.new("BindableFunction")
		controller:SetAttribute("PluginVersion", "test")
		controller:SetAttribute("ProtocolVersion", 1)
		controller.Parent = parent
		return controller
	end

	it("blocks the shared startSession path in either controller order", function()
		local folder = Instance.new("Folder")
		local first = fakeController(folder)
		local second = fakeController(folder)
		for _, pair in { { first, second }, { second, first } } do
			first:SetAttribute("SyncActive", false)
			second:SetAttribute("SyncActive", false)
			expect(AgentControl.tryClaim({ agentController = pair[1] })).to.equal(true)
			local state
			local app = {
				agentController = pair[2],
				setState = function(_, value)
					state = value
				end,
				claimSyncLock = function()
					error("Must not reach server connection while another version owns sync")
				end,
			}
			App.startSession(app)
			expect(state.appStatus).to.equal("Error")
			expect(pair[2]:GetAttribute("SyncActive")).to.equal(false)
			pair[1]:SetAttribute("SyncActive", false)
			expect(AgentControl.tryClaim(app)).to.equal(true)
		end
		folder:Destroy()
	end)

	it("exposes status and settings before a server connection exists", function()
		local app = {
			state = { appStatus = "NotConnected" },
			getHostAndPort = function()
				return "localhost", "34872"
			end,
		}
		local status = AgentControl.handle(app, { action = "status" })
		expect(status.syncStatus).to.equal("Disconnected")
		expect(status.bridge.available).to.equal(false)
		expect(status.pluginVersion).to.be.a("string")
		expect(AgentControl.handle(app, { action = "settings.get" }).twoWaySync).to.be.a("boolean")
	end)

	it("validates setting types choices and bounds without writing plugin settings", function()
		for _, pair in
			{
				{ "twoWaySync", true },
				{ "confirmationBehavior", "Initial" },
				{ "largeChangesConfirmationThreshold", 10 },
			}
		do
			expect(pcall(AgentControl.validateSetting, pair[1], pair[2])).to.equal(true)
		end
		for _, pair in
			{
				{ "twoWaySync", "true" },
				{ "confirmationBehavior", "unknown" },
				{ "priorEndpoints", {} },
				{ "largeChangesConfirmationThreshold", 0 },
				{ "largeChangesConfirmationThreshold", 0 / 0 },
				{ "largeChangesConfirmationThreshold", math.huge },
				{ "largeChangesConfirmationThreshold", -math.huge },
				{ "largeChangesConfirmationThreshold", 1.5 },
			}
		do
			expect(pcall(AgentControl.validateSetting, pair[1], pair[2])).to.equal(false)
		end
	end)

	it("rejects missing requests and non-finite ports before starting a session", function()
		local Config = require(script.Parent.Config)
		local Version = require(script.Parent.Version)
		local app = {
			setHost = function()
				error("Invalid input must not change the endpoint")
			end,
		}
		expect(pcall(AgentControl.handle, app, nil)).to.equal(false)
		expect(pcall(AgentControl.handle, app, {})).to.equal(false)
		for _, port in { 0 / 0, math.huge, -math.huge, 0, 65536, 1.5, "34882" } do
			local ok, err = pcall(AgentControl.handle, app, {
				action = "connect",
				host = "127.0.0.1",
				port = port,
				expectedPlaceId = game.PlaceId,
				expectedVersion = Version.display(Config.version),
			})
			expect(ok).to.equal(false)
			expect(tostring(err):find("Use an integer port", 1, true) ~= nil).to.equal(true)
		end
	end)

	it("assigns a fresh confirmation identity even for identical diffs", function()
		local patch = PatchSet.newEmpty()
		local first = AgentControl.confirmation(patch, "test")
		local second = AgentControl.confirmation(patch, "test")
		expect(first.id == second.id).to.equal(false)
		expect(first.changedInstances).to.equal(0)
	end)

	it("releases the version claim when a session cannot be created", function()
		local ServeSession = require(script.Parent.ServeSession)
		local originalNew = ServeSession.new
		local folder = Instance.new("Folder")
		local controller = fakeController(folder)
		local released = false
		local state
		local app = {
			agentController = controller,
			claimSyncLock = function()
				return true
			end,
			releaseSyncLock = function()
				released = true
			end,
			getHostAndPort = function()
				return "localhost", "34872"
			end,
			setState = function(_, value)
				state = value
			end,
		}
		ServeSession.new = function()
			error("simulated session construction failure")
		end
		local ok = pcall(App.startSession, app)
		ServeSession.new = originalNew
		expect(ok).to.equal(true)
		expect(controller:GetAttribute("SyncActive")).to.equal(false)
		expect(released).to.equal(true)
		expect(state.appStatus).to.equal("Error")
		folder:Destroy()
	end)

	it("refuses every call when agent control is turned off", function()
		local Settings = require(script.Parent.Settings)
		local previous = Settings:get("agentControlEnabled")
		Settings:set("agentControlEnabled", false)
		local app = {
			state = { appStatus = "NotConnected" },
			getHostAndPort = function()
				return "localhost", "34872"
			end,
		}
		local ok, err = pcall(AgentControl.handle, app, { action = "status" })
		Settings:set("agentControlEnabled", previous)
		expect(ok).to.equal(false)
		expect(tostring(err):find("turned off", 1, true) ~= nil).to.equal(true)
	end)

	it("ignores siblings that are not Rojo controllers when claiming sync", function()
		local folder = Instance.new("Folder")
		local controller = fakeController(folder)
		local impostor = Instance.new("BindableFunction")
		impostor:SetAttribute("SyncActive", true)
		impostor.Parent = folder
		local otherVersion = fakeController(folder)
		expect(AgentControl.tryClaim({ agentController = controller })).to.equal(true)
		controller:SetAttribute("SyncActive", false)
		otherVersion:SetAttribute("SyncActive", true)
		expect(AgentControl.tryClaim({ agentController = controller })).to.equal(false)
		folder:Destroy()
	end)

	it("mounts the controller again when it was removed from its folder", function()
		local app = {}
		local original = AgentControl.mount(app)
		app.agentController = original
		-- Removing only this controller stands in for a deleted folder without
		-- unmounting the controllers of plugins running in this Studio.
		original.Parent = nil
		-- Whether the claim succeeds depends on other Rojo plugins syncing in
		-- this Studio, so only the remount is checked.
		local ok = pcall(AgentControl.tryClaim, app)
		local remounted = app.agentController
		local folderName = remounted.Parent and remounted.Parent.Name
		remounted:Destroy()
		expect(ok).to.equal(true)
		expect(folderName).to.equal("RojoAgentControls")
		expect(remounted ~= original).to.equal(true)
		expect(remounted:GetAttribute("PluginVersion")).to.equal(original:GetAttribute("PluginVersion"))
	end)

	it("only lets agent control make sync confirmation stricter unless allowed", function()
		local Settings = require(script.Parent.Settings)
		local previousAllowed = Settings:get("agentControlCanChangeConfirmation")
		local previousBehavior = Settings:get("confirmationBehavior")
		local previousThreshold = Settings:get("largeChangesConfirmationThreshold")
		Settings:set("agentControlCanChangeConfirmation", false)
		Settings:set("largeChangesConfirmationThreshold", 10)
		local behaviors = { "Never", "Initial", "Large Changes", "Unlisted PlaceId", "Always" }
		local strictness = { Never = 0, Initial = 1, ["Large Changes"] = 1, ["Unlisted PlaceId"] = 1, Always = 2 }
		local transitions = {}
		for _, from in behaviors do
			for _, to in behaviors do
				Settings:set("confirmationBehavior", from)
				table.insert(transitions, {
					from = from,
					to = to,
					allowed = pcall(AgentControl.checkConfirmationChange, "confirmationBehavior", to),
				})
			end
		end
		local results = {
			raise = pcall(AgentControl.checkConfirmationChange, "largeChangesConfirmationThreshold", 20),
			lower = pcall(AgentControl.checkConfirmationChange, "largeChangesConfirmationThreshold", 5),
			enableTwoWay = pcall(AgentControl.checkConfirmationChange, "twoWaySync", true),
			disableTwoWay = pcall(AgentControl.checkConfirmationChange, "twoWaySync", false),
		}
		Settings:set("agentControlCanChangeConfirmation", true)
		Settings:set("confirmationBehavior", "Always")
		local allowedNever = pcall(AgentControl.checkConfirmationChange, "confirmationBehavior", "Never")
		local allowedTwoWay = pcall(AgentControl.checkConfirmationChange, "twoWaySync", true)
		Settings:set("agentControlCanChangeConfirmation", previousAllowed)
		Settings:set("confirmationBehavior", previousBehavior)
		Settings:set("largeChangesConfirmationThreshold", previousThreshold)
		for _, transition in transitions do
			local expected = transition.from == transition.to or strictness[transition.to] > strictness[transition.from]
			if transition.allowed ~= expected then
				error(
					string.format(
						"%s -> %s should be %s",
						transition.from,
						transition.to,
						if expected then "allowed" else "refused"
					)
				)
			end
		end
		expect(#transitions).to.equal(25)
		expect(results.raise).to.equal(false)
		expect(results.lower).to.equal(true)
		expect(results.enableTwoWay).to.equal(false)
		expect(results.disableTwoWay).to.equal(true)
		expect(allowedNever).to.equal(true)
		expect(allowedTwoWay).to.equal(true)
		expect(pcall(AgentControl.validateSetting, "agentControlEnabled", false)).to.equal(false)
		expect(pcall(AgentControl.validateSetting, "studioBridgeAutoEnable", false)).to.equal(false)
	end)

	it("lets agents turn backups before a resync off only when they may skip confirmation", function()
		local Settings = require(script.Parent.Settings)
		local previousAllowed = Settings:get("agentControlCanChangeConfirmation")
		Settings:set("agentControlCanChangeConfirmation", false)
		local turnOn = pcall(AgentControl.checkConfirmationChange, "autoBackupBeforeSync", true)
		local turnOff = pcall(AgentControl.checkConfirmationChange, "autoBackupBeforeSync", false)
		Settings:set("agentControlCanChangeConfirmation", true)
		local turnOffAllowed = pcall(AgentControl.checkConfirmationChange, "autoBackupBeforeSync", false)
		Settings:set("agentControlCanChangeConfirmation", previousAllowed)
		expect(pcall(AgentControl.validateSetting, "autoBackupBeforeSync", false)).to.equal(true)
		expect(pcall(AgentControl.validateSetting, "autoBackupBeforeSync", "off")).to.equal(false)
		expect(turnOn).to.equal(true)
		expect(turnOff).to.equal(false)
		expect(turnOffAllowed).to.equal(true)
	end)

	it("accepts Reject only when the confirmation UI offers it", function()
		local Settings = require(script.Parent.Settings)
		local Config = require(script.Parent.Config)
		local Version = require(script.Parent.Version)
		local previous = Settings:get("twoWaySync")
		local previousAllowed = Settings:get("agentControlCanChangeConfirmation")
		Settings:set("agentControlCanChangeConfirmation", true)
		local bindable = Instance.new("BindableEvent")
		local decision
		bindable.Event:Connect(function(value)
			decision = value
		end)
		local app = {
			agentConfirmation = { id = "confirm" },
			confirmationBindable = bindable,
			state = { appStatus = "Confirming" },
			getHostAndPort = function()
				return "localhost", "34872"
			end,
		}
		local request = {
			action = "confirm",
			confirmationId = "confirm",
			decision = "Reject",
			expectedPlaceId = game.PlaceId,
			expectedVersion = Version.display(Config.version),
		}
		Settings:set("twoWaySync", false)
		local refused = pcall(AgentControl.handle, app, request)
		Settings:set("twoWaySync", true)
		local accepted = pcall(AgentControl.handle, app, request)
		Settings:set("twoWaySync", previous)
		Settings:set("agentControlCanChangeConfirmation", previousAllowed)
		task.wait()
		bindable:Destroy()
		expect(refused).to.equal(false)
		expect(accepted).to.equal(true)
		expect(decision).to.equal("Reject")
	end)

	it("only lets agent control abort a sync confirmation unless allowed", function()
		local Settings = require(script.Parent.Settings)
		local Config = require(script.Parent.Config)
		local Version = require(script.Parent.Version)
		local previousAllowed = Settings:get("agentControlCanChangeConfirmation")
		local previousTwoWay = Settings:get("twoWaySync")
		local bindable = Instance.new("BindableEvent")
		local decisions = {}
		bindable.Event:Connect(function(value)
			table.insert(decisions, value)
		end)
		local app = {
			confirmationBindable = bindable,
			state = { appStatus = "Confirming" },
			getHostAndPort = function()
				return "localhost", "34872"
			end,
		}
		local function confirm(decision)
			app.agentConfirmation = { id = "confirm" }
			return pcall(AgentControl.handle, app, {
				action = "confirm",
				confirmationId = "confirm",
				decision = decision,
				expectedPlaceId = game.PlaceId,
				expectedVersion = Version.display(Config.version),
			})
		end
		Settings:set("agentControlCanChangeConfirmation", false)
		Settings:set("twoWaySync", true)
		local accept = confirm("Accept")
		local reject = confirm("Reject")
		local abort = confirm("Abort")
		Settings:set("agentControlCanChangeConfirmation", previousAllowed)
		Settings:set("twoWaySync", previousTwoWay)
		task.wait()
		bindable:Destroy()
		expect(accept).to.equal(false)
		expect(reject).to.equal(false)
		expect(abort).to.equal(true)
		expect(#decisions).to.equal(1)
		expect(decisions[1]).to.equal("Abort")
	end)

	it("warns once per session the first time agent control changes something", function()
		local LogService = game:GetService("LogService")
		local Config = require(script.Parent.Config)
		local Version = require(script.Parent.Version)
		local warnings = {}
		local connection = LogService.MessageOut:Connect(function(message, kind)
			if kind == Enum.MessageType.MessageWarning and string.find(message, "Agent control performed", 1, true) then
				table.insert(warnings, message)
			end
		end)
		AgentControl._resetMutationWarning()
		local app = {
			state = { appStatus = "NotConnected" },
			endSession = function() end,
			getHostAndPort = function()
				return "localhost", "34872"
			end,
		}
		AgentControl.handle(app, { action = "status" })
		for _ = 1, 2 do
			AgentControl.handle(app, {
				action = "disconnect",
				expectedPlaceId = game.PlaceId,
				expectedVersion = Version.display(Config.version),
			})
		end
		task.wait()
		connection:Disconnect()
		expect(#warnings).to.equal(1)
		expect(string.find(warnings[1], "'disconnect'", 1, true) ~= nil).to.equal(true)
	end)

	it("rejects a mutation without an explicit matching place and plugin version", function()
		local called = false
		local app = {
			endSession = function()
				called = true
			end,
		}
		expect(pcall(AgentControl.handle, app, { action = "disconnect" })).to.equal(false)
		expect(called).to.equal(false)
	end)
end
