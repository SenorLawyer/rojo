local StudioService = game:GetService("StudioService")
local RunService = game:GetService("RunService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local SerializationService = game:GetService("SerializationService")
local ScriptEditorService = game:GetService("ScriptEditorService")
local Selection = game:GetService("Selection")

local Packages = script.Parent.Parent.Packages
local Log = require(Packages.Log)
local Fmt = require(Packages.Fmt)
local t = require(Packages.t)
local Promise = require(Packages.Promise)
local Timer = require(script.Parent.Timer)

local ChangeBatcher = require(script.Parent.ChangeBatcher)
local encodePatchUpdate = require(script.Parent.ChangeBatcher.encodePatchUpdate)
local InstanceMap = require(script.Parent.InstanceMap)
local PatchSet = require(script.Parent.PatchSet)
local Reconciler = require(script.Parent.Reconciler)
local strict = require(script.Parent.strict)
local Settings = require(script.Parent.Settings)
local orderSwaps = require(script.Parent.orderSwaps)
local StudioBridge = require(script.Parent.StudioBridge)
local hashSource = require(script.Parent.hashSource)
local Config = require(script.Parent.Config)
local Version = require(script.Parent.Version)

local Status = strict("Session.Status", {
	NotStarted = "NotStarted",
	Connecting = "Connecting",
	Connected = "Connected",
	Disconnected = "Disconnected",
})

local function debugPatch(object)
	return Fmt.debugify(object, function(patch, output)
		output:writeLine("Patch {{")
		output:indent()

		for removed in ipairs(patch.removed) do
			output:writeLine("Remove ID {}", removed)
		end

		for id, added in pairs(patch.added) do
			output:writeLine("Add ID {} {:#?}", id, added)
		end

		for _, updated in ipairs(patch.updated) do
			output:writeLine("Update ID {} {:#?}", updated.id, updated)
		end

		output:unindent()
		output:write("}")
	end)
end

-- A script that loses this many non-blank lines in one change is held back
-- instead of being emptied. Smaller scripts are often emptied on purpose.
local WIPE_GUARD_MIN_LINES = 5
-- Bounds what the sync summary sends with every bridge poll.
local MAX_SUMMARY_HELD = 20
local MAX_SUMMARY_PATH_BYTES = 4096
-- How many verifies' sources are kept for their fixes.
local MAX_VERIFY_RECORDS = 4
-- Undo history names of Rojo's own changes, which `OnUndo` reports.
local PATCH_WAYPOINT_PREFIX = "Rojo: Patch "
local RESYNC_WAYPOINT_PREFIX = "Rojo: Resync "

local function countNonBlankLines(source: string): number
	local count = 0
	for line in string.gmatch(source, "[^\n]+") do
		if string.find(line, "%S") then
			count += 1
		end
	end
	return count
end

-- Reads a script's source the way the reconciler does: from its editor when
-- it is open in Studio, since that can differ from `Source` until saved.
local function readSource(instance): string?
	local ok, source = pcall(function()
		return ScriptEditorService:GetEditorSource(instance)
	end)
	if ok and type(source) == "string" then
		return source
	end
	ok, source = pcall(function()
		return (instance :: any).Source
	end)
	if ok and type(source) == "string" then
		return source
	end
	return nil
end

-- Holds by the Studio instance they protect. A reconnect makes a new session
-- with new IDs, and the file may still be empty, so the new session adopts
-- these instead of starting without holds. Weak keys let destroyed scripts go.
local heldByInstance = setmetatable({}, { __mode = "k" })

local function nowMillis()
	return DateTime.now().UnixTimestampMillis
end

local function attemptReparent(instance, parent)
	return pcall(function()
		instance.Parent = parent
	end)
end

local ServeSession = {}
ServeSession.__index = ServeSession

ServeSession.Status = Status

local validateServeOptions = t.strictInterface({
	apiContext = t.table,
	twoWaySync = t.boolean,
})

function ServeSession.new(options)
	assert(validateServeOptions(options))

	-- Declare self ahead of time to capture it in a closure
	local self
	local function onInstanceChanged(instance, propertyName)
		if not self.__twoWaySync then
			return
		end

		self.__changeBatcher:add(instance, propertyName)
	end

	local function onChangesFlushed(patch)
		self:__sendChanges(patch)
	end

	local instanceMap = InstanceMap.new(onInstanceChanged)
	local changeBatcher = ChangeBatcher.new(instanceMap, onChangesFlushed)
	local reconciler = Reconciler.new(instanceMap)

	local connections = {}

	local connection = StudioService:GetPropertyChangedSignal("ActiveScript"):Connect(function()
		local activeScript = StudioService.ActiveScript

		if activeScript ~= nil then
			self:__onActiveScriptChanged(activeScript)
		end
	end)
	table.insert(connections, connection)
	table.insert(
		connections,
		game:GetPropertyChangedSignal("PlaceId"):Connect(function()
			self:__onPlaceIdChanged()
		end)
	)
	-- Undoing a Rojo change puts back what Studio had before it, so Studio no
	-- longer matches the files on disk, and nothing would say so.
	table.insert(
		connections,
		ChangeHistoryService.OnUndo:Connect(function(waypoint)
			self:__onRojoHistoryChanged(waypoint, 1)
		end)
	)
	table.insert(
		connections,
		ChangeHistoryService.OnRedo:Connect(function(waypoint)
			self:__onRojoHistoryChanged(waypoint, -1)
		end)
	)

	self = {
		__status = Status.NotStarted,
		__expectedPlaceId = game.PlaceId,
		__apiContext = options.apiContext,
		__twoWaySync = options.twoWaySync,
		__reconciler = reconciler,
		__instanceMap = instanceMap,
		__changeBatcher = changeBatcher,
		__statusChangedCallback = nil,
		__connections = connections,
		__precommitCallbacks = {},
		__postcommitCallbacks = {},
		__updateLoadingText = function() end,
		-- Scripts whose emptying was held back, by ID. See `__holdScriptWipes`.
		__heldWipes = {},
		-- The sources recent verifies saw, by verify request ID.
		__verifiedRecords = {},
		-- The latest source write for each script, so an older pending write
		-- cannot finish after a newer one. See `__writeSource`.
		__sourceGenerations = {},
		-- Scripts whose hold ended with a class change still to apply.
		__refreshAfterHold = {},
		-- Holds moving to a script that replaces a held one, by the new ID.
		__pendingHoldTransfers = {},
		__syncStats = {
			connectedAt = nil,
			lastPatchAt = nil,
			patchesApplied = 0,
			lastPatchUnapplied = 0,
			-- Script sources Studio is still writing, and those it could not
			-- write since connecting.
			pendingSourceWrites = 0,
			failedSourceWrites = 0,
			undoneRojoPatches = 0,
		},
	}

	setmetatable(self, ServeSession)

	-- Turning the guard off ends its holds at once, rather than at the next
	-- change from disk, and brings those scripts back in line with the files.
	self.__disconnectGuardSetting = Settings:onChanged("guardScriptWipes", function(enabled)
		if not enabled then
			self:__releaseAllHolds()
		end
	end)

	return self
end

function ServeSession:__releaseAllHolds()
	-- Holds just end. Studio keeps the code it has until the next change from
	-- disk, and `rojo studio verify --fix` brings the scripts in line now,
	-- with its checks against newer changes. Writing anything here would race
	-- the ordinary sync of a file that gets its code back meanwhile.
	-- A class change held back with a wipe is kept: the server will not send
	-- it again, so clearing the hold queues the script to be fetched whole
	-- with the next patch, which runs in order with the rest of the sync.
	local count = 0
	for id in table.clone(self.__heldWipes) do
		self:__clearHold(id)
		count += 1
	end
	table.clear(heldByInstance)
	if count > 0 then
		Log.warn(
			"Turned off the wipe guard, which was holding back {} empty files. Studio keeps their code until they change; run `rojo studio verify --fix` to sync them now.",
			count
		)
	end
end

--[=[
	Plugins and servers with the same protocol work together, but a plugin from
	another release may lack features the server's docs describe, which is easy
	to miss when both say "Rojo". Returns a warning when the major or minor
	versions differ.
]=]
function ServeSession.versionMismatchWarning(serverVersion)
	if type(serverVersion) ~= "string" then
		return nil
	end
	local server = Version.parse(serverVersion)
	if type(server[1]) ~= "number" or type(server[2]) ~= "number" then
		return nil
	end
	local plugin = Config.version
	if server[1] == plugin[1] and server[2] == plugin[2] then
		return nil
	end
	local fix = if Version.compare(server, plugin) > 0
		then "Run `rojo plugin install` with the Rojo that runs `rojo serve`, then restart Studio, to update the plugin."
		else "Update Rojo in the project's rokit.toml (then `rokit install`) to match the plugin."
	return string.format(
		"The Rojo plugin is version %s but the server is version %s. Syncing works, but features differ between them. %s",
		Version.display(plugin),
		serverVersion,
		fix
	)
end

function ServeSession:__fmtDebug(output)
	output:writeLine("ServeSession {{")
	output:indent()

	output:writeLine("API Context: {:#?}", self.__apiContext)
	output:writeLine("Instances: {:#?}", self.__instanceMap)

	output:unindent()
	output:write("}")
end

function ServeSession:getStatus()
	return self.__status
end

function ServeSession:onStatusChanged(callback)
	self.__statusChangedCallback = callback
end

function ServeSession:setConfirmCallback(callback)
	self.__userConfirmCallback = callback
end

function ServeSession:setUpdateLoadingTextCallback(callback)
	self.__updateLoadingText = callback
end

function ServeSession:setLoadingText(text: string)
	self.__phaseStartedAt = os.clock()
	self.__phase = text
	self.__updateLoadingText(text)
end

function ServeSession:getBridgeStatus()
	return {
		available = self.__studioBridge ~= nil and not self.__studioBridge.stopped,
		error = self.__bridgeError,
	}
end

function ServeSession:restartStudioBridge()
	assert(self.__status == Status.Connected, "Connect normal sync before starting the Studio bridge")
	assert(self.__bridgePairing, "Start rojo serve with --studio-token-file to enable the Studio bridge")
	if self.__studioBridge then
		self.__studioBridge:stop()
	end
	self.__bridgeError = nil
	local bridge
	bridge = StudioBridge.new(self.__apiContext, self.__bridgePairing, function(err, intentional)
		if self.__studioBridge ~= bridge then
			return
		end
		-- Agent tooling is optional. Its HTTP connection must not tear down healthy sync.
		self.__bridgeError = tostring(err)
		if intentional then
			-- The user turned the bridge off, so every connect would otherwise
			-- warn about a choice they made.
			Log.debug("{}", err)
			return
		end
		Log.warn("{} Normal syncing remains connected. Retry the bridge through agent control.", err)
	end, {
		getSummary = function()
			return self:getSyncSummary()
		end,
		getSourceHashes = function(verifyId, expiresAt, isActive)
			return self:getSourceHashes(verifyId, expiresAt, isActive)
		end,
		applyResync = function(patch, expect, includeHeld, expiresAt, isActive, verifyId)
			return self:applyResync(patch, expect, includeHeld, expiresAt, isActive, verifyId)
		end,
	})
	self.__studioBridge = bridge
	bridge:start()
end

function ServeSession:__onRojoHistoryChanged(waypoint, delta)
	if self.__status ~= Status.Connected or type(waypoint) ~= "string" then
		return
	end
	local isRojo = string.sub(waypoint, 1, #PATCH_WAYPOINT_PREFIX) == PATCH_WAYPOINT_PREFIX
		or string.sub(waypoint, 1, #RESYNC_WAYPOINT_PREFIX) == RESYNC_WAYPOINT_PREFIX
	if not isRojo then
		return
	end
	local stats = self.__syncStats
	stats.undoneRojoPatches = math.max(0, stats.undoneRojoPatches + delta)
	if delta > 0 then
		Log.warn(
			"Undid '{}', a change Rojo synced from disk. Studio no longer matches the project's files for the instances it changed{}."
				.. " Redo it, or run `rojo studio verify --fix` (or reconnect) to sync them again.",
			waypoint,
			if self.__twoWaySync then ", and two-way sync may write the undone values back to disk" else ""
		)
	end
end

function ServeSession:__setHold(id, instance, entry)
	self.__heldWipes[id] = entry
	if instance then
		entry.instance = instance
		heldByInstance[instance] = entry
	end
end

function ServeSession:__clearHold(id)
	local entry = self.__heldWipes[id]
	if entry then
		if entry.instance then
			heldByInstance[entry.instance] = nil
		end
		self.__heldWipes[id] = nil
		-- A class change held back with the wipe was never applied, and the
		-- server will not send it again, so the instance is fetched whole.
		if entry.deferredClass and self.__instanceMap.fromIds[id] then
			self.__refreshAfterHold[id] = true
		end
	end
end

local SCRIPT_CLASSES = { Script = true, LocalScript = true, ModuleScript = true }

-- Hydration pairs instances by name and class. A held script whose class
-- change was held back no longer matches the server's, so the catch-up
-- patch would remove it and add the server's empty copy, the wipe the hold
-- prevents. Pairing it by name under the same parent instead turns that into
-- an update the guard holds back.
-- The one unpaired child of `serverParent` named `name`, if exactly one is.
local function onlyChildNamed(self, serverInstances, serverParent, name, scriptsOnly)
	local found
	for _, childId in serverParent.Children do
		local child = serverInstances[childId]
		if
			child
			and child.Name == name
			and (not scriptsOnly or SCRIPT_CLASSES[child.ClassName])
			and self.__instanceMap.fromIds[childId] == nil
		then
			if found then
				return nil
			end
			found = childId
		end
	end
	return found
end

function ServeSession:__rematchHolds(serverInstances)
	-- A held script's ancestors may not have hydrated either, for example a
	-- folder that became a Model on disk, and then the catch-up patch would
	-- replace the whole subtree. So each held script is paired by name from
	-- its nearest paired ancestor down; a class that differs then arrives as
	-- an update, which keeps the children. Passes repeat until nothing more is
	-- paired, since one held script can sit inside another; each pairing
	-- also hydrates what is under it.
	local progress = true
	while progress do
		progress = false
		for instance in heldByInstance do
			if self.__instanceMap.fromInstances[instance] or not instance.Parent then
				continue
			end
			local chain = { instance }
			local ancestor = instance.Parent
			while ancestor and not self.__instanceMap.fromInstances[ancestor] do
				table.insert(chain, 1, ancestor)
				ancestor = ancestor.Parent
			end
			local serverId = ancestor and self.__instanceMap.fromInstances[ancestor]
			for index, link in chain do
				local serverParent = serverId and serverInstances[serverId]
				if not serverParent then
					serverId = nil
					break
				end
				local childId = onlyChildNamed(self, serverInstances, serverParent, link.Name, index == #chain)
				if not childId then
					serverId = nil
					break
				end
				self.__instanceMap:insert(childId, link)
				self.__reconciler:hydrate(serverInstances, childId, link)
				serverId = childId
				progress = true
			end
		end
	end
end

-- Takes out of a catch-up patch every removal of a held script, or of
-- something containing one, that this session could not pair with the
-- server's tree. The kept instances are no longer synced; the user is told.
function ServeSession:__keepUnpairedHolds(patch)
	if not Settings:get("guardScriptWipes") then
		return
	end
	local index = 1
	while index <= #patch.removed do
		local removed = patch.removed[index]
		local instance = if typeof(removed) == "Instance" then removed else self.__instanceMap.fromIds[removed]
		local keeps
		if instance then
			for held in heldByInstance do
				if
					(held == instance or held:IsDescendantOf(instance))
					and not self.__instanceMap.fromInstances[held]
				then
					keeps = held
					break
				end
			end
		end
		if keeps then
			Log.warn(
				"Kept {} in Studio although Rojo would remove it, because it contains {}, whose file on disk is empty and whose code Rojo is holding back. It is no longer synced: restore the file's code, or move it back where it was, and reconnect.",
				instance:GetFullName(),
				keeps:GetFullName()
			)
			table.remove(patch.removed, index)
		else
			index += 1
		end
	end
end

-- A file that has code again while Studio was disconnected ends its hold,
-- even when its code matches what Studio kept and so no change arrives.
function ServeSession:__releaseRecoveredHolds(serverInstances)
	for id, entry in self.__heldWipes do
		local server = serverInstances[id]
		local encoded = server and server.Properties and server.Properties.Source
		local ty, value
		if type(encoded) == "table" then
			ty, value = next(encoded)
		end
		if ty == "String" and type(value) == "string" and string.find(value, "%S") then
			-- The catch-up patch applies any class change itself.
			entry.deferredClass = nil
			self:__clearHold(id)
		end
	end
end

-- Takes over holds from an earlier session of this Studio for the instances
-- this session now tracks.
function ServeSession:__adoptHolds()
	for instance, entry in heldByInstance do
		local id = self.__instanceMap.fromInstances[instance]
		if id and not self.__heldWipes[id] then
			self.__heldWipes[id] = entry
		end
	end
end

--[=[
	Returns a small summary of this session's sync health. The Studio bridge
	sends it with every poll, so `rojo studio sessions` can show it without
	asking each Studio.
]=]
function ServeSession:getSyncSummary()
	local stats = self.__syncStats
	local held = {}
	local heldCount = 0
	for id, entry in self.__heldWipes do
		heldCount += 1
		table.insert(held, { id = id, path = entry.path, lines = entry.lines, heldAt = entry.heldAt })
	end
	table.sort(held, function(a, b)
		return a.path < b.path
	end)
	-- The server ignores a summary over 16 KiB, so the listed paths share a
	-- byte budget. The count still covers every hold.
	local listed = {}
	local budget = MAX_SUMMARY_PATH_BYTES
	for _, entry in held do
		if #listed >= MAX_SUMMARY_HELD then
			break
		end
		entry.path = StudioBridge.truncateUtf8(entry.path, 256)
		budget -= #entry.path + 64
		if budget < 0 then
			break
		end
		table.insert(listed, entry)
	end
	held = listed
	return {
		status = self.__status,
		projectName = self.__projectName,
		serverVersion = self.__serverVersion,
		pluginVersion = Version.display(Config.version),
		placeId = game.PlaceId,
		twoWaySync = self.__twoWaySync,
		connectedAt = stats.connectedAt,
		lastPatchAt = stats.lastPatchAt,
		patchesApplied = stats.patchesApplied,
		lastPatchUnapplied = stats.lastPatchUnapplied,
		undoneRojoPatches = stats.undoneRojoPatches,
		pendingSourceWrites = stats.pendingSourceWrites,
		failedSourceWrites = stats.failedSourceWrites,
		heldWipeCount = heldCount,
		heldWipes = held,
	}
end

--[=[
	Hashes the source of every script Rojo tracks, for `rojo studio verify`.
	Yields between batches so a large place does not freeze Studio.
]=]
function ServeSession:getSourceHashes(verifyId, expiresAt, isActive)
	assert(self.__status == Status.Connected, "Rojo is not syncing this place. Connect it first.")
	local scripts = {}
	local detached = {}
	-- Patches may add or remove IDs while this yields, which would break
	-- iterating the live table.
	local tracked = table.clone(self.__instanceMap.fromIds)
	-- The exact sources seen, which a following `verify --fix` compares with
	-- before writing, so a newer edit is never overwritten even if its
	-- hashes happen to match.
	local seen = {}
	local hashed = 0
	local bytesSinceYield = 0
	local lastAsked = os.clock()
	for id, instance in tracked do
		if not instance:IsA("LuaSourceContainer") then
			continue
		end
		if not instance:IsDescendantOf(game) then
			table.insert(detached, id)
			continue
		end
		local source = readSource(instance)
		if source then
			local hash, length, second = hashSource(source)
			table.insert(scripts, { id = id, hash = hash, hash2 = second, length = length })
			seen[id] = source
		end
		hashed += 1
		bytesSinceYield += if source then #source else 0
		-- Yield by work done, so many scripts or a few huge ones both keep
		-- Studio responsive, and stop when the request is gone.
		if hashed % 200 == 0 or bytesSinceYield > 512 * 1024 then
			bytesSinceYield = 0
			task.wait()
			assert(self.__status == Status.Connected, "Rojo stopped syncing this place during verify")
			assert(
				type(expiresAt) ~= "number" or DateTime.now().UnixTimestampMillis < expiresAt,
				"The verify request expired before every script was hashed"
			)
			if isActive and os.clock() - lastAsked > 1 then
				lastAsked = os.clock()
				assert(isActive(), "The verify request was cancelled")
			end
		end
	end
	local held = {}
	for id in self.__heldWipes do
		table.insert(held, id)
	end
	-- Kept per verify, so two verifies at once cannot replace each other's
	-- record, and only for the last few, so memory stays bounded.
	-- Only the generations of the scripts seen, not every one ever written.
	local generations = {}
	for id in seen do
		generations[id] = self.__sourceGenerations[id]
	end
	self:__rememberVerified(verifyId, seen, generations)
	return { scripts = scripts, detached = detached, held = held }
end

--[=[
	Applies the script updates that `rojo studio verify --fix` built from the
	server's tree. `expect` maps each ID to the hash Studio had when verify
	checked it (`from`) and the hash of the source being sent (`to`). An
	update whose script changed since the check is skipped, so a fix never
	undoes a newer sync. Without `includeHeld` the wipe guard still applies,
	which covers a file that became empty after the check.

	The sources are written here with `UpdateSourceAsync`, whose callback
	checks `from` against the source at the moment of writing, so the check
	and the write cannot be split by a newer edit. The writes are awaited
	inside one undo step, so a script counts as updated only once its source
	was written and the whole resync undoes together.
]=]
function ServeSession:__rememberVerified(verifyId, seen, generations)
	if type(verifyId) ~= "string" then
		return
	end
	local records = self.__verifiedRecords
	table.insert(records, { id = verifyId, sources = seen, generations = generations or {} })
	while #records > MAX_VERIFY_RECORDS do
		table.remove(records, 1)
	end
end

function ServeSession:__verifiedRecordFor(verifyId)
	for _, record in self.__verifiedRecords do
		if record.id == verifyId then
			return record
		end
	end
	return nil
end

function ServeSession:__verifiedSourcesFor(verifyId)
	local record = self:__verifiedRecordFor(verifyId)
	return record and record.sources
end

function ServeSession:applyResync(patch, expect, includeHeld, expiresAt, isActive, verifyId)
	assert(self.__status == Status.Connected, "Rojo is not syncing this place. Connect it first.")
	-- The server stops waiting at `expiresAt` and reports the batch as failed,
	-- so nothing may change after it.
	-- A disconnected session has stopped syncing, so its resync must stop too.
	local function expired()
		return self.__status ~= Status.Connected
			or (type(expiresAt) == "number" and DateTime.now().UnixTimestampMillis >= expiresAt)
	end
	assert(
		type(patch) == "table"
			and type(patch.updated) == "table"
			and (patch.removed == nil or next(patch.removed) == nil)
			and (patch.added == nil or next(patch.added) == nil),
		"A resync may only update existing instances"
	)
	assert(type(expect) == "table", "A resync needs the expected script hashes")
	local writes = {}
	local skipped = 0
	for _, update in patch.updated do
		local expected = expect[update.id]
		local encoded = update.changedProperties and update.changedProperties.Source
		local ty, target
		if type(encoded) == "table" then
			ty, target = next(encoded)
		end
		local instance = self.__instanceMap.fromIds[update.id]
		if
			type(expected) ~= "table"
			or ty ~= "String"
			or type(target) ~= "string"
			or instance == nil
			or not instance:IsA("LuaSourceContainer")
		then
			skipped += 1
			continue
		end
		table.insert(writes, { id = update.id, instance = instance, target = target, from = expected.from })
	end
	-- A file may have become empty after verify checked it. Without
	-- includeHeld such a write is held back like a normal sync's would be.
	-- Only the callback below, which sees the source being replaced, decides
	-- that; a write that is cancelled must never touch an existing hold.
	local guardWipes = not includeHeld and Settings:get("guardScriptWipes")

	-- Without its own undo step, a resync would land in whatever another
	-- recording holds, and could not be undone on its own.
	local recording =
		ChangeHistoryService:TryBeginRecording(RESYNC_WAYPOINT_PREFIX .. DateTime.now():FormatLocalTime("LTS", "en-us"))
	assert(
		recording or #writes == 0,
		"Studio is recording another change, so the resync could not get its own undo step. Run verify --fix again."
	)

	-- Without the sources its own verify saw, a write could undo a newer edit
	-- whose hashes match, so every write is skipped.
	local record = self:__verifiedRecordFor(verifyId)
	local verified = record and record.sources
	local updated, failed = 0, 0
	local ok, err = pcall(function()
		local cancelled = false
		for _, write in writes do
			-- `isActive` asks the server, so it yields; it cannot run inside
			-- the update callback, which must not.
			if cancelled or expired() or (isActive and not isActive()) then
				cancelled = true
				failed += 1
				continue
			end
			-- The script may have been removed, or replaced by another
			-- instance, since the batch was built.
			if self.__instanceMap.fromIds[write.id] ~= write.instance or not write.instance:IsDescendantOf(game) then
				skipped += 1
				continue
			end
			-- Only the hold that existed before this write may end with it. A
			-- hold made while the write yielded is for a newer empty file.
			local holdBefore = self.__heldWipes[write.id]
			local seenSource = verified and verified[write.id]
			local changed, held, late = false, nil, false
			self.__instanceMap:pauseInstance(write.instance)
			-- A sync write started after verify looked carries a newer change
			-- from disk than this fix, so the fix gives way to it.
			local seenGeneration = record and record.generations and record.generations[write.id] or 0
			if (self.__sourceGenerations[write.id] or 0) ~= seenGeneration then
				skipped += 1
				continue
			end
			-- A held script whose class changed on disk too cannot be fixed by
			-- writing its source: the class change would have to replace the
			-- instance. Its hold ends the usual ways instead, by writing code
			-- to its file or deleting it.
			local holdNow = self.__heldWipes[write.id]
			if includeHeld and holdNow and holdNow.deferredClass then
				skipped += 1
				Log.warn(
					"{} is held and its class changed on disk too, so --include-held cannot empty it. Write code to its file, or delete the file.",
					holdNow.path
				)
				continue
			end
			-- Shared with ordinary sync writes: whichever write was started
			-- last for a script is the only one that may still commit. Older
			-- pending writes started before verify looked are cancelled.
			local generation = (self.__sourceGenerations[write.id] or 0) + 1
			self.__sourceGenerations[write.id] = generation
			local done, wrote, abandoned = false, false, false
			task.spawn(function()
				wrote = pcall(function()
					ScriptEditorService:UpdateSourceAsync(write.instance, function(current)
						-- Returning nil cancels the update. The callback can run
						-- again when Studio retries, so each check is repeated.
						if abandoned or self.__sourceGenerations[write.id] ~= generation then
							late = true
							return nil
						end
						if expired() then
							late = true
							return nil
						end
						if seenSource == nil or current ~= seenSource or hashSource(current) ~= write.from then
							changed = true
							return nil
						end
						-- Without includeHeld a held script is left alone whatever
						-- the fix would write: a hold may have started after
						-- verify built the fix, and only code on disk, deletion
						-- or an explicit --include-held ends it.
						if not includeHeld and (self.__heldWipes[write.id] or heldByInstance[write.instance]) then
							held = self.__heldWipes[write.id] and self.__heldWipes[write.id].lines or 0
							return nil
						end
						if guardWipes and not string.find(write.target, "%S") then
							local lines = countNonBlankLines(current)
							if lines >= WIPE_GUARD_MIN_LINES then
								held = lines
								return nil
							end
						end
						return write.target
					end)
				end)
				done = true
			end)
			-- Studio can keep a write waiting. The server stops waiting at the
			-- deadline, so the write is abandoned then: its callback refuses to
			-- commit if it runs later, and the undo recording is closed.
			while not done and not expired() do
				task.wait(0.05)
			end
			if not done then
				abandoned = true
				late = true
				wrote = false
			end
			if held then
				skipped += 1
				if not self.__heldWipes[write.id] then
					self:__setHold(
						write.id,
						write.instance,
						{ path = write.instance:GetFullName(), lines = held, heldAt = nowMillis() }
					)
				end
			elseif changed then
				skipped += 1
			elseif wrote and not late then
				updated += 1
				if holdBefore and self.__heldWipes[write.id] == holdBefore then
					self:__clearHold(write.id)
				end
			else
				-- The write failed, or was cancelled because the command expired.
				failed += 1
			end
		end
	end)
	if recording then
		ChangeHistoryService:FinishRecording(recording, Enum.FinishRecordingOperation.Commit)
	end
	if not ok then
		error(err, 0)
	end
	return { updated = updated, failed = failed, skipped = skipped }
end

--[=[
	Takes changes out of `patch` that would empty a script with several lines
	of code, and remembers them, so a broken write on disk (an editor or agent
	saving an empty file) cannot wipe the script in Studio. A later change that
	gives the script source again, or removes it, ends the hold.
]=]
function ServeSession:__holdScriptWipes(patch)
	-- The server can replace a script by removing it and adding a new one
	-- with the same name, for example when its class changes between
	-- LocalScript and Script. If the old one is held and the new one would
	-- be empty, the new one gets the old code and the hold instead.
	local guard = Settings:get("guardScriptWipes")
	if not guard then
		-- Turning the guard off ends every hold, so nothing it held back keeps
		-- affecting sync, fixes or health. Class changes it held back are
		-- fetched again by `__clearHold`.
		for id in table.clone(self.__heldWipes) do
			self:__clearHold(id)
		end
		table.clear(heldByInstance)
		return
	end
	-- Code moves to a replacement only when exactly one script of that name
	-- goes and exactly one comes under the same parent. Otherwise it is not
	-- clear which new script is which, and copying held code into a script
	-- that was never held would be worse than not carrying it over.
	local removedHeld = {}
	local removedCount = {}
	for _, removed in patch.removed do
		local instance = self.__instanceMap.fromIds[removed]
		if guard and instance and instance.Parent and instance:IsA("LuaSourceContainer") then
			local parentId = self.__instanceMap.fromInstances[instance.Parent]
			if parentId then
				local key = parentId .. "/" .. instance.Name
				removedCount[key] = (removedCount[key] or 0) + 1
			end
		end
	end
	local addedCount = {}
	for _, virtual in patch.added do
		if SCRIPT_CLASSES[virtual.ClassName] and virtual.Parent then
			local key = virtual.Parent .. "/" .. virtual.Name
			addedCount[key] = (addedCount[key] or 0) + 1
		end
	end
	for _, removed in patch.removed do
		local entry = self.__heldWipes[removed]
		local instance = self.__instanceMap.fromIds[removed]
		if guard and entry and instance and instance.Parent then
			local parentId = self.__instanceMap.fromInstances[instance.Parent]
			local key = parentId and parentId .. "/" .. instance.Name
			if key and removedCount[key] == 1 and addedCount[key] == 1 then
				removedHeld[key] = { entry = entry, source = readSource(instance) or "" }
			elseif key then
				Log.warn(
					"Could not tell which new script replaces {}, whose code Rojo was holding back, because several share its name; its code is not carried over.",
					instance:GetFullName()
				)
			end
		end
		if entry then
			-- The instance goes away, so there is nothing left to refresh.
			entry.deferredClass = nil
		end
		self:__clearHold(removed)
	end
	if not guard then
		return
	end
	for id, virtual in patch.added do
		local carried = SCRIPT_CLASSES[virtual.ClassName]
			and virtual.Parent
			and removedHeld[virtual.Parent .. "/" .. virtual.Name]
		local encoded = carried and virtual.Properties and virtual.Properties.Source
		local ty, value
		if type(encoded) == "table" then
			ty, value = next(encoded)
		end
		if carried and ty == "String" and type(value) == "string" and not string.find(value, "%S") then
			virtual.Properties.Source = { String = carried.source }
			self.__pendingHoldTransfers[id] = carried.entry
		end
	end
	local index = 1
	while index <= #patch.updated do
		local update = patch.updated[index]
		local encoded = update.changedProperties and update.changedProperties.Source
		local ty, value
		if type(encoded) == "table" then
			ty, value = next(encoded)
		end
		if ty ~= "String" or type(value) ~= "string" then
			-- Changing the class recreates the instance from the update's other
			-- properties, without the source the hold keeps, so it waits until
			-- the hold ends.
			-- A script class becoming a non-script, such as an init file's
			-- directory turning back into a Folder when the file is deleted, is
			-- the script's file going away, which ends the hold.
			if
				self.__heldWipes[update.id]
				and update.changedClassName ~= nil
				and not SCRIPT_CLASSES[update.changedClassName]
			then
				self:__clearHold(update.id)
				index += 1
				continue
			end
			if self.__heldWipes[update.id] and update.changedClassName ~= nil then
				local instance = self.__instanceMap.fromIds[update.id]
				-- Changing back to the class Studio already has needs no
				-- replacement any more.
				if instance and instance.ClassName == update.changedClassName then
					self.__heldWipes[update.id].deferredClass = nil
				else
					Log.warn(
						"Rojo held back changing the class of {} because its file on disk is empty.",
						self.__heldWipes[update.id].path
					)
					self.__heldWipes[update.id].deferredClass = update.changedClassName
				end
				table.remove(patch.updated, index)
				continue
			end
			index += 1
			continue
		end
		if string.find(value, "%S") then
			self:__clearHold(update.id)
			index += 1
			continue
		end
		local instance = self.__instanceMap.fromIds[update.id]
		local lines = 0
		if instance and instance:IsA("LuaSourceContainer") then
			local source = readSource(instance)
			if source then
				lines = countNonBlankLines(source)
			end
		end
		-- A script that is already held stays held, even if its source was
		-- shortened in Studio since. Only the ways out of a hold end it.
		if lines < WIPE_GUARD_MIN_LINES and not self.__heldWipes[update.id] then
			index += 1
			continue
		end
		local path = if instance then instance:GetFullName() else self.__heldWipes[update.id].path
		if not self.__heldWipes[update.id] then
			Log.warn(
				"Rojo held back emptying {} ({} lines of code) because its file on disk is now empty."
					.. " If that was a broken save, write the file again. If it is intended, delete the file,"
					.. " or run `rojo studio verify --fix --include-held`.",
				path,
				lines
			)
		end
		if not self.__heldWipes[update.id] then
			self:__setHold(update.id, instance, { path = path, lines = lines, heldAt = nowMillis() })
		end
		if update.changedClassName ~= nil then
			self.__heldWipes[update.id].deferredClass = if instance
					and instance.ClassName == update.changedClassName
				then nil
				else update.changedClassName
		end
		update.changedProperties.Source = nil
		local empty = next(update.changedProperties) == nil
			and update.changedName == nil
			and update.changedClassName == nil
			and update.changedMetadata == nil
		-- A class change would recreate the script without its source, which is
		-- the wipe the hold prevents, so the whole update waits.
		if empty or update.changedClassName ~= nil then
			table.remove(patch.updated, index)
		else
			index += 1
		end
	end
end

--[=[
	Hooks a function to run before patch application.
	The provided function is called with the incoming patch and an InstanceMap
	as parameters.
]=]
function ServeSession:hookPrecommit(callback)
	table.insert(self.__precommitCallbacks, callback)
	Log.trace("Added precommit callback: {}", callback)

	return function()
		-- Remove the callback from the list
		for i, cb in self.__precommitCallbacks do
			if cb == callback then
				table.remove(self.__precommitCallbacks, i)
				Log.trace("Removed precommit callback: {}", callback)
				break
			end
		end
	end
end

--[=[
	Hooks a function to run after patch application.
	The provided function is called with the applied patch, the current
	InstanceMap, and a PatchSet containing any unapplied changes.
]=]
function ServeSession:hookPostcommit(callback)
	table.insert(self.__postcommitCallbacks, callback)
	Log.trace("Added postcommit callback: {}", callback)

	return function()
		-- Remove the callback from the list
		for i, cb in self.__postcommitCallbacks do
			if cb == callback then
				table.remove(self.__postcommitCallbacks, i)
				Log.trace("Removed postcommit callback: {}", callback)
				break
			end
		end
	end
end

function ServeSession:start()
	if self.__status ~= Status.NotStarted then
		return
	end

	self:__setStatus(Status.Connecting)
	if self.__status == Status.Disconnected then
		return
	end
	self:setLoadingText("Connecting to server...")

	self.__apiContext
		:connect(function()
			if self.__status == Status.Connecting then
				self:setLoadingText(
					"Still waiting for the Rojo server...\nIf Studio is asking whether Rojo may access this address, choose Allow.\nOtherwise the server may be busy."
				)
			end
		end)
		:andThen(function(serverInfo)
			if self.__status == Status.Disconnected then
				return
			end

			self.__serverVersion = serverInfo.serverVersion
			self.__projectName = serverInfo.projectName
			local mismatch = ServeSession.versionMismatchWarning(serverInfo.serverVersion)
			if mismatch then
				Log.warn("{}", mismatch)
			end
			self:setLoadingText("Loading initial data from server...")
			return self:__initialSync(serverInfo):andThen(function()
				if self.__status == Status.Disconnected then
					return
				end

				self:setLoadingText("Starting sync loop...")
				self:__applyGameAndPlaceId(serverInfo)

				return self.__apiContext:connectWebSocket({
					["messages"] = function(messagesPacket)
						if self.__status == Status.Disconnected then
							return
						end

						Log.debug("Received {} messages from Rojo server", #messagesPacket.messages)

						-- Applying a patch can yield on fallback requests, during
						-- which the user may cancel or the session may fail, so
						-- the rest of the packet must not be applied afterwards.
						for _, message in messagesPacket.messages do
							if self.__status == Status.Disconnected then
								return
							end
							self:__applyPatch(message)
						end
						if self.__status == Status.Disconnected then
							return
						end
						self.__apiContext:setMessageCursor(messagesPacket.messageCursor)
					end,
				}, function()
					if self.__status ~= Status.Disconnected then
						self.__syncStats.connectedAt = nowMillis()
						self:__setStatus(Status.Connected, serverInfo.projectName)
						if serverInfo.studioBridge and self.__status ~= Status.Disconnected then
							self.__bridgePairing = serverInfo.studioBridge
							local ok, err = pcall(self.restartStudioBridge, self)
							if not ok then
								self.__bridgeError = tostring(err)
								Log.warn("Studio bridge unavailable: {}. Normal syncing remains connected.", err)
							end
						end
					end
				end)
			end)
		end)
		:catch(function(err)
			if self.__status ~= Status.Disconnected then
				self:__stopInternal(err)
			end
		end)
end

function ServeSession:stop()
	self:__stopInternal()
end

function ServeSession:__applyGameAndPlaceId(serverInfo)
	if serverInfo.gameId ~= nil then
		game:SetUniverseId(serverInfo.gameId)
	end

	if serverInfo.placeId ~= nil then
		self.__expectedPlaceId = serverInfo.placeId
		game:SetPlaceId(serverInfo.placeId)
	end
end

function ServeSession:__onPlaceIdChanged()
	if game.PlaceId ~= self.__expectedPlaceId then
		-- Publishing a local file invalidates the allowlist check and hydration
		-- performed for the previous place identity.
		self:__stopInternal("The place ID changed. Reconnect to sync this place with the Rojo server.")
	end
end

function ServeSession:__onActiveScriptChanged(activeScript)
	if not Settings:get("openScriptsExternally") then
		Log.trace("Not opening script {} because feature not enabled.", activeScript)

		return
	end

	if self.__status ~= Status.Connected then
		Log.trace("Not opening script {} because session is not connected.", activeScript)

		return
	end

	local scriptId = self.__instanceMap.fromInstances[activeScript]
	if scriptId == nil then
		Log.trace("Not opening script {} because it is not known by Rojo.", activeScript)

		return
	end

	Log.debug("Trying to open script {} externally...", activeScript)

	-- Force-close the script inside Studio... with a small delay in the middle
	-- to prevent Studio from crashing.
	spawn(function()
		local existingParent = activeScript.Parent
		activeScript.Parent = nil

		for _ = 1, 3 do
			RunService.Heartbeat:Wait()
		end

		activeScript.Parent = existingParent
	end)

	-- Notify the Rojo server to open this script
	self.__apiContext:open(scriptId):catch(function(err)
		Log.warn("Could not open {} externally: {}", activeScript:GetFullName(), err)
	end)
end

function ServeSession:__sendChanges(patch)
	self.__apiContext:write(patch):catch(function(err)
		if self.__status == Status.Disconnected then
			return
		end

		-- The batcher does not keep changes it has flushed, so a failed write
		-- means the server and Studio now disagree. Continuing would sync on
		-- top of that difference without the user knowing.
		local count = PatchSet.countInstances(patch)
		Log.warn("Could not send changes to {} instance(s) to the Rojo server: {}", count, err)
		self:__stopInternal(
			string.format("Couldn't send Studio's changes to the Rojo server: %s\nReconnect to resync.", tostring(err))
		)
	end)
end

-- Separate so specs can supply instances without a real serialized model.
function ServeSession:__deserializeInstances(modelContents)
	return SerializationService:DeserializeInstancesAsync(modelContents)
end

function ServeSession:__replaceInstances(idList)
	if #idList == 0 then
		return true, PatchSet.newEmpty()
	end
	-- It would be annoying if selection went away, so we try to preserve it.
	local selection = Selection:Get()
	local selectionMap = {}
	for i, instance in selection do
		selectionMap[instance] = i
	end

	-- TODO: Should we do this in multiple requests so we can more granularly mark failures?
	local presentIds = idList
	local modelSuccess, replacements = self.__apiContext
		:serialize(idList)
		:andThen(function(response)
			-- The server skips ids removed since the patch was sent. Those
			-- instances are about to be removed by a later message, so they are
			-- left alone here rather than counted as failures.
			if response.missingIds ~= nil and #response.missingIds > 0 then
				local missing = {}
				for _, id in response.missingIds do
					missing[id] = true
				end
				presentIds = {}
				for _, id in idList do
					if not missing[id] then
						table.insert(presentIds, id)
					end
				end
				Log.debug("Serialize endpoint skipped {} removed instances", #idList - #presentIds)
			end
			if #presentIds == 0 then
				return {}
			end

			Log.debug("Deserializing results from serialize endpoint")
			local objects = self:__deserializeInstances(response.modelContents)
			if not objects[1] then
				return Promise.reject("Serialize endpoint did not deserialize into any Instances")
			end
			if #objects[1]:GetChildren() ~= #presentIds then
				return Promise.reject("Serialize endpoint did not return the correct number of Instances")
			end

			local instanceMap = {}
			for _, item in objects[1]:GetChildren() do
				instanceMap[item.Name] = item.Value
			end
			return instanceMap
		end)
		:await()
	if not modelSuccess then
		Log.warn("Could not replace instances through the sync fallback: {}", replacements)
		return false
	end
	if #presentIds == 0 then
		return true, PatchSet.newEmpty()
	end

	local refSuccess, refPatch = self.__apiContext
		:refPatch(presentIds)
		:andThen(function(response)
			return response.patch
		end)
		:await()
	if not refSuccess then
		Log.warn("Could not fetch references for the sync fallback: {}", refPatch)
		return false
	end

	-- Roblox appends to GetChildren() on every reparent, so the order in which
	-- we re-parent replacements determines their final sibling order.
	-- We process ancestors before descendants (so each replacement's
	-- parent already exists when we re-parent it) and siblings in their original
	-- GetChildren() order. Because the loop below moves the old instance's
	-- children into the replacement *before* re-parenting the replacement, this
	-- rebuilds GetChildren() exactly as it was before the swap.
	local swaps = {}
	for id, replacement in replacements do
		local oldInstance = self.__instanceMap.fromIds[id]
		if not oldInstance then
			-- TODO: Why would this happen?
			Log.warn("Instance {} not found in InstanceMap during sync replacement", id)
			continue
		end

		table.insert(swaps, {
			id = id,
			replacement = replacement,
			oldInstance = oldInstance,
		})
	end

	for _, swap in orderSwaps(swaps) do
		local id, replacement, oldInstance = swap.id, swap.replacement, swap.oldInstance

		-- The server's copy is read when the fallback runs, so its file may
		-- have become empty since the patch was sent, and a hold may have
		-- started while the fallback waited. The swap is the wipe the guard
		-- prevents, so it is held like any other.
		if Settings:get("guardScriptWipes") and replacement:IsA("LuaSourceContainer") then
			local newSource = readSource(replacement) or ""
			local oldSource = readSource(oldInstance) or ""
			local lines = countNonBlankLines(oldSource)
			if not string.find(newSource, "%S") and (self.__heldWipes[id] or lines >= WIPE_GUARD_MIN_LINES) then
				if not self.__heldWipes[id] then
					Log.warn(
						"Rojo held back replacing {} ({} lines of code) with an empty copy, because its file on disk is now empty.",
						oldInstance:GetFullName(),
						lines
					)
					self:__setHold(
						id,
						oldInstance,
						{ path = oldInstance:GetFullName(), lines = lines, heldAt = nowMillis() }
					)
				end
				if replacement.ClassName ~= oldInstance.ClassName then
					self.__heldWipes[id].deferredClass = replacement.ClassName
				end
				replacement:Destroy()
				continue
			end
		end

		self.__instanceMap:insert(id, replacement)
		Log.trace("Swapping Instance {} out via api/models/ endpoint", id)
		local oldParent = oldInstance.Parent
		for _, child in oldInstance:GetChildren() do
			-- Some children cannot be reparented, such as a TouchTransmitter
			local reparentSuccess, reparentError = attemptReparent(child, replacement)
			if not reparentSuccess then
				Log.warn(
					"Could not reparent child {} of instance {} during sync replacement: {}",
					child.Name,
					oldInstance.Name,
					reparentError
				)
			end
		end

		-- ChangeHistoryService doesn't like it if an Instance has been
		-- Destroyed. So, we have to accept the potential memory hit and
		-- just set the parent to `nil`.
		local deleteSuccess, deleteError = attemptReparent(oldInstance, nil)
		local replaceSuccess, replaceError = attemptReparent(replacement, oldParent)

		if not (deleteSuccess and replaceSuccess) then
			Log.warn(
				"Could not swap instances {} and {} during sync replacement: {}",
				oldInstance.Name,
				replacement.Name,
				(deleteError or "") .. "\n" .. (replaceError or "")
			)

			-- We need to revert the failed swap to avoid losing the old instance and children.
			for _, child in replacement:GetChildren() do
				attemptReparent(child, oldInstance)
			end
			attemptReparent(oldInstance, oldParent)

			-- Our replacement should never have existed in the first place, so we can just destroy it.
			replacement:Destroy()
			continue
		end

		if selectionMap[oldInstance] then
			-- This is a bit funky, but it saves the order of Selection
			-- which might matter for some use cases.
			selection[selectionMap[oldInstance]] = replacement
		end
	end

	-- Restore the selection even if applying references fails, since the
	-- swapped instances are already in place.
	local patchApplySuccess, unappliedPatch = pcall(self.__reconciler.applyPatch, self.__reconciler, refPatch)
	Selection:Set(selection)
	if not patchApplySuccess then
		error(unappliedPatch, 0)
	end
	return true, unappliedPatch
end

function ServeSession:__applyPatch(patch, options)
	options = options or {}
	if not options.force then
		self:__holdScriptWipes(patch)
	end
	local patchTimestamp = DateTime.now():FormatLocalTime("LTS", "en-us")
	local historyRecording =
		ChangeHistoryService:TryBeginRecording((options.waypoint or PATCH_WAYPOINT_PREFIX) .. patchTimestamp)
	if not historyRecording then
		-- There can only be one recording at a time
		Log.debug("Failed to begin history recording for " .. patchTimestamp .. ". Another recording is in progress.")
	end

	-- A recording left open blocks every later recording in this Studio
	-- session, so it must be finished however patch application exits. Partial
	-- changes are committed so the user can still undo them.
	local success, result = pcall(self.__applyPatchInRecording, self, patch)
	if historyRecording then
		ChangeHistoryService:FinishRecording(historyRecording, Enum.FinishRecordingOperation.Commit)
	end
	if not success then
		error(result, 0)
	end
	-- A removal names only the root of the removed subtree, so holds of
	-- scripts inside it are found by what the instance map still tracks.
	for id in self.__heldWipes do
		if self.__instanceMap.fromIds[id] == nil then
			self:__clearHold(id)
		end
	end
	-- Generations of removed scripts are forgotten, so they do not pile up
	-- over a long session. A pending write for one then sees no generation,
	-- which cancels it as superseded.
	for id in self.__sourceGenerations do
		if self.__instanceMap.fromIds[id] == nil then
			self.__sourceGenerations[id] = nil
		end
	end
	local stats = self.__syncStats
	stats.patchesApplied += 1
	stats.lastPatchAt = nowMillis()
	stats.lastPatchUnapplied = PatchSet.countInstances(result)
	return result
end

--[=[
	Takes the source changes of existing scripts out of the patch the
	reconciler applies, so they can be written by `__writeSource`. Returns the
	patch for the reconciler and the writes. The patch itself is left as it
	is, since hooks show its changes to the user.
]=]
local function stringSource(encoded)
	local ty, value
	if type(encoded) == "table" then
		ty, value = next(encoded)
	end
	if ty == "String" and type(value) == "string" then
		return value
	end
	return nil
end

function ServeSession:__splitSourceWrites(patch)
	-- Every script source goes through `__writeSource`, including those of
	-- new scripts and of scripts recreated by a class change: the reconciler's
	-- own writer finishes later without checking holds or newer writes. The
	-- reconciler gets the patch without those sources, and the writes are
	-- made once it has created the instances. Instances are looked up then.
	local writes = {}
	local updated = {}
	for _, update in patch.updated do
		local target = stringSource(update.changedProperties and update.changedProperties.Source)
		local instance = self.__instanceMap.fromIds[update.id]
		local isScript = if update.changedClassName ~= nil
			then SCRIPT_CLASSES[update.changedClassName] == true
			else instance ~= nil and instance:IsA("LuaSourceContainer")
		if target == nil or not isScript then
			table.insert(updated, update)
			continue
		end
		table.insert(writes, { id = update.id, target = target })
		local rest = table.clone(update)
		rest.changedProperties = table.clone(update.changedProperties)
		rest.changedProperties.Source = nil
		local empty = next(rest.changedProperties) == nil
			and rest.changedName == nil
			and rest.changedMetadata == nil
			and rest.changedClassName == nil
		if not empty then
			table.insert(updated, rest)
		end
	end
	local added = {}
	for id, virtual in patch.added do
		local target = SCRIPT_CLASSES[virtual.ClassName]
			and virtual.Properties
			and stringSource(virtual.Properties.Source)
		if target then
			local copy = table.clone(virtual)
			copy.Properties = table.clone(virtual.Properties)
			copy.Properties.Source = nil
			added[id] = copy
			table.insert(writes, { id = id, target = target })
		else
			added[id] = virtual
		end
	end
	return { removed = patch.removed, added = added, updated = updated }, writes
end

--[=[
	Writes a script's source as the reconciler would, but decides in the
	editor's update callback, which runs when Studio commits the write: by
	then a newer change may have replaced this one, or a hold may have started
	for the script, and either cancels it.
]=]
function ServeSession:__writeSource(write)
	local generation = (self.__sourceGenerations[write.id] or 0) + 1
	self.__sourceGenerations[write.id] = generation
	self.__instanceMap:pauseInstance(write.instance)
	self.__syncStats.pendingSourceWrites += 1
	task.spawn(function()
		local ok, err = pcall(function()
			ScriptEditorService:UpdateSourceAsync(write.instance, function(current)
				if self.__status == Status.Disconnected or self.__sourceGenerations[write.id] ~= generation then
					return nil
				end
				local blank = not string.find(write.target, "%S")
				if blank and Settings:get("guardScriptWipes") then
					local held = self.__heldWipes[write.id] or heldByInstance[write.instance]
					local lines = countNonBlankLines(current)
					if held or lines >= WIPE_GUARD_MIN_LINES then
						if not held then
							Log.warn(
								"Rojo held back emptying {} ({} lines of code) because its file on disk is now empty.",
								write.instance:GetFullName(),
								lines
							)
							self:__setHold(
								write.id,
								write.instance,
								{ path = write.instance:GetFullName(), lines = lines, heldAt = nowMillis() }
							)
						end
						return nil
					end
				end
				return write.target
			end)
		end)
		self.__syncStats.pendingSourceWrites -= 1
		if not ok then
			self.__syncStats.failedSourceWrites += 1
			Log.warn("Could not write the source of {}: {}", write.instance:GetFullName(), err)
		end
	end)
end

function ServeSession:__applyPatchInRecording(patch)
	Timer.start("precommitCallbacks")
	-- Precommit callbacks must be serial in order to obey the contract that
	-- they execute before commit
	for _, callback in self.__precommitCallbacks do
		local success, err = pcall(callback, patch, self.__instanceMap)
		if not success then
			Log.warn("Precommit hook errored: {}", err)
		end
	end
	Timer.stop()

	local reconcilerPatch, sourceWrites = self:__splitSourceWrites(patch)
	local unappliedPatch = self.__reconciler:applyPatch(reconcilerPatch)
	for _, write in sourceWrites do
		write.instance = self.__instanceMap.fromIds[write.id]
		if write.instance and write.instance:IsA("LuaSourceContainer") then
			self:__writeSource(write)
			local carried = self.__pendingHoldTransfers[write.id]
			if carried then
				self:__setHold(write.id, write.instance, {
					path = write.instance:GetFullName(),
					lines = carried.lines,
					heldAt = carried.heldAt,
				})
			end
		end
	end
	table.clear(self.__pendingHoldTransfers)

	-- The fallback replaces an instance with the server's whole copy, which
	-- for a held script has the empty source the guard held back. Those
	-- updates stay unapplied instead.
	local heldUpdates = {}
	local index = 1
	while index <= #unappliedPatch.updated do
		if Settings:get("guardScriptWipes") and self.__heldWipes[unappliedPatch.updated[index].id] then
			table.insert(heldUpdates, table.remove(unappliedPatch.updated, index))
		else
			index += 1
		end
	end

	if Settings:get("enableSyncFallback") and not PatchSet.isEmpty(unappliedPatch) then
		-- Some changes did not apply, let's try replacing them instead
		local addedIdList = PatchSet.addedIdList(unappliedPatch)
		local updatedIdList = PatchSet.updatedIdList(unappliedPatch)

		Log.debug("ServeSession:__replaceInstances(unappliedPatch.added)")
		Timer.start("ServeSession:__replaceInstances(unappliedPatch.added)")
		local addSuccess, unappliedAddedRefs = self:__replaceInstances(addedIdList)
		Timer.stop()

		Log.debug("ServeSession:__replaceInstances(unappliedPatch.updated)")
		Timer.start("ServeSession:__replaceInstances(unappliedPatch.updated)")
		local updateSuccess, unappliedUpdateRefs = self:__replaceInstances(updatedIdList)
		Timer.stop()

		-- Update the unapplied patch to reflect which Instances were replaced successfully
		if addSuccess then
			table.clear(unappliedPatch.added)
			PatchSet.assign(unappliedPatch, unappliedAddedRefs)
		end
		if updateSuccess then
			table.clear(unappliedPatch.updated)
			PatchSet.assign(unappliedPatch, unappliedUpdateRefs)
		end
	end
	for _, update in heldUpdates do
		table.insert(unappliedPatch.updated, update)
	end
	local refresh = {}
	for id in self.__refreshAfterHold do
		table.insert(refresh, id)
	end
	if #refresh > 0 then
		table.clear(self.__refreshAfterHold)
		local refreshed = self:__replaceInstances(refresh)
		if not refreshed then
			Log.warn("Could not apply a class change Rojo held back with a script's wipe. Reconnect to sync it.")
		end
	end

	if not PatchSet.isEmpty(unappliedPatch) then
		Log.debug(
			"Could not apply all changes requested by the Rojo server:\n{}",
			PatchSet.humanSummary(self.__instanceMap, unappliedPatch)
		)
	end

	Timer.start("postcommitCallbacks")
	-- Postcommit callbacks can be called with spawn since regardless of firing order, they are
	-- guaranteed to be called after the commit
	for _, callback in self.__postcommitCallbacks do
		task.spawn(function()
			local success, err = pcall(callback, patch, self.__instanceMap, unappliedPatch)
			if not success then
				Log.warn("Postcommit hook errored: {}", err)
			end
		end)
	end
	Timer.stop()

	return unappliedPatch
end

function ServeSession:__initialSync(serverInfo)
	return self.__apiContext:read({ serverInfo.rootInstanceId }):andThen(function(readResponseBody)
		if self.__status == Status.Disconnected then
			return
		end

		-- Tell the API Context that we're up-to-date with the version of
		-- the tree defined in this response.
		self.__apiContext:setMessageCursor(readResponseBody.messageCursor)

		-- For any instances that line up with the Rojo server's view, start
		-- tracking them in the reconciler.
		Log.trace("Matching existing Roblox instances to Rojo IDs")
		self:setLoadingText("Hydrating instance map...")
		self.__reconciler:hydrate(readResponseBody.instances, serverInfo.rootInstanceId, game)
		-- Before the catch-up patch, which the wipe guard checks against them.
		self:__rematchHolds(readResponseBody.instances)
		self:__adoptHolds()
		self:__releaseRecoveredHolds(readResponseBody.instances)

		-- Calculate the initial patch to apply to the DataModel to catch us
		-- up to what Rojo thinks the place should look like.
		Log.trace("Computing changes that plugin needs to make to catch up to server...")
		self:setLoadingText("Finding differences between server and Studio...")
		local success, catchUpPatch =
			self.__reconciler:diff(readResponseBody.instances, serverInfo.rootInstanceId, game)

		if not success then
			Log.error("Could not compute a diff to catch up to the Rojo server: {:#?}", catchUpPatch)
		end

		-- A held script that could not be paired again, for example because
		-- it was renamed or moved in Studio, would be removed with whatever
		-- contains it. It is kept instead, unsynced, so its code is not lost.
		self:__keepUnpairedHolds(catchUpPatch)

		for _, update in catchUpPatch.updated do
			if update.id == self.__instanceMap.fromInstances[game] and update.changedClassName ~= nil then
				-- Non-place projects will try to update the classname of game from DataModel to
				-- something like Folder, ModuleScript, etc. This would fail, so we exit with a clear
				-- message instead of crashing.
				return Promise.reject(
					"Cannot sync a model as a place."
						.. "\nEnsure Rojo is serving a project file that has a DataModel at the root of its tree and try again."
						.. "\nSee project file docs: https://rojo.space/docs/v7/project-format/"
				)
			end
		end

		Log.trace("Computed hydration patch: {:#?}", debugPatch(catchUpPatch))

		local userDecision = "Accept"
		if self.__userConfirmCallback ~= nil then
			userDecision = self.__userConfirmCallback(self.__instanceMap, catchUpPatch, serverInfo)
		end
		-- Confirmation can yield while the user disconnects or unloads the plugin.
		if self.__status == Status.Disconnected then
			return
		end

		if userDecision == "Abort" then
			return Promise.reject("Aborted Rojo sync operation")
		elseif userDecision == "Reject" then
			if not self.__twoWaySync then
				return Promise.reject("Cannot reject sync operation without two-way sync enabled")
			end
			-- The user wants their studio DOM to write back to their Rojo DOM
			-- so we will reverse the patch and send it back

			local inversePatch = PatchSet.newEmpty()

			-- Send back the current properties
			for _, change in catchUpPatch.updated do
				local instance = self.__instanceMap.fromIds[change.id]
				if not instance then
					continue
				end

				local update = encodePatchUpdate(instance, change.id, change.changedProperties)
				table.insert(inversePatch.updated, update)
			end
			-- Add the removed instances back to Rojo
			-- selene:allow(empty_if, unused_variable, empty_loop)
			for _, instance in catchUpPatch.removed do
				-- TODO: Generate ID for our instance and add it to inversePatch.added
			end
			-- Remove the additions we've rejected
			for id, _change in catchUpPatch.added do
				table.insert(inversePatch.removed, id)
			end

			return self.__apiContext:write(inversePatch)
		elseif userDecision == "Accept" then
			self:__applyPatch(catchUpPatch)
			return Promise.resolve()
		else
			return Promise.reject("Invalid user decision: " .. userDecision)
		end
	end)
end

function ServeSession:__stopInternal(err)
	if self.__status == Status.Disconnected then
		return
	end

	self.__status = Status.Disconnected
	if self.__disconnectGuardSetting then
		self.__disconnectGuardSetting()
		self.__disconnectGuardSetting = nil
	end
	if self.__studioBridge then
		self.__studioBridge:stop()
		self.__studioBridge = nil
	end
	self.__apiContext:disconnect()
	self.__instanceMap:stop()
	self.__changeBatcher:stop()

	for _, connection in ipairs(self.__connections) do
		connection:Disconnect()
	end
	self.__connections = {}
	self:__setStatus(Status.Disconnected, err)
end

function ServeSession:__setStatus(status, detail)
	self.__status = status

	if self.__statusChangedCallback ~= nil then
		self.__statusChangedCallback(status, detail)
	end
end

return ServeSession
