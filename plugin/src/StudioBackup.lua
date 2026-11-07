--[[
	Copies of the scripts `rojo studio verify --fix` is about to overwrite,
	which the server saves in the same folder as `rojo studio backup create`,
	so a fix that went wrong can be put back with `rojo studio backup restore`.
]]
local SerializationService = game:GetService("SerializationService")

local StudioBridge = require(script.Parent.StudioBridge)

local StudioBackup = {}

-- The server's model limit (MAX_MODEL_BYTES in src/web/studio.rs).
StudioBackup.MAX_MODEL_BYTES = 5 * 1024 * 1024
-- Restoring counts every instance against the import limit, so a larger
-- backup could never be put back.
StudioBackup.MAX_INSTANCES = 10_000
-- The attribute on each script copy naming the script it copies, by its
-- full path in the place.
StudioBackup.ORIGIN_ATTRIBUTE = "RojoBackupOf"

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

--[[
	Serializes a copy of each script in `entries`, each `{ instance, source }`,
	made with copyScript from that exact source and without the script's
	children. Returns the model, within the server's model limit, and the
	path and class of each copy.
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
		local model = StudioBackup.serializeInstances(copies)
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

return StudioBackup
