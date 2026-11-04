return function()
	local App = require(script.Parent)
	local ServeSession = require(script.Parent.Parent.ServeSession)
	local ApiContext = require(script.Parent.Parent.ApiContext)
	local Version = require(script.Parent.Parent.Version)
	local AppComponent = App({}).component
	local originalSessionNew = ServeSession.new
	local originalApiNew = ApiContext.new
	local originalGetUpdateMessage = Version.getUpdateMessage
	local created
	local failOnStart

	local function fakeApp()
		return {
			knownProjects = {},
			claimSyncLock = function()
				return true
			end,
			getHostAndPort = function()
				return "localhost", "34872"
			end,
			setState = function() end,
			addNotification = function() end,
			clearRunningConnectionInfo = function() end,
			releaseSyncLock = function() end,
			confirmationBindable = { Fire = function() end },
		}
	end

	beforeEach(function()
		created = {}
		failOnStart = false
		ApiContext.new = function()
			return {}
		end
		ServeSession.new = function()
			local session = {
				setUpdateLoadingTextCallback = function() end,
				hookPrecommit = function()
					return function() end
				end,
				hookPostcommit = function()
					return function() end
				end,
				onStatusChanged = function(self, callback)
					self.callback = callback
				end,
				setConfirmCallback = function() end,
				start = function(self)
					if failOnStart then
						self.callback(ServeSession.Status.Disconnected, "connection failed")
					end
				end,
				stop = function(self)
					self.callback(ServeSession.Status.Disconnected)
				end,
			}
			table.insert(created, session)
			return session
		end
	end)

	afterEach(function()
		ServeSession.new = originalSessionNew
		ApiContext.new = originalApiNew
		Version.getUpdateMessage = originalGetUpdateMessage
	end)

	it("does not leak a second session when connect is called twice", function()
		local app = fakeApp()
		AppComponent.startSession(app)
		AppComponent.startSession(app)
		expect(#created).to.equal(1)
		expect(app.serveSession).to.equal(created[1])
	end)

	it("does not restore a session that disconnects synchronously during start", function()
		local app = fakeApp()
		failOnStart = true
		AppComponent.startSession(app)
		expect(app.serveSession).to.equal(nil)
		AppComponent.startSession(app)
		expect(#created).to.equal(2)
	end)

	it("allows reconnect after disconnecting", function()
		local app = fakeApp()
		AppComponent.startSession(app)
		AppComponent.endSession(app)
		AppComponent.startSession(app)
		expect(#created).to.equal(2)
		expect(app.serveSession).to.equal(created[2])
	end)

	it("ignores stale status callbacks from a previous session", function()
		local app = fakeApp()
		AppComponent.startSession(app)
		AppComponent.endSession(app)
		AppComponent.startSession(app)
		created[1].callback(ServeSession.Status.Disconnected, "late error")
		expect(app.serveSession).to.equal(created[2])
	end)

	it("does not reconnect after the app is unmounted", function()
		local app = fakeApp()
		app.__unmounted = true
		AppComponent.startSession(app)
		expect(#created).to.equal(0)
	end)

	it("shows a cancellable page once the initial sync is answered", function()
		local app = fakeApp()
		local states = {}
		app.setState = function(_, state)
			table.insert(states, state)
		end
		AppComponent.startSession(app)
		local session = app.serveSession
		table.clear(states)

		expect(AppComponent.onConfirmationAnswered(app, session, "Accept")).to.equal("Accept")
		expect(#states).to.equal(1)
		expect(states[1].appStatus).to.equal("Connecting")

		expect(AppComponent.onConfirmationAnswered(app, session, "Reject")).to.equal("Reject")
		expect(states[2].appStatus).to.equal("Connecting")

		-- The connecting page's Cancel must end the session that is still working.
		AppComponent.endSession(app)
		expect(app.serveSession).to.equal(nil)
	end)

	it("leaves the page alone when the answer is an abort or the session ended", function()
		local app = fakeApp()
		local states = {}
		app.setState = function(_, state)
			table.insert(states, state)
		end
		AppComponent.startSession(app)
		local session = app.serveSession
		table.clear(states)

		expect(AppComponent.onConfirmationAnswered(app, session, "Abort")).to.equal("Abort")
		AppComponent.endSession(app)
		table.clear(states)
		expect(AppComponent.onConfirmationAnswered(app, session, "Accept")).to.equal("Accept")
		expect(#states).to.equal(0)
	end)

	it("defers update checks and ignores results after unmounting", function()
		local app = fakeApp()
		local gate = Instance.new("BindableEvent")
		local notifications = 0
		local calls = 0
		app.addNotification = function()
			notifications += 1
		end
		Version.getUpdateMessage = function()
			calls += 1
			gate.Event:Wait()
			return "update available"
		end
		AppComponent.checkForUpdates(app)
		expect(calls).to.equal(0)
		task.wait()
		expect(calls).to.equal(1)
		app.__unmounted = true
		gate:Fire()
		task.wait()
		expect(notifications).to.equal(0)
		gate:Destroy()
	end)
end
