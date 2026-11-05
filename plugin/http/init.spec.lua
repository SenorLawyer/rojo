return function()
	local Http = require(script.Parent)
	local Promise = require(script.Parent.Parent.Promise)

	it("should load", function()
		require(script.Parent)
	end)

	it("round trips empty Unicode and NUL-containing MessagePack strings through base64", function()
		local encoding = game:GetService("EncodingService")
		for _, value in { "", "雪🥚", "before\0after", string.rep("x\0", 32768) } do
			local wire = encoding:Base64Encode(buffer.fromstring(Http.msgpackEncode({ value = value })))
			local decoded = buffer.tostring(encoding:Base64Decode(wire))
			expect(Http.msgpackDecode(decoded).value).to.equal(value)
		end
	end)

	it("preserves non-finite MessagePack numbers instead of silently converting them to null", function()
		local result =
			Http.msgpackDecode(Http.msgpackEncode({ nan = 0 / 0, positive = math.huge, negative = -math.huge }))
		expect(result.nan ~= result.nan).to.equal(true)
		expect(result.positive).to.equal(math.huge)
		expect(result.negative).to.equal(-math.huge)
	end)

	it("rejects truncated MessagePack without poisoning the next decode", function()
		expect(pcall(Http.msgpackDecode, string.char(0xDA, 0x01))).to.equal(false)
		expect(Http.msgpackDecode(Http.msgpackEncode("healthy"))).to.equal("healthy")
	end)

	describe("loopback transports", function()
		local StreamRequest = require(script.Parent.StreamRequest)
		local HttpError = require(script.Parent.Error)
		local encoding = game:GetService("EncodingService")
		local originalStream = Http._transport.stream
		local originalRequestAsync = Http._transport.requestAsync
		local clients, streamConfigs, plainRequests

		local function base64(value)
			return buffer.tostring(encoding:Base64Encode(buffer.fromstring(value)))
		end

		beforeEach(function()
			clients = {}
			streamConfigs = {}
			plainRequests = {}
			local stream = StreamRequest.new(function(options)
				local client = { options = options, events = {}, closed = false }
				for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
					client.events[name] = Instance.new("BindableEvent")
					client[name] = client.events[name].Event
				end
				function client:Close()
					self.closed = true
				end
				table.insert(clients, client)
				return client
			end)
			Http._transport.stream = function(options, config)
				table.insert(streamConfigs, config)
				return stream(options, config)
			end
			Http._transport.requestAsync = function(params)
				table.insert(plainRequests, params)
				return { StatusCode = 200, Success = true, Headers = {}, Body = "plain\0body" }
			end
		end)

		afterEach(function()
			Http._transport.stream = originalStream
			Http._transport.requestAsync = originalRequestAsync
			for _, client in clients do
				for _, event in client.events do
					event:Destroy()
				end
			end
		end)

		it("decodes responses from servers with encoded streams", function()
			local url = "http://localhost:47001/api/rojo"
			Http._forgetPlainOrigin(url)
			local pending = Http.post(url, "a\0b")
			local client = clients[1]
			expect(client.options.Headers["x-rojo-content-transfer"]).to.equal("base64")
			expect(client.options.Body).to.equal(base64("a\0b"))
			client.events.Opened:Fire(200, "X-Rojo-Content-Transfer: base64\r\n")
			client.events.MessageReceived:Fire(base64("binary\0reply"))
			client.events.Closed:Fire()
			expect(pending:expect().body).to.equal("binary\0reply")
			expect(#plainRequests).to.equal(0)
			expect(Http.usesEncodedStreams(url)).to.equal(true)
		end)

		it("falls back to RequestAsync for servers without encoded streams and remembers it per origin", function()
			local url = "http://127.0.0.1:47002/api/rojo"
			local encodedUrl = "http://127.0.0.1:47003/api/rojo"
			Http._forgetPlainOrigin(url)
			Http._forgetPlainOrigin(encodedUrl)
			local pending = Http.post(url, "raw\0request")
			-- RawStream truncates at NUL, so the stream's body must not be used.
			clients[1].events.Opened:Fire(200, "Content-Type: application/msgpack\r\n")
			clients[1].events.MessageReceived:Fire("plain")
			clients[1].events.Closed:Fire()
			expect(pending:expect().body).to.equal("plain\0body")
			expect(#plainRequests).to.equal(1)
			expect(plainRequests[1].Url).to.equal(url)
			expect(plainRequests[1].Body).to.equal("raw\0request")
			expect(plainRequests[1].Headers).to.equal(nil)
			expect(clients[1].closed).to.equal(true)

			-- Later requests to the same origin skip the stream entirely.
			expect(Http.usesEncodedStreams(url)).to.equal(false)
			expect(Http.get("http://127.0.0.1:47002/api/read/1"):expect().body).to.equal("plain\0body")
			expect(#clients).to.equal(1)
			expect(#plainRequests).to.equal(2)

			-- Another port may be a different server.
			expect(Http.usesEncodedStreams(encodedUrl)).to.equal(true)
			local other = Http.get(encodedUrl)
			task.wait()
			expect(#clients).to.equal(2)
			other:cancel()
		end)

		it("falls back for error responses from servers without encoded streams", function()
			local url = "http://127.0.0.1:47004/api/rojo"
			Http._forgetPlainOrigin(url)
			Http._transport.requestAsync = function(params)
				table.insert(plainRequests, params)
				return { StatusCode = 404, Success = false, Headers = {}, Body = "Not found" }
			end
			local pending = Http.get(url)
			clients[1].events.Error:Fire(404, "Not Found")
			-- Studio reported the status without headers, so this cannot tell
			-- the server apart yet; a response with headers can.
			local ok, err = pending:await()
			expect(ok).to.equal(false)
			expect(#plainRequests).to.equal(0)
			expect(Http.usesEncodedStreams(url)).to.equal(true)

			local second = Http.get(url)
			task.wait()
			clients[2].events.Opened:Fire(404, "Content-Type: text/plain\r\n")
			clients[2].events.MessageReceived:Fire("Not found")
			clients[2].events.Closed:Fire()
			ok, err = second:await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Not found", 1, true) ~= nil).to.equal(true)
			expect(#plainRequests).to.equal(1)
			expect(Http.usesEncodedStreams(url)).to.equal(false)
		end)

		it("decodes error bodies reported without headers, keeping text that is not base64", function()
			local url = "http://localhost:47006/api/rojo"
			Http._forgetPlainOrigin(url)
			local pending = Http.get(url)
			clients[1].events.MessageReceived:Fire(base64("identity changed"))
			clients[1].events.Error:Fire(409, "Conflict")
			local _, err = pending:await()
			expect(string.find(tostring(err), "409: identity changed", 1, true) ~= nil).to.equal(true)

			local second = Http.get(url)
			task.wait()
			clients[2].events.MessageReceived:Fire("Service is busy")
			clients[2].events.Error:Fire(503, "Service Unavailable")
			_, err = second:await()
			expect(string.find(tostring(err), "503: Service is busy", 1, true) ~= nil).to.equal(true)

			local third = Http.get(url)
			task.wait()
			clients[3].events.Error:Fire(503, "Service Unavailable")
			_, err = third:await()
			expect(string.find(tostring(err), "503: Service Unavailable", 1, true) ~= nil).to.equal(true)
		end)

		it("reports undecodable encoded responses as HTTP errors", function()
			local url = "http://localhost:47007/api/rojo"
			Http._forgetPlainOrigin(url)
			local pending = Http.get(url)
			clients[1].events.Opened:Fire(200, "X-Rojo-Content-Transfer: base64\r\n")
			clients[1].events.MessageReceived:Fire("not base64!")
			clients[1].events.Closed:Fire()
			local ok, err = pending:await()
			expect(ok).to.equal(false)
			expect(getmetatable(err)).to.equal(HttpError)
			expect(string.find(tostring(err), "Could not decode", 1, true) ~= nil).to.equal(true)
		end)

		it("lets the caller's timeout bound local streams and bounds the queue wait separately", function()
			local url = "http://localhost:47008/api/read/1"
			Http._forgetPlainOrigin(url)
			for _, case in
				{
					{ options = { timeout = 300 }, timeout = 300, queueTimeout = Http.queueWaitLimit },
					{
						options = { timeout = 5 },
						timeout = StreamRequest.DEFAULT_TIMEOUT,
						queueTimeout = Http.queueWaitLimit,
					},
					{ options = nil, timeout = nil, queueTimeout = nil },
				}
			do
				local pending = Http.get(url, case.options)
				local config = streamConfigs[#streamConfigs]
				expect(config.timeout).to.equal(case.timeout)
				expect(config.queueTimeout).to.equal(case.queueTimeout)
				pending:cancel()
				task.wait()
			end
		end)

		it("reports when a queued stream request is actually sent", function()
			local url = "http://localhost:47010/api/write"
			Http._forgetPlainOrigin(url)
			local busy = Http.get(url)
			local started = false
			local queued = Http.post(url, "change", {
				timeout = 60,
				onStarted = function()
					started = true
				end,
			})
			task.wait()
			expect(started).to.equal(false)
			busy:cancel()
			task.wait()
			expect(started).to.equal(true)
			queued:cancel()
		end)

		it("does not count a created stream client as a permission answer", function()
			-- Whether creating the client waits for Studio's prompt is not
			-- documented, so only the server's response shows the prompt closed.
			local url = "http://[::1]:47009/api/rojo"
			Http._forgetPlainOrigin(url)
			Http._forgetPermissionAnswer(url)
			local release = Instance.new("BindableEvent")
			local created = {}
			Http._transport.stream = StreamRequest.new(function()
				release.Event:Wait()
				local client = { events = {} }
				for _, name in { "Opened", "MessageReceived", "Error", "Closed" } do
					client.events[name] = Instance.new("BindableEvent")
					client[name] = client.events[name].Event
				end
				function client:Close() end
				table.insert(created, client)
				return client
			end)
			local cancelled = Http.get(url)
			-- The caller gives up while Studio may still show the prompt.
			cancelled:cancel()
			expect(Http.hasOutstandingRequest(url)).to.equal(true)
			release:Fire()
			task.wait()
			expect(Http.hasOutstandingRequest(url)).to.equal(false)
			expect(Http.isPermissionPending(url)).to.equal(true)

			local answered = Http.get(url)
			answered:catch(function() end)
			task.wait()
			release:Fire()
			task.wait()
			expect(Http.isPermissionPending(url)).to.equal(true)
			created[#created].events.Opened:Fire(200, "")
			task.wait()
			expect(Http.isPermissionPending(url)).to.equal(false)
			answered:cancel()
			release:Destroy()
			for _, client in created do
				for _, event in client.events do
					event:Destroy()
				end
			end
		end)

		it("records only clear permission failures from stream client creation", function()
			local url = "http://[::1]:47011/api/rojo"
			for _, case in
				{
					{ message = "ConnectFail", pending = true, denied = false },
					{ message = "Something unexpected", pending = true, denied = false },
					{ message = "HttpService permission denied", pending = false, denied = true },
					{
						message = "Http requests are not enabled. Enable via game settings",
						pending = false,
						denied = false,
					},
				}
			do
				Http._forgetPlainOrigin(url)
				Http._forgetPermissionAnswer(url)
				Http._transport.stream = StreamRequest.new(function()
					error(case.message)
				end)
				Http.get(url):await()
				task.wait()
				expect(Http.isPermissionPending(url)).to.equal(case.pending)
				expect(Http.isPermissionDenied(url)).to.equal(case.denied)
			end
		end)

		it("uses RequestAsync for loopback requests that ask for it or when EncodingService is missing", function()
			local url = "http://localhost:47012/api/rojo"
			Http._forgetPlainOrigin(url)
			expect(Http.get(url, { requestAsync = true }):expect().body).to.equal("plain\0body")
			expect(#clients).to.equal(0)
			Http._transport.encodingAvailable = false
			local ok, response = pcall(function()
				return Http.get(url):expect()
			end)
			Http._transport.encodingAvailable = true
			expect(ok).to.equal(true)
			expect(response.body).to.equal("plain\0body")
			expect(#clients).to.equal(0)
			expect(#plainRequests).to.equal(2)
		end)

		it("sizes the encoded response limit so base64 does not lower the decoded limit", function()
			local url = "http://localhost:47014/api/read/1"
			Http._forgetPlainOrigin(url)
			local pending = Http.get(url)
			local config = streamConfigs[#streamConfigs]
			expect(config.maxBytes >= math.ceil(Http.maxDecodedResponseBytes * 4 / 3)).to.equal(true)
			-- Only a little slack on top of the exact base64 size.
			expect(config.maxBytes <= math.ceil(Http.maxDecodedResponseBytes * 4 / 3) + 4096).to.equal(true)
			pending:cancel()
		end)

		it("resends a GET that is too large for a stream with RequestAsync, but not a POST", function()
			local url = "http://localhost:47015/api/read/1"
			Http._forgetPlainOrigin(url)
			local previousLimit = Http.maxDecodedResponseBytes
			Http.maxDecodedResponseBytes = 3
			local read = Http.get(url)
			local write = Http.post("http://localhost:47015/api/write", "change")
			Http.maxDecodedResponseBytes = previousLimit
			clients[1].events.Opened:Fire(200, "X-Rojo-Content-Transfer: base64\r\n")
			clients[1].events.MessageReceived:Fire(string.rep("A", 4096))
			expect(read:expect().body).to.equal("plain\0body")
			expect(clients[1].closed).to.equal(true)
			expect(#plainRequests).to.equal(1)
			expect(plainRequests[1].Url).to.equal(url)
			expect(plainRequests[1].Headers).to.equal(nil)
			-- The origin does support encoded streams.
			expect(Http.usesEncodedStreams(url)).to.equal(true)

			task.wait()
			clients[2].events.Opened:Fire(200, "X-Rojo-Content-Transfer: base64\r\n")
			clients[2].events.MessageReceived:Fire(string.rep("A", 4096))
			local ok, err = write:await()
			expect(ok).to.equal(false)
			expect(err.responseTooLarge).to.equal(true)
			-- A write may already have been applied, so it is not sent again.
			expect(#plainRequests).to.equal(1)
		end)

		it("counts a stream request as outstanding only once it leaves the queue", function()
			-- Every loopback host shares one transport, so a request to one host
			-- can queue behind a request to another.
			local busyUrl = "http://127.0.0.1:47016/api/read/1"
			local queuedUrl = "http://localhost:47016/api/write"
			Http._forgetPlainOrigin(busyUrl)
			Http._forgetPlainOrigin(queuedUrl)
			local busy = Http.get(busyUrl)
			local queued = Http.post(queuedUrl, "change", { timeout = 60 })
			expect(Http.hasOutstandingRequest(busyUrl)).to.equal(true)
			-- A queued request has not touched Studio yet, so it cannot be
			-- blocked on the permission prompt.
			expect(Http.hasOutstandingRequest(queuedUrl)).to.equal(false)
			busy:cancel()
			task.wait()
			expect(Http.hasOutstandingRequest(busyUrl)).to.equal(false)
			expect(Http.hasOutstandingRequest(queuedUrl)).to.equal(true)
			queued:cancel()
			task.wait()
			expect(Http.hasOutstandingRequest(queuedUrl)).to.equal(false)
		end)

		it("sends a request with RequestAsync when no stream client is free", function()
			local url = "http://localhost:47013/api/write"
			Http._forgetPlainOrigin(url)
			local budget = Http.StreamBudget.new()
			local releaseSocket = budget:tryAcquire(Http.StreamBudget.Priority.Socket)
			local releaseOther = budget:tryAcquire(Http.StreamBudget.Priority.Request)
			local created = 0
			Http._transport.stream = StreamRequest.new(function()
				created += 1
				error("no client should be created")
			end, budget)
			local started = 0
			local response = Http.post(url, "binary\0write", {
				timeout = 5,
				onStarted = function()
					started += 1
				end,
			}):expect()
			expect(created).to.equal(0)
			expect(response.body).to.equal("plain\0body")
			expect(#plainRequests).to.equal(1)
			-- The original request is sent, not the stream's encoded one.
			expect(plainRequests[1].Body).to.equal("binary\0write")
			expect(plainRequests[1].Headers).to.equal(nil)
			expect(started).to.equal(1)
			-- The server was never asked, so it is not treated as a plain server.
			expect(Http.usesEncodedStreams(url)).to.equal(true)
			releaseOther()
			releaseSocket()
		end)
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

		it("starts the deadline when the request is sent, not while it is queued", function()
			local url = "http://queued-start.test:34872/api/rojo"
			Http._recordPermissionAnswer(url, false)
			local markStarted
			local started = Promise.new(function(resolve)
				markStarted = resolve
			end)
			local request =
				Http.timeout(Promise.new(function() end), url, 0.05, "timed out", nil, { started = started })
			request:catch(function() end)
			task.wait(0.15)
			expect(request:getStatus()).to.equal(Promise.Status.Started)
			markStarted()
			task.wait(0.02)
			expect(request:getStatus()).to.equal(Promise.Status.Started)
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

		it("tracks outstanding requests per host until each finishes", function()
			local url = "http://outstanding-host.test:34872/api/rojo"
			expect(Http.hasOutstandingRequest(url)).to.equal(false)
			local finishFirst = Http._beginOutstandingRequest(url)
			local finishSecond = Http._beginOutstandingRequest("http://OUTSTANDING-host.test:1/other")
			expect(Http.hasOutstandingRequest(url)).to.equal(true)
			expect(Http.hasOutstandingRequest("http://other-outstanding-host.test/")).to.equal(false)

			finishFirst()
			-- Finishing the same request twice must not hide the other one.
			finishFirst()
			expect(Http.hasOutstandingRequest(url)).to.equal(true)
			finishSecond()
			expect(Http.hasOutstandingRequest(url)).to.equal(false)
		end)
	end)
	describe("readable error responses", function()
		local HttpResponse = require(script.Parent.Response)

		local function response(body, contentType)
			return HttpResponse.fromRobloxResponse({
				Success = false,
				StatusCode = 409,
				StatusMessage = "Conflict",
				Headers = { ["content-type"] = contentType },
				Body = body,
			})
		end

		it("uses the details of a MessagePack error body", function()
			local body = Http.msgpackEncode({ kind = "BadRequest", details = "Project Git checkout changed." })
			local readable = Http.readableErrorResponse(response(body, "application/msgpack"))
			expect(readable.body).to.equal("Project Git checkout changed.")
			expect(readable.code).to.equal(409)
			expect(readable:isSuccess()).to.equal(false)
		end)

		it("recognizes a MessagePack error body labelled as text", function()
			local body = Http.msgpackEncode({ kind = "BadRequest", details = "Wrong session ID" })
			expect(Http.readableErrorResponse(response(body, "text/plain")).body).to.equal("Wrong session ID")
		end)

		it("leaves other bodies alone", function()
			local plain = response("Not found", "text/plain")
			expect(Http.readableErrorResponse(plain)).to.equal(plain)
			local other = Http.msgpackEncode({ message = "no details" })
			local unrelated = response(other, "application/msgpack")
			expect(Http.readableErrorResponse(unrelated)).to.equal(unrelated)
		end)
	end)
end
