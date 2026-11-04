return function()
	local StreamRequest = require(script.Parent.StreamRequest)
	local clients
	local request
	beforeEach(function()
		clients = {}
		request = StreamRequest.new(function(options)
			local events = {}
			local client = { options = options, closed = false, events = events }
			for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
				events[name] = Instance.new("BindableEvent")
				client[name] = events[name].Event
			end
			function client:Close()
				self.closed = true
				self.events.Closed:Fire()
			end
			table.insert(clients, client)
			return client
		end)
	end)
	afterEach(function()
		for _, client in clients do
			for _, event in client.events do
				event:Destroy()
			end
		end
	end)
	it("preserves binary chunks and HTTP error bodies", function()
		local pending = request({ Url = "test" })
		local client = clients[1]
		client.events.Opened:Fire(410, "Content-Type: application/json\r\n")
		client.events.MessageReceived:Fire(string.char(0, 255))
		client.events.MessageReceived:Fire("response")
		client.events.Closed:Fire()
		local result = pending:expect()
		expect(result.Success).to.equal(false)
		expect(result.StatusCode).to.equal(410)
		expect(result.Body).to.equal(string.char(0, 255) .. "response")
		expect(client.closed).to.equal(true)
	end)
	it("closes abandoned HTTP requests and starts queued work without overlap", function()
		local first = request({ Url = "first" })
		local second = request({ Url = "second" })
		expect(#clients).to.equal(1)
		first:cancel()
		expect(clients[1].closed).to.equal(true)
		task.wait()
		expect(#clients).to.equal(2)
		clients[1].events.Error:Fire(500, "late")
		clients[2].events.Opened:Fire(200, "")
		clients[2].events.MessageReceived:Fire("current")
		clients[2].events.Closed:Fire()
		expect(second:expect().Body).to.equal("current")
	end)
	it("never sends a cancelled queued write", function()
		local first = request({ Url = "first" })
		local second = request({ Url = "write", Method = "POST", Body = "change" })
		second:cancel()
		first:cancel()
		task.wait()
		expect(#clients).to.equal(1)
	end)
	it("keeps separate transport queues independent while a bridge poll is pending", function()
		local pollClient
		local poll = StreamRequest.new(function()
			pollClient = {}
			for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
				pollClient[name] = Instance.new("BindableEvent")
			end
			return {
				Opened = pollClient.Opened.Event,
				MessageReceived = pollClient.MessageReceived.Event,
				Error = pollClient.Error.Event,
				Closed = pollClient.Closed.Event,
				Close = function() end,
			}
		end)({ Url = "poll" })
		local normal = request({ Url = "read" })
		clients[1].events.Opened:Fire(200, "")
		clients[1].events.MessageReceived:Fire("sync data")
		clients[1].events.Closed:Fire()
		expect(normal:expect().Body).to.equal("sync data")
		expect(poll:getStatus()).to.equal("Started")
		poll:cancel()
		for _, event in pollClient do
			event:Destroy()
		end
	end)
	it("rejects failed streams and releases their slot", function()
		local first = request({ Url = "first" })
		first:catch(function() end)
		clients[1].events.Error:Fire(0, "connection lost")
		expect(select(1, first:await())).to.equal(false)
		expect(clients[1].closed).to.equal(true)
		local second = request({ Url = "second" })
		expect(#clients).to.equal(2)
		second:cancel()
	end)
	it("reports connection failures as HttpError kinds", function()
		local HttpError = require(script.Parent.Error)
		local first = request({ Url = "first" })
		clients[1].events.Error:Fire(0, "ConnectFail")
		local ok, err = first:await()
		expect(ok).to.equal(false)
		expect(err.type).to.equal(HttpError.Kind.ConnectFailed)
		local failing = StreamRequest.new(function()
			error("Http requests are not enabled. Enable via game settings")
		end)
		local disabledOk, disabledErr = failing({ Url = "disabled" }):await()
		expect(disabledOk).to.equal(false)
		expect(disabledErr.type).to.equal(HttpError.Kind.HttpNotEnabled)
	end)
	it("turns non-2xx statuses reported through Error into HTTP responses", function()
		local first = request({ Url = "first" })
		clients[1].events.Error:Fire(410, '{"error":"Request expired"}')
		local result = first:expect()
		expect(result.Success).to.equal(false)
		expect(result.StatusCode).to.equal(410)
		expect(result.Body).to.equal('{"error":"Request expired"}')
		expect(clients[1].closed).to.equal(true)

		local second = request({ Url = "second" })
		task.wait()
		local client = clients[2]
		client.events.Opened:Fire(503, "")
		client.events.MessageReceived:Fire("streamed body")
		client.events.Error:Fire(503, "Service Unavailable")
		local streamed = second:expect()
		expect(streamed.StatusCode).to.equal(503)
		expect(streamed.Body).to.equal("streamed body")
	end)
	it("parses empty header values without swallowing the next header", function()
		local pending = request({ Url = "headers" })
		clients[1].events.Opened:Fire(200, "Empty:\r\nX-Rojo-Content-Transfer: base64\r\n")
		clients[1].events.Closed:Fire()
		local result = pending:expect()
		expect(result.Headers.empty).to.equal("")
		expect(result.Headers["x-rojo-content-transfer"]).to.equal("base64")
	end)
	it("times out an inactive request, closes it and releases the slot", function()
		local HttpError = require(script.Parent.Error)
		local stalled = request({ Url = "stalled" }, { timeout = 0.05 })
		local queued = request({ Url = "queued" })
		task.wait(0.2)
		local ok, err = stalled:await()
		expect(ok).to.equal(false)
		expect(err.type).to.equal(HttpError.Kind.Timeout)
		expect(clients[1].closed).to.equal(true)
		expect(#clients).to.equal(2)
		clients[2].events.Opened:Fire(200, "")
		clients[2].events.Closed:Fire()
		expect(queued:expect().StatusCode).to.equal(200)
	end)
	it("keeps a request alive while its response is still arriving", function()
		local pending = request({ Url = "slow" }, { timeout = 0.15 })
		local client = clients[1]
		client.events.Opened:Fire(200, "")
		for _ = 1, 4 do
			task.wait(0.075)
			client.events.MessageReceived:Fire("x")
		end
		client.events.Closed:Fire()
		expect(pending:expect().Body).to.equal("xxxx")
	end)
	it("bounds the wait for a busy slot separately from the active request", function()
		local HttpError = require(script.Parent.Error)
		local first = request({ Url = "first" })
		local second = request({ Url = "second" }, { queueTimeout = 0.05 })
		task.wait(0.2)
		local ok, err = second:await()
		expect(ok).to.equal(false)
		expect(err.type).to.equal(HttpError.Kind.Timeout)
		expect(first:getStatus()).to.equal("Started")
		expect(#clients).to.equal(1)
		first:cancel()
	end)
	it("does not time out while Studio's HTTP permission prompt may be open", function()
		local promptOpen = true
		local pending = request({ Url = "prompt" }, {
			timeout = 0.05,
			holdWhile = function()
				return promptOpen
			end,
		})
		task.wait(0.2)
		expect(pending:getStatus()).to.equal("Started")
		clients[1].events.Opened:Fire(200, "")
		clients[1].events.Closed:Fire()
		expect(pending:expect().StatusCode).to.equal(200)
		promptOpen = false
	end)
	it("stops holding a request once the permission wait limit is reached", function()
		local HttpError = require(script.Parent.Error)
		local pending = request({ Url = "unanswered-prompt" }, {
			timeout = 0.05,
			holdWhile = function()
				return true
			end,
			holdLimit = 0.1,
		})
		task.wait(0.4)
		expect(pending:getStatus()).to.equal("Rejected")
		local _, err = pending:await()
		expect(err.type).to.equal(HttpError.Kind.Timeout)
	end)
	it("keeps Studio's wording for a denied HTTP permission", function()
		local denied = StreamRequest.new(function()
			error("HttpService permission denied")
		end)
		local ok, err = denied({ Url = "denied" }):await()
		expect(ok).to.equal(false)
		expect(string.find(string.lower(tostring(err)), "permission denied", 1, true) ~= nil).to.equal(true)
	end)
	it("closes a client created after its request was cancelled", function()
		local created
		local release = Instance.new("BindableEvent")
		local yielding = StreamRequest.new(function()
			release.Event:Wait()
			created = { closed = false }
			function created:Close()
				self.closed = true
			end
			return created
		end)
		local pending = yielding({ Url = "slow-create" })
		pending:cancel()
		release:Fire()
		task.wait()
		expect(created).to.be.ok()
		expect(created.closed).to.equal(true)
		release:Destroy()
	end)
	it("settles and frees the slot when closing a failed client throws", function()
		local throwingClients = {}
		local throwing = StreamRequest.new(function()
			local client = { events = {}, closeCalls = 0 }
			for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
				client.events[name] = Instance.new("BindableEvent")
				client[name] = client.events[name].Event
			end
			function client:Close()
				self.closeCalls += 1
				error("WebStreamClient is already closed")
			end
			table.insert(throwingClients, client)
			return client
		end)
		local first = throwing({ Url = "conflict" })
		throwingClients[1].events.Error:Fire(409, "Conflict")
		local result = first:expect()
		expect(result.StatusCode).to.equal(409)
		expect(throwingClients[1].closeCalls).to.equal(1)

		local second = throwing({ Url = "after-conflict" })
		task.wait()
		expect(#throwingClients).to.equal(2)
		throwingClients[2].events.Error:Fire(0, "connection lost")
		expect(select(1, second:await())).to.equal(false)
		local third = throwing({ Url = "after-failure" })
		task.wait()
		expect(#throwingClients).to.equal(3)
		third:cancel()
		expect(throwingClients[3].closeCalls).to.equal(1)
		for _, client in throwingClients do
			for _, event in client.events do
				event:Destroy()
			end
		end
	end)

	it("closes the client on every way a request can end", function()
		local HttpError = require(script.Parent.Error)
		local cases = {
			openedError = function(client)
				client.events.Opened:Fire(409, "Content-Type: text/plain\r\n")
				client.events.MessageReceived:Fire("identity changed")
				client.events.Closed:Fire()
			end,
			errorWithStatus = function(client)
				client.events.Error:Fire(409, "Conflict")
			end,
			errorAfterOpened = function(client)
				client.events.Opened:Fire(200, "")
				client.events.Error:Fire(0, "connection reset")
			end,
			errorBeforeOpened = function(client)
				client.events.Error:Fire(0, "ConnectFail")
			end,
			closedWithoutResponse = function(client)
				client.events.Closed:Fire()
			end,
		}
		for name, run in cases do
			local pending = request({ Url = name })
			local client = clients[#clients]
			run(client)
			local ok, value = pending:await()
			expect(client.closed).to.equal(true)
			if name == "openedError" or name == "errorWithStatus" then
				-- A 409 settles as soon as it arrives, not after a timeout.
				expect(ok).to.equal(true)
				expect(value.StatusCode).to.equal(409)
			else
				expect(ok).to.equal(false)
			end
			task.wait()
		end

		local timedOut = request({ Url = "timeout" }, { timeout = 0.05 })
		local timedOutClient = clients[#clients]
		local ok, err = timedOut:await()
		expect(ok).to.equal(false)
		expect(err.type).to.equal(HttpError.Kind.Timeout)
		expect(timedOutClient.closed).to.equal(true)
		task.wait()

		local cancelled = request({ Url = "cancel" })
		local cancelledClient = clients[#clients]
		cancelled:cancel()
		expect(cancelledClient.closed).to.equal(true)
	end)

	it("reports whether headers and the body came from the server", function()
		local opened = request({ Url = "opened" })
		clients[1].events.Opened:Fire(200, "")
		clients[1].events.MessageReceived:Fire("body")
		clients[1].events.Closed:Fire()
		local openedResult = opened:expect()
		expect(openedResult.HeadersReceived).to.equal(true)
		expect(openedResult.BodyFromServer).to.equal(true)

		local bare = request({ Url = "bare-error" })
		task.wait()
		clients[2].events.Error:Fire(503, "Service Unavailable")
		local bareResult = bare:expect()
		expect(bareResult.HeadersReceived).to.equal(false)
		expect(bareResult.BodyFromServer).to.equal(false)
		expect(bareResult.Body).to.equal("Service Unavailable")
	end)

	it("reports client creation even after its request ended", function()
		local release = Instance.new("BindableEvent")
		local yielding = StreamRequest.new(function()
			release.Event:Wait()
			local client = {}
			function client:Close() end
			return client
		end)
		local created
		local pending = yielding({ Url = "prompt" }, {
			onCreated = function(ok, err)
				created = { ok = ok, err = err }
			end,
		})
		pending:cancel()
		expect(created).to.equal(nil)
		release:Fire()
		task.wait()
		expect(created.ok).to.equal(true)
		release:Destroy()

		local failed
		StreamRequest.new(function()
			error("HttpService permission denied")
		end)({ Url = "denied" }, {
			onCreated = function(ok, err)
				failed = { ok = ok, err = err }
			end,
		}):catch(function() end)
		task.wait()
		expect(failed.ok).to.equal(false)
		expect(string.find(tostring(failed.err), "permission denied", 1, true) ~= nil).to.equal(true)
	end)

	it("keeps the slot while a cancelled request's client is still being created", function()
		local release = Instance.new("BindableEvent")
		local created = {}
		local creating = 0
		local yielding = StreamRequest.new(function()
			creating += 1
			if creating == 1 then
				release.Event:Wait()
			end
			local client = { closed = false, events = {} }
			for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
				client.events[name] = Instance.new("BindableEvent")
				client[name] = client.events[name].Event
			end
			function client:Close()
				self.closed = true
			end
			table.insert(created, client)
			return client
		end)
		local first = yielding({ Url = "slow-create" })
		local second = yielding({ Url = "next" })
		first:cancel()
		task.wait()
		-- A second client must not be created alongside the first.
		expect(creating).to.equal(1)
		release:Fire()
		task.wait()
		expect(created[1].closed).to.equal(true)
		expect(creating).to.equal(2)
		created[2].events.Opened:Fire(200, "")
		created[2].events.Closed:Fire()
		expect(second:expect().StatusCode).to.equal(200)
		release:Destroy()
		for _, client in created do
			for _, event in client.events do
				event:Destroy()
			end
		end
	end)

	it("reports when a request leaves the queue and when the server first responds", function()
		local events = {}
		local first = request({ Url = "first" }, {
			onStarted = function()
				table.insert(events, "first started")
			end,
			onResponse = function()
				table.insert(events, "first response")
			end,
		})
		local second = request({ Url = "second" }, {
			onStarted = function()
				table.insert(events, "second started")
			end,
		})
		task.wait()
		expect(table.concat(events, ",")).to.equal("first started")
		clients[1].events.Opened:Fire(200, "")
		clients[1].events.MessageReceived:Fire("a")
		clients[1].events.MessageReceived:Fire("b")
		task.wait()
		expect(table.concat(events, ",")).to.equal("first started,first response")
		clients[1].events.Closed:Fire()
		first:expect()
		task.wait()
		expect(table.concat(events, ",")).to.equal("first started,first response,second started")
		second:cancel()
	end)

	it("reports a status delivered through Error as a server response", function()
		local responded = false
		local pending = request({ Url = "status-error" }, {
			onResponse = function()
				responded = true
			end,
		})
		clients[1].events.Error:Fire(410, "Gone")
		pending:expect()
		task.wait()
		expect(responded).to.equal(true)

		local failedResponded = false
		local failed = request({ Url = "connect-fail" }, {
			onResponse = function()
				failedResponded = true
			end,
		})
		task.wait()
		clients[2].events.Error:Fire(0, "ConnectFail")
		failed:await()
		task.wait()
		expect(failedResponded).to.equal(false)
	end)

	it("fails and closes a response that exceeds its byte limit", function()
		local pending = request({ Url = "huge" }, { maxBytes = 8 })
		local client = clients[1]
		client.events.Opened:Fire(200, "")
		client.events.MessageReceived:Fire("12345")
		client.events.MessageReceived:Fire("6789")
		local ok, err = pending:await()
		expect(ok).to.equal(false)
		expect(string.find(tostring(err), "exceeded 8 bytes", 1, true) ~= nil).to.equal(true)
		expect(client.closed).to.equal(true)
		expect(StreamRequest.DEFAULT_MAX_BYTES).to.equal(64 * 1024 * 1024)
	end)

	it("releases a request only once it finished and its client creation returned", function()
		local release = Instance.new("BindableEvent")
		local yielding = StreamRequest.new(function()
			release.Event:Wait()
			local client = {}
			function client:Close() end
			return client
		end)
		local released = 0
		local pending = yielding({ Url = "slow-create" }, {
			onReleased = function()
				released += 1
			end,
		})
		pending:cancel()
		expect(released).to.equal(0)
		release:Fire()
		task.wait()
		expect(released).to.equal(1)
		release:Destroy()

		local queuedReleased = 0
		local first = request({ Url = "first" })
		local queued = request({ Url = "queued" }, {
			onReleased = function()
				queuedReleased += 1
			end,
		})
		queued:cancel()
		expect(queuedReleased).to.equal(1)
		first:cancel()
		expect(queuedReleased).to.equal(1)
	end)

	describe("stream budget", function()
		local StreamBudget = require(script.Parent.StreamBudget)
		local HttpError = require(script.Parent.Error)
		local budget
		local budgeted
		local budgetClients
		local gate
		local failCreation

		local function newClient(options)
			local client = { options = options, closed = false, events = {} }
			for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
				client.events[name] = Instance.new("BindableEvent")
				client[name] = client.events[name].Event
			end
			function client:Close()
				self.closed = true
			end
			table.insert(budgetClients, client)
			return client
		end

		beforeEach(function()
			budget = StreamBudget.new()
			budgetClients = {}
			gate = nil
			failCreation = false
			budgeted = StreamRequest.new(function(options)
				if gate then
					gate.Event:Wait()
				end
				if failCreation then
					error("could not create the client")
				end
				return newClient(options)
			end, budget)
		end)

		afterEach(function()
			for _, client in budgetClients do
				for _, event in client.events do
					event:Destroy()
				end
			end
			if gate then
				gate:Destroy()
			end
		end)

		it("gives the slot back exactly once however a request ends", function()
			local cases = {
				success = function(client)
					client.events.Opened:Fire(200, "")
					client.events.Closed:Fire()
				end,
				errorWithStatus = function(client)
					client.events.Error:Fire(409, "Conflict")
				end,
				errorWithoutStatus = function(client)
					client.events.Error:Fire(0, "connection reset")
				end,
				closedWithoutResponse = function(client)
					client.events.Closed:Fire()
				end,
			}
			for _, run in cases do
				local pending = budgeted({ Url = "case" })
				pending:catch(function() end)
				expect(budget:liveCount()).to.equal(1)
				local client = budgetClients[#budgetClients]
				run(client)
				pending:await()
				expect(client.closed).to.equal(true)
				expect(budget:liveCount()).to.equal(0)
				-- A late event from a finished client must not release twice.
				client.events.Error:Fire(0, "late")
				client.events.Closed:Fire()
				expect(budget:liveCount()).to.equal(0)
				task.wait()
			end

			local timedOut = budgeted({ Url = "timeout" }, { timeout = 0.05 })
			expect(budget:liveCount()).to.equal(1)
			local ok, err = timedOut:await()
			expect(ok).to.equal(false)
			expect(err.type).to.equal(HttpError.Kind.Timeout)
			expect(budget:liveCount()).to.equal(0)
			task.wait()

			local cancelled = budgeted({ Url = "cancel" })
			expect(budget:liveCount()).to.equal(1)
			cancelled:cancel()
			expect(budgetClients[#budgetClients].closed).to.equal(true)
			expect(budget:liveCount()).to.equal(0)
		end)

		it("gives the slot back when creating the client fails", function()
			failCreation = true
			local ok = budgeted({ Url = "fails" }):await()
			expect(ok).to.equal(false)
			expect(budget:liveCount()).to.equal(0)
		end)

		it("keeps the slot until a client created after cancellation is closed", function()
			gate = Instance.new("BindableEvent")
			local pending = budgeted({ Url = "slow-create" })
			expect(budget:liveCount()).to.equal(1)
			pending:cancel()
			-- The client may still arrive, so its slot is still taken.
			expect(budget:liveCount()).to.equal(1)
			gate:Fire()
			task.wait()
			expect(budgetClients[1].closed).to.equal(true)
			expect(budget:liveCount()).to.equal(0)
		end)

		it("gives the slot back when a cancelled client's creation fails late", function()
			gate = Instance.new("BindableEvent")
			failCreation = true
			local pending = budgeted({ Url = "slow-failure" })
			pending:cancel()
			expect(budget:liveCount()).to.equal(1)
			gate:Fire()
			task.wait()
			expect(budget:liveCount()).to.equal(0)
		end)

		it("hands queued work back to the caller while the request ahead is stuck creating its client", function()
			gate = Instance.new("BindableEvent")
			local stuck = budgeted({ Url = "stuck-create" })
			expect(budget:liveCount()).to.equal(1)
			local released = 0
			local started = 0
			local queued = budgeted({ Url = "queued" }, {
				queueTimeout = 0.05,
				onReleased = function()
					released += 1
				end,
				onStarted = function()
					started += 1
				end,
			})
			task.wait(0.2)
			-- Waiting longer would fail every later request, so the queued one
			-- falls back like a request whose budget ran out.
			expect(queued:getStatus()).to.equal("Resolved")
			expect(queued:expect()).to.equal(StreamRequest.BudgetExhausted)
			expect(released).to.equal(1)
			expect(started).to.equal(0)
			-- Only the stuck request holds a slot, and no second client was made.
			expect(budget:liveCount()).to.equal(1)
			expect(#budgetClients).to.equal(0)

			stuck:cancel()
			gate:Fire()
			task.wait()
			expect(budgetClients[1].closed).to.equal(true)
			expect(budget:liveCount()).to.equal(0)
			expect(released).to.equal(1)
		end)

		it("still times out queued work behind a request that is waiting on the server", function()
			local active = budgeted({ Url = "slow-server" })
			local ok, err = budgeted({ Url = "queued" }, { queueTimeout = 0.05 }):await()
			expect(ok).to.equal(false)
			expect(err.type).to.equal(HttpError.Kind.Timeout)
			expect(#budgetClients).to.equal(1)
			active:cancel()
			expect(budget:liveCount()).to.equal(0)
		end)

		it("creates no client when the budget is exhausted", function()
			-- A socket and another transport's request already hold both slots.
			local releaseSocket = budget:tryAcquire(StreamBudget.Priority.Socket)
			local releaseOther = budget:tryAcquire(StreamBudget.Priority.Request)
			expect(releaseSocket).to.be.ok()
			expect(releaseOther).to.be.ok()
			local released = 0
			local result = budgeted({ Url = "exhausted" }, {
				onReleased = function()
					released += 1
				end,
			}):expect()
			expect(result).to.equal(StreamRequest.BudgetExhausted)
			expect(#budgetClients).to.equal(0)
			expect(released).to.equal(1)
			expect(budget:liveCount()).to.equal(2)

			-- Once a slot is free again, requests use streams again.
			releaseOther()
			local pending = budgeted({ Url = "after" })
			expect(#budgetClients).to.equal(1)
			pending:cancel()
			releaseSocket()
			expect(budget:liveCount()).to.equal(0)
		end)
	end)

	describe("StreamBudget", function()
		local StreamBudget = require(script.Parent.StreamBudget)

		it("keeps a slot free for the sync socket", function()
			local budget = StreamBudget.new()
			local releaseRequest = budget:tryAcquire(StreamBudget.Priority.Request)
			expect(releaseRequest).to.be.ok()
			-- The last slot is the socket's.
			expect(budget:tryAcquire(StreamBudget.Priority.Request)).to.equal(nil)
			local releaseSocket = budget:tryAcquire(StreamBudget.Priority.Socket)
			expect(releaseSocket).to.be.ok()
			expect(budget:tryAcquire(StreamBudget.Priority.Socket)).to.equal(nil)
			releaseRequest()
			-- With the socket open, its reserve is in use and a request may
			-- take the other slot.
			local again = budget:tryAcquire(StreamBudget.Priority.Request)
			expect(again).to.be.ok()
			again()
			releaseSocket()
			expect(budget:liveCount()).to.equal(0)
		end)

		it("lets the socket take a slot while a request holds the other", function()
			local budget = StreamBudget.new()
			local releaseRequest = budget:tryAcquire(StreamBudget.Priority.Request)
			local releaseSocket = budget:tryAcquire(StreamBudget.Priority.Socket)
			expect(releaseSocket).to.be.ok()
			expect(budget:liveCount()).to.equal(2)
			releaseRequest()
			releaseRequest()
			releaseSocket()
			releaseSocket()
			expect(budget:liveCount()).to.equal(0)
		end)

		it("allows at most three Rojo stream clients by default", function()
			expect(StreamBudget.DEFAULT_LIMIT).to.equal(3)
			expect(StreamBudget.shared.limit).to.equal(3)
			expect(StreamBudget.shared.socketReserve).to.equal(1)
			expect(StreamBudget.shared.bridgeReserve).to.equal(1)
		end)

		it("keeps a slot for the bridge that requests and sockets cannot take", function()
			local budget = StreamBudget.new()
			local releaseRequest = budget:tryAcquire(StreamBudget.Priority.Request)
			local releaseSocket = budget:tryAcquire(StreamBudget.Priority.Socket)
			expect(releaseRequest).to.be.ok()
			expect(releaseSocket).to.be.ok()
			-- The last slot is the bridge's.
			expect(budget:tryAcquire(StreamBudget.Priority.Request)).to.equal(nil)
			expect(budget:tryAcquire(StreamBudget.Priority.Socket)).to.equal(nil)
			local releaseBridge = budget:tryAcquire(StreamBudget.Priority.Bridge)
			expect(releaseBridge).to.be.ok()
			expect(budget:tryAcquire(StreamBudget.Priority.Bridge)).to.equal(nil)
			releaseBridge()
			releaseBridge()
			releaseRequest()
			releaseSocket()
			expect(budget:liveCount()).to.equal(0)
		end)

		it("lets the bridge start before sync without taking the socket's slot", function()
			local budget = StreamBudget.new()
			local releaseBridge = budget:tryAcquire(StreamBudget.Priority.Bridge)
			local releaseRequest = budget:tryAcquire(StreamBudget.Priority.Request)
			expect(releaseRequest).to.be.ok()
			expect(budget:tryAcquire(StreamBudget.Priority.Request)).to.equal(nil)
			local releaseSocket = budget:tryAcquire(StreamBudget.Priority.Socket)
			expect(releaseSocket).to.be.ok()
			releaseBridge()
			releaseRequest()
			releaseSocket()
			expect(budget:liveCount()).to.equal(0)
		end)
	end)
end
