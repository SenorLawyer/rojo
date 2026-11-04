return function()
	local Promise = require(script.Parent.Parent.Packages.Promise)
	local ServeSession = require(script.Parent.ServeSession)
	local PatchSet = require(script.Parent.PatchSet)

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
		api.connect = function(onWaitingForPermission)
			return Promise.new(function()
				onWaitingForPermission()
			end)
		end
		session:start()
		expect(string.find(loadingText, "choose Allow", 1, true) ~= nil).to.equal(true)
		expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
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

	it("keeps syncing when the place identity still matches", function()
		session:__onPlaceIdChanged()
		expect(session:getStatus()).to.equal(ServeSession.Status.NotStarted)
	end)
end
