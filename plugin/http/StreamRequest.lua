local HttpService = game:GetService("HttpService")
local Promise = require(script.Parent.Parent.Promise)
local HttpError = require(script.Parent.Error)
local StreamBudget = require(script.Parent.StreamBudget)

-- RequestAsync had a built-in timeout. Streams do not, so a request that never
-- completes would hold this transport's only slot forever. The active timeout
-- measures inactivity, so large responses that keep arriving are not cut off.
local DEFAULT_TIMEOUT = 30
-- Queued work is bounded separately so a backlog fails visibly instead of
-- silently waiting behind a slow request.
local DEFAULT_QUEUE_TIMEOUT = 60
-- The longest a `holdWhile` hook can keep an active request from idling.
local DEFAULT_HOLD_LIMIT = 120
-- Bodies are buffered in memory until the stream closes, so a response that
-- never ends must not grow without bound.
local DEFAULT_MAX_BYTES = 64 * 1024 * 1024

local StreamRequest = {}

local function connectionError(message, opened)
	local lower = string.lower(tostring(message))
	if string.find(lower, "http requests are not enabled", 1, true) then
		return HttpError.new(HttpError.Kind.HttpNotEnabled)
	elseif string.find(lower, "permission denied", 1, true) then
		-- Keep Studio's wording so a denied HTTP permission prompt is
		-- recognised, as it is for RequestAsync failures.
		return HttpError.new(HttpError.Kind.Unknown, tostring(message))
	elseif string.find(lower, "timed? ?out") then
		return HttpError.new(HttpError.Kind.Timeout)
	elseif not opened then
		-- Without a response status the server was never reached.
		return HttpError.new(HttpError.Kind.ConnectFailed)
	end
	return HttpError.new(HttpError.Kind.Unknown, "HTTP stream failed: " .. tostring(message))
end

local function timeoutError(detail)
	local err = HttpError.new(HttpError.Kind.Timeout)
	err.message = err.message .. " " .. detail
	return err
end

StreamRequest.connectionError = connectionError
StreamRequest.DEFAULT_TIMEOUT = DEFAULT_TIMEOUT
StreamRequest.DEFAULT_QUEUE_TIMEOUT = DEFAULT_QUEUE_TIMEOUT
StreamRequest.DEFAULT_MAX_BYTES = DEFAULT_MAX_BYTES

-- What a request resolves with when no stream client could be taken from its
-- budget. No client was created and nothing was sent, so the caller can send
-- the request another way.
StreamRequest.BudgetExhausted = setmetatable({}, {
	__tostring = function()
		return "StreamRequest.BudgetExhausted"
	end,
})

--[[
	`budget` is the StreamBudget every client must take a slot from before it
	is created. Studio's own clients default to the budget shared by the whole
	plugin. A custom `createClient` does not create Studio clients, so it gets
	a budget of its own unless one is passed.
]]
function StreamRequest.new(createClient, budget)
	if budget == nil then
		budget = if createClient then StreamBudget.new() else StreamBudget.shared
	end
	createClient = createClient
		or function(options)
			return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.RawStream, options)
		end
	local queue = {}
	local active
	local pump

	local function release(job)
		if job.onReleased and not job.released then
			job.released = true
			job.onReleased()
		end
	end

	local function releaseBudget(job)
		local releaseSlot = job.releaseBudget
		job.releaseBudget = nil
		if releaseSlot then
			releaseSlot()
		end
	end

	local function finish(job, response, err)
		if job.finished then
			return
		end
		job.finished = true
		if not job.creating then
			release(job)
		end
		-- A timer may be the thread finishing this job; it clears its own field
		-- first because a running thread cannot cancel itself.
		if job.timer then
			task.cancel(job.timer)
			job.timer = nil
		end
		if job.queueTimer then
			task.cancel(job.queueTimer)
			job.queueTimer = nil
		end
		for _, connection in job.connections do
			connection:Disconnect()
		end
		-- The slot is freed first, and closing is protected, because closing a
		-- client that already failed can throw. A throw here must not leave this
		-- transport's only slot taken or the caller waiting. The client is closed
		-- before the promise settles, so a caller never sees a result while its
		-- client is still open. A job whose client is still being created keeps
		-- the slot until creation returns: freeing it now would let the next
		-- job create a second client alongside it. Studio caps how many stream
		-- clients a plugin can have open and what happens past that cap is
		-- undocumented, so Rojo never has more open than its budget allows.
		if active == job and not job.creating then
			active = nil
		end
		task.defer(pump)
		local client = job.client
		job.client = nil
		if client then
			pcall(client.Close, client)
		end
		-- A client still being created keeps its budget slot until creation
		-- returns and it has been closed.
		if not job.creating then
			releaseBudget(job)
		end
		if err then
			job.reject(err)
		elseif response then
			job.resolve(response)
		end
	end

	local function checkIdle(job)
		job.timer = nil
		if job.finished then
			return
		end
		if job.holdWhile and job.holdWhile() then
			-- Time spent on Studio's HTTP permission prompt says nothing about
			-- the server, so it does not count as inactivity. A prompt that was
			-- never shown looks the same as one left open, so the hold is
			-- capped; inactivity counts again from the moment the cap is hit.
			local now = os.clock()
			job.heldSince = job.heldSince or now
			job.lastActivity = math.max(job.lastActivity, math.min(now, job.heldSince + job.holdLimit))
		end
		local idle = os.clock() - job.lastActivity
		if idle >= job.timeout then
			finish(job, nil, timeoutError(string.format("No response activity for %s seconds.", tostring(job.timeout))))
		else
			job.timer = task.delay(job.timeout - idle, checkIdle, job)
		end
	end

	pump = function()
		if active then
			return
		end
		local job = table.remove(queue, 1)
		while job and job.finished do
			job = table.remove(queue, 1)
		end
		if not job then
			return
		end
		local releaseSlot = budget:tryAcquire(StreamBudget.Priority.Request)
		if not releaseSlot then
			-- Rojo's stream budget is used up, so no client is created: what
			-- Studio does past its own limit is undocumented. The job ends
			-- without taking this transport's slot, and the caller sends it
			-- another way.
			job.finished = true
			if job.queueTimer then
				task.cancel(job.queueTimer)
				job.queueTimer = nil
			end
			release(job)
			job.resolve(StreamRequest.BudgetExhausted)
			task.defer(pump)
			return
		end
		job.releaseBudget = releaseSlot
		active = job
		if job.queueTimer then
			task.cancel(job.queueTimer)
			job.queueTimer = nil
		end
		if job.onStarted then
			-- Lets callers start their own deadline now instead of counting the
			-- time this job spent queued behind other requests.
			task.spawn(job.onStarted)
		end
		job.lastActivity = os.clock()
		job.timer = task.delay(job.timeout, checkIdle, job)
		job.creating = true
		local ok, client = pcall(createClient, job.options)
		job.creating = false
		if job.onCreated then
			-- Called even when the job already ended, so callers can still
			-- classify a creation failure such as a denied permission.
			task.spawn(job.onCreated, ok, if ok then nil else client)
		end
		if job.finished then
			-- The job ended while its client was being created, so it kept the
			-- slot and could not be released then. Creating the client can
			-- yield, and nothing else will close a late client.
			release(job)
			if ok then
				pcall(client.Close, client)
			end
			releaseBudget(job)
			if active == job then
				active = nil
			end
			task.defer(pump)
			return
		end
		if not ok then
			finish(job, nil, connectionError(client, false))
			return
		end
		job.client = client
		local chunks = {}
		local bytes = 0
		local status
		local headers = {}
		-- Only headers Studio passed as a string were parsed, so headers in any
		-- other form count as missing rather than as headers without Rojo's.
		local headersReceived = false
		local responding = false
		-- `HeadersReceived` tells callers whether `Headers` came from the server.
		-- `BodyFromServer` is false when Studio's error message stands in for a
		-- body that never arrived.
		local function touch()
			job.lastActivity = os.clock()
			if not responding then
				responding = true
				if job.onResponse then
					task.spawn(job.onResponse)
				end
			end
		end
		table.insert(
			job.connections,
			client.Opened:Connect(function(code, rawHeaders)
				touch()
				status = code
				-- `[ \t]*` rather than `%s*` so an empty value cannot consume the
				-- line break and swallow the following header.
				if type(rawHeaders) == "string" then
					headersReceived = true
					for name, value in string.gmatch(rawHeaders, "([^\r\n:]+):[ \t]*([^\r\n]*)") do
						headers[string.lower(name)] = value
					end
				end
			end)
		)
		table.insert(
			job.connections,
			client.MessageReceived:Connect(function(chunk)
				touch()
				if job.finished then
					return
				end
				bytes += #chunk
				if bytes > job.maxBytes then
					local err = HttpError.new(
						HttpError.Kind.Unknown,
						string.format("The HTTP response exceeded %d bytes.", job.maxBytes)
					)
					-- Lets callers resend a request that is safe to repeat in a
					-- way that is not limited by this buffer.
					err.responseTooLarge = true
					finish(job, nil, err)
					return
				end
				table.insert(chunks, chunk)
			end)
		)
		table.insert(
			job.connections,
			client.Error:Connect(function(code, message)
				code = tonumber(code) or 0
				if code >= 300 and code <= 599 then
					-- A status means the server answered.
					touch()
					-- WebStreamClient may report a non-2xx response through Error
					-- instead of Opened/Closed. Surface it as an ordinary HTTP
					-- response so callers can read its status and body.
					local body = table.concat(chunks)
					finish(job, {
						StatusCode = code,
						Success = false,
						Headers = headers,
						HeadersReceived = headersReceived,
						Body = if body ~= "" then body else tostring(message or ""),
						BodyFromServer = body ~= "",
					})
					return
				end
				finish(job, nil, connectionError(message, status ~= nil))
			end)
		)
		table.insert(
			job.connections,
			client.Closed:Connect(function()
				if not status then
					finish(job, nil, HttpError.new(HttpError.Kind.ConnectFailed))
					return
				end
				finish(job, {
					StatusCode = status,
					Success = status >= 200 and status < 300,
					Headers = headers,
					HeadersReceived = headersReceived,
					Body = table.concat(chunks),
					BodyFromServer = true,
				})
			end)
		)
	end

	-- `config.timeout` is the inactivity limit once the request is active and
	-- `config.queueTimeout` bounds the wait for this transport's slot. While
	-- `config.holdWhile()` returns true, the active request is not idle, for
	-- at most `config.holdLimit` seconds in total. `config.onReleased` is
	-- called once the request has finished and no call that creates its client
	-- is still running, since that call can keep blocking on Studio's HTTP
	-- permission prompt after the request was cancelled or timed out.
	-- `config.onCreated(ok, err)` is called once that call returns, with its
	-- error if it failed, even if the request already finished.
	-- `config.onStarted()` is called when the request leaves the queue and
	-- takes the slot, and `config.onResponse()` once, when a status, headers or
	-- body first arrive from the server. `config.maxBytes` caps the buffered
	-- body; a longer response fails the request with an error whose
	-- `responseTooLarge` is true and closes its client.
	return function(options, config)
		config = config or {}
		return Promise.new(function(resolve, reject, onCancel)
			local job = {
				options = options,
				resolve = resolve,
				reject = reject,
				connections = {},
				timeout = config.timeout or DEFAULT_TIMEOUT,
				holdWhile = config.holdWhile,
				holdLimit = config.holdLimit or DEFAULT_HOLD_LIMIT,
				onReleased = config.onReleased,
				onCreated = config.onCreated,
				onStarted = config.onStarted,
				onResponse = config.onResponse,
				maxBytes = config.maxBytes or DEFAULT_MAX_BYTES,
			}
			onCancel(function()
				finish(job)
			end)
			job.queueTimer = task.delay(config.queueTimeout or DEFAULT_QUEUE_TIMEOUT, function()
				job.queueTimer = nil
				if active == job then
					return
				end
				local method = string.upper(options and options.Method or "GET")
				if active and active.creating and method == "GET" then
					-- The request ahead is still inside CreateWebStreamClient,
					-- which may never return. This job took no budget slot and
					-- created no client, so it ends like a job whose budget ran
					-- out and the caller sends it another way instead of failing.
					-- Only reads do this: a write sent another way could reach the
					-- server before the stuck one ahead of it.
					finish(job, StreamRequest.BudgetExhausted)
				else
					finish(job, nil, timeoutError("Waited too long for an HTTP stream slot."))
				end
			end)
			-- Each transport runs one request at a time, and every client it
			-- creates also takes a slot from the plugin's stream budget.
			table.insert(queue, job)
			-- Cancelling a promise kills its executor thread. Starting the job on
			-- its own thread keeps a yielding client creation running, so the
			-- client is still closed when the request was cancelled meanwhile.
			task.spawn(pump)
		end)
	end
end

return StreamRequest
