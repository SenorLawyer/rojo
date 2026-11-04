--[[
	Counts the stream clients Rojo holds from HttpService:CreateWebStreamClient.

	Studio caps how many stream clients can be open at once, and what happens
	past that cap is not documented. Rojo therefore stays well under the cap
	and leaves the rest for other plugins: everything that
	creates a stream client takes a slot from here first and does not create
	one when no slot is free.

	The sync WebSocket is what keeps Studio in sync, so one slot is held back
	for it. HTTP requests may use the other slots and fall back to
	RequestAsync, which takes no stream client, when none is free.
]]

local StreamBudget = {}
StreamBudget.__index = StreamBudget

StreamBudget.Priority = {
	Socket = "Socket",
	Request = "Request",
}

StreamBudget.DEFAULT_LIMIT = 2
StreamBudget.DEFAULT_SOCKET_RESERVE = 1

function StreamBudget.new(limit: number?, socketReserve: number?)
	return setmetatable({
		limit = limit or StreamBudget.DEFAULT_LIMIT,
		socketReserve = socketReserve or StreamBudget.DEFAULT_SOCKET_RESERVE,
		live = 0,
		liveSockets = 0,
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
	if isSocket then
		if self.live >= self.limit then
			return nil
		end
	else
		-- Requests may not use the reserved slots a socket is not using yet.
		local unusedReserve = math.max(0, self.socketReserve - self.liveSockets)
		if self.live + unusedReserve >= self.limit then
			return nil
		end
	end

	self.live += 1
	if isSocket then
		self.liveSockets += 1
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
		end
	end
end

function StreamBudget:liveCount(): number
	return self.live
end

-- Shared by every stream client Rojo's plugin creates in Studio.
StreamBudget.shared = StreamBudget.new()

return StreamBudget
