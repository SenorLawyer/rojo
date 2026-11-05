return function()
	local Packages = script.Parent.Parent.Packages
	local Promise = require(Packages.Promise)
	local Http = require(Packages.Http)
	local ApiContext = require(script.Parent.ApiContext)

	local function signal()
		local listeners = {}
		return {
			Connect = function(_, callback)
				local connection = { Connected = true }
				function connection:Disconnect()
					self.Connected = false
				end
				table.insert(listeners, { callback = callback, connection = connection })
				return connection
			end,
			Fire = function(_, ...)
				for _, listener in listeners do
					if listener.connection.Connected then
						listener.callback(...)
					end
				end
			end,
			isConnected = function()
				for _, listener in listeners do
					if listener.connection.Connected then
						return true
					end
				end
				return false
			end,
		}
	end

	local function socketContext()
		local socket = {
			Opened = signal(),
			ConnectionState = Enum.WebStreamClientState.Open,
			MessageReceived = signal(),
			Closed = signal(),
			Error = signal(),
			closeCount = 0,
			Close = function(self)
				self.closeCount += 1
			end,
		}
		local context = ApiContext.new("http://localhost:34872")
		context.__sessionId = "test-session"
		-- Specs leave sockets open, which must not use up Studio's budget.
		context.__streamBudget = Http.StreamBudget.new()
		context.__createWebSocket = function()
			return socket
		end
		return context, socket
	end

	local function packet(cursor, sessionId)
		return Http.msgpackEncode({
			sessionId = sessionId or "test-session",
			packetType = "messages",
			body = { messages = {}, messageCursor = cursor },
		})
	end

	describe("request lifecycle", function()
		local originalGet = Http.get
		afterEach(function()
			Http.get = originalGet
		end)

		it("cancels pending HTTP requests and their consumers when disconnected", function()
			local pending = Promise.new(function() end)
			Http.get = function()
				return pending
			end
			local context = ApiContext.new("http://localhost:34872")
			local request = context:connect()
			context:disconnect()
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(request:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(next(context.__activeRequests)).to.equal(nil)
		end)

		it("removes settled requests from the active set", function()
			Http.get = function()
				return Promise.resolve("response")
			end
			local context = ApiContext.new("http://localhost:34872")
			local ok, response = context:__request("get", "unused"):await()
			expect(ok).to.equal(true)
			expect(response).to.equal("response")
			expect(next(context.__activeRequests)).to.equal(nil)
		end)

		it("does not issue HTTP requests after disconnecting", function()
			local calls = 0
			Http.get = function()
				calls += 1
				return Promise.resolve()
			end
			local context = ApiContext.new("http://localhost:34872")
			context:disconnect()
			local ok = context:connect():await()
			expect(ok).to.equal(false)
			expect(calls).to.equal(0)
		end)

		it("reports malformed server info without indexing a primitive", function()
			Http.get = function()
				return Promise.resolve({ code = 200, body = Http.msgpackEncode(7) })
			end
			local context = ApiContext.new("http://localhost:34872")
			local ok, err = context:connect():await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Invalid response from the Rojo server", 1, true) ~= nil).to.equal(true)
		end)

		it("bounds a stalled server handshake and cancels its request", function()
			local pending = Promise.new(function() end)
			Http.get = function()
				return pending
			end
			-- Permission answers are per host and last for the session, so each
			-- test that depends on them uses its own host.
			local baseUrl = "http://handshake-answered.test:34872"
			Http._recordPermissionAnswer(baseUrl, false)
			local context = ApiContext.new(baseUrl)
			context.__handshakeTimeout = 0.01
			local ok, err = context:connect():await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Timed out connecting", 1, true) ~= nil).to.equal(true)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(next(context.__activeRequests)).to.equal(nil)
		end)

		it("waits on Studio's permission prompt instead of timing out the handshake", function()
			local pending = Promise.new(function() end)
			Http.get = function()
				return pending
			end
			local context = ApiContext.new("http://handshake-prompt.test:34872")
			context.__handshakeTimeout = 0.01
			local waitingCalls = 0
			local request = context:connect(function()
				waitingCalls += 1
			end)
			task.wait(0.1)
			expect(request:getStatus()).to.equal(Promise.Status.Started)
			expect(waitingCalls).to.equal(1)

			-- Cancel from the connecting page must still stop the request.
			context:disconnect()
			expect(request:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("bounds a stalled initial read", function()
			local pending = Promise.new(function() end)
			Http.get = function()
				return pending
			end
			local baseUrl = "http://read-answered.test:34872"
			Http._recordPermissionAnswer(baseUrl, false)
			local context = ApiContext.new(baseUrl)
			context.__initialReadTimeout = 0.01
			local ok, err = context:read({ "ROOT" }):await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Timed out loading", 1, true) ~= nil).to.equal(true)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("gives the initial read longer than fallback requests", function()
			local context = ApiContext.new("http://localhost:34872")
			expect(context.__initialReadTimeout).to.equal(300)
			expect(context.__transferTimeout).to.equal(60)
		end)

		it("gives up on a possible permission prompt that never gets an answer", function()
			local pending = Promise.new(function() end)
			Http.get = function()
				return pending
			end
			local originalLimit = Http.permissionWaitLimit
			Http.permissionWaitLimit = 0.05
			local context = ApiContext.new("http://handshake-prompt-cap.test:34872")
			context.__handshakeTimeout = 0.01
			local waitingCalls = 0
			local ok, err = context
				:connect(function()
					waitingCalls += 1
				end)
				:await()
			Http.permissionWaitLimit = originalLimit
			expect(ok).to.equal(false)
			expect(waitingCalls).to.equal(1)
			-- The host may have no prompt at all, so the message covers both causes.
			expect(string.find(tostring(err), "choose Allow", 1, true) ~= nil).to.equal(true)
			expect(string.find(tostring(err), "rojo serve", 1, true) ~= nil).to.equal(true)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(next(context.__activeRequests)).to.equal(nil)
		end)

		it("bounds a background probe even while permission may be pending", function()
			local pending = Promise.new(function() end)
			Http.get = function()
				return pending
			end
			local context = ApiContext.new("http://probe-cap.test:34872")
			expect(context.__probeTimeout).to.equal(30)
			context.__probeTimeout = 0.05
			local ok, err = context:probe():await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Timed out connecting", 1, true) ~= nil).to.equal(true)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(next(context.__activeRequests)).to.equal(nil)
		end)

		it("starts a stream request's timeout once it is sent, not while it waits for the slot", function()
			local pending = Promise.new(function() end)
			local seenOptions
			Http.get = function(_, options)
				seenOptions = options
				return pending
			end
			local baseUrl = "http://127.0.0.1:34991"
			Http._forgetPlainOrigin(baseUrl)
			Http._recordPermissionAnswer(baseUrl, false)
			local context = ApiContext.new(baseUrl)
			context.__handshakeTimeout = 0.05
			local request = context:connect()
			request:catch(function() end)
			-- Still queued behind other sync requests.
			task.wait(0.15)
			expect(request:getStatus()).to.equal(Promise.Status.Started)
			seenOptions.onStarted()
			task.wait(0.15)
			expect(request:getStatus()).to.equal(Promise.Status.Rejected)
			expect(pending:getStatus()).to.equal(Promise.Status.Cancelled)
		end)

		it("sends background probes with RequestAsync instead of the sync stream", function()
			local requestAsync = {}
			Http.get = function(_, options)
				table.insert(requestAsync, options.requestAsync == true)
				return Promise.new(function() end)
			end
			local context = ApiContext.new("http://localhost:34992")
			context:probe():catch(function() end)
			context:connect():catch(function() end)
			context:disconnect()
			expect(requestAsync[1]).to.equal(true)
			expect(requestAsync[2]).to.equal(false)
		end)

		it("passes each request's own timeout to the HTTP transport", function()
			local originalPost = Http.post
			local seen = {}
			Http.get = function(url, options)
				seen[if string.find(url, "/api/read/", 1, true) then "read" else "handshake"] = options.timeout
				return Promise.new(function() end)
			end
			Http.post = function(_, _, options)
				seen.write = options.timeout
				return Promise.new(function() end)
			end
			local context = ApiContext.new("http://transport-timeout.test:34872")
			local requests =
				{ context:connect(), context:read({ "A" }), context:write({ removed = {}, updated = {}, added = {} }) }
			Http.post = originalPost
			context:disconnect()
			for _, request in requests do
				request:catch(function() end)
			end
			expect(seen.handshake).to.equal(context.__handshakeTimeout)
			expect(seen.read).to.equal(context.__initialReadTimeout)
			expect(seen.write).to.equal(context.__transferTimeout)
		end)

		it("bounds stalled fallback serialization requests", function()
			local originalPost = Http.post
			local pending = {}
			Http.post = function()
				local request = Promise.new(function() end)
				table.insert(pending, request)
				return request
			end
			local baseUrl = "http://serialize-answered.test:34872"
			Http._recordPermissionAnswer(baseUrl, false)
			local context = ApiContext.new(baseUrl)
			context.__transferTimeout = 0.01
			local serializeOk, serializeErr = context:serialize({ "A" }):await()
			local refOk, refErr = context:refPatch({ "A" }):await()
			Http.post = originalPost
			expect(serializeOk).to.equal(false)
			expect(string.find(tostring(serializeErr), "Timed out serializing", 1, true) ~= nil).to.equal(true)
			expect(refOk).to.equal(false)
			expect(string.find(tostring(refErr), "Timed out fetching references", 1, true) ~= nil).to.equal(true)
			for _, request in pending do
				expect(request:getStatus()).to.equal(Promise.Status.Cancelled)
			end
		end)

		it("bounds stalled write and open requests", function()
			local originalPost = Http.post
			local pending = {}
			Http.post = function()
				local request = Promise.new(function() end)
				table.insert(pending, request)
				return request
			end
			local baseUrl = "http://write-open-answered.test:34872"
			Http._recordPermissionAnswer(baseUrl, false)
			local context = ApiContext.new(baseUrl)
			context.__transferTimeout = 0.01
			context.__handshakeTimeout = 0.01
			local writeOk, writeErr = context:write({ removed = {}, updated = {}, added = {} }):await()
			local openOk, openErr = context:open("A"):await()
			Http.post = originalPost
			expect(writeOk).to.equal(false)
			expect(string.find(tostring(writeErr), "Timed out sending changes", 1, true) ~= nil).to.equal(true)
			expect(openOk).to.equal(false)
			expect(string.find(tostring(openErr), "Timed out asking the Rojo server to open", 1, true) ~= nil).to.equal(
				true
			)
			for _, request in pending do
				expect(request:getStatus()).to.equal(Promise.Status.Cancelled)
			end
			expect(next(context.__activeRequests)).to.equal(nil)
		end)
	end)

	describe("WebSocket lifecycle", function()
		it("bounds a socket that never opens", function()
			local context, socket = socketContext()
			context.__handshakeTimeout = 0.01
			socket.ConnectionState = Enum.WebStreamClientState.Connecting
			local ok, err = context:connectWebSocket({}):await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Timed out opening", 1, true) ~= nil).to.equal(true)
			expect(socket.closeCount).to.equal(1)
			expect(socket.Opened.isConnected()).to.equal(false)
		end)

		it("tells the server which session the socket belongs to", function()
			local context, socket = socketContext()
			local socketUrl
			context.__createWebSocket = function(_, url)
				socketUrl = url
				return socket
			end
			context:connectWebSocket({})
			expect(socketUrl).to.equal("ws://localhost:34872/api/socket/-1?sessionId=test-session")
			context:disconnect()
		end)

		it("notifies the session only after the socket opens", function()
			local context, socket = socketContext()
			socket.ConnectionState = Enum.WebStreamClientState.Connecting
			local calls = 0
			context:connectWebSocket({}, function()
				calls += 1
			end)
			expect(calls).to.equal(0)
			socket.Opened:Fire()
			expect(calls).to.equal(1)
			context:disconnect()
		end)
		it("includes server closure details and asks for a resync", function()
			local context, socket = socketContext()
			local request = context:connectWebSocket({})
			socket.Closed:Fire(1012, "Subscription history is stale")
			local ok, err = request:await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Reconnect to resync", 1, true) ~= nil).to.equal(true)
			expect(string.find(tostring(err), "Subscription history is stale", 1, true) ~= nil).to.equal(true)
		end)
		it("closes the socket and disconnects listeners on explicit disconnect", function()
			local context, socket = socketContext()
			local request = context:connectWebSocket({})
			context:disconnect()
			expect(request:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(socket.closeCount).to.equal(1)
			expect(socket.MessageReceived.isConnected()).to.equal(false)
			expect(socket.Closed.isConnected()).to.equal(false)
			expect(socket.Error.isConnected()).to.equal(false)
			expect(next(context.__activeRequests)).to.equal(nil)
		end)

		it("fails and cleans up when a patch handler errors", function()
			local context, socket = socketContext()
			local request = context:connectWebSocket({
				messages = function()
					error("patch failed")
				end,
			})
			socket.MessageReceived:Fire(packet(1))
			local ok, err = request:await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "patch failed", 1, true) ~= nil).to.equal(true)
			expect(socket.closeCount).to.equal(1)
			expect(socket.MessageReceived.isConnected()).to.equal(false)
		end)

		it("fails when the server session changes instead of silently ignoring updates", function()
			local context, socket = socketContext()
			local calls = 0
			local request = context:connectWebSocket({
				messages = function()
					calls += 1
				end,
			})
			socket.MessageReceived:Fire(packet(1, "restarted-session"))
			local ok, err = request:await()
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "Server changed ID", 1, true) ~= nil).to.equal(true)
			expect(calls).to.equal(0)
		end)

		it("settles the connection even if closing the socket throws", function()
			local context, socket = socketContext()
			socket.Close = function()
				error("socket already closed")
			end
			local request = context:connectWebSocket({})
			socket.Error:Fire(500, "boom")
			local ok = request:await()
			expect(ok).to.equal(false)
			expect(socket.Error.isConnected()).to.equal(false)
		end)

		it("fails malformed packets through the connection promise", function()
			local context, socket = socketContext()
			local request = context:connectWebSocket({})
			socket.MessageReceived:Fire("invalid messagepack")
			local ok = request:await()
			expect(ok).to.equal(false)
			expect(socket.closeCount).to.equal(1)
		end)

		it("preserves message order when the first handler yields", function()
			local context, socket = socketContext()
			local gate = Instance.new("BindableEvent")
			local applied = {}
			context:connectWebSocket({
				messages = function(body)
					if body.messageCursor == 1 then
						gate.Event:Wait()
					end
					table.insert(applied, body.messageCursor)
				end,
			})
			task.spawn(function()
				socket.MessageReceived:Fire(packet(1))
			end)
			socket.MessageReceived:Fire(packet(2))
			expect(#applied).to.equal(0)
			gate:Fire()
			task.wait()
			expect(applied[1]).to.equal(1)
			expect(applied[2]).to.equal(2)
			context:disconnect()
			gate:Destroy()
		end)

		describe("stream budget", function()
			local function slots(context)
				return context.__streamBudget:liveCount()
			end

			it("gives the socket's slot back however the socket ends", function()
				local endings = {
					serverClose = function(_, socket)
						-- The server closes the socket with 1008 when the checkout changes.
						socket.Closed:Fire(1008, "Checkout changed")
					end,
					error = function(_, socket)
						socket.Error:Fire(500, "boom")
					end,
					badPacket = function(_, socket)
						socket.MessageReceived:Fire("invalid messagepack")
					end,
					disconnect = function(context)
						context:disconnect()
					end,
				}
				for name, finish in endings do
					local context, socket = socketContext()
					local request = context:connectWebSocket({})
					request:catch(function() end)
					expect(slots(context)).to.equal(1)
					finish(context, socket)
					expect(socket.closeCount).to.equal(1)
					expect(slots(context)).to.equal(0)
					-- Later events from a closed socket release nothing more.
					socket.Closed:Fire(1000, name)
					expect(slots(context)).to.equal(0)
				end

				local context, socket = socketContext()
				context.__handshakeTimeout = 0.01
				socket.ConnectionState = Enum.WebStreamClientState.Connecting
				expect(context:connectWebSocket({}):await()).to.equal(false)
				expect(slots(context)).to.equal(0)
			end)

			it("gives the slot back when creating the socket fails", function()
				local context = socketContext()
				context.__createWebSocket = function()
					error("could not connect")
				end
				local ok, err = context:connectWebSocket({}):await()
				expect(ok).to.equal(false)
				expect(string.find(tostring(err), "could not connect", 1, true) ~= nil).to.equal(true)
				expect(slots(context)).to.equal(0)
			end)

			it("closes a socket that was created after its request was cancelled", function()
				local context, socket = socketContext()
				local gate = Instance.new("BindableEvent")
				context.__createWebSocket = function()
					gate.Event:Wait()
					return socket
				end
				local request = context:connectWebSocket({})
				context:disconnect()
				expect(request:getStatus()).to.equal(Promise.Status.Cancelled)
				-- The socket may still arrive, so its slot stays taken.
				expect(slots(context)).to.equal(1)
				gate:Fire()
				task.wait()
				expect(socket.closeCount).to.equal(1)
				expect(socket.Opened.isConnected()).to.equal(false)
				expect(context.__wsClient).to.equal(nil)
				expect(slots(context)).to.equal(0)
				gate:Destroy()
			end)

			it("opens the socket while a request holds the other slot", function()
				local context = socketContext()
				local releaseRequest = context.__streamBudget:tryAcquire(Http.StreamBudget.Priority.Request)
				expect(releaseRequest).to.be.ok()
				local request = context:connectWebSocket({})
				expect(request:getStatus()).to.equal(Promise.Status.Started)
				expect(slots(context)).to.equal(2)
				context:disconnect()
				releaseRequest()
				expect(slots(context)).to.equal(0)
			end)

			it("refuses a socket instead of creating one past the budget", function()
				local context = socketContext()
				local created = 0
				context.__createWebSocket = function()
					created += 1
					error("no socket should be created")
				end
				local releaseFirst = context.__streamBudget:tryAcquire(Http.StreamBudget.Priority.Socket)
				local releaseSecond = context.__streamBudget:tryAcquire(Http.StreamBudget.Priority.Socket)
				local ok = context:connectWebSocket({}):await()
				expect(ok).to.equal(false)
				expect(created).to.equal(0)
				releaseFirst()
				releaseSecond()
			end)

			it("uses the budget shared by the whole plugin by default", function()
				local context = ApiContext.new("http://localhost:34872")
				expect(context.__streamBudget).to.equal(Http.StreamBudget.shared)
			end)
		end)
	end)

	describe("connection error messages", function()
		it("shows the servePlaceIds line that would allow this place", function()
			local message = ApiContext.unlistedPlaceMessage(123, { 111, 222 })
			expect(message:find("Your place ID is 123", 1, true) ~= nil).to.equal(true)
			expect(message:find('"servePlaceIds": [111, 222, 123]', 1, true) ~= nil).to.equal(true)
		end)

		it("explains that an unpublished place cannot be listed", function()
			local message = ApiContext.unlistedPlaceMessage(0, { 111 })
			expect(message:find("not published", 1, true) ~= nil).to.equal(true)
			expect(message:find('"servePlaceIds"', 1, true)).to.equal(nil)
		end)

		it("says which side to update on a protocol mismatch", function()
			local Config = require(script.Parent.Config)
			local newer = ApiContext.protocolFixHint(Config.protocolVersion + 1)
			local older = ApiContext.protocolFixHint(Config.protocolVersion - 1)
			expect(newer:find("rojo plugin install", 1, true) ~= nil).to.equal(true)
			expect(older:find("rokit.toml", 1, true) ~= nil).to.equal(true)
		end)
	end)
end
