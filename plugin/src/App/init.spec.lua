return function()
	local App = require(script.Parent)
	local ServeSession = require(script.Parent.Parent.ServeSession)
	local ApiContext = require(script.Parent.Parent.ApiContext)
	local Version = require(script.Parent.Parent.Version)
	local Http = require(script:FindFirstAncestor("Rojo").Packages.Http)
	local AppComponent = App({}).component
	local originalSessionNew = ServeSession.new
	local originalApiNew = ApiContext.new
	local originalGetUpdateMessage = Version.getUpdateMessage
	local created
	local failOnStart
	local controllers

	local function fakeApp()
		local controller = Instance.new("BindableFunction")
		controller.Parent = controllers
		return {
			agentController = controller,
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
		controllers = Instance.new("Folder")
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
		controllers:Destroy()
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

	it("wakes a pending confirmation when the session stops on its own", function()
		local app = fakeApp()
		local bindable = Instance.new("BindableEvent")
		app.confirmationBindable = bindable
		AppComponent.startSession(app)
		local session = app.serveSession

		local answer
		task.spawn(function()
			answer = bindable.Event:Wait()
		end)
		-- A place-ID change stops the session internally, not through endSession.
		session.callback(ServeSession.Status.Disconnected, "The place ID changed.")
		task.wait()
		expect(answer).to.equal("Abort")
		expect(app.serveSession).to.equal(nil)
		bindable:Destroy()
	end)

	describe("background probes", function()
		local function probeApp(host)
			local app = fakeApp()
			app.activeProbes = {}
			app.getHostAndPort = function()
				return host, "34872"
			end
			app.getBaseUrl = AppComponent.getBaseUrl
			return app
		end

		it("skips probing while a request is blocked on an unanswered prompt", function()
			local app = probeApp("probe-blocked.test")
			expect(AppComponent.isProbeBlocked(app)).to.equal(false)

			local finish = Http._beginOutstandingRequest("http://probe-blocked.test:34872/api/rojo")
			expect(AppComponent.isProbeBlocked(app)).to.equal(true)
			finish()
			expect(AppComponent.isProbeBlocked(app)).to.equal(false)
		end)

		it("probes alongside other requests once the host has answered", function()
			local app = probeApp("probe-answered.test")
			Http._recordPermissionAnswer("http://probe-answered.test:34872/api/rojo", false)
			local finish = Http._beginOutstandingRequest("http://probe-answered.test:34872/api/rojo")
			expect(AppComponent.isProbeBlocked(app)).to.equal(false)
			finish()
		end)

		it("skips probing while an earlier probe is still active", function()
			local app = probeApp("probe-active.test")
			app.activeProbes[{}] = true
			expect(AppComponent.isProbeBlocked(app)).to.equal(true)
		end)
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
