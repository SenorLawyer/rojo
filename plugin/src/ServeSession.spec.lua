return function()
	local Promise = require(script.Parent.Parent.Packages.Promise)
	local ServeSession = require(script.Parent.ServeSession)
	local PatchSet = require(script.Parent.PatchSet)
	local StudioBridge = require(script.Parent.StudioBridge)
	local nativeRequest = StudioBridge.__requestAsync

	local session
	local api
	local connectResolve
	local readResolve
	local syncCalls
	local socketCalls

	beforeEach(function()
		syncCalls = 0
		socketCalls = 0
		api = {
			connect = function()
				return Promise.new(function(resolve)
					connectResolve = resolve
				end)
			end,
			read = function()
				return Promise.new(function(resolve)
					readResolve = resolve
				end)
			end,
			disconnect = function() end,
			setMessageCursor = function() end,
			connectWebSocket = function()
				socketCalls += 1
				return Promise.resolve()
			end,
		}
		session = ServeSession.new({ apiContext = api, twoWaySync = false })
	end)

	afterEach(function()
		session:stop()
		StudioBridge.__requestAsync = nativeRequest
		StudioBridge.manualStart = false
	end)

	it("keeps normal syncing connected when the optional bridge request fails", function()
		api.__baseUrl = "http://127.0.0.1:34872"
		api.connectWebSocket = function(_, _, opened)
			opened()
			return Promise.resolve()
		end
		session.__initialSync = function()
			return Promise.resolve()
		end
		StudioBridge.__requestAsync = function()
			error("simulated Studio bridge request timed out")
		end
		-- A timed-out request is retried before the bridge gives up.
		local previousDelay = StudioBridge.retryDelay
		StudioBridge.retryDelay = 0
		session:start()
		connectResolve({
			projectName = "test",
			studioBridge = { protocolVersion = 1, pluginToken = "test", identity = {} },
		})
		local deadline = os.clock() + 2
		while session:getBridgeStatus().available ~= false and os.clock() < deadline do
			task.wait()
		end
		StudioBridge.retryDelay = previousDelay
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(session:getBridgeStatus().available).to.equal(false)
		expect(session:getBridgeStatus().error:find("timed out") ~= nil).to.equal(true)
	end)

	it("stops the bridge and its poll as soon as the sync connection closes", function()
		api.__baseUrl = "http://127.0.0.1:34872"
		local closeSocket
		local events = {}
		api.connectWebSocket = function(_, _, opened)
			opened()
			return Promise.new(function(_, reject)
				closeSocket = reject
			end)
		end
		api.disconnect = function()
			table.insert(events, "sync disconnected")
		end
		session.__initialSync = function()
			return Promise.resolve()
		end
		local poll = Promise.new(function() end)
		local polls = 0
		StudioBridge.__requestAsync = function(_, options, token)
			local route = string.match(options.Url, "/api/studio/(%w+)$")
			table.insert(events, route)
			if route == "poll" then
				polls += 1
				token.transport = poll
				return poll:expect()
			end
			return { Success = true, StatusCode = 200, Body = "{}" }
		end
		StudioBridge.manualStart = true
		session:start()
		connectResolve({
			projectName = "test",
			studioBridge = { protocolVersion = 1, pluginToken = "test", identity = {} },
		})
		task.wait()
		StudioBridge.manualStart = false
		local bridge = session.__studioBridge
		expect(bridge).to.be.ok()
		expect(polls).to.equal(1)

		-- The server closes the socket when the checkout changes.
		closeSocket("Connection to the Rojo server closed. Reconnect to resync.")
		task.wait()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(bridge.stopped).to.equal(true)
		expect(session.__studioBridge).to.equal(nil)
		expect(poll:getStatus()).to.equal(Promise.Status.Cancelled)
		-- The bridge is stopped before sync is torn down and never polls again.
		local stoppedAt = table.find(events, "disconnect")
		local syncAt = table.find(events, "sync disconnected")
		expect(stoppedAt ~= nil and syncAt ~= nil and stoppedAt < syncAt).to.equal(true)
		task.wait()
		expect(polls).to.equal(1)
	end)

	it("ignores a late connection result after stopping", function()
		session.__initialSync = function()
			syncCalls += 1
			return Promise.resolve()
		end
		session:start()
		session:stop()
		connectResolve({ projectName = "test", rootInstanceId = "ROOT" })
		task.wait()
		expect(syncCalls).to.equal(0)
		expect(socketCalls).to.equal(0)
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
	end)

	it("does not reconnect after stopping during initial sync", function()
		local finishSync
		session.__initialSync = function()
			return Promise.new(function(resolve)
				finishSync = resolve
			end)
		end
		session:start()
		connectResolve({ projectName = "test", rootInstanceId = "ROOT" })
		session:stop()
		finishSync()
		task.wait()
		expect(socketCalls).to.equal(0)
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
	end)

	it("does not apply the catch-up patch after confirmation stops the session", function()
		local patchCalls = 0
		session.__reconciler = {
			hydrate = function() end,
			diff = function()
				return true, PatchSet.newEmpty()
			end,
		}
		session.__applyPatch = function()
			patchCalls += 1
		end
		session:setConfirmCallback(function()
			session:stop()
			return "Accept"
		end)
		local pending = session:__initialSync({ rootInstanceId = "ROOT" })
		readResolve({ messageCursor = 0, instances = {} })
		pending:await()
		expect(patchCalls).to.equal(0)
	end)

	it("reports a stop once even if it is requested repeatedly", function()
		local calls = 0
		session:onStatusChanged(function(status)
			if status == ServeSession.Status.Disconnected then
				calls += 1
			end
		end)
		session:stop()
		session:stop()
		expect(calls).to.equal(1)
	end)

	it("does not start the same session twice", function()
		local calls = 0
		api.connect = function()
			calls += 1
			return Promise.new(function() end)
		end
		session:start()
		session:start()
		expect(calls).to.equal(1)
	end)

	it("disconnects when the place identity changes", function()
		session.__expectedPlaceId = game.PlaceId + 1
		session:__onPlaceIdChanged()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
	end)

	it("explains a slow handshake that may be waiting on Studio's permission prompt", function()
		local loadingText
		session:setUpdateLoadingTextCallback(function(text)
			loadingText = text
		end)
		api.connect = function(_, onWaitingForPermission)
			return Promise.new(function()
				onWaitingForPermission()
			end)
		end
		session:start()
		expect(string.find(loadingText, "choose Allow", 1, true) ~= nil).to.equal(true)
		expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
	end)

	it("stops applying a packet's messages once the session disconnects mid-packet", function()
		local handlers
		local cursorCalls = 0
		api.setMessageCursor = function()
			cursorCalls += 1
		end
		api.connectWebSocket = function(_, packetHandlers)
			handlers = packetHandlers
			return Promise.new(function() end)
		end
		session.__initialSync = function()
			return Promise.resolve()
		end
		local patchCalls = 0
		session.__applyPatch = function()
			patchCalls += 1
			-- Simulates Cancel while the patch yields on a fallback request.
			session:stop()
		end

		session:start()
		connectResolve({ projectName = "test", rootInstanceId = "ROOT" })
		task.wait()
		expect(handlers).to.be.ok()

		handlers.messages({
			messages = { PatchSet.newEmpty(), PatchSet.newEmpty(), PatchSet.newEmpty() },
			messageCursor = 3,
		})
		expect(patchCalls).to.equal(1)
		expect(cursorCalls).to.equal(0)
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
	end)

	it("finishes the history recording when applying a patch fails", function()
		local ChangeHistoryService = game:GetService("ChangeHistoryService")
		local wasRecording = ChangeHistoryService:IsRecordingInProgress()
		session.__applyPatchInRecording = function()
			error("fallback failed")
		end
		local ok, err = pcall(session.__applyPatch, session, PatchSet.newEmpty())
		expect(ok).to.equal(false)
		expect(string.find(tostring(err), "fallback failed", 1, true) ~= nil).to.equal(true)
		expect(ChangeHistoryService:IsRecordingInProgress()).to.equal(wasRecording)
	end)

	it("ends the session with a readable error when sending Studio's changes fails", function()
		local details
		session:onStatusChanged(function(status, detail)
			if status == ServeSession.Status.Disconnected then
				details = detail
			end
		end)
		api.write = function()
			return Promise.reject("Timed out sending changes to the Rojo server.")
		end
		local patch = PatchSet.newEmpty()
		table.insert(patch.removed, "REMOVED")
		session:__sendChanges(patch)
		task.wait()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(string.find(details, "Couldn't send Studio's changes to the Rojo server", 1, true) ~= nil).to.equal(true)
		expect(string.find(details, "Timed out sending changes", 1, true) ~= nil).to.equal(true)
		expect(string.find(details, "Reconnect to resync.", 1, true) ~= nil).to.equal(true)
	end)

	it("ignores a failed write that finishes after the session stopped", function()
		local calls = 0
		local rejectWrite
		api.write = function()
			return Promise.new(function(_, reject)
				rejectWrite = reject
			end)
		end
		session:__sendChanges(PatchSet.newEmpty())
		session:stop()
		session:onStatusChanged(function()
			calls += 1
		end)
		rejectWrite("late failure")
		task.wait()
		expect(calls).to.equal(0)
	end)

	it("keeps the session when asking the server to open a script fails", function()
		local Settings = require(script.Parent.Settings)
		local originalGet = Settings.get
		Settings.get = function(self, name)
			if name == "openScriptsExternally" then
				return true
			end
			return originalGet(self, name)
		end

		local handleFailure
		api.open = function()
			return {
				catch = function(_, handler)
					handleFailure = handler
				end,
			}
		end
		local activeScript = Instance.new("ModuleScript")
		session.__instanceMap.fromInstances[activeScript] = "SCRIPT"
		session.__status = ServeSession.Status.Connected

		local ok, err = pcall(session.__onActiveScriptChanged, session, activeScript)
		Settings.get = originalGet
		if not ok then
			error(err, 0)
		end
		expect(handleFailure).to.be.ok()
		handleFailure("Timed out asking the Rojo server to open the script.")
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)

		session.__instanceMap.fromInstances[activeScript] = nil
		task.wait(0.1)
		activeScript:Destroy()
	end)

	it("keeps syncing when the place identity still matches", function()
		session:__onPlaceIdChanged()
		expect(session:getStatus()).to.equal(ServeSession.Status.NotStarted)
	end)

	describe("sync fallback replacement", function()
		local function model(ids)
			local root = Instance.new("Folder")
			for _, id in ids do
				local pointer = Instance.new("ObjectValue")
				pointer.Name = id
				pointer.Value = Instance.new("Folder")
				pointer.Parent = root
			end
			return { root }
		end

		it("leaves out ids the server reports as missing", function()
			local container = Instance.new("Folder")
			local present = Instance.new("Folder")
			present.Parent = container
			local missing = Instance.new("Folder")
			missing.Parent = container
			session.__instanceMap:insert("PRESENT", present)
			session.__instanceMap:insert("MISSING", missing)

			local refPatchIds
			api.serialize = function()
				return Promise.resolve({ modelContents = buffer.create(0), missingIds = { "MISSING" } })
			end
			api.refPatch = function(_, ids)
				refPatchIds = ids
				return Promise.resolve({ patch = PatchSet.newEmpty() })
			end
			session.__deserializeInstances = function()
				return model({ "PRESENT" })
			end

			local ok, unapplied = session:__replaceInstances({ "PRESENT", "MISSING" })
			expect(ok).to.equal(true)
			expect(PatchSet.isEmpty(unapplied)).to.equal(true)
			expect(#refPatchIds).to.equal(1)
			expect(refPatchIds[1]).to.equal("PRESENT")
			expect(session.__instanceMap.fromIds["PRESENT"]).never.to.equal(present)
			expect(session.__instanceMap.fromIds["MISSING"]).to.equal(missing)
			expect(missing.Parent).to.equal(container)
			container:Destroy()
		end)

		it("succeeds without further requests when every id is missing", function()
			local refPatchCalls = 0
			api.serialize = function()
				return Promise.resolve({ modelContents = buffer.create(0), missingIds = { "GONE" } })
			end
			api.refPatch = function()
				refPatchCalls += 1
				return Promise.resolve({ patch = PatchSet.newEmpty() })
			end
			session.__deserializeInstances = function()
				error("nothing should be deserialized")
			end

			local ok, unapplied = session:__replaceInstances({ "GONE" })
			expect(ok).to.equal(true)
			expect(PatchSet.isEmpty(unapplied)).to.equal(true)
			expect(refPatchCalls).to.equal(0)
		end)
	end)

	describe("script wipe guard", function()
		local Settings = require(script.Parent.Settings)
		local hashSource = require(script.Parent.hashSource)
		local container
		local module

		-- A resync as the verify that `expectFrom` recorded sends it.
		local function resyncAs(patch, expected, includeHeld, expiresAt, isActive)
			return session:applyResync(patch, expected, includeHeld, expiresAt, isActive, "VERIFY")
		end

		local function sourceUpdate(id, source, extra)
			local update = { id = id, changedProperties = { Source = { String = source } } }
			for key, value in extra or {} do
				update.changedProperties[key] = value
			end
			return { removed = {}, added = {}, updated = { update } }
		end

		beforeEach(function()
			container = Instance.new("Folder")
			module = Instance.new("ModuleScript")
			module.Name = "UIController"
			module.Source = "local a = 1\nlocal b = 2\nlocal c = 3\nlocal d = 4\nreturn a + b + c + d\n"
			module.Parent = container
			container.Parent = workspace
			session.__instanceMap:insert("UI", module)
			Settings:set("guardScriptWipes", true)
		end)

		afterEach(function()
			container:Destroy()
		end)

		it("holds back emptying a script with several lines", function()
			local patch = sourceUpdate("UI", "")
			session:__holdScriptWipes(patch)
			expect(#patch.updated).to.equal(0)
			expect(session.__heldWipes.UI).to.be.ok()
			expect(session.__heldWipes.UI.lines).to.equal(5)
			expect(session:getSyncSummary().heldWipeCount).to.equal(1)
		end)

		it("keeps the rest of an update whose source it holds back", function()
			local patch = sourceUpdate("UI", "  \n", { Disabled = { Bool = true } })
			session:__holdScriptWipes(patch)
			expect(#patch.updated).to.equal(1)
			expect(patch.updated[1].changedProperties.Source).to.equal(nil)
			expect(patch.updated[1].changedProperties.Disabled).to.be.ok()
		end)

		it("holds back a class change of a held script", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local classChange = {
				removed = {},
				added = {},
				updated = { { id = "UI", changedClassName = "Script", changedProperties = {} } },
			}
			session:__holdScriptWipes(classChange)
			expect(#classChange.updated).to.equal(0)
			local combined = sourceUpdate("UI", "")
			combined.updated[1].changedClassName = "Script"
			session:__holdScriptWipes(combined)
			expect(#combined.updated).to.equal(0)
			expect(session.__heldWipes.UI).to.be.ok()
		end)

		it("keeps holding a held script that was shortened in Studio", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local hold = session.__heldWipes.UI
			module.Source = "return 1"
			local again = sourceUpdate("UI", " ")
			session:__holdScriptWipes(again)
			expect(#again.updated).to.equal(0)
			expect(session.__heldWipes.UI).to.equal(hold)
		end)

		it("lists held scripts within a byte budget", function()
			for index = 1, 40 do
				session.__heldWipes["ID" .. index] = { path = string.rep("x", 900) .. index, lines = 9, heldAt = 0 }
			end
			local summary = session:getSyncSummary()
			expect(summary.heldWipeCount).to.equal(40)
			local bytes = 0
			for _, entry in summary.heldWipes do
				expect(#entry.path <= 256).to.equal(true)
				bytes += #entry.path
			end
			expect(bytes <= 4096).to.equal(true)
			expect(#summary.heldWipes > 0).to.equal(true)
		end)

		local function serverCopy(className, source)
			api.serialize = function()
				return Promise.resolve({ modelContents = buffer.create(0) })
			end
			api.refPatch = function()
				return Promise.resolve({ patch = PatchSet.newEmpty() })
			end
			session.__deserializeInstances = function()
				local root = Instance.new("Folder")
				local pointer = Instance.new("ObjectValue")
				pointer.Name = "UI"
				local copy = Instance.new(className)
				copy.Name = "UIController"
				(copy :: any).Source = source
				pointer.Value = copy
				pointer.Parent = root
				return { root }
			end
		end

		it("does not let the sync fallback swap in an empty copy", function()
			serverCopy("ModuleScript", "")
			local ok = session:__replaceInstances({ "UI" })
			expect(ok).to.equal(true)
			expect(session.__instanceMap.fromIds.UI).to.equal(module)
			expect(module.Parent).to.equal(container)
			expect(string.find(module.Source, "return a") ~= nil).to.equal(true)
			expect(session.__heldWipes.UI).to.be.ok()
		end)

		it("applies a held class change once the file has code again", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:__holdScriptWipes({
				removed = {},
				added = {},
				updated = { { id = "UI", changedClassName = "Script", changedProperties = {} } },
			})
			expect(session.__heldWipes.UI.deferredClass).to.equal("Script")
			serverCopy("Script", "print('code again')")
			session.__status = ServeSession.Status.Connected
			session:__applyPatch(sourceUpdate("UI", "print('code again')"))
			expect(session.__heldWipes.UI).to.equal(nil)
			local now = session.__instanceMap.fromIds.UI
			expect(now.ClassName).to.equal("Script")
			expect(now.Source).to.equal("print('code again')")
		end)

		it("pairs a held script with the server's after its class changed", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			again.__instanceMap:insert("FOLDER", container)
			again:__rematchHolds({
				FOLDER = { Name = "Folder", ClassName = "Folder", Children = { "NEW-ID" } },
				["NEW-ID"] = { Name = "UIController", ClassName = "Script", Children = {} },
			})
			again:__adoptHolds()
			expect(again.__instanceMap.fromIds["NEW-ID"]).to.equal(module)
			expect(again.__heldWipes["NEW-ID"]).to.be.ok()
			again:__clearHold("NEW-ID")
			again:stop()
		end)

		it("keeps a recovered script's code when it is rebuilt with a new class on reconnect", function()
			local kept = module.Source
			module:SetAttribute("Tag", "kept")
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			again.__status = ServeSession.Status.Connected
			again.__instanceMap:insert("FOLDER", container)
			local instances = {
				FOLDER = {
					Id = "FOLDER",
					Name = container.Name,
					ClassName = "Folder",
					Properties = {},
					Children = { "NEW-ID" },
				},
				["NEW-ID"] = {
					Id = "NEW-ID",
					Name = "UIController",
					ClassName = "Script",
					Properties = {
						Source = { String = kept },
						Attributes = { Attributes = { Tag = { String = "kept" } } },
					},
					Children = {},
				},
			}
			-- The same steps as reconnecting with the guard on.
			local rematched = again:__rematchHolds(instances)
			again:__adoptHolds()
			again:__releaseRecoveredHolds(instances)
			expect(again.__heldWipes["NEW-ID"]).to.equal(nil)
			local ok, patch = again.__reconciler:diff(instances, "FOLDER", container)
			expect(ok).to.equal(true)
			expect(patch.updated[1].changedProperties.Source).to.equal(nil)
			again:__completeRematchedClassChanges(patch, instances, rematched)
			expect(patch.updated[1].changedProperties.Source.String).to.equal(kept)
			again:__applyPatch(patch)
			task.wait(0.3)
			local now = again.__instanceMap.fromIds["NEW-ID"]
			expect(now.ClassName).to.equal("Script")
			expect(now.Source).to.equal(kept)
			expect(now:GetAttribute("Tag")).to.equal("kept")
			again:stop()
		end)

		it("ends holds from an earlier session when it reconnects with the guard off", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			local rematches = 0
			again.__rematchHolds = function()
				rematches += 1
				return {}
			end
			again.__reconciler = {
				hydrate = function() end,
				diff = function()
					return true, PatchSet.newEmpty()
				end,
			}
			again.__applyPatch = function() end
			Settings:set("guardScriptWipes", false)
			local pending = again:__initialSync({ rootInstanceId = "ROOT" })
			readResolve({ messageCursor = 0, instances = {} })
			pending:await()
			Settings:set("guardScriptWipes", true)
			expect(rematches).to.equal(0)
			again.__instanceMap:insert("UI", module)
			again:__adoptHolds()
			expect(again.__heldWipes.UI).to.equal(nil)
			again:stop()
		end)

		it("refuses to write a script whose source changed even when its hash matches", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			local expected = { UI = { from = hashSource(kept), to = hashSource("return 'x'") } }
			-- verify saw different text that happens to hash the same.
			session.__verifiedRecords = { { id = "VERIFY", sources = { UI = "something else" } } }
			local result = resyncAs(sourceUpdate("UI", "return 'x'"), expected, false)
			expect(result.skipped).to.equal(1)
			expect(module.Source).to.equal(kept)
		end)

		it("never lets a source write empty a script that is held when it commits", function()
			session:__setHold("UI", module, { path = "UI", lines = 5, heldAt = 0 })
			module.Source = "return 1"
			session:__writeSource({ id = "UI", instance = module, target = "" })
			task.wait(0.2)
			expect(module.Source).to.equal("return 1")
			session:__clearHold("UI")
		end)

		it("gives way to a sync write started after verify looked", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			session.__verifiedRecords = {
				{ id = "VERIFY", sources = { UI = kept }, generations = { UI = 0 } },
			}
			local expected = { UI = { from = hashSource(kept), to = hashSource("return 'stale fix'") } }
			-- A newer change from disk started writing after verify.
			session.__sourceGenerations.UI = 1
			local result = resyncAs(sourceUpdate("UI", "return 'stale fix'"), expected, false)
			expect(result.skipped).to.equal(1)
			expect(module.Source).to.equal(kept)
		end)

		it("ends a hold when the held script's file is deleted and its folder stays", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local deleted = {
				removed = {},
				added = {},
				updated = { { id = "UI", changedClassName = "Folder", changedProperties = {} } },
			}
			session:__holdScriptWipes(deleted)
			expect(#deleted.updated).to.equal(1)
			expect(session.__heldWipes.UI).to.equal(nil)
		end)

		it("writes the sources of new scripts through the guarded writer", function()
			local reconcilerPatch, writes = session:__splitSourceWrites({
				removed = {},
				added = {
					NEW = {
						Id = "NEW",
						ClassName = "ModuleScript",
						Name = "New",
						Parent = "FOLDER",
						Properties = { Source = { String = "return 'new'" } },
						Children = {},
					},
				},
				updated = {},
			})
			expect(reconcilerPatch.added.NEW.Properties.Source).to.equal(nil)
			expect(#writes).to.equal(1)
			expect(writes[1].target).to.equal("return 'new'")
		end)

		it("lets a fix cancel an older pending sync write", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			-- An ordinary write started before verify looked, then the fix took over.
			local generation = (session.__sourceGenerations.UI or 0) + 1
			session.__sourceGenerations.UI = generation
			session.__verifiedRecords = {
				{ id = "VERIFY", sources = { UI = kept }, generations = { UI = generation } },
			}
			local expected = { UI = { from = hashSource(kept), to = hashSource("return 'fixed'") } }
			local result = resyncAs(sourceUpdate("UI", "return 'fixed'"), expected, false)
			expect(result.updated).to.equal(1)
			expect(session.__sourceGenerations.UI > generation).to.equal(true)
			expect(module.Source).to.equal("return 'fixed'")
		end)

		it("lets only the latest source write for a script finish", function()
			session:__writeSource({ id = "UI", instance = module, target = "return 'first'" })
			session:__writeSource({ id = "UI", instance = module, target = "return 'second'" })
			task.wait(0.2)
			expect(module.Source).to.equal("return 'second'")
		end)

		it("pairs nested held scripts in any order after a reconnect", function()
			local inner = Instance.new("ModuleScript")
			inner.Name = "Inner"
			inner.Source = "local a = 1\nlocal b = 2\nlocal c = 3\nlocal d = 4\nreturn a\n"
			inner.Parent = module
			session.__instanceMap:insert("INNER", inner)
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:__holdScriptWipes({
				removed = {},
				added = {},
				updated = { { id = "INNER", changedProperties = { Source = { String = "" } } } },
			})
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			again.__instanceMap:insert("FOLDER", container)
			again:__rematchHolds({
				FOLDER = { Name = "Folder", ClassName = "Folder", Children = { "UI2" } },
				UI2 = { Name = "UIController", ClassName = "Script", Children = { "INNER2" }, Properties = {} },
				INNER2 = { Name = "Inner", ClassName = "ModuleScript", Children = {}, Properties = {} },
			})
			expect(again.__instanceMap.fromIds["UI2"]).to.equal(module)
			expect(again.__instanceMap.fromIds["INNER2"]).to.equal(inner)
			again:__adoptHolds()
			expect(again.__heldWipes["INNER2"]).to.be.ok()
			again:__clearHold("UI2")
			again:__clearHold("INNER2")
			again:stop()
		end)

		it("does not empty a held script whose class changed too, even with includeHeld", function()
			session.__status = ServeSession.Status.Connected
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session.__heldWipes.UI.deferredClass = true
			local kept = module.Source
			session.__verifiedRecords = { { id = "VERIFY", sources = { UI = kept }, generations = {} } }
			local expected = { UI = { from = hashSource(kept), to = hashSource("") } }
			local result = resyncAs(sourceUpdate("UI", ""), expected, true)
			expect(result.skipped).to.equal(1)
			expect(module.Source).to.equal(kept)
			expect(session.__heldWipes.UI).to.be.ok()
		end)

		it("does not carry a hold when several same-named scripts are replaced", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local twin = Instance.new("ModuleScript")
			twin.Name = "UIController"
			twin.Parent = container
			session.__instanceMap:insert("TWIN", twin)
			session.__instanceMap:insert("FOLDER", container)
			local function added(id)
				return {
					Id = id,
					Parent = "FOLDER",
					Name = "UIController",
					ClassName = "Script",
					Properties = { Source = { String = "" } },
					Children = {},
				}
			end
			local replacement = { removed = { "UI", "TWIN" }, added = { A = added("A"), B = added("B") }, updated = {} }
			session:__holdScriptWipes(replacement)
			expect(replacement.added.A.Properties.Source.String).to.equal("")
			expect(replacement.added.B.Properties.Source.String).to.equal("")
			expect(next(session.__pendingHoldTransfers)).to.equal(nil)
		end)

		it("carries a hold to a script that replaces a held one", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local kept = module.Source
			session.__instanceMap:insert("FOLDER", container)
			local replacement = {
				removed = { "UI" },
				added = {
					NEW = {
						Id = "NEW",
						Parent = "FOLDER",
						Name = "UIController",
						ClassName = "LocalScript",
						Properties = { Source = { String = "" } },
						Children = {},
					},
				},
				updated = {},
			}
			session:__holdScriptWipes(replacement)
			expect(replacement.added.NEW.Properties.Source.String).to.equal(kept)
			expect(session.__pendingHoldTransfers.NEW).to.be.ok()
			expect(session.__heldWipes.UI).to.equal(nil)
			table.clear(session.__pendingHoldTransfers)
		end)

		it("forgets a held class change that was changed back", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local change = function(className)
				session:__holdScriptWipes({
					removed = {},
					added = {},
					updated = { { id = "UI", changedClassName = className, changedProperties = {} } },
				})
			end
			change("Script")
			expect(session.__heldWipes.UI.deferredClass).to.equal("Script")
			change("ModuleScript")
			expect(session.__heldWipes.UI.deferredClass).to.equal(nil)
		end)

		it("pairs a held script under a parent whose class changed", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			-- Only the workspace is paired; the folder became a Model on disk.
			again.__instanceMap:insert("WS", workspace)
			again:__rematchHolds({
				WS = { Name = "Workspace", ClassName = "Workspace", Children = { "MODEL" } },
				MODEL = { Name = container.Name, ClassName = "Model", Children = { "UI2" }, Properties = {} },
				UI2 = { Name = "UIController", ClassName = "ModuleScript", Children = {}, Properties = {} },
			})
			expect(again.__instanceMap.fromIds.MODEL).to.equal(container)
			expect(again.__instanceMap.fromIds.UI2).to.equal(module)
			again:__adoptHolds()
			expect(again.__heldWipes.UI2).to.be.ok()
			again:__clearHold("UI2")
			again:stop()
		end)

		it("releases an adopted hold whose file has code again", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			again.__instanceMap:insert("UI", module)
			again:__adoptHolds()
			expect(again.__heldWipes.UI).to.be.ok()
			again:__releaseRecoveredHolds({
				UI = { Properties = { Source = { String = module.Source } } },
			})
			expect(again.__heldWipes.UI).to.equal(nil)
			again:stop()
		end)

		it("keeps a held script that could not be paired out of the catch-up removals", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			local catchUp = { removed = { container }, added = {}, updated = {} }
			again:__keepUnpairedHolds(catchUp)
			expect(#catchUp.removed).to.equal(0)
			local other = Instance.new("Folder")
			local unrelated = { removed = { other }, added = {}, updated = {} }
			again:__keepUnpairedHolds(unrelated)
			expect(#unrelated.removed).to.equal(1)
			other:Destroy()
			again:stop()
		end)

		it("keeps a hold when the same Studio reconnects", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local hold = session.__heldWipes.UI
			session:stop()
			local again = ServeSession.new({ apiContext = api, twoWaySync = false })
			again.__instanceMap:insert("NEW-ID", module)
			again:__adoptHolds()
			expect(again.__heldWipes["NEW-ID"]).to.equal(hold)
			module.Source = "return 1"
			local empty = sourceUpdate("NEW-ID", "")
			again:__holdScriptWipes(empty)
			expect(#empty.updated).to.equal(0)
			again:__clearHold("NEW-ID")
			again:stop()
		end)

		it("lets small scripts be emptied", function()
			module.Source = "return 1"
			local patch = sourceUpdate("UI", "")
			session:__holdScriptWipes(patch)
			expect(#patch.updated).to.equal(1)
			expect(session.__heldWipes.UI).to.equal(nil)
		end)

		it("ends the hold when the file has code again or is removed", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			expect(session.__heldWipes.UI).to.be.ok()
			local fixed = sourceUpdate("UI", "return 'fixed'")
			session:__holdScriptWipes(fixed)
			expect(session.__heldWipes.UI).to.equal(nil)
			expect(#fixed.updated).to.equal(1)

			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:__holdScriptWipes({ removed = { "UI" }, added = {}, updated = {} })
			expect(session.__heldWipes.UI).to.equal(nil)
		end)

		it("ends existing holds as soon as the setting is turned off", function()
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			expect(session.__heldWipes.UI).to.be.ok()
			-- No patch arrives: the setting change alone ends the hold.
			Settings:set("guardScriptWipes", false)
			task.wait()
			local held = session.__heldWipes.UI
			local summary = session:getSyncSummary()
			Settings:set("guardScriptWipes", true)
			expect(held).to.equal(nil)
			expect(summary.heldWipeCount).to.equal(0)
		end)

		it("leaves held scripts as they are when the guard is turned off", function()
			session.__status = ServeSession.Status.Connected
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local kept = module.Source
			local reads = 0
			api.read = function()
				reads += 1
				return Promise.resolve({ instances = {} })
			end
			Settings:set("guardScriptWipes", false)
			task.wait(0.2)
			Settings:set("guardScriptWipes", true)
			expect(session.__instanceMap.fromIds.UI).to.equal(module)
			expect(module.Source).to.equal(kept)
			expect(reads).to.equal(0)
			expect(session.__heldWipes.UI).to.equal(nil)
		end)

		it("still applies a held class change after the guard is turned off", function()
			session.__status = ServeSession.Status.Connected
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session:__holdScriptWipes({
				removed = {},
				added = {},
				updated = { { id = "UI", changedClassName = "Script", changedProperties = {} } },
			})
			expect(session.__heldWipes.UI.deferredClass).to.equal("Script")
			Settings:set("guardScriptWipes", false)
			expect(session.__heldWipes.UI).to.equal(nil)
			expect(session.__refreshAfterHold.UI).to.equal(true)
			serverCopy("Script", "print('code again')")
			session:__applyPatch(sourceUpdate("UI", "print('code again')"))
			Settings:set("guardScriptWipes", true)
			local now = session.__instanceMap.fromIds.UI
			expect(now.ClassName).to.equal("Script")
			expect(now.Source).to.equal("print('code again')")
			expect(next(session.__refreshAfterHold)).to.equal(nil)
		end)

		it("does nothing when the setting is off", function()
			Settings:set("guardScriptWipes", false)
			local patch = sourceUpdate("UI", "")
			session:__holdScriptWipes(patch)
			Settings:set("guardScriptWipes", true)
			expect(#patch.updated).to.equal(1)
		end)

		-- Also records `source` as what verify saw, as `getSourceHashes` does.
		local function expectFrom(source, target)
			session.__verifiedRecords = { { id = "VERIFY", sources = { UI = source } } }
			return { UI = { from = hashSource(source), to = hashSource(target) } }
		end

		it("applies a held wipe when resynced with includeHeld", function()
			session.__status = ServeSession.Status.Connected
			session:__applyPatch(sourceUpdate("UI", ""))
			local kept = module.Source
			expect(string.find(kept, "return a") ~= nil).to.equal(true)
			local result = resyncAs(sourceUpdate("UI", ""), expectFrom(kept, ""), true)
			expect(result.updated).to.equal(1)
			expect(result.failed).to.equal(0)
			expect(module.Source).to.equal("")
			expect(session.__heldWipes.UI).to.equal(nil)
		end)

		it("leaves a held script alone in a fix without includeHeld", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			local expected = expectFrom(kept, "return 'one line'")
			-- The file became empty after verify built the fix.
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local hold = session.__heldWipes.UI
			local result = resyncAs(sourceUpdate("UI", "return 'one line'"), expected, false)
			expect(result.skipped).to.equal(1)
			expect(module.Source).to.equal(kept)
			expect(session.__heldWipes.UI).to.equal(hold)
		end)

		it("keeps an existing hold when a stale resync is skipped", function()
			session.__status = ServeSession.Status.Connected
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			local hold = session.__heldWipes.UI
			expect(hold).to.be.ok()
			local result = resyncAs(
				sourceUpdate("UI", "return 'from disk'"),
				expectFrom("return 'what verify saw'", "return 'from disk'"),
				false
			)
			expect(result.skipped).to.equal(1)
			expect(session.__heldWipes.UI).to.equal(hold)
		end)

		it("stops writing once the server says the batch was cancelled", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			local asked = 0
			local result = resyncAs(
				sourceUpdate("UI", "return 'cancelled'"),
				expectFrom(kept, "return 'cancelled'"),
				false,
				nil,
				function()
					asked += 1
					return false
				end
			)
			expect(asked).to.equal(1)
			expect(result.failed).to.equal(1)
			expect(module.Source).to.equal(kept)
		end)

		it("skips a script removed from the place since the batch was built", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			module.Parent = nil
			local result = resyncAs(sourceUpdate("UI", "return 'x'"), expectFrom(kept, "return 'x'"), false)
			expect(result.skipped).to.equal(1)
			expect(module.Source).to.equal(kept)
		end)

		it("writes nothing once the command expired", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			local result = resyncAs(
				sourceUpdate("UI", "return 'late'"),
				expectFrom(kept, "return 'late'"),
				false,
				DateTime.now().UnixTimestampMillis - 1
			)
			expect(result.failed).to.equal(1)
			expect(module.Source).to.equal(kept)
		end)

		it("refuses to write without its own undo step", function()
			session.__status = ServeSession.Status.Connected
			local ChangeHistoryService = game:GetService("ChangeHistoryService")
			local other = ChangeHistoryService:TryBeginRecording("Another change")
			local kept = module.Source
			local ok = pcall(
				session.applyResync,
				session,
				sourceUpdate("UI", "return 'x'"),
				expectFrom(kept, "return 'x'"),
				false
			)
			if other then
				ChangeHistoryService:FinishRecording(other, Enum.FinishRecordingOperation.Cancel)
			end
			expect(other ~= nil).to.equal(true)
			expect(ok).to.equal(false)
			expect(module.Source).to.equal(kept)
		end)

		it("writes the resync inside one undo step", function()
			session.__status = ServeSession.Status.Connected
			local ChangeHistoryService = game:GetService("ChangeHistoryService")
			local kept = module.Source
			resyncAs(sourceUpdate("UI", "return 'resynced'"), expectFrom(kept, "return 'resynced'"), false)
			expect(module.Source).to.equal("return 'resynced'")
			local canUndo, waypoint = ChangeHistoryService:GetCanUndo()
			expect(canUndo).to.equal(true)
			expect(string.sub(waypoint, 1, 13)).to.equal("Rojo: Resync ")
		end)

		it("still guards against wipes in a resync without includeHeld", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			local result = resyncAs(sourceUpdate("UI", ""), expectFrom(kept, ""), false)
			expect(module.Source).to.equal(kept)
			expect(result.updated).to.equal(0)
			expect(result.skipped).to.equal(1)
			expect(session.__heldWipes.UI).to.be.ok()
		end)

		it("starts no hold when the guard is turned off while a resync waits", function()
			session.__status = ServeSession.Status.Connected
			local kept = module.Source
			local isActive = function()
				-- The server check yields, and the user turns the guard off meanwhile.
				Settings:set("guardScriptWipes", false)
				task.wait()
				return true
			end
			local result =
				session:applyResync(sourceUpdate("UI", ""), expectFrom(kept, ""), false, nil, isActive, "VERIFY")
			Settings:set("guardScriptWipes", true)
			expect(result.updated).to.equal(1)
			expect(module.Source).to.equal("")
			expect(session.__heldWipes.UI).to.equal(nil)
			expect(session:getSyncSummary().heldWipeCount).to.equal(0)
		end)

		it("skips a script that changed since verify checked it", function()
			session.__status = ServeSession.Status.Connected
			local result = resyncAs(
				sourceUpdate("UI", "return 'from disk'"),
				expectFrom("return 'what verify saw'", "return 'from disk'"),
				false
			)
			expect(result.skipped).to.equal(1)
			expect(result.updated).to.equal(0)
			expect(string.find(module.Source, "return a") ~= nil).to.equal(true)
		end)

		it("refuses a resync that adds or removes instances or has no expectations", function()
			session.__status = ServeSession.Status.Connected
			local ok = pcall(session.applyResync, session, { removed = { "UI" }, added = {}, updated = {} }, {})
			expect(ok).to.equal(false)
			expect(pcall(session.applyResync, session, sourceUpdate("UI", "x"), nil)).to.equal(false)
		end)

		it("keeps held scripts out of the sync fallback", function()
			session.__status = ServeSession.Status.Connected
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			session.__reconciler = {
				applyPatch = function()
					return {
						removed = {},
						added = {},
						updated = { { id = "UI", changedProperties = {}, changedMetadata = {} } },
					}
				end,
			}
			local replaced = {}
			session.__replaceInstances = function(_, ids)
				for _, id in ids do
					table.insert(replaced, id)
				end
				return true, PatchSet.newEmpty()
			end
			local unapplied = session:__applyPatchInRecording(PatchSet.newEmpty())
			expect(#replaced).to.equal(0)
			expect(#unapplied.updated).to.equal(1)
			expect(string.find(module.Source, "return a") ~= nil).to.equal(true)
		end)

		it("drops holds of scripts removed with their parent", function()
			session.__status = ServeSession.Status.Connected
			session:__holdScriptWipes(sourceUpdate("UI", ""))
			expect(session.__heldWipes.UI).to.be.ok()
			-- The server removes the parent; the instance map forgets its
			-- descendants, which the patch does not list.
			session.__instanceMap:removeId("UI")
			session:__applyPatch({ removed = { "PARENT" }, added = {}, updated = {} })
			expect(session.__heldWipes.UI).to.equal(nil)
		end)
	end)

	describe("verify support", function()
		local hashSource = require(script.Parent.hashSource)

		it("hashes tracked scripts and lists ones removed from the place", function()
			local container = Instance.new("Folder")
			local inPlace = Instance.new("ModuleScript")
			inPlace.Source = "return 1\n"
			inPlace.Parent = container
			container.Parent = workspace
			local removed = Instance.new("ModuleScript")
			session.__instanceMap:insert("IN", inPlace)
			session.__instanceMap:insert("OUT", removed)
			session.__instanceMap:insert("FOLDER", container)
			session.__status = ServeSession.Status.Connected

			local report = session:getSourceHashes()
			container:Destroy()
			expect(#report.scripts).to.equal(1)
			local hash, length = hashSource("return 1\n")
			expect(report.scripts[1].id).to.equal("IN")
			expect(report.scripts[1].hash).to.equal(hash)
			expect(report.scripts[1].length).to.equal(length)
			expect(#report.detached).to.equal(1)
			expect(report.detached[1]).to.equal("OUT")
		end)

		it("keeps each verify's sources apart and only the last few", function()
			session:__rememberVerified("A", { UI = "first" })
			session:__rememberVerified("B", { UI = "second" })
			expect(session:__verifiedSourcesFor("A").UI).to.equal("first")
			expect(session:__verifiedSourcesFor("B").UI).to.equal("second")
			for index = 1, 10 do
				session:__rememberVerified("X" .. index, {})
			end
			expect(session:__verifiedSourcesFor("A")).to.equal(nil)
			expect(#session.__verifiedRecords <= 4).to.equal(true)
		end)

		it("refuses to hash before syncing starts", function()
			expect(pcall(session.getSourceHashes, session)).to.equal(false)
		end)

		it("counts undone Rojo changes, but not other undo steps", function()
			session.__status = ServeSession.Status.Connected
			session:__onRojoHistoryChanged("Rojo: Patch 10:00:00 AM", 1)
			session:__onRojoHistoryChanged("Move Part", 1)
			expect(session:getSyncSummary().undoneRojoPatches).to.equal(1)
			session:__onRojoHistoryChanged("Rojo: Patch 10:00:00 AM", -1)
			expect(session:getSyncSummary().undoneRojoPatches).to.equal(0)
		end)

		it("records patch counts in the sync summary", function()
			session.__status = ServeSession.Status.Connected
			session:__applyPatch(PatchSet.newEmpty())
			local summary = session:getSyncSummary()
			expect(summary.patchesApplied).to.equal(1)
			expect(summary.lastPatchUnapplied).to.equal(0)
			expect(type(summary.lastPatchAt)).to.equal("number")
		end)
	end)

	describe("backups before the first sync", function()
		local Settings = require(script.Parent.Settings)
		local HttpService = game:GetService("HttpService")
		local nativeBridgeRequest = StudioBridge.requestAsync
		local pairing = { protocolVersion = 1, pluginToken = "plugin-token", identity = { digest = "d" } }
		local fixture
		local previousSetting

		beforeEach(function()
			previousSetting = Settings:get("autoBackupBeforeSync")
			fixture = Instance.new("Folder")
			fixture.Name = "__RojoSyncBackupSpec"
			fixture.Parent = game:GetService("ReplicatedStorage")
			api.__baseUrl = "http://127.0.0.1:34872"
		end)

		afterEach(function()
			Settings:set("autoBackupBeforeSync", previousSetting)
			StudioBridge.requestAsync = nativeBridgeRequest
			fixture:Destroy()
		end)

		-- Runs the first sync with a catch-up patch that removes `removed`,
		-- and records the order in which the backup and the patch happen.
		local function syncRemoving(removed, serverInfo, events)
			events = events or {}
			local patch = PatchSet.newEmpty()
			table.insert(patch.removed, removed)
			session.__reconciler = {
				hydrate = function() end,
				diff = function()
					return true, patch
				end,
			}
			session.__applyPatch = function(_, applied)
				expect(applied).to.equal(patch)
				table.insert(events, "patch")
			end
			local pending = session:__initialSync(serverInfo)
			readResolve({ messageCursor = 0, instances = {} })
			pending:await()
			return events
		end

		it("saves what the catch-up patch removes before applying it", function()
			Settings:set("autoBackupBeforeSync", true)
			local removed = Instance.new("ModuleScript")
			removed.Name = "OnlyInStudio"
			removed.Parent = fixture
			local body
			local events
			StudioBridge.requestAsync = function(options)
				table.insert(events, "backup")
				body = HttpService:JSONDecode(options.Body)
				return {
					Success = true,
					StatusCode = 200,
					Body = HttpService:JSONEncode({ backup = { id = "20261107-000000-auto-sync" } }),
				}
			end
			events = {}
			syncRemoving(removed, { rootInstanceId = "ROOT", studioBridge = pairing }, events)
			expect(#events).to.equal(2)
			expect(events[1]).to.equal("backup")
			expect(events[2]).to.equal("patch")
			expect(body.instances[1].path).to.equal("game/ReplicatedStorage/__RojoSyncBackupSpec/OnlyInStudio")
			expect(body.instances[1].className).to.equal("ModuleScript")
			expect(type(body.model)).to.equal("string")
			expect(body.identity.digest).to.equal("d")
		end)

		it("still syncs when the backup cannot be saved", function()
			Settings:set("autoBackupBeforeSync", true)
			local removed = Instance.new("Folder")
			removed.Parent = fixture
			local calls = 0
			StudioBridge.requestAsync = function()
				calls += 1
				error("simulated server failure")
			end
			local refused = syncRemoving(removed, { rootInstanceId = "ROOT", studioBridge = pairing })
			expect(calls).to.equal(1)
			expect(refused[1]).to.equal("patch")
			-- A server without a Studio bridge cannot save one at all.
			local unpaired = syncRemoving(removed, { rootInstanceId = "ROOT" })
			expect(calls).to.equal(1)
			expect(unpaired[1]).to.equal("patch")
		end)

		it("warns about changed services it cannot back up and syncs anyway", function()
			Settings:set("autoBackupBeforeSync", true)
			local Log = require(script.Parent.Parent.Packages.Log)
			local nativeWarn = Log.warn
			local warnings = {}
			Log.warn = function(template, ...)
				table.insert(warnings, { template = template, args = { ... } })
			end
			local calls = 0
			StudioBridge.requestAsync = function()
				calls += 1
				error("no request expected")
			end
			local patch = PatchSet.newEmpty()
			table.insert(patch.updated, { id = "LIGHTING", changedProperties = { Brightness = { Float32 = 1 } } })
			session.__instanceMap.fromIds.LIGHTING = game:GetService("Lighting")
			local applied = false
			session.__reconciler = {
				hydrate = function() end,
				diff = function()
					return true, patch
				end,
			}
			session.__applyPatch = function()
				applied = true
			end
			local pending = session:__initialSync({ rootInstanceId = "ROOT", studioBridge = pairing })
			readResolve({ messageCursor = 0, instances = {} })
			pending:await()
			Log.warn = nativeWarn
			session.__instanceMap.fromIds.LIGHTING = nil
			expect(applied).to.equal(true)
			expect(calls).to.equal(0)
			expect(#warnings).to.equal(1)
			expect(warnings[1].template:find("cannot back up", 1, true)).to.be.ok()
			expect(warnings[1].args[2]).to.equal("game/Lighting")
		end)

		it("syncs anyway when collecting what to back up fails", function()
			Settings:set("autoBackupBeforeSync", true)
			local Log = require(script.Parent.Parent.Packages.Log)
			local StudioBackup = require(script.Parent.StudioBackup)
			local nativeWarn = Log.warn
			local warnings = {}
			Log.warn = function(template, ...)
				table.insert(warnings, { template = template, args = { ... } })
			end
			-- The wipe guard reads removed instances too; the stand-in below is
			-- not an Instance, which only this test's failure injection needs.
			local previousGuard = Settings:get("guardScriptWipes")
			Settings:set("guardScriptWipes", false)
			-- An affected instance that cannot be read, and no bridge at all.
			local inaccessible = setmetatable({}, {
				__index = function(_, key)
					error("The current thread cannot access '" .. tostring(key) .. "'")
				end,
			})
			session.__instanceMap.fromIds.LOCKED = inaccessible
			local unpaired = syncRemoving("LOCKED", { rootInstanceId = "ROOT" })
			session.__instanceMap.fromIds.LOCKED = nil
			-- And collecting itself failing.
			local nativeCollect = StudioBackup.collect
			StudioBackup.collect = function()
				error("simulated failure while collecting")
			end
			local failed = syncRemoving(Instance.new("Folder"), { rootInstanceId = "ROOT", studioBridge = pairing })
			StudioBackup.collect = nativeCollect
			Log.warn = nativeWarn
			Settings:set("guardScriptWipes", previousGuard)
			expect(unpaired[1]).to.equal("patch")
			expect(failed[1]).to.equal("patch")
			expect(#warnings).to.equal(2)
			expect(warnings[1].template:find("cannot back up", 1, true)).to.be.ok()
			expect(warnings[1].args[2]).to.equal(StudioBackup.INACCESSIBLE)
			expect(tostring(warnings[2].args[1]):find("simulated failure while collecting", 1, true)).to.be.ok()
		end)

		it("warns when a backed-up script is edited before the sync overwrites it", function()
			Settings:set("autoBackupBeforeSync", true)
			local Log = require(script.Parent.Parent.Packages.Log)
			local nativeWarn = Log.warn
			local nativeRead = ServeSession.readEditorText
			local warnings = {}
			Log.warn = function(template, ...)
				table.insert(warnings, { template = template, args = { ... } })
			end
			ServeSession.readEditorText = function(instance)
				return instance.Source
			end
			local removed = Instance.new("ModuleScript")
			removed.Name = "BeingEdited"
			removed.Source = "return 'captured'"
			removed.Parent = fixture
			StudioBridge.requestAsync = function()
				-- Someone types while the backup is being sent.
				removed.Source = "return 'typed during the upload'"
				return {
					Success = true,
					StatusCode = 200,
					Body = HttpService:JSONEncode({ backup = { id = "20261107-000000-auto-sync" } }),
				}
			end
			local events = syncRemoving(removed, { rootInstanceId = "ROOT", studioBridge = pairing })
			Log.warn = nativeWarn
			ServeSession.readEditorText = nativeRead
			expect(events[1]).to.equal("patch")
			expect(#warnings).to.equal(1)
			expect(warnings[1].template:find("newest edits are not in the backup", 1, true)).to.be.ok()
			expect(warnings[1].args[2]).to.equal("game/ReplicatedStorage/__RojoSyncBackupSpec/BeingEdited")
		end)

		it("does not apply the patch when the session stops while checking for later edits", function()
			Settings:set("autoBackupBeforeSync", true)
			local StudioBackup = require(script.Parent.StudioBackup)
			local nativeChanged = StudioBackup.changedSince
			local nativeRead = ServeSession.readEditorText
			ServeSession.readEditorText = function(instance)
				return instance.Source
			end
			StudioBackup.changedSince = function()
				-- The user disconnects while the scan yields.
				session:stop()
				return {}
			end
			StudioBridge.requestAsync = function()
				return {
					Success = true,
					StatusCode = 200,
					Body = HttpService:JSONEncode({ backup = { id = "20261107-000000-auto-sync" } }),
				}
			end
			local removed = Instance.new("ModuleScript")
			removed.Name = "StillHere"
			removed.Parent = fixture
			local events = syncRemoving(removed, { rootInstanceId = "ROOT", studioBridge = pairing })
			StudioBackup.changedSince = nativeChanged
			ServeSession.readEditorText = nativeRead
			expect(#events).to.equal(0)
			expect(removed.Parent).to.equal(fixture)
		end)

		it("skips a catch-up source write that lands after its backup's protection ran out", function()
			local Log = require(script.Parent.Parent.Packages.Log)
			local nativeWarn = Log.warn
			local nativeClock = ServeSession.clock
			local warnings = {}
			Log.warn = function(template, ...)
				table.insert(warnings, { template = template, args = { ... } })
			end
			local now = 1000
			ServeSession.clock = function()
				return now
			end
			local module = Instance.new("ModuleScript")
			module.Name = "Late"
			module.Source = "return 'studio'"
			module.Parent = fixture
			session.__instanceMap:insert("LATE", module)
			session.__status = ServeSession.Status.Connected
			-- Queued while the deadline was set, written after it passed.
			session.__catchUpWriteDeadline = now + 300
			now += 301
			session:__writeSource({ id = "LATE", instance = module, target = "return 'disk'" })
			session.__catchUpWriteDeadline = nil
			task.wait(0.3)
			local late = module.Source
			-- In time, the same write goes through.
			session.__catchUpWriteDeadline = now + 300
			session:__writeSource({ id = "LATE", instance = module, target = "return 'disk'" })
			session.__catchUpWriteDeadline = nil
			task.wait(0.3)
			Log.warn = nativeWarn
			ServeSession.clock = nativeClock
			expect(late).to.equal("return 'studio'")
			expect(module.Source).to.equal("return 'disk'")
			expect(#warnings).to.equal(1)
			expect(warnings[1].template:find("came too late", 1, true)).to.be.ok()
		end)

		it("gives the catch-up patch's writes a deadline from when the backup was saved", function()
			Settings:set("autoBackupBeforeSync", true)
			local nativeClock = ServeSession.clock
			ServeSession.clock = function()
				return 5000
			end
			StudioBridge.requestAsync = function()
				return {
					Success = true,
					StatusCode = 200,
					Body = HttpService:JSONEncode({ backup = { id = "20261107-000000-auto-sync" } }),
				}
			end
			local removed = Instance.new("Folder")
			removed.Parent = fixture
			local deadline
			local patch = PatchSet.newEmpty()
			table.insert(patch.removed, removed)
			session.__reconciler = {
				hydrate = function() end,
				diff = function()
					return true, patch
				end,
			}
			session.__applyPatch = function()
				deadline = session.__catchUpWriteDeadline
			end
			local pending = session:__initialSync({ rootInstanceId = "ROOT", studioBridge = pairing })
			readResolve({ messageCursor = 0, instances = {} })
			pending:await()
			ServeSession.clock = nativeClock
			expect(deadline).to.equal(5000 + 300)
			expect(session.__catchUpWriteDeadline).to.equal(nil)
		end)

		it("takes no backup when it is turned off", function()
			Settings:set("autoBackupBeforeSync", false)
			local removed = Instance.new("Folder")
			removed.Parent = fixture
			local calls = 0
			StudioBridge.requestAsync = function()
				calls += 1
				error("no request expected")
			end
			local events = syncRemoving(removed, { rootInstanceId = "ROOT", studioBridge = pairing })
			expect(calls).to.equal(0)
			expect(events[1]).to.equal("patch")
		end)

		it("exports the scripts verify --fix overwrites, leaving out those not in the place", function()
			local fixed = Instance.new("ModuleScript")
			fixed.Name = "Fixed"
			fixed.Source = "return 1"
			fixed.Parent = fixture
			session.__instanceMap:insert("FIXED", fixed)
			session.__status = ServeSession.Status.Connected
			local hashSource = require(script.Parent.hashSource)
			session:__rememberVerified("VERIFY", { FIXED = "return 1" })
			local exported = session:exportForBackup(
				{ "FIXED", "GONE" },
				{ FIXED = (hashSource("return 1")) },
				"VERIFY"
			)
			expect(exported.missing).to.equal(1)
			expect(#exported.ids).to.equal(1)
			expect(exported.ids[1]).to.equal("FIXED")
			expect(#exported.instances).to.equal(1)
			expect(exported.instances[1].path).to.equal("game/ReplicatedStorage/__RojoSyncBackupSpec/Fixed")
			expect(type(exported.model)).to.equal("string")
			expect(exported.bytes > 0).to.equal(true)
			local none = session:exportForBackup({ "GONE" }, {})
			expect(none.model).to.equal(nil)
			expect(#none.ids).to.equal(0)
			expect(#none.instances).to.equal(0)
		end)

		it("leaves out a script whose editor text cannot be read rather than export its saved source", function()
			local unreadable = Instance.new("ModuleScript")
			unreadable.Name = "Unreadable"
			unreadable.Source = "return 1"
			unreadable.Parent = fixture
			session.__instanceMap:insert("UNREADABLE", unreadable)
			session.__status = ServeSession.Status.Connected
			local hashSource = require(script.Parent.hashSource)
			local nativeRead = ServeSession.readEditorText
			ServeSession.readEditorText = function()
				return nil
			end
			local exported = session:exportForBackup({ "UNREADABLE" }, { UNREADABLE = (hashSource("return 1")) })
			ServeSession.readEditorText = nativeRead
			expect(exported.missing).to.equal(1)
			expect(#exported.ids).to.equal(0)
			expect(exported.model).to.equal(nil)
		end)

		it("warns about scripts whose unsaved editor text it could not back up", function()
			Settings:set("autoBackupBeforeSync", true)
			local Log = require(script.Parent.Parent.Packages.Log)
			local nativeWarn = Log.warn
			local nativeRead = ServeSession.readEditorText
			local warnings = {}
			Log.warn = function(template, ...)
				table.insert(warnings, { template = template, args = { ... } })
			end
			ServeSession.readEditorText = function()
				return nil
			end
			local sent = 0
			StudioBridge.requestAsync = function()
				sent += 1
				return {
					Success = true,
					StatusCode = 200,
					Body = HttpService:JSONEncode({ backup = { id = "20261107-000000-auto-sync" } }),
				}
			end
			local removed = Instance.new("ModuleScript")
			removed.Name = "OpenInEditor"
			removed.Parent = fixture
			local events = syncRemoving(removed, { rootInstanceId = "ROOT", studioBridge = pairing })
			Log.warn = nativeWarn
			ServeSession.readEditorText = nativeRead
			expect(sent).to.equal(1)
			expect(events[1]).to.equal("patch")
			expect(#warnings).to.equal(1)
			expect(warnings[1].template:find("Unsaved editor text could not be backed up", 1, true)).to.be.ok()
			expect(warnings[1].args[2]).to.equal("game/ReplicatedStorage/__RojoSyncBackupSpec/OpenInEditor")
		end)

		it("leaves out a script whose source differs from what verify saw though the hashes match", function()
			local hashSource = require(script.Parent.hashSource)
			-- Two sources known to share a hash and length.
			local checked, current = "return '07ilh49'", "return '0v3kbm8'"
			expect((hashSource(checked))).to.equal((hashSource(current)))
			local collided = Instance.new("ModuleScript")
			collided.Name = "Collided"
			collided.Source = current
			collided.Parent = fixture
			session.__instanceMap:insert("COLLIDED", collided)
			session.__status = ServeSession.Status.Connected
			session:__rememberVerified("VERIFY", { COLLIDED = checked })
			local exported = session:exportForBackup({ "COLLIDED" }, { COLLIDED = (hashSource(checked)) }, "VERIFY")
			expect(exported.missing).to.equal(1)
			expect(#exported.ids).to.equal(0)
			-- Without a record of what the verify saw, nothing is exported.
			collided.Source = checked
			expect(#session:exportForBackup({ "COLLIDED" }, { COLLIDED = (hashSource(checked)) }, "OTHER").ids).to.equal(
				0
			)
			expect(#session:exportForBackup({ "COLLIDED" }, { COLLIDED = (hashSource(checked)) }, "VERIFY").ids).to.equal(
				1
			)
		end)

		it("leaves out a script whose source changed since verify hashed it", function()
			local edited = Instance.new("ModuleScript")
			edited.Name = "Edited"
			edited.Source = "return 'edited after the check'"
			edited.Parent = fixture
			session.__instanceMap:insert("EDITED", edited)
			session.__status = ServeSession.Status.Connected
			local hashSource = require(script.Parent.hashSource)
			-- Verify saw "return 'checked'"; Studio has a newer edit now.
			local exported = session:exportForBackup({ "EDITED" }, { EDITED = (hashSource("return 'checked'")) })
			expect(exported.missing).to.equal(1)
			expect(#exported.ids).to.equal(0)
			expect(exported.model).to.equal(nil)
			-- A script without an expected hash is left out too.
			expect(#session:exportForBackup({ "EDITED" }, {}).ids).to.equal(0)
		end)
	end)

	describe("versionMismatchWarning", function()
		local Config = require(script.Parent.Config)
		local Version = require(script.Parent.Version)

		it("stays quiet for the same major and minor version", function()
			local same = string.format("%d.%d.99", Config.version[1], Config.version[2])
			expect(ServeSession.versionMismatchWarning(same)).to.equal(nil)
			expect(ServeSession.versionMismatchWarning(nil)).to.equal(nil)
			expect(ServeSession.versionMismatchWarning("garbage")).to.equal(nil)
		end)

		it("says which side to update", function()
			local newer = string.format("%d.%d.0", Config.version[1], Config.version[2] + 1)
			local older = string.format("%d.%d.0", Config.version[1] - 1, 0)
			local newerWarning = ServeSession.versionMismatchWarning(newer)
			local olderWarning = ServeSession.versionMismatchWarning(older)
			expect(newerWarning:find("rojo plugin install", 1, true) ~= nil).to.equal(true)
			expect(olderWarning:find("rokit.toml", 1, true) ~= nil).to.equal(true)
			expect(newerWarning:find(Version.display(Config.version), 1, true) ~= nil).to.equal(true)
		end)
	end)
end
