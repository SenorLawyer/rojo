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
			.. "\n\nGo to https://github.com/SenorLawyer/rojo for more details."
		):format(
			Version.display(Config.version),
			Config.protocolVersion,
			Config.expectedServerVersionString,
			infoResponseBody.serverVersion,
			infoResponseBody.protocolVersion
		)

		return Promise.reject(message)
	end

	return Promise.resolve(infoResponseBody)
end

local function rejectWrongPlaceId(infoResponseBody)
	if infoResponseBody.expectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.expectedPlaceIds, game.PlaceId)

		if not foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.expectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to only be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'servePlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
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

local ApiContext = {}
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
		__handshakeTimeout = 15,
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
	}?
)
	if not self.__connected then
		return Promise.reject("Cannot send a request after disconnecting")
	end

	local request = Http[method](url, body)
	if options then
		request = Http.timeout(request, url, options.timeout, options.timeoutMessage, options.onWaitingForPermission, {
			permissionRejectionValue = options.permissionTimeoutMessage,
			limit = options.limit,
		})
	end
	self.__activeRequests[request] = true
	return request:finally(function()
		self.__activeRequests[request] = nil
	end)
end

function ApiContext:setMessageCursor(index)
	self.__messageCursor = index
end

function ApiContext:__handshake(onWaitingForPermission: (() -> ())?, limit: number?)
	local url = ("%s/api/rojo"):format(self.__baseUrl)

	return self:__request("get", url, nil, {
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
function ApiContext:probe()
	return self:__handshake(nil, self.__probeTimeout)
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

	return self:__request("post", url, body)
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
		url ..= "?sessionId=" .. HttpService:UrlEncode(self.__sessionId)
	end
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	local request = Promise.new(function(resolve, reject, onCancel)
		local success, wsClient = pcall(self.__createWebSocket, self, url)
		if not success then
			reject("Failed to create WebSocket client: " .. tostring(wsClient))
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
		end

		onCancel(cleanup)
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

	return self:__request("post", url, "")
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
