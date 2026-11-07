--[[
	Copies of Studio instances that Rojo is about to change or remove, saved
	by the server in the same folder as `rojo studio backup create`, so a
	resync that went wrong can be put back with `rojo studio backup restore`.
]]
local HttpService = game:GetService("HttpService")
local SerializationService = game:GetService("SerializationService")

local StudioBridge = require(script.Parent.StudioBridge)
local hashSource = require(script.Parent.hashSource)

local StudioBackup = {}

-- The server's model limit (MAX_MODEL_BYTES in src/web/studio.rs).
StudioBackup.MAX_MODEL_BYTES = 5 * 1024 * 1024
-- Restoring counts every instance against the import limit, so a larger
-- backup could never be put back.
StudioBackup.MAX_INSTANCES = 10_000
-- The most script source a backup reads before it is serialized. A subtree
-- within the instance limit can still hold hundreds of MB of source, which
-- would freeze Studio while it is read, hashed and serialized only to fail
-- the model limit afterwards.
StudioBackup.MAX_SOURCE_BYTES = 16 * 1024 * 1024
-- Scripts handled between yields, so reading many keeps Studio responsive.
StudioBackup.SCRIPTS_PER_YIELD = 200
-- The most other variable-size data, StringValue values and attributes, a
-- backup reads before it is serialized, for the same reason.
StudioBackup.MAX_CONTENT_BYTES = 16 * 1024 * 1024
-- How long preparing a backup may take before it is given up, checked
-- between steps, so a slow one cannot hold up the sync for long.
StudioBackup.MAX_PREPARE_SECONDS = 10
local MAX_BODY_BYTES = 8 * 1024 * 1024

-- The folder in a backup that holds scripts as their open editors show them.
StudioBackup.DRAFTS_FOLDER = "Rojo editor drafts"
-- The attribute on each script copy in a `verify --fix` backup naming the
-- script it copies, by its full path in the place.
StudioBackup.ORIGIN_ATTRIBUTE = "RojoBackupOf"

-- Services and the DataModel cannot be saved as models. Errors when the
-- instance cannot be read.
local function canCopy(instance)
	return instance ~= game and instance.Parent ~= game
end

-- What to call an instance whose properties Studio will not let Rojo read.
StudioBackup.INACCESSIBLE = "an instance Rojo cannot read"

-- The instance's path, or INACCESSIBLE when it cannot be read.
local function safePath(instance)
	local ok, path = pcall(StudioBridge.pathOf, instance)
	return if ok then path else StudioBackup.INACCESSIBLE
end

-- The instance's parent, or nil when it cannot be read.
local function safeParent(instance)
	local ok, parent = pcall(function()
		return instance.Parent
	end)
	return if ok then parent else nil
end

local function hasChanges(update)
	return update.changedName ~= nil
		or update.changedClassName ~= nil
		or (update.changedProperties ~= nil and next(update.changedProperties) ~= nil)
end

--[[
	The instances in Studio that applying `patch` would remove or change, as
	backup roots sorted by path, leaving out any inside another root, whose
	copy holds them already. Also returns the sorted paths of those that
	cannot be backed up, such as services, which the patch changes anyway, or
	instances whose properties cannot be read, which are skipped rather than
	stopping the backup.
]]
function StudioBackup.collect(patch, instanceMap)
	local candidates = {}
	local skipped = {}
	local function consider(instance)
		if instance == nil or candidates[instance] or skipped[instance] then
			return
		end
		local read, copyable = pcall(canCopy, instance)
		if read and copyable then
			candidates[instance] = true
		else
			skipped[instance] = true
		end
	end
	for _, removed in patch.removed do
		consider(if typeof(removed) == "Instance" then removed else instanceMap.fromIds[removed])
	end
	for _, update in patch.updated do
		if hasChanges(update) then
			consider(instanceMap.fromIds[update.id])
		end
	end

	local roots = {}
	for instance in candidates do
		local inside = false
		local parent = safeParent(instance)
		while parent do
			if candidates[parent] then
				inside = true
				break
			end
			parent = safeParent(parent)
		end
		if inside then
			continue
		end
		local read, path = pcall(StudioBridge.pathOf, instance)
		if read then
			table.insert(roots, { instance = instance, path = path })
		else
			skipped[instance] = true
		end
	end
	table.sort(roots, function(a, b)
		return a.path < b.path
	end)
	local skippedPaths = {}
	for instance in skipped do
		table.insert(skippedPaths, safePath(instance))
	end
	table.sort(skippedPaths)
	return roots, skippedPaths
end

-- How many instances a backup of `roots` holds, counted before anything is
-- copied or serialized.
function StudioBackup.countInstances(roots)
	local count = 0
	for _, root in roots do
		count += #root.instance:GetDescendants() + 1
	end
	return count
end

--[[
	A new script of the same class as `script`, with its name, `source`, the
	properties that decide how it runs and its attributes, but none of its
	children. Built with Instance.new rather than Clone, which would copy
	every descendant.
]]
function StudioBackup.copyScript(script, source)
	local copy = Instance.new(script.ClassName)
	copy.Name = script.Name;
	(copy :: any).Source = source
	for _, property in { "Enabled", "RunContext" } do
		pcall(function()
			(copy :: any)[property] = (script :: any)[property]
		end)
	end
	for name, value in script:GetAttributes() do
		copy:SetAttribute(name, value)
	end
	return copy
end

-- Serializes `instances` as one model. Replaced by tests.
function StudioBackup.serializeInstances(instances)
	return SerializationService:SerializeInstancesAsync(instances)
end

-- Seconds, for the time budget. Replaced by tests.
StudioBackup.clock = os.clock

-- Deserializes a model, to check what serializing emitted. Replaced by tests.
function StudioBackup.deserializeInstances(model)
	return SerializationService:DeserializeInstancesAsync(model)
end

-- Fails once preparing a backup has taken longer than MAX_PREPARE_SECONDS.
local function checkTime(started)
	assert(
		StudioBackup.clock() - started <= StudioBackup.MAX_PREPARE_SECONDS,
		string.format("preparing it took longer than %g seconds", StudioBackup.MAX_PREPARE_SECONDS)
	)
end

-- Runs `work` on its own thread and waits for it at most what is left of
-- the time budget since `started`. On time out its result is dropped.
local function withinBudget(started, work)
	checkTime(started)
	local finished, ok, result = false, false, nil
	task.spawn(function()
		ok, result = pcall(work)
		finished = true
	end)
	while not finished do
		task.wait()
		if not finished then
			checkTime(started)
		end
	end
	if not ok then
		error(result, 0)
	end
	return result
end

-- Yields every SCRIPTS_PER_YIELD calls, counted in `counter.n`, and checks
-- the time budget when `started` is given.
local function pace(counter, started)
	counter.n += 1
	if counter.n % StudioBackup.SCRIPTS_PER_YIELD == 0 then
		task.wait()
		if started then
			checkTime(started)
		end
	end
end

-- The bytes of variable-size data an instance holds that are cheap to read:
-- a StringValue's value, the text of a TextLabel and the like, and its
-- attributes. Script source is counted separately.
local function contentBytes(instance)
	local bytes = 0
	if instance:IsA("StringValue") then
		local ok, value = pcall(function()
			return (instance :: any).Value
		end)
		if ok and type(value) == "string" then
			bytes += #value
		end
	end
	-- Many classes have a `Text`; reading it on others just fails.
	local hasText, text = pcall(function()
		return (instance :: any).Text
	end)
	if hasText and type(text) == "string" then
		bytes += #text
	end
	local ok, attributes = pcall(instance.GetAttributes, instance)
	if ok then
		for name, value in attributes do
			bytes += #name + (if type(value) == "string" then #value else 16)
		end
	end
	return bytes
end

--[[
	Walks `roots` and their descendants before anything is hashed or
	serialized. Fails as soon as their script source passes MAX_SOURCE_BYTES,
	or their other variable-size data (StringValue values and attributes)
	passes MAX_CONTENT_BYTES, or the walk takes longer than the time budget
	since `started`, so a huge subtree costs little before it is refused.
	Returns the scripts, and the paths of instances below the roots that are
	not Archivable, which SerializeInstancesAsync leaves out along with what
	is inside them.
]]
function StudioBackup.preflight(roots, started)
	started = started or StudioBackup.clock()
	local scripts = {}
	local unarchivable = {}
	local sourceBytes = 0
	local otherBytes = 0
	local counter = { n = 0 }
	for _, root in roots do
		local candidates = root.instance:GetDescendants()
		table.insert(candidates, root.instance)
		for _, candidate in candidates do
			pace(counter, started)
			local readArchivable, archivable = pcall(function()
				return candidate.Archivable
			end)
			if readArchivable and archivable == false then
				table.insert(unarchivable, (StudioBridge.pathOf(candidate)))
			end
			otherBytes += contentBytes(candidate)
			assert(
				otherBytes <= StudioBackup.MAX_CONTENT_BYTES,
				string.format(
					"its values and attributes hold more than %d MiB, over what a backup reads",
					StudioBackup.MAX_CONTENT_BYTES // (1024 * 1024)
				)
			)
			if not candidate:IsA("LuaSourceContainer") then
				continue
			end
			local read, source = pcall(function()
				return (candidate :: any).Source
			end)
			if read and type(source) == "string" then
				sourceBytes += #source
			end
			assert(
				sourceBytes <= StudioBackup.MAX_SOURCE_BYTES,
				string.format(
					"its scripts hold more than %d MiB of source, over what a backup reads",
					StudioBackup.MAX_SOURCE_BYTES // (1024 * 1024)
				)
			)
			table.insert(scripts, candidate)
		end
	end
	table.sort(unarchivable)
	return scripts, unarchivable
end

local function serializeChecked(instances)
	local model = StudioBackup.serializeInstances(instances)
	assert(
		buffer.len(model) <= StudioBackup.MAX_MODEL_BYTES,
		string.format("the copy is %.1f MiB, over the 5 MiB model limit", buffer.len(model) / (1024 * 1024))
	)
	return model
end

-- Checks that `model` holds exactly `instances` as its roots, in order, so
-- the record's list of roots says what the model holds.
local function checkRoundTrip(model, instances)
	local copies = StudioBackup.deserializeInstances(model)
	local matches = #copies == #instances
	if matches then
		for index, copy in copies do
			local original = instances[index]
			if copy.Name ~= original.Name or copy.ClassName ~= original.ClassName then
				matches = false
				break
			end
		end
	end
	local count = #copies
	for _, copy in copies do
		copy:Destroy()
	end
	assert(matches, string.format("the model holds %d roots, not the %d it was given, in order", count, #instances))
end

--[[
	Serializes `roots` from `collect` as they are in the place, with
	everything inside them, in one native model. Serializing them in one call
	keeps references between instances in the backup, such as an ObjectValue
	pointing at another backed-up instance; references to instances outside
	the backup are not kept. Nothing in the place is copied, and the size is
	counted first, so a backup over the import limit fails before any work.
	A root that is not Archivable cannot be in the model, so it is left out
	and named with the others that are not.

	`readEditor`, when given, reads the text a script's editor holds, or
	returns nil when it cannot. The model only holds `Source`, so a script
	whose editor shows other text also gets a draft copy with that text, in a
	folder named DRAFTS_FOLDER. Returns:

	- the model, checked to hold exactly the roots recorded, in order;
	- the paths and classes of its roots;
	- the paths of scripts whose editor text or source could not be read,
	  whose unsaved text the backup may therefore miss;
	- `captured`, a list of `{ instance, path, hash }` with the hash of the
	  text the backup holds for every other script, so `changedSince` can
	  tell which were edited after the backup was taken;
	- the paths of instances that are not Archivable and so not in it;
	- `guarded`, mapping every script in the backup to the hash of the text
	  it holds, or `false` when that text could not be read, so a later write
	  can tell whether it would overwrite what the backup does not hold.
]]
function StudioBackup.serialize(roots, readEditor)
	local started = StudioBackup.clock()
	local unarchivable = {}
	local kept = {}
	for _, root in roots do
		local read, archivable = pcall(function()
			return root.instance.Archivable
		end)
		if read and archivable == false then
			table.insert(unarchivable, root.path or (StudioBridge.pathOf(root.instance)))
		else
			table.insert(kept, root)
		end
	end
	roots = kept
	assert(#roots > 0, "none of the instances can be saved, since none is Archivable")
	local count = StudioBackup.countInstances(roots)
	assert(
		count <= StudioBackup.MAX_INSTANCES,
		string.format("it would hold %d instances, over the %d a backup can restore", count, StudioBackup.MAX_INSTANCES)
	)
	local scripts, below = StudioBackup.preflight(roots, started)
	for _, path in below do
		table.insert(unarchivable, path)
	end
	table.sort(unarchivable)
	local instances = {}
	local described = {}
	local unreadable = {}
	local captured = {}
	local guarded = {}
	local drafts
	local counter = { n = 0 }
	-- `preflight` counted saved sources, but an editor can hold far more
	-- unsaved text, which the backup holds too as a draft. So what is read is
	-- counted again here, against the same budget, before serializing.
	local capturedBytes = 0
	local function capture(bytes)
		capturedBytes += bytes
		assert(
			capturedBytes <= StudioBackup.MAX_SOURCE_BYTES,
			string.format(
				"its scripts hold more than %d MiB of source and unsaved editor text, over what a backup reads",
				StudioBackup.MAX_SOURCE_BYTES // (1024 * 1024)
			)
		)
	end
	local ok, result = pcall(function()
		for _, root in roots do
			table.insert(instances, root.instance)
			table.insert(described, {
				path = root.path or (StudioBridge.pathOf(root.instance)),
				className = root.instance.ClassName,
			})
		end
		if readEditor then
			for _, script in scripts do
				pace(counter, started)
				local editor = readEditor(script)
				local read, saved = pcall(function()
					return (script :: any).Source
				end)
				if editor == nil or not read or type(saved) ~= "string" then
					table.insert(unreadable, (StudioBridge.pathOf(script)))
					guarded[script] = false
					if read and type(saved) == "string" then
						capture(#saved)
					end
					continue
				end
				-- The saved source is in the model, and a different editor text
				-- is a draft beside it, so both count.
				capture(if editor == saved then #saved else #saved + #editor)
				local hash = (hashSource(editor))
				guarded[script] = hash
				table.insert(captured, {
					instance = script,
					path = (StudioBridge.pathOf(script)),
					hash = hash,
				})
				if editor == saved then
					continue
				end
				if drafts == nil then
					drafts = Instance.new("Folder")
					drafts.Name = StudioBackup.DRAFTS_FOLDER
					count += 1
				end
				count += 1
				assert(
					count <= StudioBackup.MAX_INSTANCES,
					string.format("it would hold more than %d instances", StudioBackup.MAX_INSTANCES)
				)
				local draft = StudioBackup.copyScript(script, editor)
				draft:SetAttribute("RojoDraftOf", (StudioBridge.pathOf(script)))
				draft.Parent = drafts
			end
		end
		if drafts then
			table.insert(instances, drafts)
			table.insert(described, { path = StudioBackup.DRAFTS_FOLDER, className = "Folder" })
		end
		-- Reading yielded, and the roots may have grown meanwhile, so they are
		-- counted again: a backup over the limit could never be restored.
		local now = StudioBackup.countInstances(roots) + (if drafts then #drafts:GetChildren() + 1 else 0)
		assert(
			now <= StudioBackup.MAX_INSTANCES,
			string.format(
				"it would hold %d instances, over the %d a backup can restore",
				now,
				StudioBackup.MAX_INSTANCES
			)
		)
		-- Serializing, and reading the model back, are single engine calls
		-- that can take long; they get what is left of the time budget, and a
		-- result that comes later is dropped.
		return withinBudget(started, function()
			local model = serializeChecked(instances)
			checkRoundTrip(model, instances)
			return model
		end)
	end)
	if drafts then
		drafts:Destroy()
	end
	if not ok then
		error(result, 0)
	end
	return result, described, unreadable, captured, unarchivable, guarded
end

--[[
	The paths of the scripts in `captured`, from `serialize`, whose editor
	text `readEditor` now reads differently from what the backup holds, such
	as a script someone typed in while the backup was sent. Scripts whose
	text cannot be read now are left out, since that was warned about.
]]
function StudioBackup.changedSince(captured, readEditor)
	local changed = {}
	local counter = { n = 0 }
	for _, entry in captured do
		pace(counter)
		local ok, text = pcall(readEditor, entry.instance)
		if ok and text ~= nil and (hashSource(text)) ~= entry.hash then
			table.insert(changed, entry.path)
		end
	end
	return changed
end

--[[
	Serializes a copy of each script in `entries`, each `{ instance, source }`,
	made with copyScript from that exact source and without the script's
	children, for the backup `rojo studio verify --fix` takes of the scripts
	it overwrites.
]]
function StudioBackup.serializeScripts(entries)
	assert(#entries <= StudioBackup.MAX_INSTANCES, "too many scripts for one backup")
	local copies = {}
	local described = {}
	local ok, result = pcall(function()
		for _, entry in entries do
			local script = entry.instance
			local path = (StudioBridge.pathOf(script))
			local copy = StudioBackup.copyScript(script, entry.source)
			-- Many copies can share a name, such as a module per feature, so
			-- each says where it came from. The copy is new, so the script in
			-- the place is not touched.
			copy:SetAttribute(StudioBackup.ORIGIN_ATTRIBUTE, path)
			table.insert(copies, copy)
			table.insert(described, { path = path, className = script.ClassName })
		end
		return serializeChecked(copies)
	end)
	for _, copy in copies do
		copy:Destroy()
	end
	if not ok then
		error(result, 0)
	end
	return result, described
end

--[[
	Sends a backup to the server's Studio bridge, which saves it. `pairing`
	is the bridge pairing from the server's info, whose plugin token is what
	the bridge accepts from this plugin. Returns the server's answer, with the
	saved record as `backup`.
]]
function StudioBackup.send(baseUrl, pairing, payload)
	assert(
		type(pairing) == "table" and pairing.protocolVersion == 1 and type(pairing.pluginToken) == "string",
		"the server was not started with --studio-token-file, so it has no Studio bridge to save backups"
	)
	assert(StudioBridge.isLoopbackUrl(baseUrl), "backups are only sent to a Rojo server on this computer")
	payload.identity = pairing.identity
	local body = HttpService:JSONEncode(StudioBridge.sanitize(payload))
	assert(#body <= MAX_BODY_BYTES, "the backup request would be over 8 MiB")
	local response = StudioBridge.requestAsync({
		Url = baseUrl .. "/api/studio/backup",
		Method = "POST",
		Headers = { ["Content-Type"] = "application/json", ["x-rojo-studio-token"] = pairing.pluginToken },
		Body = body,
	})
	local decoded, value = pcall(HttpService.JSONDecode, HttpService, response.Body)
	if not response.Success then
		local reason = if decoded
				and type(value) == "table"
				and type(value.error) == "string"
			then value.error
			else tostring(response.StatusCode)
		error("the server did not save it: " .. reason, 0)
	end
	assert(
		decoded and type(value) == "table" and type(value.backup) == "table",
		"the server's answer was not understood"
	)
	return value
end

return StudioBackup
