--[[
	Keeps Rojo's HTTP traffic inside the limits Studio puts on plugins.

	Studio counts every HTTP request a place makes, plugins and the command
	bar together, against a per-minute limit, and gives plugins a handful of
	parallel requests between them. Past either limit, requests fail with
	"Number of requests exceeded limit". A plugin that retries such a failure
	at once spends the budget as fast as it refills, so the place never
	recovers until Studio restarts. Rojo therefore:

	- notes every throttled failure and sends nothing for a cooldown that
	  doubles while the throttling goes on, up to a minute (`Throttle`), and
	- paces the Studio bridge, whose traffic agents drive, so a burst of
	  agent commands cannot use up the per-minute limit in the first place
	  (`Throttle.Pacer`).
]]

local Throttle = {}
Throttle.__index = Throttle

Throttle.MIN_COOLDOWN = 5
Throttle.MAX_COOLDOWN = 60

-- Studio's wording when it refuses to send a request because of its HTTP
-- limits.
local THROTTLE_PATTERNS = {
	"number of requests exceeded limit",
	"too many requests",
}

function Throttle.isThrottleError(err): boolean
	if type(err) == "table" and err.throttled then
		return true
	end
	local message = string.lower(tostring(err))
	for _, pattern in THROTTLE_PATTERNS do
		if string.find(message, pattern, 1, true) then
			return true
		end
	end
	return false
end

-- `clock` is replaced by tests.
function Throttle.new(clock: (() -> number)?)
	return setmetatable({
		clock = clock or os.clock,
		cooldownUntil = 0,
		cooldownStarted = -math.huge,
		cooldown = 0,
	}, Throttle)
end

-- Seconds left before Rojo may send again.
function Throttle:remaining(): number
	return math.max(0, self.cooldownUntil - self.clock())
end

--[[
	Records a failure. Returns true when it was Studio throttling HTTP, and
	as a second value whether that started a new cooldown. A throttled
	failure after the cooldown ended starts a new one, twice as long as the
	last unless a request got through since. Failures of requests that were
	already in flight when a cooldown started do not lengthen it, since they
	say nothing new.
]]
function Throttle:noteFailure(err): (boolean, boolean)
	if not Throttle.isThrottleError(err) then
		return false, false
	end
	if self:remaining() > 0 then
		return true, false
	end
	if self.cooldown == 0 then
		self.cooldown = Throttle.MIN_COOLDOWN
	else
		self.cooldown = math.min(self.cooldown * 2, Throttle.MAX_COOLDOWN)
	end
	self.cooldownStarted = self.clock()
	self.cooldownUntil = self.cooldownStarted + self.cooldown
	return true, true
end

--[[
	A request got through, so the next throttling starts from the shortest
	cooldown again. `sentAt`, a reading of this throttle's clock taken when
	the request was sent, lets a request sent before the last cooldown
	started be ignored, since its success says nothing about the throttling
	ending. Neither does one that finishes during a cooldown.
]]
function Throttle:noteSuccess(sentAt: number?)
	if self:remaining() > 0 then
		return
	end
	if sentAt ~= nil and sentAt < self.cooldownStarted then
		return
	end
	self.cooldown = 0
end

--[[
	Yields until the cooldown is over. `isCancelled`, checked about once a
	second, ends the wait early; returns false when it did.
]]
function Throttle:wait(isCancelled: (() -> boolean)?): boolean
	while true do
		if isCancelled and isCancelled() then
			return false
		end
		local remaining = self:remaining()
		if remaining <= 0 then
			return true
		end
		task.wait(math.min(remaining, 1))
	end
end

--[[
	A token bucket: `burst` requests may go at once, then one every
	1 / `perSecond` seconds.
]]
local Pacer = {}
Pacer.__index = Pacer

function Pacer.new(perSecond: number, burst: number, clock: (() -> number)?)
	clock = clock or os.clock
	return setmetatable({
		perSecond = perSecond,
		burst = burst,
		tokens = burst,
		updated = clock(),
		clock = clock,
	}, Pacer)
end

function Pacer:__refill()
	local now = self.clock()
	self.tokens = math.min(self.burst, self.tokens + (now - self.updated) * self.perSecond)
	self.updated = now
end

-- Takes a token if one is free. Returns how long to wait otherwise.
function Pacer:tryTake(): (boolean, number)
	self:__refill()
	if self.tokens >= 1 then
		self.tokens -= 1
		return true, 0
	end
	return false, (1 - self.tokens) / self.perSecond
end

-- Yields until a token is free and takes it. Returns false, without taking
-- one, when `isCancelled` returns true first.
function Pacer:take(isCancelled: (() -> boolean)?): boolean
	while true do
		if isCancelled and isCancelled() then
			return false
		end
		local taken, wait = self:tryTake()
		if taken then
			return true
		end
		task.wait(wait)
	end
end

Throttle.Pacer = Pacer

-- Shared by all of Rojo's HTTP traffic in this plugin.
Throttle.shared = Throttle.new()

return Throttle
