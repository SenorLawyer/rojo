return function()
	local SerializationService = game:GetService("SerializationService")
	local StudioBackup = require(script.Parent.StudioBackup)

	local fixture

	local function make(className, name, parent)
		local instance = Instance.new(className)
		instance.Name = name
		instance.Parent = parent
		return instance
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
		fixture:Destroy()
	end)

	describe("serializeScripts", function()
		it("copies each script without its children, with the given text and how it runs", function()
			local script = make("Script", "Server", fixture)
			script.Source = "print('saved')"
			script.Enabled = false
			script:SetAttribute("Owner", "team")
			make("Folder", "Child", script)
			local model, described =
				StudioBackup.serializeScripts({ { instance = script, source = "print('editor')" } })
			expect(described[1].path).to.equal("game/ReplicatedStorage/__RojoBackupSpec/Server")
			local roots = SerializationService:DeserializeInstancesAsync(model)
			expect(#roots).to.equal(1)
			local copy = roots[1]
			expect(copy.ClassName).to.equal("Script")
			expect(copy.Source).to.equal("print('editor')")
			expect(copy.Enabled).to.equal(false)
			expect(copy:GetAttribute("Owner")).to.equal("team")
			expect(copy:GetAttribute("RojoBackupOf")).to.equal("game/ReplicatedStorage/__RojoBackupSpec/Server")
			expect(script:GetAttribute("RojoBackupOf")).to.equal(nil)
			expect(#copy:GetChildren()).to.equal(0)
			expect(#script:GetChildren()).to.equal(1)
			destroyAll(roots)
		end)

		it("refuses a copy over the model limit and leaves nothing behind", function()
			local script = make("ModuleScript", "Module", fixture)
			local previous = StudioBackup.MAX_MODEL_BYTES
			local nativeSerialize = StudioBackup.serializeInstances
			local copies
			StudioBackup.serializeInstances = function(instances)
				copies = instances
				return nativeSerialize(instances)
			end
			StudioBackup.MAX_MODEL_BYTES = 16
			local ok, err = pcall(StudioBackup.serializeScripts, { { instance = script, source = "return 1" } })
			StudioBackup.MAX_MODEL_BYTES = previous
			StudioBackup.serializeInstances = nativeSerialize
			expect(ok).to.equal(false)
			expect(tostring(err):find("5 MiB", 1, true)).to.be.ok()
			-- The copies are destroyed again.
			expect(copies[1].Parent).to.equal(nil)
			expect(pcall(function()
				copies[1].Parent = fixture
			end)).to.equal(false)
		end)
	end)
end
