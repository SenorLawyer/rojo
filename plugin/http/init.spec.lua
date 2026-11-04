return function()
	local Http = require(script.Parent)
	local Promise = require(script.Parent.Parent.Promise)

	it("should load", function()
		require(script.Parent)
	end)

	describe("permission-aware timeouts", function()
		-- Each test uses its own host because permission answers last for the session.
		it("rejects a stalled request once the host's permission is answered", function()
			local url = "http://answered-timeout.test:34872/api/rojo"
			Http._recordPermissionAnswer(url, false)
			local pending = Promise.new(function() end)
			local ok, err = Http.timeout(pending, url, 0.01, "timed out"):await()
			expect(ok).to.equal(false)
			expect(err).to.equal("timed out")
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("waits for an open permission prompt instead of timing out", function()
			local url = "http://pending-timeout.test:34872/api/rojo"
			local waitingCalls = 0
			local request = Http.timeout(Promise.new(function() end), url, 0.01, "timed out", function()
				waitingCalls += 1
			end)
			task.wait(0.1)
			expect(request:getStatus()).to.equal(Promise.Status.Started)
			expect(waitingCalls).to.equal(1)

			-- The deadline restarts when the user answers the prompt.
			Http._recordPermissionAnswer(url, false)
			expect(request:getStatus()).to.equal(Promise.Status.Started)
			local ok, err = request:await()
			expect(ok).to.equal(false)
			expect(err).to.equal("timed out")
			expect(waitingCalls).to.equal(1)
		end)

		it("caps the wait on a possible permission prompt", function()
			-- Studio usually remembers the permission, so a first request to a
			-- host that stalls may be a stalled server rather than a prompt.
			expect(Http.permissionWaitLimit).to.equal(120)
			local url = "http://pending-capped.test:34872/api/rojo"
			local pending = Promise.new(function() end)
			local waitingCalls = 0
			local ok, err = Http.timeout(pending, url, 0.01, "timed out", function()
				waitingCalls += 1
			end, { permissionWaitLimit = 0.05, permissionRejectionValue = "no answer" }):await()
			expect(ok).to.equal(false)
			expect(err).to.equal("no answer")
			expect(waitingCalls).to.equal(1)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("enforces a hard limit regardless of permission state", function()
			local url = "http://pending-hard-limit.test:34872/api/rojo"
			local pending = Promise.new(function() end)
			local ok, err = Http.timeout(pending, url, 10, "timed out", nil, { limit = 0.05 }):await()
			expect(ok).to.equal(false)
			expect(err).to.equal("timed out")
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("does not extend a stalled request's deadline when other requests finish", function()
			local url = "http://answered-twice.test:34872/api/rojo"
			Http._recordPermissionAnswer(url, false)
			local request = Http.timeout(Promise.new(function() end), url, 0.2, "timed out")
			request:catch(function() end)
			task.wait(0.15)
			-- Another request to the same host finishing is not a new answer.
			Http._recordPermissionAnswer(url, false)
			task.wait(0.15)
			expect(request:getStatus()).to.equal(Promise.Status.Rejected)
		end)

		it("resolves with the request's result when it finishes in time", function()
			local url = "http://answered-success.test/api"
			local ok, value = Http.timeout(Promise.resolve("done"), url, 5, "timed out"):await()
			expect(ok).to.equal(true)
			expect(value).to.equal("done")
		end)

		it("cancels the request and stops waiting when cancelled", function()
			local url = "http://pending-cancel.test/api"
			local pending = Promise.new(function() end)
			local request = Http.timeout(pending, url, 0.01, "timed out")
			task.wait(0.05)
			request:cancel()
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("tracks permission per host regardless of port or path", function()
			Http._recordPermissionAnswer("http://denied-host.test:1/a", true)
			expect(Http.isPermissionPending("https://denied-host.test:2/b")).to.equal(false)
			expect(Http.isPermissionDenied("https://DENIED-HOST.test/c")).to.equal(true)
			expect(Http.isPermissionPending("http://never-asked.test/")).to.equal(true)

			Http._recordPermissionAnswer("http://denied-host.test:1/a", false)
			expect(Http.isPermissionDenied("http://denied-host.test/")).to.equal(false)
		end)
	end)
end
