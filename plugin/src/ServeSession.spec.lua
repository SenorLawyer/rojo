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
end
