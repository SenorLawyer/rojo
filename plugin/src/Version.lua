local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin
local Packages = Rojo.Packages

local Http = require(Packages.Http)
local Promise = require(Packages.Promise)
local Log = require(Packages.Log)

local Config = require(Plugin.Config)
local Settings = require(Plugin.Settings)
local timeUtil = require(Plugin.timeUtil)

type LatestReleaseInfo = {
	version: { number },
	prerelease: boolean,
	publishedUnixTimestamp: number,
}

local function compare(a, b)
	if a > b then
		return 1
	elseif a < b then
		return -1
	end

	return 0
end

-- 100 is the most GitHub returns at once. The 8.x line publishes to the same
-- repository, so a smaller page could leave newer 7.7 releases off it.
local RELEASES_URL = "https://api.github.com/repos/SenorLawyer/rojo/releases?per_page=100"

local Version = {}

--[[
	Splits a version suffix into the community fork's build number and any real
	prerelease tag. Fork releases are tagged like "7.7.2-fork.1"; that suffix
	marks a release build of the fork on top of upstream 7.7.2, not a
	prerelease, so "-fork.N" alone must not make a version count as older.
]]
local function splitSuffix(suffix: string?): (number, string?)
	if suffix == nil or suffix == "" then
		return 0, nil
	end

	local before, forkBuild, after = string.match(suffix, "^(.-)%-fork%.(%d+)(.*)$")
	if forkBuild == nil then
		return 0, suffix
	end

	local prerelease = before .. after
	return tonumber(forkBuild) :: number, if prerelease == "" then nil else prerelease
end

function Version.isPrerelease(version): boolean
	local _, prerelease = splitSuffix(version[4])
	return prerelease ~= nil
end

--[[
	Compares two versions of the form {major, minor, revision, suffix?}.

	If a is newer than b, 1.
	If a is older than b, -1.
	If a and b are the same, 0.

	A "-fork.N" suffix orders fork builds of the same upstream version after the
	upstream release and each other; any remaining suffix is a prerelease tag.
]]
function Version.compare(a, b)
	local major = compare(a[1], b[1])
	local minor = compare(a[2] or 0, b[2] or 0)
	local revision = compare(a[3] or 0, b[3] or 0)

	if major ~= 0 then
		return major
	end

	if minor ~= 0 then
		return minor
	end

	if revision ~= 0 then
		return revision
	end

	local aForkBuild, aPrerelease = splitSuffix(a[4])
	local bForkBuild, bPrerelease = splitSuffix(b[4])

	local forkBuild = compare(aForkBuild, bForkBuild)
	if forkBuild ~= 0 then
		return forkBuild
	end

	-- If neither are prerelease, they are the same
	if aPrerelease == nil and bPrerelease == nil then
		return 0
	end

	-- If one is prerelease it is older
	if aPrerelease ~= nil and bPrerelease == nil then
		return -1
	end
	if aPrerelease == nil and bPrerelease ~= nil then
		return 1
	end

	-- If they are both prereleases, compare those based on number
	local aPrereleaseNumeric = string.match(aPrerelease, "(%d+).*$")
	local bPrereleaseNumeric = string.match(bPrerelease, "(%d+).*$")

	if aPrereleaseNumeric == nil or bPrereleaseNumeric == nil then
		-- If one or both lack a number, comparing isn't meaningful
		return 0
	end
	return compare(tonumber(aPrereleaseNumeric) or 0, tonumber(bPrereleaseNumeric) or 0)
end

function Version.parse(versionString: string)
	local version = { string.match(versionString, "^v?(%d+)%.(%d+)%.(%d+)(.*)$") }
	for i, v in version do
		version[i] = tonumber(v) or v
	end

	if version[4] == "" then
		version[4] = nil
	end

	return version
end

function Version.display(version)
	local output = ("%d.%d.%d"):format(version[1], version[2], version[3])

	if version[4] ~= nil then
		output = output .. version[4]
	end

	return output
end

--[[
	The GitHub API rate limit for unauthenticated requests is rather low,
	and we don't release often enough to warrant checking it more than once a day.
--]]
Version._cachedLatestCompatible = nil :: {
	value: LatestReleaseInfo?,
	timestamp: number,
	includePrereleases: boolean,
}?

--[[
	A user may choose to reject requests to api.github.com. If they do, we want
	to disable the setting for checking for updates to indicate that it does
	nothing.
]]
Version._apiBlocked = nil :: boolean?

function Version.isApiBlocked(): boolean
	-- The Http module still sees a denial that arrives after our request was
	-- abandoned, such as when the plugin unloads while the prompt is open.
	return Version._apiBlocked == true or Http.isPermissionDenied(RELEASES_URL)
end

function Version.retrieveLatestCompatible(options: {
	version: { number },
	includePrereleases: boolean?,
}): LatestReleaseInfo?
	local includePrereleases = options.includePrereleases == true
	local cached = Version._cachedLatestCompatible
	-- The latest release depends on whether prereleases count, so a cached
	-- answer for the other setting must not be reused.
	if cached and cached.includePrereleases == includePrereleases and os.clock() - cached.timestamp < 60 * 60 * 24 then
		Log.debug("Using cached latest compatible version")
		return cached.value
	end

	Log.debug("Retrieving latest compatible version from GitHub")

	-- Http.timeout doesn't count time spent on Studio's permission prompt, so a
	-- slow Allow or Deny is still observed and recorded below.
	local success, releases = Http.timeout(
		Http.get(RELEASES_URL),
		RELEASES_URL,
		15,
		"Timed out checking for Rojo updates"
	)
		:andThen(function(response)
			if response.code >= 400 then
				local message = string.format("HTTP %s:\n%s", tostring(response.code), response.body)

				return Promise.reject(message)
			end

			return response
		end)
		:andThen(Http.Response.json)
		:await()

	if success == false or type(releases) ~= "table" or next(releases) ~= 1 then
		-- Roblox's HTTP errors are weird!
		if
			Http.isPermissionDenied(RELEASES_URL)
			or string.find(tostring(releases), "^Unknown HTTP error: HttpService permission denied")
		then
			Version._apiBlocked = true
		end
		return nil
	end
	Version._apiBlocked = false

	-- Iterate through releases, looking for the latest compatible version
	local latestCompatible: LatestReleaseInfo? = nil
	for _, release in releases do
		local releaseVersion = Version.parse(release.tag_name)
		if releaseVersion[1] == nil then
			continue
		end
		local isPrerelease = release.prerelease or Version.isPrerelease(releaseVersion)

		-- Skip prereleases if they are not requested
		if not includePrereleases and isPrerelease then
			continue
		end

		-- Skip releases that are potentially incompatible
		if releaseVersion[1] > options.version[1] then
			continue
		end

		-- Skip releases that are older than the latest compatible version
		if latestCompatible ~= nil and Version.compare(releaseVersion, latestCompatible.version) <= 0 then
			continue
		end

		latestCompatible = {
			version = releaseVersion,
			prerelease = isPrerelease,
			publishedUnixTimestamp = DateTime.fromIsoDate(release.published_at).UnixTimestamp,
		}
	end

	-- Don't return anything if the latest found is not newer than the current version
	if latestCompatible == nil or Version.compare(latestCompatible.version, options.version) <= 0 then
		-- Cache as nil so we don't try again for a day
		Version._cachedLatestCompatible = {
			value = nil,
			timestamp = os.clock(),
			includePrereleases = includePrereleases,
		}

		return nil
	end

	-- Cache the latest compatible version
	Version._cachedLatestCompatible = {
		value = latestCompatible,
		timestamp = os.clock(),
		includePrereleases = includePrereleases,
	}

	return latestCompatible
end

local function formatUpdateMessage(latestCompatibleVersion): string?
	if not latestCompatibleVersion then
		return
	end

	return string.format(
		"A newer compatible version of Rojo, %s, was published %s! Go to the Rojo releases page to learn more.",
		Version.display(latestCompatibleVersion.version),
		timeUtil.elapsedToText(DateTime.now().UnixTimestamp - latestCompatibleVersion.publishedUnixTimestamp)
	)
end

-- Render functions must never fetch releases or wait for network permission.
function Version.getCachedUpdateMessage(): string?
	if not Settings:get("checkForUpdates") or not Version._cachedLatestCompatible then
		return nil
	end
	return formatUpdateMessage(Version._cachedLatestCompatible.value)
end

function Version.getUpdateMessage(): string?
	if not Settings:get("checkForUpdates") then
		return nil
	end

	local isLocalInstall = string.find(debug.traceback(), "\n[^\n]-user_.-$") ~= nil
	return formatUpdateMessage(Version.retrieveLatestCompatible({
		version = Config.version,
		includePrereleases = isLocalInstall and Settings:get("checkForPrereleases"),
	}))
end

return Version
