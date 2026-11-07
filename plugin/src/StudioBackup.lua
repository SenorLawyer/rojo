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
	The instances in Studio that applying `patch` would remove or change.
	Removed instances are copied with everything inside them; changed ones
	without their children, which a change to a parent leaves alone, so a
	changed folder does not copy a whole map. Returns the entries, sorted by
	path, and how many instances cannot be copied, such as services.
]]
function StudioBackup.collect(patch, instanceMap)
	local deep = {}
	local shallow = {}
	local skipped = 0
	for _, removed in patch.removed do
		local instance = if typeof(removed) == "Instance" then removed else instanceMap.fromIds[removed]
		if instance == nil then
			continue
		end
		if canCopy(instance) then
			deep[instance] = true
		else
			skipped += 1
		end
	end
	for _, update in patch.updated do
		local instance = instanceMap.fromIds[update.id]
		if instance == nil or not hasChanges(update) then
			continue
		end
		if canCopy(instance) then
			shallow[instance] = true
		else
			skipped += 1
		end
	end

	-- An instance inside a removed one is already in that one's copy.
	local function insideRemoved(instance)
		local parent = instance.Parent
		while parent do
			if deep[parent] then
				return true
			end
			parent = parent.Parent
		end
		return false
	end

	local entries = {}
	for instance in deep do
		if not insideRemoved(instance) then
			table.insert(entries, { instance = instance, deep = true, path = (StudioBridge.pathOf(instance)) })
		end
	end
	for instance in shallow do
		if not deep[instance] and not insideRemoved(instance) then
			table.insert(entries, { instance = instance, deep = false, path = (StudioBridge.pathOf(instance)) })
		end
	end
	table.sort(entries, function(a, b)
		return a.path < b.path
	end)
	return entries, skipped
end

--[[
	Serializes `entries` from `collect` as one native model with a root per
	entry. `readSource`, when given, reads a script's source the way sync does,
	so a script open in an editor is saved with what the editor shows.
	Returns the model buffer and the entries' paths and classes. Errors when
	the model would be over the server's limits.
]]
function StudioBackup.serialize(entries, readSource)
	local roots = {}
	local copies = {}
	local described = {}
	local ok, result = pcall(function()
		local count = 0
		for _, entry in entries do
			local instance = entry.instance
			local root = instance
			if entry.deep then
				count += #instance:GetDescendants() + 1
			else
				root = instance:Clone()
				assert(root ~= nil, instance:GetFullName() .. " cannot be copied because it is not Archivable")
				table.insert(copies, root)
				root:ClearAllChildren()
				if readSource and root:IsA("LuaSourceContainer") then
					local source = readSource(instance)
					if source ~= nil then
						(root :: any).Source = source
					end
				end
				count += 1
			end
			assert(
				count <= StudioBackup.MAX_INSTANCES,
				string.format("the copy would have more than %d instances", StudioBackup.MAX_INSTANCES)
			)
			table.insert(roots, root)
			table.insert(described, {
				path = entry.path or (StudioBridge.pathOf(instance)),
				className = instance.ClassName,
			})
		end
		local model = SerializationService:SerializeInstancesAsync(roots)
		assert(
			buffer.len(model) <= StudioBackup.MAX_MODEL_BYTES,
			string.format("the copy is %.1f MiB, over the 5 MiB model limit", buffer.len(model) / (1024 * 1024))
		)
		return model
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
