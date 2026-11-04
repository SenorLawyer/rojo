--[[
	Counts the stream clients Rojo holds from HttpService:CreateWebStreamClient.

	Studio caps how many stream clients can be open at once, and what happens
	past that cap is not documented. Rojo therefore stays well under the cap
	and leaves the rest for other plugins: everything that
	creates a stream client takes a slot from here first and does not create
	one when no slot is free.

	The sync WebSocket is what keeps Studio in sync, so one slot is held back
	for it. The agent bridge long-polls the server, so it has a slot of its
	own as well: RequestAsync slows down for the rest of a plugin's life once
	it is used for requests the server holds open, which made the bridge
	fall further behind until its session expired. HTTP requests may use
	the remaining slot and fall back to RequestAsync, which takes no stream
	client, when none is free. Neither can take a slot held back for the
	other two.
]]

local StreamBudget = {}
StreamBudget.__index = StreamBudget

StreamBudget.Priority = {
	Socket = "Socket",
	Bridge = "Bridge",
	Request = "Request",
}

-- Studio allows four stream clients per plugin; Rojo uses at most three.
StreamBudget.DEFAULT_LIMIT = 3
StreamBudget.DEFAULT_SOCKET_RESERVE = 1
StreamBudget.DEFAULT_BRIDGE_RESERVE = 1

function StreamBudget.new(limit: number?, socketReserve: number?, bridgeReserve: number?)
	return setmetatable({
		limit = limit or StreamBudget.DEFAULT_LIMIT,
		socketReserve = socketReserve or StreamBudget.DEFAULT_SOCKET_RESERVE,
		bridgeReserve = bridgeReserve or StreamBudget.DEFAULT_BRIDGE_RESERVE,
		live = 0,
		liveSockets = 0,
		liveBridges = 0,
	}, StreamBudget)
end

--[[
	Takes a slot for a client that is about to be created. Returns a function
	that gives the slot back, or nil when no slot is free. Call the function
	once the client has been closed, or once creating it failed; calling it
	again does nothing, so every way a client can end may call it.
]]
function StreamBudget:tryAcquire(priority: string): (() -> ())?
	local isSocket = priority == StreamBudget.Priority.Socket
	local isBridge = priority == StreamBudget.Priority.Bridge
	-- A client may not use slots held back for the other kinds that they are
	-- not using yet.
	local heldBack = 0
	if not isSocket then
		heldBack += math.max(0, self.socketReserve - self.liveSockets)
	end
	if not isBridge then
		heldBack += math.max(0, self.bridgeReserve - self.liveBridges)
	end
	if self.live + heldBack >= self.limit then
		return nil
	end

	self.live += 1
	if isSocket then
		self.liveSockets += 1
	elseif isBridge then
		self.liveBridges += 1
	end
	local released = false
	return function()
		if released then
			return
		end
		released = true
		self.live -= 1
		if isSocket then
			self.liveSockets -= 1
		elseif isBridge then
			self.liveBridges -= 1
		end
	end
end

function StreamBudget:liveCount(): number
	return self.live
end

-- Shared by every stream client Rojo's plugin creates in Studio.
StreamBudget.shared = StreamBudget.new()

return StreamBudget
