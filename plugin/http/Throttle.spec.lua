return function()
	local Throttle = require(script.Parent.Throttle)

	local function fakeClock()
		local clock = { now = 100 }
		return clock, function()
			return clock.now
		end
	end

	describe("isThrottleError", function()
		it("recognizes Studio's HTTP limit errors and nothing else", function()
			expect(Throttle.isThrottleError("Number of requests exceeded limit")).to.equal(true)
			expect(Throttle.isThrottleError("Unknown HTTP error: Number of requests exceeded limit")).to.equal(true)
			expect(Throttle.isThrottleError("HTTP 429 Too Many Requests")).to.equal(true)
			expect(Throttle.isThrottleError({ throttled = true })).to.equal(true)
			expect(Throttle.isThrottleError("HttpError: ConnectFail")).to.equal(false)
			expect(Throttle.isThrottleError("The HTTP response exceeded 8 bytes.")).to.equal(false)
			expect(Throttle.isThrottleError(nil)).to.equal(false)
		end)
	end)

	describe("cooldown", function()
		it("starts at the shortest cooldown and doubles up to the longest while throttling goes on", function()
			local clock, now = fakeClock()
			local throttle = Throttle.new(now)
			expect(throttle:remaining()).to.equal(0)
			expect(throttle:noteFailure("HttpError: Timedout")).to.equal(false)
			expect(throttle:remaining()).to.equal(0)

			expect(throttle:noteFailure("Number of requests exceeded limit")).to.equal(true)
			expect(throttle:remaining()).to.equal(Throttle.MIN_COOLDOWN)
			clock.now += Throttle.MIN_COOLDOWN
			expect(throttle:remaining()).to.equal(0)
			throttle:noteFailure("Number of requests exceeded limit")
			expect(throttle:remaining()).to.equal(Throttle.MIN_COOLDOWN * 2)
			for _ = 1, 10 do
				clock.now += 1000
				throttle:noteFailure("Number of requests exceeded limit")
			end
			expect(throttle:remaining()).to.equal(Throttle.MAX_COOLDOWN)
		end)

		it("starts from the shortest cooldown again after a request gets through", function()
			local clock, now = fakeClock()
			local throttle = Throttle.new(now)
			throttle:noteFailure("Number of requests exceeded limit")
			clock.now += 100
			throttle:noteFailure("Number of requests exceeded limit")
			clock.now += 100
			throttle:noteSuccess()
			throttle:noteFailure("Number of requests exceeded limit")
			expect(throttle:remaining()).to.equal(Throttle.MIN_COOLDOWN)
		end)

		it("lengthens the cooldown once per throttling, not once per request that failed in it", function()
			local clock, now = fakeClock()
			local throttle = Throttle.new(now)
			-- Four requests in flight all fail when Studio starts throttling.
			for _ = 1, 4 do
				throttle:noteFailure("Number of requests exceeded limit")
			end
			expect(throttle:remaining()).to.equal(Throttle.MIN_COOLDOWN)
			-- One sent before the cooldown finishes during it, which says
			-- nothing about the throttling ending.
			throttle:noteSuccess()
			clock.now += Throttle.MIN_COOLDOWN
			throttle:noteFailure("Number of requests exceeded limit")
			expect(throttle:remaining()).to.equal(Throttle.MIN_COOLDOWN * 2)
		end)

		it("ignores a success from a request sent before the cooldown started", function()
			local clock, now = fakeClock()
			local throttle = Throttle.new(now)
			local sentAt = clock.now
			clock.now += 1
			throttle:noteFailure("Number of requests exceeded limit")
			clock.now += Throttle.MIN_COOLDOWN
			-- A long poll sent earlier returns after the cooldown ended.
			throttle:noteSuccess(sentAt)
			throttle:noteFailure("Number of requests exceeded limit")
			expect(throttle:remaining()).to.equal(Throttle.MIN_COOLDOWN * 2)
		end)

		it("never shortens a running cooldown", function()
			local _, now = fakeClock()
			local throttle = Throttle.new(now)
			throttle:noteFailure("Number of requests exceeded limit")
			throttle:noteFailure("Number of requests exceeded limit")
			local remaining = throttle:remaining()
			throttle:noteSuccess()
			throttle:noteFailure("Number of requests exceeded limit")
			expect(throttle:remaining()).to.equal(remaining)
		end)

		it("stops waiting once cancelled", function()
			local throttle = Throttle.new()
			throttle:noteFailure("Number of requests exceeded limit")
			local started = os.clock()
			expect(throttle:wait(function()
				return true
			end)).to.equal(false)
			expect(os.clock() - started < 1).to.equal(true)
		end)
	end)

	describe("Pacer", function()
		it("allows a burst and then one request per interval", function()
			local clock, now = fakeClock()
			local pacer = Throttle.Pacer.new(4, 3, now)
			for _ = 1, 3 do
				expect((pacer:tryTake())).to.equal(true)
			end
			local taken, wait = pacer:tryTake()
			expect(taken).to.equal(false)
			expect(math.abs(wait - 0.25) < 1e-9).to.equal(true)
			clock.now += 0.25
			expect((pacer:tryTake())).to.equal(true)
			expect((pacer:tryTake())).to.equal(false)
			-- Idle time refills the burst, but no further.
			clock.now += 100
			for _ = 1, 3 do
				expect((pacer:tryTake())).to.equal(true)
			end
			expect((pacer:tryTake())).to.equal(false)
		end)

		it("does not take a token once cancelled", function()
			local _, now = fakeClock()
			local pacer = Throttle.Pacer.new(1, 1, now)
			expect(pacer:take(function()
				return true
			end)).to.equal(false)
			expect((pacer:tryTake())).to.equal(true)
		end)
	end)
end
