return function()
	local HttpService = game:GetService("HttpService")
	local SerializationService = game:GetService("SerializationService")
	local StudioBackup = require(script.Parent.StudioBackup)
	local StudioBridge = require(script.Parent.StudioBridge)
	local PatchSet = require(script.Parent.PatchSet)

	local nativeRequest = StudioBridge.requestAsync
	local fixture

	local function make(className, name, parent)
		local instance = Instance.new(className)
		instance.Name = name
		instance.Parent = parent
		return instance
	end

	local function byName(instances)
		local found = {}
		for _, instance in instances do
			found[instance.Name] = instance
		end
		return found
	end

	local function destroyAll(instances)
		for _, instance in instances do
			instance:Destroy()
		end
	end

	beforeEach(function()
		fixture = make("Folder", "__RojoBackupSpec", game:GetService("ReplicatedStorage"))
	end)

	afterEach(function()
		StudioBridge.requestAsync = nativeRequest
		fixture:Destroy()
	end)

	describe("collect", function()
		it("finds each removed or changed instance once, outside any other root", function()
			local map = make("Folder", "Map", fixture)
			local inside = make("Part", "Inside", map)
			local script = make("ModuleScript", "Module", fixture)
			local unchanged = make("ModuleScript", "Unchanged", fixture)
			local instanceMap = {
				fromIds = {
					MAP = map,
					INSIDE = inside,
					SCRIPT = script,
					UNCHANGED = unchanged,
					LIGHTING = game:GetService("Lighting"),
				},
			}
			local patch = PatchSet.newEmpty()
			-- Removals come as instances or IDs.
			table.insert(patch.removed, map)
			table.insert(patch.removed, "INSIDE")
			table.insert(patch.updated, { id = "INSIDE", changedProperties = { Anchored = { Bool = true } } })
			table.insert(patch.updated, { id = "SCRIPT", changedProperties = { Source = { String = "" } } })
			table.insert(patch.updated, { id = "UNCHANGED", changedProperties = {} })
			table.insert(patch.updated, { id = "LIGHTING", changedProperties = { Brightness = { Float32 = 1 } } })
			table.insert(patch.updated, { id = "LIGHTING", changedProperties = { Ambient = { Float32 = 1 } } })
			table.insert(patch.updated, { id = "GONE", changedName = "Other" })

			local roots, skipped = StudioBackup.collect(patch, instanceMap)
			expect(#skipped).to.equal(1)
			expect(skipped[1]).to.equal("game/Lighting")
			expect(#roots).to.equal(2)
			expect(roots[1].instance).to.equal(map)
			expect(roots[1].path).to.equal("game/ReplicatedStorage/__RojoBackupSpec/Map")
			expect(roots[2].instance).to.equal(script)
			expect(StudioBackup.countInstances(roots)).to.equal(3)
		end)

		it("finds nothing to copy in a patch that only adds", function()
			local patch = PatchSet.newEmpty()
			patch.added.NEW = { Name = "New", ClassName = "Folder", Children = {}, Properties = {} }
			local roots, skipped = StudioBackup.collect(patch, { fromIds = {} })
			expect(#roots).to.equal(0)
			expect(#skipped).to.equal(0)
		end)
	end)

	describe("serialize", function()
		it("keeps references between backed-up roots through a real round trip", function()
			local first = make("ObjectValue", "First", fixture)
			local second = make("ObjectValue", "Second", fixture)
			local outside = make("Folder", "Outside", fixture)
			first.Value = second
			second.Value = first
			local third = make("ObjectValue", "Third", fixture)
			third.Value = outside
			local model = StudioBackup.serialize({
				{ instance = first },
				{ instance = second },
				{ instance = third },
			})
			local roots = SerializationService:DeserializeInstancesAsync(model)
			local found = byName(roots)
			expect(#roots).to.equal(3)
			expect(found.First.Value).to.equal(found.Second)
			expect(found.Second.Value).to.equal(found.First)
			-- References outside the backup are not kept.
			expect(found.Third.Value).to.equal(nil)
			-- Nothing in the place was moved or copied.
			expect(first.Parent).to.equal(fixture)
			expect(first.Value).to.equal(second)
			expect(#fixture:GetChildren()).to.equal(4)
			destroyAll(roots)
		end)

		it("saves an open editor's unsaved text as a draft beside the script", function()
			local removed = make("Folder", "Removed", fixture)
			local edited = make("ModuleScript", "Edited", removed)
			edited.Source = "return 'saved'"
			make("Folder", "Child", edited)
			local clean = make("ModuleScript", "Clean", removed)
			clean.Source = "return 'same'"
			local model, described = StudioBackup.serialize({ { instance = removed } }, function(instance)
				if instance == edited then
					return "return 'editor'"
				end
				return instance.Source
			end)
			expect(#described).to.equal(2)
			expect(described[2].path).to.equal(StudioBackup.DRAFTS_FOLDER)
			local roots = byName(SerializationService:DeserializeInstancesAsync(model))
			local copied = roots.Removed
			expect(copied.Edited.Source).to.equal("return 'saved'")
			expect(copied.Edited:FindFirstChild("Child")).to.be.ok()
			local drafts = roots[StudioBackup.DRAFTS_FOLDER]
			expect(#drafts:GetChildren()).to.equal(1)
			local draft = drafts:GetChildren()[1]
			expect(draft.ClassName).to.equal("ModuleScript")
			expect(draft.Name).to.equal("Edited")
			expect(draft.Source).to.equal("return 'editor'")
			expect(#draft:GetChildren()).to.equal(0)
			expect(draft:GetAttribute("RojoDraftOf")).to.equal("game/ReplicatedStorage/__RojoBackupSpec/Removed/Edited")
			-- The place keeps its scripts as they were.
			expect(edited.Source).to.equal("return 'saved'")
			expect(#fixture:GetChildren()).to.equal(1)
			for _, root in roots do
				root:Destroy()
			end
		end)

		it("refuses a backup over the instance limit before serializing", function()
			local removed = make("Folder", "Removed", fixture)
			make("Part", "One", removed)
			make("Part", "Two", removed)
			local previous = StudioBackup.MAX_INSTANCES
			StudioBackup.MAX_INSTANCES = 2
			local ok, err = pcall(StudioBackup.serialize, { { instance = removed } })
			StudioBackup.MAX_INSTANCES = previous
			expect(ok).to.equal(false)
			expect(tostring(err):find("3 instances", 1, true)).to.be.ok()
		end)

		it("refuses a backup over the byte limit", function()
			local removed = make("Folder", "Removed", fixture)
			local previous = StudioBackup.MAX_MODEL_BYTES
			StudioBackup.MAX_MODEL_BYTES = 16
			local ok, err = pcall(StudioBackup.serialize, { { instance = removed } })
			StudioBackup.MAX_MODEL_BYTES = previous
			expect(ok).to.equal(false)
			expect(tostring(err):find("5 MiB", 1, true)).to.be.ok()
		end)
	end)

	describe("serializeScripts", function()
		it("copies each script without its children, with its editor's text and how it runs", function()
			local script = make("Script", "Server", fixture)
			script.Source = "print('saved')"
			script.Enabled = false
			script:SetAttribute("Owner", "team")
			make("Folder", "Child", script)
			local model, described = StudioBackup.serializeScripts({ script }, function()
				return "print('editor')"
			end)
			expect(described[1].path).to.equal("game/ReplicatedStorage/__RojoBackupSpec/Server")
			local roots = SerializationService:DeserializeInstancesAsync(model)
			expect(#roots).to.equal(1)
			local copy = roots[1]
			expect(copy.ClassName).to.equal("Script")
			expect(copy.Source).to.equal("print('editor')")
			expect(copy.Enabled).to.equal(false)
			expect(copy:GetAttribute("Owner")).to.equal("team")
			expect(#copy:GetChildren()).to.equal(0)
			expect(#script:GetChildren()).to.equal(1)
			destroyAll(roots)
		end)
	end)

	describe("send", function()
		local pairing = { protocolVersion = 1, pluginToken = "plugin-token", identity = { digest = "d" } }

		it("posts the backup to the bridge with the plugin token and pairing identity", function()
			local sent
			StudioBridge.requestAsync = function(options)
				sent = options
				return {
					Success = true,
					StatusCode = 200,
					Body = HttpService:JSONEncode({ backup = { id = "20261107-000000-auto-sync" } }),
				}
			end
			local answer = StudioBackup.send("http://127.0.0.1:34872", pairing, { model = "AAAA", instances = {} })
			expect(answer.backup.id).to.equal("20261107-000000-auto-sync")
			expect(sent.Url).to.equal("http://127.0.0.1:34872/api/studio/backup")
			expect(sent.Headers["x-rojo-studio-token"]).to.equal("plugin-token")
			local body = HttpService:JSONDecode(sent.Body)
			expect(body.identity.digest).to.equal("d")
			expect(body.model).to.equal("AAAA")
		end)

		it("reports the server's reason for refusing a backup", function()
			StudioBridge.requestAsync = function()
				return {
					Success = false,
					StatusCode = 409,
					Body = HttpService:JSONEncode({ error = "Project Git checkout changed." }),
				}
			end
			local ok, err = pcall(StudioBackup.send, "http://127.0.0.1:34872", pairing, { instances = {} })
			expect(ok).to.equal(false)
			expect(tostring(err):find("Project Git checkout changed.", 1, true)).to.be.ok()
		end)

		it("sends nothing without a pairing or to a server on another computer", function()
			local calls = 0
			StudioBridge.requestAsync = function()
				calls += 1
				return { Success = true, StatusCode = 200, Body = "{}" }
			end
			expect(pcall(StudioBackup.send, "http://127.0.0.1:34872", nil, {})).to.equal(false)
			expect(pcall(StudioBackup.send, "http://example.com:34872", pairing, {})).to.equal(false)
			expect(calls).to.equal(0)
		end)
	end)
end
