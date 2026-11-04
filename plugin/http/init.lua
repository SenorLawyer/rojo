local HttpService = game:GetService("HttpService")

local Log = require(script.Parent.Log)
local msgpack = require(script.Parent.msgpack)
local Promise = require(script.Parent.Promise)

local HttpError = require(script.Error)
local HttpResponse = require(script.Response)

local lastRequestId = 0

-- Studio asks the user before a plugin's first request to each host. Until
-- that prompt is answered, a slow request says nothing about the server, so
-- we track answers here, where late results are still observed even if the
-- caller's promise has been cancelled.
local answeredHosts = {}
local answeredAt = {}
local deniedHosts = {}
local answerWaiters = {}

local function hostOf(url: string): string
	local host = string.match(url, "^%a[%w+.-]*://(%[[^%]]*%])") or string.match(url, "^%a[%w+.-]*://([^/:?#]+)") or url
	return string.lower(host)
end

local function isPermissionDeniedError(message): boolean
	return string.find(string.lower(tostring(message)), "permission denied", 1, true) ~= nil
end

local Http = {}

Http.Error = HttpError
Http.Response = HttpResponse

-- Monkey patch msgpack.UInt64.new to lossily convert the low and high bits of the integer
-- to a native Luau number. We should change the upstream decoder to emit a native
-- integer, once those are live.
function msgpack.UInt64.new(mostSignificantPart: number, leastSignificantPart: number): number
	return (mostSignificantPart % 2 ^ 32) * 2 ^ 32 + (leastSignificantPart % 2 ^ 32)
end

local function recordPermissionAnswer(url: string, denied: boolean)
	local host = hostOf(url)
	answeredHosts[host] = true
	answeredAt[host] = os.clock()
	deniedHosts[host] = if denied then true else nil

	local waiters = answerWaiters[host]
	answerWaiters[host] = nil
	if waiters then
		for resolve in waiters do
			resolve()
		end
	end
end

local function performRequest(requestParams)
	local requestId = lastRequestId + 1
	lastRequestId = requestId

	Log.trace("HTTP {}({}) {}", requestParams.Method, requestId, requestParams.Url)

	if requestParams.Body ~= nil then
		Log.trace("{}", requestParams.Body)
	end

	return Promise.new(function(resolve, reject)
		coroutine.wrap(function()
			local success, response = pcall(function()
				return HttpService:RequestAsync(requestParams)
			end)
			recordPermissionAnswer(requestParams.Url, not success and isPermissionDeniedError(response))

			if success then
				Log.trace("Request {} success, response {:#?}", requestId, response)
				local httpResponse = HttpResponse.fromRobloxResponse(response)
				if httpResponse:isSuccess() then
					resolve(httpResponse)
				else
					reject(HttpError.fromResponse(httpResponse))
				end
			else
				Log.trace("Request {} failure: {:?}", requestId, response)
				reject(HttpError.fromRobloxErrorString(response))
			end
		end)()
	end)
end

function Http.get(url)
	return performRequest({
		Url = url,
		Method = "GET",
	})
end

function Http.post(url, body)
	return performRequest({
		Url = url,
		Method = "POST",
		Body = body,
	})
end

-- Whether Studio may still be showing its HTTP permission prompt for this URL's host.
function Http.isPermissionPending(url: string): boolean
	return answeredHosts[hostOf(url)] == nil
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

--[[
	Bounds a request without counting time the user spends on Studio's HTTP
	permission prompt, which only blocks the first request to a host each
	session. If the prompt may still be open when the deadline passes,
	onWaitingForPermission is called and the deadline restarts once the prompt
	is answered. Cancelling the returned promise cancels the request.
]]
function Http.timeout(request, url: string, seconds: number, rejectionValue, onWaitingForPermission: (() -> ())?)
	local host = hostOf(url)
	local startedAt = os.clock()
	local notified = false

	local function checkDeadline()
		if Http.isPermissionPending(url) then
			if onWaitingForPermission and not notified then
				notified = true
				onWaitingForPermission()
			end
			return Http.awaitPermissionAnswer(url):andThen(checkDeadline)
		end

		local remaining = math.max(startedAt, answeredAt[host] or startedAt) + seconds - os.clock()
		if remaining > 0 then
			return Promise.delay(remaining):andThen(checkDeadline)
		end
		return Promise.reject(rejectionValue)
	end

	return Promise.race({
		Promise.delay(seconds):andThen(checkDeadline),
		request,
	})
end

-- Lets tests simulate an answered (or denied) permission prompt.
Http._recordPermissionAnswer = recordPermissionAnswer

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
