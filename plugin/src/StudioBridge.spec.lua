return function()
	local StudioBridge = require(script.Parent.StudioBridge)
	local Promise = require(script.Parent.Parent.Packages.Promise)
	local SerializationService = game:GetService("SerializationService")
	local EncodingService = game:GetService("EncodingService")
	local HttpService = game:GetService("HttpService")
	local fixture
	local bridge
	local function command(action, args)
		return {
			action = action,
			args = args,
			expiresAt = DateTime.now().UnixTimestampMillis + 10_000,
			requestId = "test",
		}
	end
	beforeEach(function()
		fixture = Instance.new("Folder")
		fixture.Name = "__RojoBridgeSpec"
		fixture.Parent = game:GetService("ReplicatedStorage")
		bridge = StudioBridge.new(
			{ __baseUrl = "http://127.0.0.1:34872" },
			{ protocolVersion = 1, pluginToken = "test", identity = {} },
			function() end
		)
		bridge.request = function()
			return Promise.resolve({ active = true })
		end
	end)
	afterEach(function()
		if bridge.worker then
			bridge:stop()
		end
		fixture:Destroy()
	end)

	it("keeps polling and normal sync active after a yielded command's result expires", function()
		local gate = Instance.new("BindableEvent")
		gate.Parent = fixture
		local pollGate = Instance.new("BindableEvent")
		pollGate.Parent = fixture
		local yielded = false
		local syncConnected = true
		local statusResult
		local polls = 0
		bridge.request = StudioBridge.request
		bridge.onFailure = function()
			syncConnected = false
		end
		bridge.execute = function(self, request)
			if request.requestId == "cancelled" then
				yielded = true
				gate.Event:Wait()
				return { completed = true }
			end
			return StudioBridge.execute(self, request)
		end
		bridge.__requestAsync = function(_, options)
			local route = options.Url:match("/([^/]+)$")
			local value = HttpService:JSONDecode(options.Body)
			local code, response = 200, {}
			if route == "poll" then
				polls += 1
				if polls <= 2 then
					local request = command("status", {})
					request.requestId = if polls == 1 then "cancelled" else "status"
					response = { commands = { request } }
				else
					pollGate.Event:Wait()
					response = { commands = {} }
				end
			elseif route == "result" then
				if value.requestId == "cancelled" then
					code, response = 410, { error = "Request expired or was cancelled" }
				else
					statusResult = value.result
				end
			end
			return { Success = code == 200, StatusCode = code, Body = HttpService:JSONEncode(response) }
		end
		bridge:start()
		expect(yielded).to.equal(true)
		gate:Fire()
		task.wait()
		expect(statusResult.ok).to.equal(true)
		expect(statusResult.value.sessionId).to.equal(bridge.sessionId)
		expect(syncConnected).to.equal(true)
		expect(bridge.stopped).to.equal(false)
	end)

	it("still stops the bridge for authentication failures", function()
		local syncConnected = true
		bridge.request = StudioBridge.request
		bridge.onFailure = function()
			syncConnected = false
		end
		bridge.__requestAsync = function(_, options)
			local denied = options.Url:match("/poll$") ~= nil
			return {
				Success = not denied,
				StatusCode = if denied then 401 else 200,
				Body = if denied then '{"error":"Invalid Studio control token"}' else "{}",
			}
		end
		bridge:start()
		task.wait()
		expect(syncConnected).to.equal(false)
		expect(bridge.stopped).to.equal(true)
	end)

	it("does not discard other routes or server errors", function()
		bridge.request = StudioBridge.request
		for _, case in
			{
				{ route = "poll", code = 410, body = '{"error":"Session expired"}' },
				{ route = "result", code = 403, body = '{"error":"Wrong session"}' },
				{ route = "result", code = 500, body = '{"error":"Server failure"}' },
			}
		do
			bridge.__requestAsync = function()
				return { Success = false, StatusCode = case.code, Body = case.body }
			end
			local ok = bridge:request(case.route, {}):await()
			expect(ok).to.equal(false)
		end
	end)

	it("discards every expired result, reading the reason only from the server's body", function()
		bridge.request = StudioBridge.request
		for _, case in
			{
				{ body = '{"error":"Request expired"}', fromServer = true, reason = "Request expired" },
				-- Studio reported the status through Error without a body.
				{ body = "Gone", fromServer = false, reason = "Request expired or was cancelled" },
				{ body = '{"error":"spoofed"}', fromServer = false, reason = "Request expired or was cancelled" },
				{ body = "invalid JSON", fromServer = true, reason = "Request expired or was cancelled" },
				{ body = "{}", fromServer = true, reason = "Request expired or was cancelled" },
			}
		do
			bridge.__requestAsync = function()
				return { Success = false, StatusCode = 410, Body = case.body, BodyFromServer = case.fromServer }
			end
			local ok, result = bridge:request("result", {}):await()
			expect(ok).to.equal(true)
			expect(result.discarded).to.equal(true)
			expect(result.reason).to.equal(case.reason)
		end
	end)

	it("keeps evicting the oldest instance IDs past the tracking limit", function()
		local previous = StudioBridge.maxTrackedInstances
		StudioBridge.maxTrackedInstances = 3
		local ok, err = pcall(function()
			local children = {}
			for index = 1, 12 do
				local child = Instance.new("Folder")
				child.Name = "Tracked" .. index
				child.Parent = fixture
				children[index] = bridge:describe(child).id
			end
			local tracked = 0
			for _ in bridge.instances do
				tracked += 1
			end
			expect(tracked).to.equal(3)
			for index = 1, 9 do
				expect(bridge.instances[children[index]]).to.equal(nil)
			end
			for index = 10, 12 do
				expect(bridge.instances[children[index]]).to.be.ok()
			end
		end)
		StudioBridge.maxTrackedInstances = previous
		assert(ok, err)
	end)

	it("stays off with a clear reason when EncodingService is missing", function()
		local previous = StudioBridge.encodingAvailable
		StudioBridge.encodingAvailable = false
		local failure, intentional
		bridge.onFailure = function(message, quiet)
			failure, intentional = message, quiet
		end
		bridge:start()
		StudioBridge.encodingAvailable = previous
		expect(bridge.stopped).to.equal(true)
		expect(bridge.worker).to.equal(nil)
		expect(string.find(failure, "EncodingService", 1, true) ~= nil).to.equal(true)
		expect(intentional).never.to.equal(true)
	end)

	describe("stream transport", function()
		local Http = script.Parent.Parent.Packages.Http
		local StreamBudget = require(Http.StreamBudget)
		local StreamRequest = require(Http.StreamRequest)
		local previousStream

		-- A stand-in stream client. `respond` runs once it has been created.
		local function fakeClient(respond)
			local created = {}
			local function create(options)
				local opened = Instance.new("BindableEvent")
				local message = Instance.new("BindableEvent")
				local closed = Instance.new("BindableEvent")
				local errored = Instance.new("BindableEvent")
				local client = {
					options = options,
					closedByCaller = false,
					Opened = opened.Event,
					MessageReceived = message.Event,
					Closed = closed.Event,
					Error = errored.Event,
				}
				function client:Close()
					self.closedByCaller = true
				end
				table.insert(created, client)
				task.defer(function()
					respond(client, opened, message, closed, errored)
				end)
				return client
			end
			return create, created
		end

		-- Waits a bounded number of frames for `condition`, so a spec does
		-- not depend on how many frames the transport takes.
		local function waitFor(condition)
			for _ = 1, 100 do
				if condition() then
					return true
				end
				task.wait()
			end
			return condition()
		end

		beforeEach(function()
			previousStream = StudioBridge.bridgeStream
		end)

		afterEach(function()
			StudioBridge.bridgeStream = previousStream
		end)

		it("sends bridge requests on the bridge's own stream slot", function()
			local budget = StreamBudget.new()
			local liveDuringRequest
			local create, created = fakeClient(function(_, opened, message, closed)
				liveDuringRequest = budget:liveCount()
				opened:Fire(200, "content-type: application/json\r\n")
				message:Fire('{"commands":[]}')
				closed:Fire()
			end)
			StudioBridge.bridgeStream = StreamRequest.new(create, budget, StreamBudget.Priority.Bridge)
			-- Sync's socket and one request already hold their slots.
			local releaseSocket = budget:tryAcquire(StreamBudget.Priority.Socket)
			local releaseRequest = budget:tryAcquire(StreamBudget.Priority.Request)
			expect(releaseRequest).to.be.ok()
			local ok, response = pcall(bridge.__requestAsync, bridge, { Url = "poll", Method = "POST", Body = "{}" })
			expect(ok).to.equal(true)
			expect(response.StatusCode).to.equal(200)
			expect(response.Body).to.equal('{"commands":[]}')
			expect(created[1].options.Url).to.equal("poll")
			expect(liveDuringRequest).to.equal(3)
			expect(budget:liveCount()).to.equal(2)
			expect(next(bridge.transports)).to.equal(nil)
			releaseSocket()
			releaseRequest()
		end)

		it("closes the stream when the bridge gives up on a request", function()
			local budget = StreamBudget.new()
			local create, created = fakeClient(function() end)
			StudioBridge.bridgeStream = StreamRequest.new(create, budget, StreamBudget.Priority.Bridge)
			local finished = false
			task.spawn(function()
				pcall(bridge.__requestAsync, bridge, { Url = "poll", Method = "POST", Body = "{}" })
				finished = true
			end)
			expect(waitFor(function()
				return #created == 1
			end)).to.equal(true)
			for transport in table.clone(bridge.transports) do
				transport:cancel()
			end
			expect(waitFor(function()
				return finished and created[1].closedByCaller and budget:liveCount() == 0
			end)).to.equal(true)
			expect(#created).to.equal(1)
		end)

		it("reads a status that the stream reports through Error", function()
			local previousDelay = StudioBridge.retryDelay
			StudioBridge.retryDelay = 0
			bridge.request = StudioBridge.request
			local function respondWith(...)
				local responses = { ... }
				local sent = 0
				local create, created = fakeClient(function(_, opened, message, closed, errored)
					sent += 1
					local response = responses[sent]
					if response.code == 200 then
						opened:Fire(200, "content-type: application/json\r\n")
						message:Fire("{}")
						closed:Fire()
						return
					end
					if response.body then
						message:Fire(response.body)
					end
					errored:Fire(response.code, "HTTP " .. response.code)
				end)
				StudioBridge.bridgeStream = StreamRequest.new(create, StreamBudget.new(), StreamBudget.Priority.Bridge)
				return created
			end
			local ok, err = pcall(function()
				-- 410 for a result is a discarded result, with the reason read
				-- only from a body the server sent.
				respondWith({ code = 410 })
				local answered, result = bridge:request("result", {}):await()
				expect(answered).to.equal(true)
				expect(result.discarded).to.equal(true)
				expect(result.reason).to.equal("Request expired or was cancelled")
				respondWith({ code = 410, body = '{"error":"Request expired"}' })
				answered, result = bridge:request("result", {}):await()
				expect(answered).to.equal(true)
				expect(result.reason).to.equal("Request expired")

				-- 503 is transient: its status is recorded and it is sent again.
				local failure = {}
				respondWith({ code = 503 })
				answered = bridge:request("poll", {}, failure):await()
				expect(answered).to.equal(false)
				expect(failure.status).to.equal(503)
				local created = respondWith({ code = 503 }, { code = 200 })
				local _, attempts = bridge:__requestWithRetry("poll", {})
				expect(attempts).to.equal(2)
				expect(#created).to.equal(2)

				-- 409 means the server refused the session, so it is not retried.
				created = respondWith({ code = 409 }, { code = 200 })
				local retried = pcall(bridge.__requestWithRetry, bridge, "poll", {})
				expect(retried).to.equal(false)
				expect(#created).to.equal(1)
			end)
			StudioBridge.retryDelay = previousDelay
			assert(ok, err)
		end)

		it("falls back to RequestAsync only when no stream slot is free", function()
			local budget = StreamBudget.new(1, 0, 0)
			local release = budget:tryAcquire(StreamBudget.Priority.Socket)
			local create, created = fakeClient(function() end)
			StudioBridge.bridgeStream = StreamRequest.new(create, budget, StreamBudget.Priority.Bridge)
			local previousFallback = StudioBridge.fallbackRequestAsync
			local fallbackUrl
			StudioBridge.fallbackRequestAsync = function(options)
				fallbackUrl = options.Url
				return { Success = true, StatusCode = 200, Body = '{"commands":[]}' }
			end
			local ok, response = pcall(bridge.__requestAsync, bridge, {
				Url = "http://127.0.0.1:34872/api/studio/poll",
				Method = "POST",
				Body = "{}",
			})
			StudioBridge.fallbackRequestAsync = previousFallback
			release()
			expect(#created).to.equal(0)
			expect(ok).to.equal(true)
			expect(response.Body).to.equal('{"commands":[]}')
			expect(fallbackUrl).to.equal("http://127.0.0.1:34872/api/studio/poll")
		end)
	end)

	it("stops polling once the server answers with a conflict", function()
		bridge.request = StudioBridge.request
		local previous = StudioBridge.requestAsync
		local routes = {}
		StudioBridge.requestAsync = function(options)
			local route = string.match(options.Url, "/api/studio/(%w+)$")
			table.insert(routes, route)
			if route == "register" then
				return { Success = true, StatusCode = 200, Body = '{"registered":true}' }
			end
			-- The checkout changed, so the server refuses the session.
			return { Success = false, StatusCode = 409, Body = '{"error":"identity changed"}' }
		end
		local failure
		bridge.onFailure = function(err)
			failure = err
		end
		StudioBridge.manualStart = true
		bridge:start()
		task.wait()
		task.wait()
		StudioBridge.manualStart = false
		StudioBridge.requestAsync = previous
		expect(bridge.stopped).to.equal(true)
		expect(failure ~= nil and string.find(failure, "409", 1, true) ~= nil).to.equal(true)
		local polls = 0
		for _, route in routes do
			if route == "poll" then
				polls += 1
			end
		end
		expect(polls).to.equal(1)
		expect(routes[#routes]).to.equal("disconnect")
	end)

	local function waitUntil(condition)
		local deadline = os.clock() + 2
		while not condition() and os.clock() < deadline do
			task.wait()
		end
	end

	it("keeps polling after polls that time out while Git is busy", function()
		local pollGate = Instance.new("BindableEvent")
		pollGate.Parent = fixture
		local previousDelay = StudioBridge.retryDelay
		StudioBridge.retryDelay = 0
		bridge.request = StudioBridge.request
		local failed = false
		bridge.onFailure = function()
			failed = true
		end
		local polls = 0
		bridge.__requestAsync = function(_, options)
			local route = options.Url:match("/([^/]+)$")
			if route == "poll" then
				polls += 1
				if polls == 1 then
					error("HttpError: Timedout")
				elseif polls == 2 then
					return { Success = false, StatusCode = 503, Body = '{"error":"busy"}' }
				end
				pollGate.Event:Wait()
			end
			return { Success = true, StatusCode = 200, Body = '{"commands":[]}' }
		end
		StudioBridge.manualStart = true
		bridge:start()
		StudioBridge.manualStart = false
		waitUntil(function()
			return polls >= 3
		end)
		StudioBridge.retryDelay = previousDelay
		-- A lost poll and a 5xx are retried; the third attempt is waiting on
		-- the server again.
		expect(polls).to.equal(3)
		expect(failed).to.equal(false)
		expect(bridge.stopped).to.equal(false)
	end)

	it("stops after the same request fails three times in a row", function()
		local previousDelay = StudioBridge.retryDelay
		StudioBridge.retryDelay = 0
		bridge.request = StudioBridge.request
		local failure
		bridge.onFailure = function(err)
			failure = err
		end
		local polls = 0
		bridge.__requestAsync = function(_, options)
			if options.Url:match("/poll$") then
				polls += 1
				error("HttpError: ConnectFail")
			end
			return { Success = true, StatusCode = 200, Body = "{}" }
		end
		StudioBridge.manualStart = true
		bridge:start()
		StudioBridge.manualStart = false
		waitUntil(function()
			return failure ~= nil
		end)
		StudioBridge.retryDelay = previousDelay
		expect(polls).to.equal(3)
		expect(bridge.stopped).to.equal(true)
		expect(string.find(failure, "ConnectFail", 1, true) ~= nil).to.equal(true)
	end)

	-- Serves one Run-mode command on the first poll, if `action` is given,
	-- answers the next poll with 404, and holds every later poll open.
	local function expireAfterPoll(action)
		local pollGate = Instance.new("BindableEvent")
		pollGate.Parent = fixture
		local calls = { register = 0, poll = 0, descriptions = {} }
		bridge.request = StudioBridge.request
		bridge.execute = function()
			return {}
		end
		bridge.__requestAsync = function(_, options)
			local route = options.Url:match("/([^/]+)$")
			if route == "register" then
				calls.register += 1
				table.insert(calls.descriptions, HttpService:JSONDecode(options.Body).description)
			elseif route == "poll" then
				calls.poll += 1
				if calls.poll == 1 and action then
					return {
						Success = true,
						StatusCode = 200,
						Body = HttpService:JSONEncode({
							commands = {
								{
									action = action,
									args = {},
									requestId = "cancelled",
									expiresAt = DateTime.now().UnixTimestampMillis + 10_000,
								},
							},
						}),
					}
				elseif calls.poll == (if action then 2 else 1) then
					return { Success = false, StatusCode = 404, Body = '{"error":"Studio session expired"}' }
				end
				pollGate.Event:Wait()
			elseif route == "result" then
				-- The run was cancelled, so its result is discarded.
				return { Success = false, StatusCode = 410, Body = '{"error":"Request expired or was cancelled"}' }
			end
			return { Success = true, StatusCode = 200, Body = '{"commands":[]}' }
		end
		return calls
	end

	it("registers again once when its session expired during Run mode", function()
		local calls = expireAfterPoll("spec")
		local failed = false
		bridge.onFailure = function()
			failed = true
		end
		StudioBridge.manualStart = true
		bridge:start()
		StudioBridge.manualStart = false
		waitUntil(function()
			return calls.poll >= 3
		end)
		expect(failed).to.equal(false)
		expect(bridge.stopped).to.equal(false)
		expect(calls.register).to.equal(2)
		expect(calls.poll).to.equal(3)
		-- The same description, so the server treats it as the same session.
		expect(calls.descriptions[2].sessionId).to.equal(calls.descriptions[1].sessionId)
		expect(calls.descriptions[2].placeName).to.equal(calls.descriptions[1].placeName)
		expect(calls.descriptions[2].mode).to.equal(calls.descriptions[1].mode)
	end)

	it("stops when its session expired without a Run-mode command", function()
		local calls = expireAfterPoll(nil)
		local failure
		bridge.onFailure = function(err)
			failure = err
		end
		StudioBridge.manualStart = true
		bridge:start()
		StudioBridge.manualStart = false
		waitUntil(function()
			return failure ~= nil
		end)
		expect(bridge.stopped).to.equal(true)
		expect(calls.register).to.equal(1)
		expect(string.find(failure, "404", 1, true) ~= nil).to.equal(true)
	end)

	it("resends a result whose first attempt was lost", function()
		local previousDelay = StudioBridge.retryDelay
		StudioBridge.retryDelay = 0
		bridge.request = StudioBridge.request
		local attempts = 0
		bridge.__requestAsync = function()
			attempts += 1
			if attempts == 1 then
				error("HttpError: Timedout")
			end
			return { Success = true, StatusCode = 200, Body = '{"accepted":true}' }
		end
		local ok, response, sent = pcall(bridge.__requestWithRetry, bridge, "result", {
			requestId = "test",
			result = { ok = true, value = {} },
		})
		StudioBridge.retryDelay = previousDelay
		expect(ok).to.equal(true)
		expect(response.accepted).to.equal(true)
		expect(sent).to.equal(2)
	end)

	it("announces the active bridge once per server", function()
		local identity = { serverSession = HttpService:GenerateGUID(false), digest = "abc" }
		local first = StudioBridge.new(
			{ __baseUrl = "http://127.0.0.1:34872" },
			{ protocolVersion = 1, pluginToken = "test", identity = identity },
			function() end
		)
		local second = StudioBridge.new(
			{ __baseUrl = "http://127.0.0.1:34872" },
			{ protocolVersion = 1, pluginToken = "test", identity = identity },
			function() end
		)
		expect(first:__announce()).to.equal(true)
		expect(second:__announce()).to.equal(false)
	end)

	it("refuses pairing from a remote address", function()
		expect(function()
			StudioBridge.new(
				{ __baseUrl = "http://example.com:34872" },
				{ protocolVersion = 1, pluginToken = "test", identity = {} },
				function() end
			)
		end).to.throw()
	end)

	it("preserves exact identity for duplicate-named siblings", function()
		local one = Instance.new("Folder")
		one.Name = "Same"
		one.Parent = fixture
		local two = Instance.new("Folder")
		two.Name = "Same"
		two.Parent = fixture
		expect(bridge:resolve(bridge:describe(one).id)).to.equal(one)
		expect(bridge:resolve(bridge:describe(two).id)).to.equal(two)
		expect(function()
			bridge:resolve("game/ReplicatedStorage/__RojoBridgeSpec/Same")
		end).to.throw()
	end)

	it("bounds tree traversal and returns useful instance IDs", function()
		for _ = 1, 5 do
			local child = Instance.new("Folder")
			child.Parent = fixture
		end
		local result = bridge:execute(command("tree", { instance = bridge:describe(fixture).id, depth = 1, limit = 3 }))
		expect(#result.nodes).to.equal(3)
		expect(result.truncated).to.equal(true)
		expect(bridge:resolve(result.nodes[1].id)).to.equal(fixture)
	end)

	it("encodes requested properties without losing typed values", function()
		local result = bridge:execute(
			command(
				"properties",
				{ instance = bridge:describe(fixture).id, properties = { "Name", "Archivable", "NotAProperty" } }
			)
		)
		expect(result.properties.Name.value).to.be.ok()
		expect(result.properties.Archivable.value).to.be.ok()
		expect(type(result.properties.NotAProperty.error)).to.equal("string")
	end)

	it("rejects expired commands before changing Studio", function()
		local request = command("tree", {})
		request.expiresAt = 0
		expect(function()
			bridge:execute(request)
		end).to.throw()
	end)

	it("reports precise unavailable execution and whole-place capabilities", function()
		local capabilities = bridge:capabilities()
		expect(capabilities.execute.available).to.equal(false)
		expect(capabilities.placeSave.available).to.equal(false)
		expect(type(capabilities.placeSave.reason)).to.equal("string")
	end)

	it("exports native UI bytes with descendants and properties", function()
		local screen = Instance.new("ScreenGui")
		screen.Name = "NativeUI"
		screen.Parent = fixture
		local label = Instance.new("TextLabel")
		label.Text = "Native export"
		label.Parent = screen
		local result = bridge:execute(command("export", { instance = bridge:describe(screen).id }))
		local instances = SerializationService:DeserializeInstancesAsync(
			EncodingService:Base64Decode(buffer.fromstring(result.model))
		)
		expect(instances[1].Name).to.equal("NativeUI")
		expect(instances[1].TextLabel.Text).to.equal("Native export")
		for _, instance in instances do
			instance:Destroy()
		end
	end)

	it("rejects model export of DataModel and services", function()
		expect(function()
			bridge:execute(command("export", { instance = "game" }))
		end).to.throw()
		expect(function()
			bridge:execute(command("export", { instance = "game/ReplicatedStorage" }))
		end).to.throw()
	end)

	it("validates and imports native models without replacing siblings", function()
		local part = Instance.new("Part")
		part.Name = "ImportedPart"
		part.Size = Vector3.new(2, 3, 4)
		local bytes = SerializationService:SerializeInstancesAsync({ part })
		part:Destroy()
		local result = bridge:execute(
			command(
				"import",
				{ parent = bridge:describe(fixture).id, model = buffer.tostring(EncodingService:Base64Encode(bytes)) }
			)
		)
		expect(result.count).to.equal(1)
		expect(fixture.ImportedPart.Size).to.equal(Vector3.new(2, 3, 4))
	end)

	it("sanitizes non-finite numbers, invalid UTF-8 and Roblox values before JSON encoding", function()
		local value = StudioBridge.sanitize({
			maxForce = math.huge,
			minimum = -math.huge,
			nan = 0 / 0,
			finite = 1.5,
			broken = "ok" .. string.char(0xFF) .. "end",
			surrogate = "\237\160\128",
			list = { 1, Vector3.new(1, 2, 3) },
			[string.char(0xC3)] = true,
		})
		local decoded = HttpService:JSONDecode(HttpService:JSONEncode(value))
		expect(decoded.maxForce.value).to.equal("inf")
		expect(decoded.minimum.value).to.equal("-inf")
		expect(decoded.nan.value).to.equal("nan")
		expect(decoded.finite).to.equal(1.5)
		expect(decoded.broken).to.equal("ok\u{FFFD}end")
		expect(decoded.surrogate).to.equal("\u{FFFD}")
		expect(decoded.list[1]).to.equal(1)
		expect(decoded.list[2].type).to.equal("Vector3")
		expect(decoded["\u{FFFD}"]).to.equal(true)
	end)

	it("truncates log text on codepoint boundaries", function()
		expect(StudioBridge.truncateUtf8("a雪", 2)).to.equal("a")
		expect(StudioBridge.truncateUtf8("a雪", 4)).to.equal("a雪")
		expect(StudioBridge.truncateUtf8(string.rep("🥚", 10), 6)).to.equal("🥚")
	end)

	it("answers with an error result when a command result cannot be encoded", function()
		bridge.request = StudioBridge.request
		local sent
		bridge.__requestAsync = function(_, options)
			sent = HttpService:JSONDecode(options.Body)
			return { Success = true, StatusCode = 200, Body = "{}" }
		end
		local nested = {}
		local current = nested
		for _ = 1, 100 do
			current.child = {}
			current = current.child
		end
		bridge:request("result", { requestId = "deep", result = { ok = true, value = nested } }):expect()
		expect(sent.requestId).to.equal("deep")
		expect(sent.result.ok).to.equal(false)
		expect(type(sent.result.error)).to.equal("string")
	end)

	it("keeps the bridge running when a command returns unencodable values", function()
		local pollGate = Instance.new("BindableEvent")
		pollGate.Parent = fixture
		local delivered
		local polls = 0
		bridge.request = StudioBridge.request
		bridge.execute = function()
			return { maxForce = math.huge, text = string.char(0xE9) }
		end
		bridge.__requestAsync = function(_, options)
			local route = options.Url:match("/([^/]+)$")
			local response = {}
			if route == "poll" then
				polls += 1
				if polls == 1 then
					response = { commands = { command("properties", {}) } }
				else
					pollGate.Event:Wait()
					response = { commands = {} }
				end
			elseif route == "result" then
				delivered = HttpService:JSONDecode(options.Body).result
			end
			return { Success = true, StatusCode = 200, Body = HttpService:JSONEncode(response) }
		end
		bridge:start()
		task.wait()
		expect(delivered.ok).to.equal(true)
		expect(delivered.value.maxForce.value).to.equal("inf")
		expect(delivered.value.text).to.equal("\u{FFFD}")
		expect(bridge.stopped).to.equal(false)
	end)

	it("cancels the underlying transport when a bridge request is cancelled or times out", function()
		bridge.request = StudioBridge.request
		local transport = Promise.new(function() end)
		bridge.__requestAsync = function(_, _, token)
			token.transport = transport
			return transport:expect()
		end
		local pending = bridge:request("poll", {})
		pending:cancel()
		expect(transport:getStatus()).to.equal(Promise.Status.Cancelled)
	end)

	it("cancels every in-flight transport when stopped", function()
		local first = Promise.new(function() end)
		local second = Promise.new(function() end)
		bridge.transports[first] = true
		bridge.transports[second] = true
		bridge:stop()
		expect(first:getStatus()).to.equal(Promise.Status.Cancelled)
		expect(second:getStatus()).to.equal(Promise.Status.Cancelled)
		expect(next(bridge.transports)).to.equal(nil)
	end)

	it("reports a failed transport without an unhandled promise rejection", function()
		local originalRequest = StudioBridge.requestAsync
		local failure = { message = "transport failed" }
		StudioBridge.requestAsync = function()
			task.wait()
			error(failure)
		end
		local unhandled = 0
		local disconnect = Promise.onUnhandledRejection(function(_, value)
			-- Other specs may leave their own unhandled rejections.
			if value == failure then
				unhandled += 1
			end
		end)
		local ok = pcall(bridge.__requestAsync, bridge, { Url = "failing" })
		task.wait()
		task.wait()
		disconnect()
		StudioBridge.requestAsync = originalRequest
		expect(ok).to.equal(false)
		expect(next(bridge.transports)).to.equal(nil)
		expect(unhandled).to.equal(0)
	end)

	it("only imports into services that belong to the place", function()
		expect(pcall(StudioBridge.checkImportParent, game)).to.equal(false)
		expect(pcall(StudioBridge.checkImportParent, workspace)).to.equal(true)
		expect(pcall(StudioBridge.checkImportParent, fixture)).to.equal(true)
		local detached = Instance.new("Folder")
		expect(pcall(StudioBridge.checkImportParent, detached)).to.equal(false)
		detached:Destroy()
		for _, name in
			{
				"CoreGui",
				"CorePackages",
				"PluginGuiService",
				"PluginDebugService",
				"RobloxPluginGuiService",
				"RobloxReplicatedStorage",
			}
		do
			local found, service = pcall(game.GetService, game, name)
			if found and service then
				local ok, err = pcall(StudioBridge.checkImportParent, service)
				expect(ok).to.equal(false)
				expect(string.find(tostring(err), "cannot target", 1, true) ~= nil).to.equal(true)
			end
		end
		local controls = game:GetService("CoreGui"):FindFirstChild("RojoAgentControls")
		if controls then
			expect(pcall(StudioBridge.checkImportParent, controls)).to.equal(false)
		end
	end)

	it("keeps the test runner's insert and removal out of separate undo steps", function()
		local ChangeHistoryService = game:GetService("ChangeHistoryService")
		expect(StudioBridge.runnerHistoryOperation).to.equal(Enum.FinishRecordingOperation.Append)
		local userRecording = ChangeHistoryService:TryBeginRecording("Rojo spec: user change")
		if not userRecording then
			-- Undo history is unavailable here, so only the setting is checked.
			return
		end
		local userChange = Instance.new("Folder")
		userChange.Name = "UserChange"
		userChange.Parent = fixture
		ChangeHistoryService:FinishRecording(userRecording, Enum.FinishRecordingOperation.Commit)

		local runner = Instance.new("Folder")
		runner.Name = "Runner"
		expect((StudioBridge.recordRunnerChange("Rojo spec: insert runner", function()
			runner.Parent = fixture
		end))).to.equal(true)
		expect((StudioBridge.recordRunnerChange("Rojo spec: remove runner", function()
			runner.Parent = nil
		end))).to.equal(true)

		-- One undo reverts the user's change; the runner must not come back.
		ChangeHistoryService:Undo()
		expect(fixture:FindFirstChild("Runner")).to.equal(nil)
		expect(fixture:FindFirstChild("UserChange")).to.equal(nil)
		ChangeHistoryService:Redo()
		expect(fixture:FindFirstChild("Runner")).to.equal(nil)
		expect(fixture:FindFirstChild("UserChange")).to.be.ok()
	end)

	it("holds instance IDs strongly and forgets destroyed instances", function()
		expect(getmetatable(bridge.instances)).to.equal(nil)
		expect(getmetatable(bridge.ids)).to.equal(nil)
		local child = Instance.new("Folder")
		child.Parent = fixture
		local id = bridge:describe(child).id
		expect(bridge:resolve(id)).to.equal(child)
		child:Destroy()
		-- Destroying may be a deferred signal.
		task.wait()
		expect(bridge.instances[id]).to.equal(nil)
		expect(bridge.ids[child]).to.equal(nil)
	end)

	it("describes paths that resolve even when names contain slashes", function()
		local child = Instance.new("Folder")
		child.Name = "Left/Right\\Back"
		child.Parent = fixture
		local description = bridge:describe(child)
		expect(description.path).to.equal("game/ReplicatedStorage/__RojoBridgeSpec/Left\\/Right\\\\Back")
		expect(description.fullName).to.equal(child:GetFullName())
		expect(bridge:resolve(description.path)).to.equal(child)
		expect(bridge:resolve(description.pathSegments)).to.equal(child)
		expect(bridge:resolve("game/ReplicatedStorage/__RojoBridgeSpec")).to.equal(fixture)
	end)

	it("only reports a truncated tree when nodes were left out", function()
		for _ = 1, 2 do
			Instance.new("Folder").Parent = fixture
		end
		local exact = bridge:execute(command("tree", { instance = bridge:describe(fixture).id, depth = 1, limit = 3 }))
		expect(#exact.nodes).to.equal(3)
		expect(exact.truncated).to.equal(false)
		Instance.new("Folder").Parent = fixture
		local over = bridge:execute(command("tree", { instance = bridge:describe(fixture).id, depth = 1, limit = 3 }))
		expect(over.truncated).to.equal(true)
	end)

	it("does not start automatically when the auto-enable setting is off", function()
		local Settings = require(script.Parent.Settings)
		local previous = Settings:get("studioBridgeAutoEnable")
		Settings:set("studioBridgeAutoEnable", false)
		local failure, intentional
		bridge.onFailure = function(message, quiet)
			failure, intentional = message, quiet
		end
		bridge:start()
		Settings:set("studioBridgeAutoEnable", previous)
		expect(bridge.stopped).to.equal(true)
		expect(bridge.worker).to.equal(nil)
		expect(type(failure)).to.equal("string")
		-- Reported as the user's choice so the session logs it at debug level.
		expect(intentional).to.equal(true)
	end)

	it("requires script opt-in and keeps imported executable scripts disabled", function()
		local source = Instance.new("Script")
		source.Source = "error('must not run')"
		local bytes = SerializationService:SerializeInstancesAsync({ source })
		source:Destroy()
		local args =
			{ parent = bridge:describe(fixture).id, model = buffer.tostring(EncodingService:Base64Encode(bytes)) }
		expect(function()
			bridge:execute(command("import", args))
		end).to.throw()
		expect(#fixture:GetChildren()).to.equal(0)
		args.allowScripts = true
		bridge:execute(command("import", args))
		expect(fixture.Script.Disabled).to.equal(true)
	end)

	describe("spec", function()
		local originalExecuteRunMode = StudioBridge.executeRunMode
		local originalHeartbeatInterval = StudioBridge.heartbeatInterval
		local originalRunModeGrace = StudioBridge.runModeGrace
		local fixturePath = "game/ReplicatedStorage/__RojoBridgeSpec"

		local function specFixture()
			local framework = Instance.new("ModuleScript")
			framework.Name = "TestEZ"
			framework.Parent = fixture
			local spec = Instance.new("ModuleScript")
			spec.Name = "Example.spec"
			spec.Parent = fixture
			return { roots = { fixturePath }, module = fixturePath .. "/TestEZ", timeoutSeconds = 5 }
		end

		local function runner()
			return game:GetService("ServerScriptService"):FindFirstChild("__RojoBridgeSpec_" .. bridge.sessionId)
		end

		-- A spec command with the deadline the server gives it: its timeout
		-- plus 30 seconds.
		local function specCommand(args)
			local request = command("spec", args)
			request.expiresAt = DateTime.now().UnixTimestampMillis + (args.timeoutSeconds + 30) * 1000
			return request
		end

		afterEach(function()
			StudioBridge.executeRunMode = originalExecuteRunMode
			StudioBridge.heartbeatInterval = originalHeartbeatInterval
			StudioBridge.runModeGrace = originalRunModeGrace
		end)

		it("advertises its bounds", function()
			local capability = bridge:capabilities().spec
			expect(capability.maxTimeoutSeconds).to.equal(300)
			expect(capability.mode).to.equal("Run")
		end)

		it("rejects bad options and missing paths before starting Run mode", function()
			local started = false
			StudioBridge.executeRunMode = function()
				started = true
				return {}
			end
			for _, case in
				{
					{ args = { timeoutSeconds = 0 }, reason = "timeoutSeconds" },
					{ args = { framework = "mocha" }, reason = "framework" },
					{ args = { roots = { fixturePath .. "/Missing" } }, reason = "does not exist" },
					{
						args = { roots = { fixturePath }, module = fixturePath .. "/Missing" },
						reason = "does not exist",
					},
				}
			do
				local ok, err = pcall(bridge.execute, bridge, command("spec", case.args))
				expect(ok).to.equal(false)
				expect(string.find(tostring(err), case.reason, 1, true)).to.be.ok()
			end
			expect(started).to.equal(false)
			expect(runner()).to.equal(nil)
		end)

		it("runs a fixed runner with the options as data and removes it afterwards", function()
			local args = specFixture()
			local seen
			StudioBridge.executeRunMode = function(testArgs)
				local inserted = runner()
				seen = {
					args = testArgs,
					source = inserted and inserted.Source,
					module = inserted and inserted:FindFirstChild("SpecRunner"),
				}
				return { framework = "testez", status = "passed", passed = 1, failed = 0, skipped = 0 }
			end
			local result = bridge:execute(specCommand(args))
			expect(seen.source).to.equal(StudioBridge.SPEC_RUNNER_SOURCE)
			expect(seen.module).to.be.ok()
			expect(seen.module:IsA("ModuleScript")).to.equal(true)
			expect(seen.args.roots[1]).to.equal(fixturePath)
			expect(seen.args.module).to.equal(fixturePath .. "/TestEZ")
			expect(seen.args.timeoutSeconds).to.equal(5)
			expect(seen.args.framework).to.equal("auto")
			expect(result.mode).to.equal("Run")
			expect(result.report.status).to.equal("passed")
			expect(result.report.passed).to.equal(1)
			expect(runner()).to.equal(nil)
		end)

		it("keeps the session alive while Run mode lasts", function()
			local args = specFixture()
			local validations = 0
			bridge.request = function(_, route)
				if route == "validate" then
					validations += 1
				end
				return Promise.resolve({ active = true })
			end
			StudioBridge.heartbeatInterval = 0.05
			StudioBridge.executeRunMode = function()
				task.wait(0.4)
				return { status = "passed" }
			end
			bridge:execute(specCommand(args))
			-- One validation before Run mode starts, then the heartbeats.
			expect(validations >= 3).to.equal(true)
			local after = validations
			task.wait(0.2)
			expect(validations).to.equal(after)
		end)

		it("does not start Run mode with too little of the deadline left", function()
			local args = specFixture()
			local started = false
			StudioBridge.executeRunMode = function()
				started = true
				return { status = "passed" }
			end
			-- As if the request had waited behind another run: 8 seconds are
			-- left for a 5-second run.
			local request = specCommand(args)
			request.expiresAt = DateTime.now().UnixTimestampMillis + 8_000
			local ok, err = pcall(bridge.execute, bridge, request)
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "run the specs again", 1, true)).to.be.ok()
			expect(started).to.equal(false)
			expect(runner()).to.equal(nil)
		end)

		it("keeps the heartbeats going after the request is cancelled", function()
			local args = specFixture()
			local validations = 0
			bridge.request = function(_, route, _value, failure)
				if route ~= "validate" then
					return Promise.resolve({ active = true })
				end
				validations += 1
				-- The validation before Run mode starts succeeds; then the
				-- request is cancelled.
				if validations == 1 then
					return Promise.resolve({ active = true })
				end
				if failure then
					failure.status = 410
				end
				return Promise.reject("Studio bridge request failed: 410")
			end
			StudioBridge.heartbeatInterval = 0.05
			StudioBridge.executeRunMode = function()
				task.wait(0.4)
				return { status = "passed" }
			end
			-- The cancelled run's result is discarded, but Run mode goes on,
			-- and the heartbeats keep its session from expiring until it ends.
			pcall(bridge.execute, bridge, specCommand(args))
			expect(validations >= 4).to.equal(true)
			local after = validations
			task.wait(0.2)
			expect(validations).to.equal(after)
		end)

		it("stops the heartbeats once a run outlasts its watchdog", function()
			local validations = 0
			bridge.request = function(_, route)
				if route == "validate" then
					validations += 1
				end
				return Promise.resolve({ active = true })
			end
			StudioBridge.heartbeatInterval = 0.05
			StudioBridge.runModeGrace = 0.15
			StudioBridge.executeRunMode = function()
				-- Far past the watchdog, as when Studio never returns.
				task.wait(0.6)
				return {}
			end
			local testRunner = Instance.new("Script")
			bridge:__runInRunMode(command("test", {}), testRunner, {}, 0)
			-- About three heartbeats fit before the watchdog; a dozen would
			-- fit in the whole run.
			expect(validations >= 1).to.equal(true)
			expect(validations <= 4).to.equal(true)
			expect(testRunner.Parent).to.equal(nil)
		end)

		it("fails when Run mode ends without a report", function()
			local args = specFixture()
			StudioBridge.executeRunMode = function()
				return nil
			end
			local ok, err = pcall(bridge.execute, bridge, specCommand(args))
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "without a report", 1, true)).to.be.ok()
			expect(runner()).to.equal(nil)
		end)

		it("removes the runner when Run mode fails", function()
			local args = specFixture()
			StudioBridge.executeRunMode = function()
				error("Studio is already running a test")
			end
			local ok, err = pcall(bridge.execute, bridge, specCommand(args))
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "already running", 1, true)).to.be.ok()
			expect(runner()).to.equal(nil)
		end)
	end)

	describe("logs", function()
		local function record(count)
			for index = 1, count do
				bridge:__recordLog("message " .. index, "MessageOutput")
			end
		end

		local function sequences(entries)
			local result = {}
			for _, entry in entries do
				table.insert(result, entry.sequence)
			end
			return table.concat(result, ",")
		end

		it("numbers entries and keeps counting when the buffer is trimmed", function()
			record(205)
			expect(#bridge.logs).to.equal(200)
			expect(bridge.logs[1].sequence).to.equal(6)
			expect(bridge.logs[1].message).to.equal("message 6")
			expect(bridge.logs[200].sequence).to.equal(205)
			expect(bridge.logSequence).to.equal(205)
		end)

		it("returns the newest entries as a list without after", function()
			record(5)
			local result = bridge:execute(command("logs", { limit = 2 }))
			expect(result.entries).to.equal(nil)
			expect(sequences(result)).to.equal("4,5")
		end)

		it("returns only entries newer than after", function()
			record(5)
			local result = bridge:execute(command("logs", { after = 3 }))
			expect(sequences(result.entries)).to.equal("4,5")
			expect(result.latestSequence).to.equal(5)
			expect(result.nextAfter).to.equal(5)
			expect(result.dropped).to.equal(0)
			expect(result.more).to.equal(false)
			expect(result.restarted).to.equal(false)
		end)

		it("returns no entries when nothing is new", function()
			record(2)
			local result = bridge:execute(command("logs", { after = 2 }))
			expect(#result.entries).to.equal(0)
			expect(result.nextAfter).to.equal(2)
			expect(result.dropped).to.equal(0)
		end)

		it("reports entries trimmed from the buffer since after", function()
			record(3)
			local first = bridge:execute(command("logs", { after = 0 }))
			expect(first.nextAfter).to.equal(3)
			record(250)
			local result = bridge:execute(command("logs", { after = first.nextAfter, limit = 200 }))
			-- 253 entries were captured and the oldest kept is 54, so 4-53 were lost.
			expect(result.dropped).to.equal(50)
			expect(result.entries[1].sequence).to.equal(54)
			expect(#result.entries).to.equal(200)
			expect(result.more).to.equal(false)
			expect(result.nextAfter).to.equal(253)
		end)

		it("returns the oldest new entries first when more than limit are waiting", function()
			record(10)
			local result = bridge:execute(command("logs", { after = 2, limit = 3 }))
			expect(sequences(result.entries)).to.equal("3,4,5")
			expect(result.more).to.equal(true)
			expect(result.nextAfter).to.equal(5)
			local rest = bridge:execute(command("logs", { after = result.nextAfter, limit = 10 }))
			expect(sequences(rest.entries)).to.equal("6,7,8,9,10")
			expect(rest.more).to.equal(false)
		end)

		it("starts over when after is ahead of the latest sequence", function()
			record(3)
			local result = bridge:execute(command("logs", { after = 99 }))
			expect(result.restarted).to.equal(true)
			expect(sequences(result.entries)).to.equal("1,2,3")
			expect(result.dropped).to.equal(0)
		end)

		it("counts cleared entries as dropped", function()
			record(3)
			bridge.logs = {}
			local result = bridge:execute(command("logs", { after = 1 }))
			expect(#result.entries).to.equal(0)
			expect(result.dropped).to.equal(2)
			expect(result.nextAfter).to.equal(3)
		end)

		it("advertises follow support", function()
			local logs = bridge:capabilities().logs
			expect(logs.follow).to.equal(true)
			expect(logs.maxEntries).to.equal(200)
		end)
	end)

	describe("sync session attachment", function()
		it("keeps the registered description free of changing sync state", function()
			local status = bridge:status()
			expect(type(status.pluginVersion)).to.equal("string")
			expect(status.sync).to.equal(nil)
			expect(status.capabilities.sources.available).to.equal(false)
		end)

		it("answers status, sources and resync through the sync session", function()
			local calls = {}
			bridge.sync = {
				getSummary = function()
					return { status = "Connected", patchesApplied = 3 }
				end,
				getSourceHashes = function()
					table.insert(calls, "sources")
					return { scripts = {}, detached = {}, held = {} }
				end,
				applyResync = function(patch)
					table.insert(calls, "resync")
					return { updated = #patch.updated, failed = 0 }
				end,
			}
			local status = bridge:execute(command("status", {}))
			expect(status.sync.patchesApplied).to.equal(3)
			expect(status.capabilities.sources.available).to.equal(true)
			expect(#bridge:execute(command("sources", {})).scripts).to.equal(0)
			local result = bridge:execute(command("resync", { patch = { updated = { {}, {} } } }))
			expect(result.updated).to.equal(2)
			expect(table.concat(calls, ",")).to.equal("sources,resync")
		end)

		it("refuses sources and resync without a sync session", function()
			expect(pcall(bridge.execute, bridge, command("sources", {}))).to.equal(false)
			expect(pcall(bridge.execute, bridge, command("resync", { patch = { updated = {} } }))).to.equal(false)
		end)

		it("sends no summary when the sync session cannot give one", function()
			bridge.sync = {
				getSummary = function()
					error("broken")
				end,
			}
			expect(bridge:syncSummary()).to.equal(nil)
		end)
	end)
end
