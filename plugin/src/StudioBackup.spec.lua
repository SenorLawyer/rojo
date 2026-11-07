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

	beforeEach(function()
		fixture = make("Folder", "__RojoBackupSpec", game:GetService("ReplicatedStorage"))
	end)

	afterEach(function()
		StudioBridge.requestAsync = nativeRequest
		fixture:Destroy()
	end)

	describe("collect", function()
		it("copies removed instances whole and changed ones on their own, once each", function()
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
			table.insert(patch.updated, { id = "GONE", changedName = "Other" })

			local entries, skipped = StudioBackup.collect(patch, instanceMap)
			expect(skipped).to.equal(1)
			expect(#entries).to.equal(2)
			expect(entries[1].instance).to.equal(map)
			expect(entries[1].deep).to.equal(true)
			expect(entries[1].path).to.equal("game/ReplicatedStorage/__RojoBackupSpec/Map")
			expect(entries[2].instance).to.equal(script)
			expect(entries[2].deep).to.equal(false)
		end)

		it("finds nothing to copy in a patch that only adds", function()
			local patch = PatchSet.newEmpty()
			patch.added.NEW = { Name = "New", ClassName = "Folder", Children = {}, Properties = {} }
			local entries, skipped = StudioBackup.collect(patch, { fromIds = {} })
			expect(#entries).to.equal(0)
			expect(skipped).to.equal(0)
		end)
	end)

	describe("serialize", function()
		it("saves a changed script without its children, with the source its editor shows", function()
			local script = make("ModuleScript", "Module", fixture)
			script.Source = "return 'saved'"
			make("Folder", "Child", script)
			local removed = make("Folder", "Removed", fixture)
			make("Part", "Inside", removed)
			local model, described = StudioBackup.serialize({
				{ instance = removed, deep = true },
				{ instance = script, deep = false },
			}, function()
				return "return 'editor'"
			end)
			expect(#described).to.equal(2)
			expect(described[2].className).to.equal("ModuleScript")
			local roots = SerializationService:DeserializeInstancesAsync(model)
			expect(#roots).to.equal(2)
			local copiedFolder, copiedScript = roots[1], roots[2]
			if copiedFolder:IsA("ModuleScript") then
				copiedFolder, copiedScript = copiedScript, copiedFolder
			end
			expect(#copiedFolder:GetChildren()).to.equal(1)
			expect(#copiedScript:GetChildren()).to.equal(0)
			expect(copiedScript.Source).to.equal("return 'editor'")
			-- The place keeps its own instances as they were.
			expect(script.Source).to.equal("return 'saved'")
			expect(#script:GetChildren()).to.equal(1)
			expect(removed.Parent).to.equal(fixture)
			for _, root in roots do
				root:Destroy()
			end
		end)

		it("refuses copies over the instance or byte limits", function()
			local previousInstances = StudioBackup.MAX_INSTANCES
			local previousBytes = StudioBackup.MAX_MODEL_BYTES
			local removed = make("Folder", "Removed", fixture)
			make("Part", "One", removed)
			make("Part", "Two", removed)
			StudioBackup.MAX_INSTANCES = 2
			local tooMany, manyError = pcall(StudioBackup.serialize, { { instance = removed, deep = true } })
			StudioBackup.MAX_INSTANCES = previousInstances
			StudioBackup.MAX_MODEL_BYTES = 16
			local tooLarge, largeError = pcall(StudioBackup.serialize, { { instance = removed, deep = true } })
			StudioBackup.MAX_MODEL_BYTES = previousBytes
			expect(tooMany).to.equal(false)
			expect(tostring(manyError):find("instances", 1, true)).to.be.ok()
			expect(tooLarge).to.equal(false)
			expect(tostring(largeError):find("5 MiB", 1, true)).to.be.ok()
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
