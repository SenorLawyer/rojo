local HttpService = game:GetService("HttpService")
-- Encoded streams need EncodingService. On a Studio build without it, the
-- plugin still loads and every request uses RequestAsync instead.
local hasEncodingService, EncodingService = pcall(game.GetService, game, "EncodingService")
if not hasEncodingService then
	EncodingService = nil
end

local Log = require(script.Parent.Log)
local msgpack = require(script.Parent.msgpack)
local Promise = require(script.Parent.Promise)

local HttpError = require(script.Error)
local HttpResponse = require(script.Response)
local StreamRequest = require(script.StreamRequest)
local StreamBudget = require(script.StreamBudget)

local lastRequestId = 0

-- Studio asks the user before a plugin's first request to each host. Until
-- that prompt is answered, a slow request says nothing about the server, so
-- we track answers here, where late results are still observed even if the
-- caller's promise has been cancelled.
local answeredHosts = {}
local answeredAt = {}
local deniedHosts = {}
local answerWaiters = {}
-- RequestAsync cannot be cancelled, so a request blocked on an unanswered
-- prompt stays outstanding even after its caller gives up on it.
local outstandingRequests = {}

local function hostOf(url: string): string
	local host = string.match(url, "^%a[%w+.-]*://(%[[^%]]*%])") or string.match(url, "^%a[%w+.-]*://([^/:?#]+)") or url
	return string.lower(host)
end

local function isPermissionDeniedError(message): boolean
	return string.find(string.lower(tostring(message)), "permission denied", 1, true) ~= nil
end

-- Whether a failure shows that Studio decided on the host's HTTP access
-- rather than that the request failed for some other reason.
local function isPermissionAnswerError(err): boolean
	if getmetatable(err) == HttpError and err.type == HttpError.Kind.HttpNotEnabled then
		return true
	end
	local message = string.lower(tostring(err))
	return isPermissionDeniedError(message) or string.find(message, "http requests are not enabled", 1, true) ~= nil
end

local Http = {}

Http.Error = HttpError
Http.Response = HttpResponse
Http.StreamBudget = StreamBudget

-- Monkey patch msgpack.UInt64.new to lossily convert the low and high bits of the integer
-- to a native Luau number. We should change the upstream decoder to emit a native
-- integer, once those are live.
function msgpack.UInt64.new(mostSignificantPart: number, leastSignificantPart: number): number
	return (mostSignificantPart % 2 ^ 32) * 2 ^ 32 + (leastSignificantPart % 2 ^ 32)
end

local function recordPermissionAnswer(url: string, denied: boolean)
	local host = hostOf(url)
	answeredHosts[host] = true
	-- Only the first answer can end a prompt. Later requests finishing must
	-- not keep pushing back the deadline of a request that is still stalled.
	if answeredAt[host] == nil then
		answeredAt[host] = os.clock()
	end
	deniedHosts[host] = if denied then true else nil

	local waiters = answerWaiters[host]
	answerWaiters[host] = nil
	if waiters then
		for resolve in waiters do
			resolve()
		end
	end
end

-- Returns a function that marks the request finished.
local function beginOutstandingRequest(url: string): () -> ()
	local host = hostOf(url)
	outstandingRequests[host] = (outstandingRequests[host] or 0) + 1
	local finished = false
	return function()
		if finished then
			return
		end
		finished = true
		outstandingRequests[host] -= 1
		if outstandingRequests[host] <= 0 then
			outstandingRequests[host] = nil
		end
	end
end

local TRANSFER_HEADER = "x-rojo-content-transfer"

-- Origins whose server answered an encoded request without the transfer
-- header, such as upstream Rojo or an older fork server. Remembered for this
-- plugin load so later requests go straight to RequestAsync.
local plainOrigins = {}

local function originOf(url: string): string
	return string.lower(string.match(url, "^(%a[%w+.-]*://[^/?#]+)") or url)
end

local function isLoopback(url: string): boolean
	return url:match("^https?://localhost[:/]") ~= nil
		or url:match("^https?://127%.0%.0%.1[:/]") ~= nil
		or url:match("^https?://%[::1%][:/]") ~= nil
end

-- Swapped out by tests.
local transport = {
	stream = StreamRequest.new(),
	requestAsync = function(requestParams)
		return HttpService:RequestAsync(requestParams)
	end,
	encodingAvailable = EncodingService ~= nil,
}

if not transport.encodingAvailable then
	Log.warn("This Studio has no EncodingService, so local requests use RequestAsync instead of HTTP streams.")
end

--[[
	How long a local stream request may wait for the sync slot before it
	starts. The caller's own timeout only starts once the request is sent, so
	a write queued behind a slow serialize is not blamed on the server, but a
	backlog still fails instead of waiting forever.
]]
Http.queueWaitLimit = 120

--[[
	The largest decoded response a local stream request may buffer. The
	server caps request bodies at 64 MiB, but the initial read of a large
	project can be bigger than that. Encoded bodies are base64, four bytes for every
	three, so the stream's own cap is sized from this one; otherwise encoding
	would quietly lower the limit by a quarter.
]]
Http.maxDecodedResponseBytes = 128 * 1024 * 1024

local function encodedResponseLimit(): number
	-- The slack covers padding and nothing else, since the server sends the
	-- base64 text without line breaks.
	return math.ceil(Http.maxDecodedResponseBytes * 4 / 3) + 1024
end

local function requestWithRequestAsync(requestParams, requestId, onStarted)
	local url = requestParams.Url
	if onStarted then
		-- RequestAsync is never queued behind other requests.
		task.spawn(onStarted)
	end
	return Promise.new(function(resolve, reject)
		-- Cancelling a promise closes its executor thread, so the request
		-- runs on its own thread. That way a late answer is still recorded,
		-- and the request still counts as outstanding, after the caller
		-- cancels.
		coroutine.wrap(function()
			local finishOutstanding = beginOutstandingRequest(url)
			local ok, response = pcall(transport.requestAsync, requestParams)
			finishOutstanding()
			recordPermissionAnswer(url, not ok and isPermissionDeniedError(response))
			if not ok then
				Log.trace("Request {} failure: {:?}", requestId, response)
				reject(HttpError.fromRobloxErrorString(tostring(response)))
				return
			end
			resolve(response)
		end)()
	end)
end

local function decodeBase64(body: string): (boolean, string)
	-- Checked first because decoding text that is not base64 may not fail.
	if #body % 4 ~= 0 or not string.find(body, "^[%w+/]*=?=?$") then
		return false, "the body is not base64"
	end
	local ok, decoded = pcall(function()
		return buffer.tostring(EncodingService:Base64Decode(buffer.fromstring(body)))
	end)
	if ok and decoded == "" and body ~= "" then
		return false, "the body decoded to nothing"
	end
	return ok, decoded
end

local function requestWithEncodedStream(requestParams, requestOptions)
	local url = requestParams.Url
	local encoded = table.clone(requestParams)
	encoded.Headers = table.clone(requestParams.Headers or {})
	encoded.Headers[TRANSFER_HEADER] = "base64"
	if encoded.Body then
		encoded.Body = buffer.tostring(EncodingService:Base64Encode(buffer.fromstring(encoded.Body)))
	end
	-- The server sends nothing until the whole response is built, so the
	-- stream's inactivity limit must not end a request before the caller's
	-- own timeout does. The caller's timeout starts when the request leaves
	-- the queue, so the queue wait is bounded separately.
	local callerTimeout = requestOptions and requestOptions.timeout
	local callerOnStarted = requestOptions and requestOptions.onStarted
	-- Counted from when the request leaves the queue, since only a request
	-- that is creating its client can be blocked on the permission prompt.
	local finishOutstanding
	-- Opening a stream client is gated by the same per-host HTTP permission
	-- prompt as RequestAsync. Whether creating the client blocks until the
	-- prompt is answered is not documented, so only a response from the
	-- server or a clear permission failure counts as an answer. Other
	-- failures, timeouts and cancelled streams record nothing; the next
	-- request to the host will.
	return transport
		.stream(encoded, {
			timeout = if callerTimeout then math.max(callerTimeout, StreamRequest.DEFAULT_TIMEOUT) else nil,
			queueTimeout = if callerTimeout then Http.queueWaitLimit else nil,
			maxBytes = encodedResponseLimit(),
			onStarted = function()
				finishOutstanding = beginOutstandingRequest(url)
				if callerOnStarted then
					callerOnStarted()
				end
			end,
			onResponse = function()
				recordPermissionAnswer(url, false)
			end,
			holdWhile = function()
				return Http.isPermissionPending(url)
			end,
			-- Read on each request so the stream's hold ends no later than
			-- the permission wait in Http.timeout.
			holdLimit = Http.permissionWaitLimit,
			-- Opening the stream client can keep blocking on the prompt
			-- after the request was cancelled or timed out.
			onReleased = function()
				if finishOutstanding then
					finishOutstanding()
				end
			end,
			-- Records a refused permission even when the request ended while
			-- Studio was still showing the prompt.
			onCreated = function(ok, err)
				if not ok and isPermissionAnswerError(err) then
					recordPermissionAnswer(url, isPermissionDeniedError(err))
				end
			end,
		})
		:andThen(function(response)
			-- Nothing was sent, so this says nothing about the permission.
			if response ~= StreamRequest.BudgetExhausted then
				recordPermissionAnswer(url, false)
			end
			return response
		end, function(err)
			if isPermissionAnswerError(err) then
				recordPermissionAnswer(url, isPermissionDeniedError(err))
			end
			-- Keep HttpError kinds so the UI can show its connection and
			-- HTTP-permission guidance instead of a raw transport message.
			if getmetatable(err) == HttpError then
				return Promise.reject(err)
			end
			return Promise.reject(HttpError.fromRobloxErrorString(tostring(err)))
		end)
end

--[[
	Rojo servers answer failed requests with a MessagePack `{kind, details}`
	body. Shown as is, that body is binary and unreadable, so error messages
	use its `details` text instead. Any other body is left alone.
]]
function Http.readableErrorResponse(response)
	local body = response.body
	if type(body) ~= "string" or body == "" then
		return response
	end
	local contentType = ""
	for name, value in response.headers or {} do
		if string.lower(tostring(name)) == "content-type" then
			contentType = string.lower(tostring(value))
		end
	end
	-- Encoded transfers label every body text/plain, so a small MessagePack map
	-- (first byte 0x80-0x8F) is recognized by its shape as well.
	local firstByte = string.byte(body, 1)
	local looksLikeMap = firstByte >= 0x80 and firstByte <= 0x8F
	if not string.find(contentType, "msgpack", 1, true) and not looksLikeMap then
		return response
	end
	local ok, decoded = pcall(msgpack.decode, body)
	if not ok or type(decoded) ~= "table" or type(decoded.kind) ~= "string" or type(decoded.details) ~= "string" then
		return response
	end
	local readable = table.clone(response)
	readable.body = decoded.details
	return setmetatable(readable, getmetatable(response))
end

local function performRequest(requestParams, requestOptions)
	local requestId = lastRequestId + 1
	lastRequestId = requestId

	Log.trace("HTTP {}({}) {}", requestParams.Method, requestId, requestParams.Url)

	if requestParams.Body ~= nil then
		Log.trace("{}", requestParams.Body)
	end

	local url = requestParams.Url
	local origin = originOf(url)
	local onStarted = requestOptions and requestOptions.onStarted
	local pending
	if Http.usesEncodedStreams(url) and not (requestOptions and requestOptions.requestAsync) then
		pending = requestWithEncodedStream(requestParams, requestOptions):andThen(function(response)
			if response == StreamRequest.BudgetExhausted then
				-- Rojo already holds as many stream clients as it allows itself.
				-- RequestAsync cannot be cancelled, but it takes no stream client,
				-- and callers bound it with their own timeout.
				Log.debug("No HTTP stream is free for request {}; using RequestAsync", requestId)
				return requestWithRequestAsync(requestParams, requestId, onStarted)
			end
			if response.Headers[TRANSFER_HEADER] == "base64" then
				local ok, decoded = decodeBase64(response.Body)
				if not ok then
					return Promise.reject(
						HttpError.new(
							HttpError.Kind.Unknown,
							"Could not decode the Rojo server's encoded response: " .. tostring(decoded)
						)
					)
				end
				response.Body = decoded
			elseif response.HeadersReceived then
				-- A server without encoded streams, such as upstream Rojo.
				-- RawStream cuts bodies at NUL bytes, so this body cannot be
				-- trusted. The server could not have read the encoded request
				-- body either, so sending the request again is safe.
				if not plainOrigins[origin] then
					Log.info("{} does not support encoded HTTP streams; using RequestAsync", origin)
				end
				plainOrigins[origin] = true
				return requestWithRequestAsync(requestParams, requestId)
			elseif response.BodyFromServer then
				-- Studio can report an error status without its headers. The
				-- server encodes every response to an encoded request, but keep
				-- the raw body if it is not base64 after all.
				local ok, decoded = decodeBase64(response.Body)
				if ok then
					response.Body = decoded
				end
			end
			return response
		end, function(err)
			if requestParams.Method == "GET" and getmetatable(err) == HttpError and err.responseTooLarge then
				-- GETs only read, so sending one again is safe, and RequestAsync
				-- does not buffer the body through the stream's byte limit.
				Log.debug("Response to request {} is too large for an HTTP stream; using RequestAsync", requestId)
				return requestWithRequestAsync(requestParams, requestId)
			end
			return Promise.reject(err)
		end)
	else
		-- External update checks and background probes must not occupy the
		-- local sync stream or wait on it, and servers without encoded
		-- streams are read in full this way.
		pending = requestWithRequestAsync(requestParams, requestId, onStarted)
	end
	return pending:andThen(function(response)
		local httpResponse = HttpResponse.fromRobloxResponse(response)
		if httpResponse:isSuccess() then
			return httpResponse
		end
		return Promise.reject(HttpError.fromResponse(Http.readableErrorResponse(httpResponse)))
	end)
end

export type RequestOptions = {
	-- The caller's own deadline, in seconds. Local stream requests are not cut
	-- off by inactivity before it passes, and wait up to Http.queueWaitLimit
	-- for the sync slot.
	timeout: number?,
	-- Called when the request is actually sent, after any wait for the sync
	-- slot. Pass Http.timeout a promise that resolves then.
	onStarted: (() -> ())?,
	-- Sends the request with RequestAsync even to a local server, so it never
	-- waits behind sync traffic or takes the sync slot.
	requestAsync: boolean?,
}

function Http.get(url, options: RequestOptions?)
	return performRequest({
		Url = url,
		Method = "GET",
	}, options)
end

function Http.post(url, body, options: RequestOptions?)
	return performRequest({
		Url = url,
		Method = "POST",
		Body = body,
	}, options)
end

--[[
	Whether Studio may still be showing its HTTP permission prompt for this
	URL's host. This stays true until a request to the host has completed in
	this plugin load, even if access was allowed in an earlier Studio session,
	because Studio offers no way to read the permission state directly.
]]
function Http.isPermissionPending(url: string): boolean
	return answeredHosts[hostOf(url)] == nil
end

-- Whether a request to this URL's host has been started and not yet finished,
-- including requests whose callers have since cancelled them.
function Http.hasOutstandingRequest(url: string): boolean
	return outstandingRequests[hostOf(url)] ~= nil
end

function Http.isPermissionDenied(url: string): boolean
	return deniedHosts[hostOf(url)] == true
end

function Http.awaitPermissionAnswer(url: string)
	local host = hostOf(url)
	if answeredHosts[host] then
		return Promise.resolve()
	end

	return Promise.new(function(resolve, _reject, onCancel)
		local waiters = answerWaiters[host]
		if waiters == nil then
			waiters = {}
			answerWaiters[host] = waiters
		end
		waiters[resolve] = true
		onCancel(function()
			waiters[resolve] = nil
		end)
	end)
end

export type TimeoutOptions = {
	-- How long to wait on a possible permission prompt before giving up.
	-- Defaults to Http.permissionWaitLimit.
	permissionWaitLimit: number?,
	-- Rejection used when the permission wait runs out. Defaults to rejectionValue.
	permissionRejectionValue: any?,
	-- Total time allowed regardless of permission state, for callers such as
	-- background probes that must never wait on a prompt.
	limit: number?,
	-- Resolves when the request is actually sent (see RequestOptions.onStarted).
	-- The deadline starts then, so time spent queued behind other requests on
	-- the sync slot is not counted against the server. `limit` still counts
	-- from the call.
	started: any?,
}

--[[
	Studio usually remembers a plugin's HTTP permission between sessions, so a
	host we haven't heard back from may have no prompt at all, just a stalled
	server. Waiting on a possible prompt is therefore capped.
]]
Http.permissionWaitLimit = 120

--[[
	Bounds a request without counting time the user spends on Studio's HTTP
	permission prompt, which only blocks the first request to a host each
	session. If no request to the host has completed in this Studio session
	when the deadline passes, the prompt may still be open, so
	onWaitingForPermission is called and the request keeps waiting. The
	deadline restarts when a request to the host completes, which is how an
	answered prompt shows up. If none completes within the permission wait
	limit, the request is rejected anyway. That includes a stalled server
	right after Studio starts, even when access was allowed in an earlier
	session. Cancelling the returned promise cancels the
	request.
]]
function Http.timeout(
	request,
	url: string,
	seconds: number,
	rejectionValue,
	onWaitingForPermission: (() -> ())?,
	options: TimeoutOptions?
)
	local host = hostOf(url)
	local startedAt
	local permissionWaitLimit = (options and options.permissionWaitLimit) or Http.permissionWaitLimit
	local permissionRejectionValue = if options and options.permissionRejectionValue ~= nil
		then options.permissionRejectionValue
		else rejectionValue
	local waitingSince = nil

	local function checkDeadline()
		if Http.isPermissionPending(url) then
			if waitingSince == nil then
				waitingSince = os.clock()
				if onWaitingForPermission then
					onWaitingForPermission()
				end
			end

			local waitRemaining = waitingSince + permissionWaitLimit - os.clock()
			if waitRemaining <= 0 then
				return Promise.reject(permissionRejectionValue)
			end
			return Promise.race({
				Http.awaitPermissionAnswer(url),
				Promise.delay(waitRemaining),
			}):andThen(checkDeadline)
		end

		local remaining = math.max(startedAt, answeredAt[host] or startedAt) + seconds - os.clock()
		if remaining > 0 then
			return Promise.delay(remaining):andThen(checkDeadline)
		end
		return Promise.reject(rejectionValue)
	end

	local function startDeadline()
		startedAt = os.clock()
		return Promise.delay(seconds):andThen(checkDeadline)
	end

	local racers = {
		if options and options.started then options.started:andThen(startDeadline) else startDeadline(),
		request,
	}
	if options and options.limit then
		table.insert(
			racers,
			Promise.delay(options.limit):andThen(function()
				return Promise.reject(rejectionValue)
			end)
		)
	end
	return Promise.race(racers)
end

-- Lets tests simulate an answered (or denied) permission prompt.
Http._recordPermissionAnswer = recordPermissionAnswer

-- Lets tests start from a host whose prompt has not been answered, since the
-- loopback hosts are shared by every test.
function Http._forgetPermissionAnswer(url: string)
	local host = hostOf(url)
	answeredHosts[host] = nil
	answeredAt[host] = nil
	deniedHosts[host] = nil
end

-- Lets tests simulate a request that is still outstanding.
Http._beginOutstandingRequest = beginOutstandingRequest

-- Lets tests replace the stream and RequestAsync transports.
Http._transport = transport

-- Lets tests start from an origin that has not been seen yet.
function Http._forgetPlainOrigin(url: string)
	plainOrigins[originOf(url)] = nil
end

-- Whether requests to this URL use encoded HTTP streams rather than
-- RequestAsync. False once its server has answered without encoding.
function Http.usesEncodedStreams(url: string): boolean
	return transport.encodingAvailable and isLoopback(url) and not plainOrigins[originOf(url)]
end

function Http.jsonEncode(object)
	return HttpService:JSONEncode(object)
end

function Http.jsonDecode(source)
	return HttpService:JSONDecode(source)
end

function Http.msgpackEncode(object)
	return msgpack.encode(object)
end

function Http.msgpackDecode(source)
	return msgpack.decode(source)
end

return Http
