--[[
	Copies of Studio instances that Rojo is about to change or remove, saved
	by the server in the same folder as `rojo studio backup create`, so a
	resync that went wrong can be put back with `rojo studio backup restore`.
]]
local HttpService = game:GetService("HttpService")
local SerializationService = game:GetService("SerializationService")

local StudioBridge = require(script.Parent.StudioBridge)

local StudioBackup = {}

-- The server's model limit (MAX_MODEL_BYTES in src/web/studio.rs).
StudioBackup.MAX_MODEL_BYTES = 5 * 1024 * 1024
-- Restoring counts every instance against the import limit, so a larger
-- backup could never be put back.
StudioBackup.MAX_INSTANCES = 10_000
local MAX_BODY_BYTES = 8 * 1024 * 1024

-- The folder in a backup that holds scripts as their open editors show them.
StudioBackup.DRAFTS_FOLDER = "Rojo editor drafts"

-- Services and the DataModel cannot be saved as models.
local function canCopy(instance)
	return instance ~= game and instance.Parent ~= game
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
	cannot be backed up, such as services, which the patch changes anyway.
]]
function StudioBackup.collect(patch, instanceMap)
	local candidates = {}
	local skipped = {}
	local function consider(instance)
		if instance == nil or candidates[instance] or skipped[instance] then
			return
		end
		if canCopy(instance) then
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
		local parent = instance.Parent
		while parent do
			if candidates[parent] then
				inside = true
				break
			end
			parent = parent.Parent
		end
		if not inside then
			table.insert(roots, { instance = instance, path = (StudioBridge.pathOf(instance)) })
		end
	end
	table.sort(roots, function(a, b)
		return a.path < b.path
	end)
	local skippedPaths = {}
	for instance in skipped do
		table.insert(skippedPaths, (StudioBridge.pathOf(instance)))
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

local function serializeChecked(instances)
	local model = SerializationService:SerializeInstancesAsync(instances)
	assert(
		buffer.len(model) <= StudioBackup.MAX_MODEL_BYTES,
		string.format("the copy is %.1f MiB, over the 5 MiB model limit", buffer.len(model) / (1024 * 1024))
	)
	return model
end

--[[
	Serializes `roots` from `collect` as they are in the place, with
	everything inside them, in one native model. Serializing them in one call
	keeps references between instances in the backup, such as an ObjectValue
	pointing at another backed-up instance; references to instances outside
	the backup are not kept. Nothing in the place is copied, and the size is
	counted first, so a backup over the import limit fails before any work.

	`readSource`, when given, reads a script's source the way sync does. The
	model only holds `Source`, so a script whose open editor shows other text
	also gets a draft copy with that text, in a folder named DRAFTS_FOLDER.
	Returns the model and the paths and classes of its roots.
]]
function StudioBackup.serialize(roots, readSource)
	local count = StudioBackup.countInstances(roots)
	assert(
		count <= StudioBackup.MAX_INSTANCES,
		string.format("it would hold %d instances, over the %d a backup can restore", count, StudioBackup.MAX_INSTANCES)
	)
	local instances = {}
	local described = {}
	local drafts
	local ok, result = pcall(function()
		for _, root in roots do
			table.insert(instances, root.instance)
			table.insert(described, {
				path = root.path or (StudioBridge.pathOf(root.instance)),
				className = root.instance.ClassName,
			})
			if not readSource then
				continue
			end
			local scripts = root.instance:GetDescendants()
			table.insert(scripts, root.instance)
			for _, script in scripts do
				if not script:IsA("LuaSourceContainer") then
					continue
				end
				local editor = readSource(script)
				local read, saved = pcall(function()
					return (script :: any).Source
				end)
				if editor == nil or not read or editor == saved then
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
		return serializeChecked(instances)
	end)
	if drafts then
		drafts:Destroy()
	end
	if not ok then
		error(result, 0)
	end
	return result, described
end

--[[
	Serializes a copy of each script in `scripts`, made with copyScript from
	the source `readSource` reads (what an open editor shows), without the
	scripts' children, for the backup `rojo studio verify --fix` takes of the
	scripts it overwrites.
]]
function StudioBackup.serializeScripts(scripts, readSource)
	assert(#scripts <= StudioBackup.MAX_INSTANCES, "too many scripts for one backup")
	local copies = {}
	local described = {}
	local ok, result = pcall(function()
		for _, script in scripts do
			local source = if readSource then readSource(script) else nil
			if source == nil then
				source = (script :: any).Source
			end
			table.insert(copies, StudioBackup.copyScript(script, source))
			table.insert(described, { path = (StudioBridge.pathOf(script)), className = script.ClassName })
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
