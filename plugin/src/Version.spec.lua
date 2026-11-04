return function()
	local Version = require(script.Parent.Version)
	local Settings = require(script.Parent.Settings)

	describe("cached update state", function()
		local originalRetrieve
		local originalCache
		local originalBlocked
		local originalCheck
		beforeEach(function()
			originalRetrieve = Version.retrieveLatestCompatible
			originalCache = Version._cachedLatestCompatible
			originalBlocked = Version._apiBlocked
			originalCheck = Settings:get("checkForUpdates")
			Settings:set("checkForUpdates", true)
			Version.retrieveLatestCompatible = function()
				error("Render must not fetch releases")
			end
		end)
		afterEach(function()
			Version.retrieveLatestCompatible = originalRetrieve
			Version._cachedLatestCompatible = originalCache
			Version._apiBlocked = originalBlocked
			Settings:set("checkForUpdates", originalCheck)
		end)

		it("does not fetch when update or permission state is unknown", function()
			Version._cachedLatestCompatible = nil
			Version._apiBlocked = nil
			expect(Version.getCachedUpdateMessage()).to.equal(nil)
			expect(Version.isApiBlocked()).to.equal(false)
		end)

		it("formats a cached update without fetching again", function()
			Version._cachedLatestCompatible = {
				value = { version = { 99, 0, 0 }, publishedUnixTimestamp = os.time() },
				timestamp = os.clock(),
			}
			local message = Version.getCachedUpdateMessage()
			expect(string.find(message, "99.0.0", 1, true) ~= nil).to.equal(true)
		end)
	end)

	describe("retrieveLatestCompatible cache", function()
		local Packages = script.Parent.Parent.Packages
		local Http = require(Packages.Http)
		local Promise = require(Packages.Promise)
		local originalGet
		local originalCache
		local originalBlocked
		local calls
		beforeEach(function()
			originalGet = Http.get
			originalCache = Version._cachedLatestCompatible
			originalBlocked = Version._apiBlocked
			calls = 0
			Http.get = function()
				calls += 1
				return Promise.resolve({
					code = 200,
					body = Http.jsonEncode({
						{ tag_name = "v7.8.0-rc1", prerelease = true, published_at = "2026-01-02T00:00:00Z" },
						{ tag_name = "v7.7.2-fork.2", prerelease = false, published_at = "2026-01-01T00:00:00Z" },
					}),
				})
			end
		end)
		afterEach(function()
			Http.get = originalGet
			Version._cachedLatestCompatible = originalCache
			Version._apiBlocked = originalBlocked
		end)

		it("does not reuse a cached answer for a different prerelease setting", function()
			Version._cachedLatestCompatible = nil
			local stable = Version.retrieveLatestCompatible({ version = { 7, 7, 2, "-fork.1" } })
			expect(Version.display(stable.version)).to.equal("7.7.2-fork.2")

			local withPrereleases = Version.retrieveLatestCompatible({
				version = { 7, 7, 2, "-fork.1" },
				includePrereleases = true,
			})
			expect(Version.display(withPrereleases.version)).to.equal("7.8.0-rc1")
			expect(calls).to.equal(2)

			-- The same setting reuses the cache.
			Version.retrieveLatestCompatible({ version = { 7, 7, 2, "-fork.1" }, includePrereleases = true })
			expect(calls).to.equal(2)
		end)
	end)

	it("should compare equal versions", function()
		expect(Version.compare({ 1, 2, 3 }, { 1, 2, 3 })).to.equal(0)
		expect(Version.compare({ 1, 2, 3, "rc1" }, { 1, 2, 3, "rc1" })).to.equal(0)
		expect(Version.compare({ 0, 4, 0 }, { 0, 4 })).to.equal(0)
		expect(Version.compare({ 0, 0, 123 }, { 0, 0, 123 })).to.equal(0)
		expect(Version.compare({ 26 }, { 26 })).to.equal(0)
		expect(Version.compare({ 26, 42 }, { 26, 42 })).to.equal(0)
		expect(Version.compare({ 1, 0, 0 }, { 1 })).to.equal(0)
	end)

	it("should compare newer, older versions", function()
		expect(Version.compare({ 1 }, { 0 })).to.equal(1)
		expect(Version.compare({ 1, 1 }, { 1, 0 })).to.equal(1)
		expect(Version.compare({ 1, 2, 3 }, { 1, 2, 0 })).to.equal(1)
	end)

	it("should compare different major versions", function()
		expect(Version.compare({ 1, 3, 2 }, { 2, 2, 1 })).to.equal(-1)
		expect(Version.compare({ 1, 2 }, { 2, 1 })).to.equal(-1)
		expect(Version.compare({ 1 }, { 2 })).to.equal(-1)
	end)

	it("should compare different minor versions", function()
		expect(Version.compare({ 1, 2, 3 }, { 1, 3, 2 })).to.equal(-1)
		expect(Version.compare({ 50, 1 }, { 50, 2 })).to.equal(-1)
	end)

	it("should compare different patch versions", function()
		expect(Version.compare({ 1, 1, 3 }, { 1, 1, 2 })).to.equal(1)
		expect(Version.compare({ 1, 1, 2 }, { 1, 1, 3 })).to.equal(-1)
		expect(Version.compare({ 1, 1, 3, "-rc1" }, { 1, 1, 2, "-rc2" })).to.equal(1)
		expect(Version.compare({ 1, 1, 2, "-rc5" }, { 1, 1, 3, "-alpha" })).to.equal(-1)
	end)

	it("should compare prerelease tags", function()
		expect(Version.compare({ 1, 0, 0, "-alpha" }, { 1, 0, 0 })).to.equal(-1)
		expect(Version.compare({ 1, 0, 0 }, { 1, 0, 0, "-alpha" })).to.equal(1)
		expect(Version.compare({ 1, 0, 0, "-rc1" }, { 1, 0, 0, "-rc2" })).to.equal(-1)
		expect(Version.compare({ 1, 0, 0, "-rc2" }, { 1, 0, 0, "-rc1" })).to.equal(1)

		-- Non number prereleases are not compared since that isn't meaningful
		expect(Version.compare({ 1, 0, 0, "-alpha" }, { 1, 0, 0, "-beta" })).to.equal(0)
	end)

	it("should parse version from strings", function()
		local a = Version.parse("v1.0.0")
		expect(a).to.be.ok()
		expect(a[1]).to.equal(1)
		expect(a[2]).to.equal(0)
		expect(a[3]).to.equal(0)
		expect(a[4]).to.equal(nil)

		local b = Version.parse("7.3.1-rc1")
		expect(b).to.be.ok()
		expect(b[1]).to.equal(7)
		expect(b[2]).to.equal(3)
		expect(b[3]).to.equal(1)
		expect(b[4]).to.equal("-rc1")
	end)

	it("should parse fork release versions", function()
		local version = Version.parse("v7.7.2-fork.1")
		expect(version[1]).to.equal(7)
		expect(version[2]).to.equal(7)
		expect(version[3]).to.equal(2)
		expect(version[4]).to.equal("-fork.1")
		expect(Version.display(version)).to.equal("7.7.2-fork.1")
	end)

	it("should treat fork builds as releases, not prereleases", function()
		local fork1 = Version.parse("7.7.2-fork.1")
		expect(Version.isPrerelease(fork1)).to.equal(false)
		expect(Version.isPrerelease(Version.parse("7.7.2"))).to.equal(false)
		expect(Version.isPrerelease(Version.parse("7.7.2-rc1"))).to.equal(true)
		expect(Version.isPrerelease(Version.parse("7.7.2-fork.1-rc1"))).to.equal(true)

		-- A fork build sits on top of the upstream release it is based on.
		expect(Version.compare(fork1, Version.parse("7.7.2"))).to.equal(1)
		expect(Version.compare(Version.parse("7.7.2"), fork1)).to.equal(-1)
		expect(Version.compare(fork1, Version.parse("7.7.2-fork.2"))).to.equal(-1)
		expect(Version.compare(Version.parse("7.7.2-fork.2"), fork1)).to.equal(1)
		expect(Version.compare(fork1, Version.parse("7.7.2-fork.1"))).to.equal(0)
		expect(Version.compare(fork1, Version.parse("7.7.1"))).to.equal(1)
		expect(Version.compare(Version.parse("7.7.1-fork.9"), fork1)).to.equal(-1)
		expect(Version.compare(Version.parse("7.7.3"), fork1)).to.equal(1)
	end)

	it("should compare prerelease tags on fork builds", function()
		local fork1 = Version.parse("7.7.2-fork.1")
		expect(Version.compare(Version.parse("7.7.2-fork.1-rc1"), fork1)).to.equal(-1)
		expect(Version.compare(Version.parse("7.7.2-fork.2-rc1"), fork1)).to.equal(1)
		expect(Version.compare(Version.parse("7.7.2-fork.1-rc2"), Version.parse("7.7.2-fork.1-rc1"))).to.equal(1)
		expect(Version.compare(fork1, Version.parse("7.7.2-rc1"))).to.equal(1)
	end)
end
