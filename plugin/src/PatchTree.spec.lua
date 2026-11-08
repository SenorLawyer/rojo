return function()
	local PatchTree = require(script.Parent.PatchTree)
	local PatchSet = require(script.Parent.PatchSet)
	local InstanceMap = require(script.Parent.InstanceMap)

	local function buildFlatTree(count, sameName)
		local container = Instance.new("Folder")
		container.Parent = workspace
		local instanceMap = InstanceMap.new()
		instanceMap:insert("CONTAINER", container)

		local patch = PatchSet.newEmpty()
		for index = 1, count do
			local id = "CHILD" .. index
			patch.added[id] = {
				Id = id,
				Parent = "CONTAINER",
				-- Zero-padded so name order matches index order.
				Name = if sameName then "Part" else string.format("Child%06d", index),
				ClassName = "Folder",
				Properties = {},
				Children = {},
			}
		end

		return PatchTree.build(patch, instanceMap, { "Property", "Old", "New" }), instanceMap, container
	end

	it("visits siblings in name order without changing the tree", function()
		local tree, instanceMap, container = buildFlatTree(50)

		local names = {}
		tree:forEach(function(node)
			if node.id ~= "CONTAINER" and node.className == "Folder" and node.patchType == "Add" then
				table.insert(names, node.name)
			end
		end)

		expect(#names).to.equal(50)
		for index, name in names do
			expect(name).to.equal(string.format("Child%06d", index))
		end
		local containerNode = tree:getNode("CONTAINER")
		for key, child in containerNode.children do
			expect(type(child)).to.equal("table")
			expect(child.id).to.equal(key)
		end

		instanceMap:stop()
		container:Destroy()
	end)

	it("marks only the last child of each parent as final, even with shared names", function()
		local tree, instanceMap, container = buildFlatTree(50, true)

		local finals, finalIds, lastSeen = {}, {}, {}
		tree:forEach(function(node, _depth, isFinalChild)
			lastSeen[node.parentId] = node.id
			if isFinalChild then
				finals[node.parentId] = (finals[node.parentId] or 0) + 1
				finalIds[node.parentId] = node.id
			end
		end)

		for parentId, lastId in lastSeen do
			expect(finals[parentId]).to.equal(1)
			expect(finalIds[parentId]).to.equal(lastId)
		end
		expect(finals.CONTAINER).to.equal(1)

		instanceMap:stop()
		container:Destroy()
	end)

	-- Every patch walks the tree once applied. Finding the previous key
	-- again for each sibling made this quadratic, and a patch adding 20,000
	-- siblings took seconds; it now takes a few milliseconds.
	it("walks many siblings in linear time", function()
		local tree, instanceMap, container = buildFlatTree(20000)

		local visited = 0
		local start = os.clock()
		tree:forEach(function()
			visited += 1
		end)
		local elapsed = os.clock() - start

		expect(visited >= 20000).to.equal(true)
		expect(elapsed < 0.5).to.equal(true)

		instanceMap:stop()
		container:Destroy()
	end)
end
