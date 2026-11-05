local Packages = script.Parent.Parent.Packages
local HttpService = game:GetService("HttpService")
local Http = require(Packages.Http)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)
local Types = require(script.Parent.Types)
local Version = require(script.Parent.Version)
local formatWebSocketError = require(script.Parent.formatWebSocketError)

local validateApiInfo = Types.ifEnabled(Types.ApiInfoResponse)
local validateApiRead = Types.ifEnabled(Types.ApiReadResponse)
local validateApiSocketPacket = Types.ifEnabled(Types.ApiSocketPacket)
local validateApiSerialize = Types.ifEnabled(Types.ApiSerializeResponse)
local validateApiRefPatch = Types.ifEnabled(Types.ApiRefPatchResponse)

-- Declared before the functions below so their messages can use these
-- helpers, which are kept on the module so specs can check the wording.
local ApiContext = {}

-- Says which side to update, so the user does not have to work out from two
-- protocol numbers whether the plugin or the server is behind.
function ApiContext.protocolFixHint(serverProtocol)
	if type(serverProtocol) == "number" and serverProtocol > Config.protocolVersion then
		return "The server is newer than this plugin. Run `rojo plugin install` with the same Rojo that runs"
			.. " `rojo serve`, then restart Studio."
	end
	return "The server is older than this plugin. Update Rojo in the project's rokit.toml (then `rokit install`),"
		.. " or install the plugin that matches the server with its `rojo plugin install`."
end

function ApiContext.unlistedPlaceMessage(placeId, expectedPlaceIds)
	local idList = {}
	local entries = {}
	for _, id in ipairs(expectedPlaceIds) do
		table.insert(idList, "- " .. tostring(id))
		table.insert(entries, tostring(id))
	end
	local hint
	if placeId == 0 then
		-- An unpublished place has no ID to add, which is easy to miss when
		-- the error only lists numbers.
		hint = "\n\nThis place is not published (its place ID is 0), so it cannot be listed."
			.. " Open the published place instead, or publish this one first."
	else
		table.insert(entries, tostring(placeId))
		hint = ("\n\nTo sync this place too, add it to 'servePlaceIds' in your .project.json file:\n\"servePlaceIds\": [%s]"):format(
			table.concat(entries, ", ")
		) .. "\nthen restart `rojo serve`. Moving a game to a group or copying a place gives it a new place ID."
	end
	return (
		"Found a Rojo server, but its project is set to only be used with a specific list of places."
		.. "\nYour place ID is %u, but needs to be one of these:"
		.. "\n%s"
		.. "%s"
	):format(placeId, table.concat(idList, "\n"), hint)
end

local function rejectFailedRequests(response)
	if response.code >= 400 then
		local message = string.format("HTTP %s:\n%s", tostring(response.code), response.body)

		return Promise.reject(message)
	end

	return response
end

local function rejectWrongProtocolVersion(infoResponseBody)
	if type(infoResponseBody) ~= "table" or type(infoResponseBody.protocolVersion) ~= "number" then
		return Promise.reject(
			"Invalid response from the Rojo server: expected server info with a numeric protocolVersion."
		)
	end

	if infoResponseBody.protocolVersion ~= Config.protocolVersion then
		local message = (
			"Found a Rojo dev server, but it's using a different protocol version, and is incompatible."
			.. "\nMake sure you have matching versions of both the Rojo plugin and server!"
			.. "\n\nYour client is version %s, with protocol version %s. It expects server version %s."
			.. "\nYour server is version %s, with protocol version %s."
			.. "\n\n%s"
			.. "\n\nGo to https://github.com/SenorLawyer/rojo for more details."
		):format(
			Version.display(Config.version),
			Config.protocolVersion,
			Config.expectedServerVersionString,
			infoResponseBody.serverVersion,
			infoResponseBody.protocolVersion,
			ApiContext.protocolFixHint(infoResponseBody.protocolVersion)
		)

		return Promise.reject(message)
	end

	return Promise.resolve(infoResponseBody)
end

local function rejectWrongPlaceId(infoResponseBody)
	if infoResponseBody.expectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.expectedPlaceIds, game.PlaceId)

		if not foundId then
			return Promise.reject(ApiContext.unlistedPlaceMessage(game.PlaceId, infoResponseBody.expectedPlaceIds))
		end
	end

	if infoResponseBody.unexpectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.unexpectedPlaceIds, game.PlaceId)

		if foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.unexpectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to not be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to not be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'blockedPlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	return Promise.resolve(infoResponseBody)
end

ApiContext.__index = ApiContext

function ApiContext.new(baseUrl)
	assert(type(baseUrl) == "string", "baseUrl must be a string")

	local self = {
		__baseUrl = baseUrl,
		__sessionId = nil,
		__messageCursor = -1,
		__wsClient = nil,
		__connected = true,
		__activeRequests = {},
		-- The server may hold any /api request for up to 15 seconds while Git
		-- holds its locks (GIT_WAIT_LIMIT in src/web/studio.rs), so a 15 second
		-- deadline here could expire on a healthy server. 30 leaves room for
		-- that wait and the request itself.
		__handshakeTimeout = 30,
		__requestTimings = {},
		-- Replaced by tests so their sockets do not count against Studio's.
		__streamBudget = Http.StreamBudget.shared,
		-- Background probes run without the user watching, so they get a hard
		-- bound that ignores Studio's permission prompt entirely.
		__probeTimeout = 30,
		-- The initial read returns the whole project and may queue behind a
		-- full resync on the server, so it gets much longer than other requests.
		__initialReadTimeout = 300,
		-- Fallback serialization can be large, so it gets far more time than
		-- the handshake. It still needs a bound because it blocks the in-order
		-- message queue while outstanding.
		__transferTimeout = 60,
	}

	return setmetatable(self, ApiContext)
end

function ApiContext:__fmtDebug(output)
	output:writeLine("ApiContext {{")
	output:indent()

	output:writeLine("Connected: {}", self.__connected)
	output:writeLine("Base URL: {}", self.__baseUrl)
	output:writeLine("Session ID: {}", self.__sessionId)
	output:writeLine("Message Cursor: {}", self.__messageCursor)

	output:unindent()
	output:write("}")
end

function ApiContext:disconnect()
	self.__connected = false
	local activeRequests = self.__activeRequests
	self.__activeRequests = {}
	for request in activeRequests do
		Log.trace("Cancelling request {}", request)
		request:cancel()
	end

	if self.__wsClient then
		Log.trace("Closing WebSocket client")
		pcall(self.__wsClient.Close, self.__wsClient)
	end
	self.__wsClient = nil
end

function ApiContext:__request(
	method,
	url,
	body,
	options: {
		timeout: number,
		timeoutMessage: string,
		onWaitingForPermission: (() -> ())?,
		permissionTimeoutMessage: string?,
		limit: number?,
		requestAsync: boolean?,
	}?
)
	if not self.__connected then
		return Promise.reject("Cannot send a request after disconnecting")
	end

	local started = os.clock()
	local requestOptions
	local sent
	if options then
		local markSent
		if Http.usesEncodedStreams(url) and not options.requestAsync then
			-- Stream requests share one sync slot, so a write can wait behind
			-- a slow serialize. The timeout starts once the request is sent, so
			-- that wait is not blamed on the server; the transport bounds the
			-- wait itself. RequestAsync is never queued, so it starts at once.
			sent = Promise.new(function(resolve)
				markSent = resolve
			end)
		end
		-- The transport needs the deadline too, or its own shorter limits would
		-- end slow requests such as the initial read before this timeout does.
		requestOptions = {
			timeout = options.timeout,
			onStarted = markSent,
			requestAsync = options.requestAsync,
		}
	end
	local request = if method == "get" then Http.get(url, requestOptions) else Http.post(url, body, requestOptions)
	if options then
		request = Http.timeout(request, url, options.timeout, options.timeoutMessage, options.onWaitingForPermission, {
			permissionRejectionValue = options.permissionTimeoutMessage,
			limit = options.limit,
			started = sent,
		})
	end
	self.__activeRequests[request] = true
	return request:finally(function()
		self.__activeRequests[request] = nil
		table.insert(self.__requestTimings, {
			method = method,
			seconds = os.clock() - started,
		})
		if #self.__requestTimings > 10 then
			table.remove(self.__requestTimings, 1)
		end
	end)
end

function ApiContext:setMessageCursor(index)
	self.__messageCursor = index
end

function ApiContext:__handshake(onWaitingForPermission: (() -> ())?, limit: number?, requestAsync: boolean?)
	local url = ("%s/api/rojo"):format(self.__baseUrl)

	return self:__request("get", url, nil, {
		requestAsync = requestAsync,
		timeout = self.__handshakeTimeout,
		timeoutMessage = "Timed out connecting to the Rojo server. Check `rojo serve` and retry.",
		onWaitingForPermission = onWaitingForPermission,
		permissionTimeoutMessage = "Timed out connecting to the Rojo server."
			.. "\nIf Studio asked whether Rojo may access this address, choose Allow and retry."
			.. "\nOtherwise the server may be busy or stalled. Check `rojo serve` and retry.",
		limit = limit,
	})
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(rejectWrongProtocolVersion)
		:andThen(function(body)
			assert(validateApiInfo(body))

			return body
		end)
		:andThen(rejectWrongPlaceId)
		:andThen(function(body)
			self.__sessionId = body.sessionId

			return body
		end)
end

--[[
	onWaitingForPermission is called if the handshake is slow while Studio may
	still be asking the user whether Rojo can access the server's host.
]]
function ApiContext:connect(onWaitingForPermission: (() -> ())?)
	return self:__handshake(onWaitingForPermission)
end

-- Checks for a server in the background. Unlike connect, this never waits on
-- a possible permission prompt past a fixed bound, so callers can't hang.
-- Probes use RequestAsync so a stalled probe never holds the sync stream slot
-- that Connect's handshake would queue behind, and they take no stream client
-- from Studio's per-plugin budget.
function ApiContext:probe()
	return self:__handshake(nil, self.__probeTimeout, true)
end

function ApiContext:read(ids)
	local url = ("%s/api/read/%s"):format(self.__baseUrl, table.concat(ids, ","))

	return self:__request("get", url, nil, {
		timeout = self.__initialReadTimeout,
		timeoutMessage = "Timed out loading the project from the Rojo server. Check `rojo serve` and reconnect.",
	})
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(body)
			if body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiRead(body))

			return body
		end)
end

function ApiContext:write(patch)
	local url = ("%s/api/write"):format(self.__baseUrl)

	local updated = {}
	for _, update in ipairs(patch.updated) do
		local fixedUpdate = {
			id = update.id,
			changedName = update.changedName,
		}

		if next(update.changedProperties) ~= nil then
			fixedUpdate.changedProperties = update.changedProperties
		end

		table.insert(updated, fixedUpdate)
	end

	-- Only add the 'added' field if the table is non-empty, or else the msgpack
	-- encode implementation will turn the table into an array instead of a map,
	-- causing API validation to fail.
	local added
	if next(patch.added) ~= nil then
		added = patch.added
	end

	local body = {
		sessionId = self.__sessionId,
		removed = patch.removed,
		updated = updated,
		added = added,
	}

	body = Http.msgpackEncode(body)

	-- Without a bound, a stalled server keeps this request tracked until the
	-- session disconnects. Writes can carry whole scripts, so they get the
	-- transfer timeout rather than the handshake's.
	return self:__request("post", url, body, {
		timeout = self.__transferTimeout,
		timeoutMessage = "Timed out sending changes to the Rojo server. Check `rojo serve` and reconnect.",
	})
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(responseBody)
			Log.info("Write response: {:?}", responseBody)

			return responseBody
		end)
end

function ApiContext:connectWebSocket(packetHandlers, onOpened)
	if not self.__connected then
		return Promise.reject("Cannot open a WebSocket after disconnecting")
	end

	local url = ("%s/api/socket/%s"):format(self.__baseUrl, self.__messageCursor)
	-- Lets the server reject a socket from a session that predates a restart,
	-- whose message cursor would mean nothing. Servers that don't check it
	-- route on the path alone and ignore the query.
	if self.__sessionId ~= nil then
		-- Session IDs are UUIDs, which are URL-safe. UrlEncode would escape "-".
		url ..= "?sessionId=" .. self.__sessionId
	end
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	local streamBudget = self.__streamBudget
	local request = Promise.new(function(resolve, reject, onCancel)
		-- Studio caps how many stream clients a plugin can have open, and what
		-- happens past that cap is undocumented, so the socket takes a slot from
		-- the plugin's budget first to stay under it. One slot is reserved for
		-- it, so this only fails when another Rojo socket is still open.
		local releaseSlot = streamBudget:tryAcquire(Http.StreamBudget.Priority.Socket)
		if not releaseSlot then
			reject("Rojo already has its sync connection open in Studio. Disconnect the other session and reconnect.")
			return
		end
		local cancelled = false
		local cleanupSocket
		onCancel(function()
			cancelled = true
			if cleanupSocket then
				cleanupSocket()
			end
		end)
		-- Cancelling a promise closes its executor thread. Creating the client
		-- on a thread of its own means a client that arrives after the request
		-- was cancelled is still closed and gives its slot back.
		task.spawn(function()
			local success, wsClient = pcall(self.__createWebSocket, self, url)
			if not success then
				releaseSlot()
				reject("Failed to create WebSocket client: " .. tostring(wsClient))
				return
			end
			if cancelled then
				pcall(wsClient.Close, wsClient)
				releaseSlot()
				return
			end
			self.__wsClient = wsClient

			local connections = {}
			local pendingMessages = {}
			local processingMessages = false
			local openingTimeout
			local opened = false
			local settled = false
			local function cleanup()
				if settled then
					return
				end
				settled = true
				if openingTimeout then
					task.cancel(openingTimeout)
					openingTimeout = nil
				end
				table.clear(pendingMessages)
				for _, connection in connections do
					connection:Disconnect()
				end
				if self.__wsClient == wsClient then
					self.__wsClient = nil
				end
				-- Closing an already-failed socket can throw, and callers reject
				-- after cleanup, so this must not prevent the promise from settling.
				local closed, closeError = pcall(wsClient.Close, wsClient)
				if not closed then
					Log.debug("Ignoring error while closing the Rojo WebSocket: {}", closeError)
				end
				releaseSlot()
			end

			cleanupSocket = cleanup
			if not self.__connected then
				cleanup()
				resolve()
				return
			end

			local function markOpened()
				if settled or opened then
					return
				end
				opened = true
				if openingTimeout then
					task.cancel(openingTimeout)
					openingTimeout = nil
				end
				if onOpened then
					local ok, err = pcall(onOpened)
					if not ok then
						cleanup()
						reject(err)
					end
				end
			end
			table.insert(connections, wsClient.Opened:Connect(markOpened))
			openingTimeout = task.delay(self.__handshakeTimeout, function()
				openingTimeout = nil
				cleanup()
				reject("Timed out opening the Rojo sync connection. Check `rojo serve` and reconnect.")
			end)

			local function handleMessage(msg)
				local data = Http.msgpackDecode(msg)
				if data.sessionId ~= self.__sessionId then
					error("Server changed ID; reconnect to sync with the restarted server")
				end

				assert(validateApiSocketPacket(data))

				Log.trace("Received websocket packet: {:#?}", data)

				local handler = packetHandlers[data.packetType]
				if handler then
					handler(data.body)
				else
					Log.warn("No handler for WebSocket packet type '%s'", data.packetType)
				end
			end

			table.insert(
				connections,
				wsClient.MessageReceived:Connect(function(msg)
					table.insert(pendingMessages, msg)
					if processingMessages then
						return
					end
					-- Applying a patch can yield for fallback requests. Preserve wire order
					-- rather than applying a later patch while the previous one is suspended.
					processingMessages = true
					while not settled and #pendingMessages > 0 do
						local trace
						local ok, err = xpcall(handleMessage, function(message)
							trace = debug.traceback(tostring(message), 2)
							return message
						end, table.remove(pendingMessages, 1))
						if not ok then
							-- Applying later messages on top of a partially applied one
							-- would leave Studio silently out of sync, so the session
							-- ends and the user reconnects to resync from scratch.
							Log.warn("Stopped syncing because a Rojo server message could not be applied:\n{}", trace)
							cleanup()
							reject("Failed to process a Rojo server message: " .. tostring(err))
						end
					end
					processingMessages = false
				end)
			)

			table.insert(
				connections,
				wsClient.Closed:Connect(function(_code, reason)
					cleanup()
					if self.__connected then
						local detail = if type(reason) == "string" and #reason > 0 then " " .. reason else ""
						reject("Connection to the Rojo server closed. Reconnect to resync." .. detail)
					else
						resolve()
					end
				end)
			)

			table.insert(
				connections,
				wsClient.Error:Connect(function(code, msg)
					cleanup()
					reject(formatWebSocketError(code, msg))
				end)
			)
			if wsClient.ConnectionState == Enum.WebStreamClientState.Open then
				markOpened()
			end
		end)
	end)
	self.__activeRequests[request] = true
	return request:finally(function()
		self.__activeRequests[request] = nil
	end)
end

function ApiContext:__createWebSocket(url)
	return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, { Url = url })
end

function ApiContext:open(id)
	local url = ("%s/api/open/%s"):format(self.__baseUrl, id)

	-- Opening a file only asks the server to launch an editor, so it should
	-- answer as quickly as the handshake does.
	return self:__request("post", url, "", {
		timeout = self.__handshakeTimeout,
		timeoutMessage = "Timed out asking the Rojo server to open the script. Check `rojo serve` and retry.",
	})
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(body)
			if body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			return nil
		end)
end

function ApiContext:serialize(ids: { string })
	local url = ("%s/api/serialize"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return self:__request("post", url, request_body, {
		timeout = self.__transferTimeout,
		timeoutMessage = "Timed out serializing instances on the Rojo server.",
	})
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiSerialize(response_body))

			return response_body
		end)
end

function ApiContext:refPatch(ids: { string })
	local url = ("%s/api/ref-patch"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return self:__request("post", url, request_body, {
		timeout = self.__transferTimeout,
		timeoutMessage = "Timed out fetching references from the Rojo server.",
	})
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiRefPatch(response_body))

			return response_body
		end)
end

return ApiContext
