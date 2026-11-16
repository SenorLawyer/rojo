--[[
	Counts the HTTP requests Rojo has in flight in Studio: the stream clients
	it holds from HttpService:CreateWebStreamClient and its RequestAsync calls.

	Studio gives plugins five parallel HTTP requests between them, stream
	clients included, and at most four stream clients. Past those limits
	requests fail with "Number of requests exceeded limit". Rojo therefore
	stays under them on its own: everything that sends a request takes a slot
	from here first and does not send when no slot is free.

	- Streams: at most three. The sync WebSocket is what keeps Studio in
	  sync, so one is held back for it. The agent bridge long-polls the
	  server, so it has one of its own as well: RequestAsync slows down for
	  the rest of a plugin's life once it is used for requests the server
	  holds open, which made the bridge fall further behind until its session
	  expired. Other HTTP requests share the third.
	- In total: at most four, so one of Studio's five stays free for other
	  plugins and the command bar. RequestAsync, which takes no stream client
	  but does take one of the five, may use what the streams leave of the
	  four, except a slot for the shared request stream: RequestAsync cannot
	  be cancelled, so stalled probes could otherwise keep sync's own
	  requests waiting.

	Nothing may take a slot held back for the socket or the bridge while that
	one is not using it.
]]

local StreamBudget = {}
StreamBudget.__index = StreamBudget

StreamBudget.Priority = {
	Socket = "Socket",
	Bridge = "Bridge",
	Request = "Request",
	-- A RequestAsync call, which takes no stream client.
	RequestAsync = "RequestAsync",
}

-- Studio allows four stream clients per plugin; Rojo uses at most three.
StreamBudget.DEFAULT_LIMIT = 3
StreamBudget.DEFAULT_SOCKET_RESERVE = 1
StreamBudget.DEFAULT_BRIDGE_RESERVE = 1
-- Studio allows plugins five parallel requests; Rojo uses at most four.
StreamBudget.DEFAULT_TOTAL_LIMIT = 4

function StreamBudget.new(limit: number?, socketReserve: number?, bridgeReserve: number?, totalLimit: number?)
	limit = limit or StreamBudget.DEFAULT_LIMIT
	return setmetatable({
		limit = limit,
		socketReserve = socketReserve or StreamBudget.DEFAULT_SOCKET_RESERVE,
		bridgeReserve = bridgeReserve or StreamBudget.DEFAULT_BRIDGE_RESERVE,
		-- A budget made with only a stream limit, as tests do, gets one more
		-- slot for RequestAsync, as the shared one does.
		totalLimit = totalLimit or (limit + 1),
		live = 0,
		liveSockets = 0,
		liveBridges = 0,
		liveRequests = 0,
		liveRequestAsync = 0,
		waiters = {},
	}, StreamBudget)
end

--[[
	Takes a slot for a request that is about to be sent. Returns a function
	that gives the slot back, or nil when no slot is free. Call the function
	once the request has finished, or once sending it failed; calling it
	again does nothing, so every way a request can end may call it.
]]
function StreamBudget:tryAcquire(priority: string): (() -> ())?
	local isSocket = priority == StreamBudget.Priority.Socket
	local isBridge = priority == StreamBudget.Priority.Bridge
	local isRequestAsync = priority == StreamBudget.Priority.RequestAsync
	local isRequest = priority == StreamBudget.Priority.Request
	-- A request may not use slots held back for the other kinds that they
	-- are not using yet.
	local heldBack = 0
	if not isSocket then
		heldBack += math.max(0, self.socketReserve - self.liveSockets)
	end
	if not isBridge then
		heldBack += math.max(0, self.bridgeReserve - self.liveBridges)
	end
	if self.live + self.liveRequestAsync + heldBack >= self.totalLimit then
		return nil
	end
	if isRequestAsync and self.liveRequests == 0 then
		if self.live + self.liveRequestAsync + heldBack + 1 >= self.totalLimit then
			return nil
		end
	end
	if not isRequestAsync and self.live + heldBack >= self.limit then
		return nil
	end

	if isRequestAsync then
		self.liveRequestAsync += 1
	else
		self.live += 1
	end
	if isSocket then
		self.liveSockets += 1
	elseif isBridge then
		self.liveBridges += 1
	elseif isRequest then
		self.liveRequests += 1
	end
	local released = false
	return function()
		if released then
			return
		end
		released = true
		if isRequestAsync then
			self.liveRequestAsync -= 1
		else
			self.live -= 1
		end
		if isSocket then
			self.liveSockets -= 1
		elseif isBridge then
			self.liveBridges -= 1
		elseif isRequest then
			self.liveRequests -= 1
		end
		-- Waiters try again on their own threads, oldest first.
		local waiters = self.waiters
		self.waiters = {}
		for _, wake in waiters do
			task.spawn(wake)
		end
	end
end

--[[
	Like tryAcquire, but yields until a slot is free. Returns nil when
	`timeout` seconds pass first, or once `isCancelled` returns true, which
	is checked whenever a slot is released and about once a second.
]]
function StreamBudget:acquire(priority: string, timeout: number, isCancelled: (() -> boolean)?): (() -> ())?
	local deadline = os.clock() + timeout
	while true do
		if isCancelled and isCancelled() then
			return nil
		end
		local release = self:tryAcquire(priority)
		if release then
			return release
		end
		local remaining = deadline - os.clock()
		if remaining <= 0 then
			return nil
		end
		local thread = coroutine.running()
		local woken = false
		local function wake()
			if not woken then
				woken = true
				task.spawn(thread)
			end
		end
		table.insert(self.waiters, wake)
		local timer = task.delay(math.min(remaining, 1), wake)
		coroutine.yield()
		if coroutine.status(timer) == "suspended" then
			task.cancel(timer)
		end
		local index = table.find(self.waiters, wake)
		if index then
			table.remove(self.waiters, index)
		end
	end
end

-- Stream clients in flight.
function StreamBudget:liveCount(): number
	return self.live
end

-- RequestAsync calls in flight.
function StreamBudget:liveRequestAsyncCount(): number
	return self.liveRequestAsync
end

-- Shared by every request Rojo's plugin sends in Studio.
StreamBudget.shared = StreamBudget.new(
	StreamBudget.DEFAULT_LIMIT,
	StreamBudget.DEFAULT_SOCKET_RESERVE,
	StreamBudget.DEFAULT_BRIDGE_RESERVE,
	StreamBudget.DEFAULT_TOTAL_LIMIT
)

return StreamBudget
