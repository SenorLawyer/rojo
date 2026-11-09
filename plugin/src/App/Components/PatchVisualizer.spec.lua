return function()
	local Rojo = script:FindFirstAncestor("Rojo")
	local Packages = Rojo.Packages
	local Roact = require(Packages.Roact)

	local PatchVisualizer = require(script.Parent.PatchVisualizer)
	local PatchTree = require(script.Parent.Parent.Parent.PatchTree)
	local PatchSet = require(script.Parent.Parent.Parent.PatchSet)
	local InstanceMap = require(script.Parent.Parent.Parent.InstanceMap)

	local fakeTheme = {
		Font = { Main = Font.fromEnum(Enum.Font.SourceSans) },
		TextSize = { Medium = 16 },
		TextColor = Color3.new(1, 1, 1),
	}

	-- Calls render without mounting, then unwraps the theme consumer and the
	-- scroller to get every row it built, in order.
	local function renderRows(patchTree)
		local updateEvent = Instance.new("BindableEvent")
		local start = os.clock()
		local element = PatchVisualizer.render({
			props = { patchTree = patchTree },
			updateEvent = updateEvent,
		})
		local elapsed = os.clock() - start

		local container = element.props.render(fakeTheme)
		local scroller = container.props[Roact.Children].VirtualScroller
		local rows = {}
		for index = 1, scroller.props.count do
			rows[index] = scroller.props.render(index).props
		end

		updateEvent:Destroy()
		return rows, elapsed
	end

	-- Rendering used to compare each row against all of its siblings to find
	-- the last one, which never stopped early when they shared a name. A patch
	-- adding thousands of Parts to one model took seconds to draw.
	it("renders thousands of same-named siblings quickly, with one final child", function()
		local container = Instance.new("Folder")
		container.Parent = workspace
		local instanceMap = InstanceMap.new()
		instanceMap:insert("CONTAINER", container)

		local count = 8000
		local patch = PatchSet.newEmpty()
		for index = 1, count do
			local id = "PART" .. index
			patch.added[id] = {
				Id = id,
				Parent = "CONTAINER",
				Name = "Part",
				ClassName = "Part",
				Properties = {},
				Children = {},
			}
		end
		local patchTree = PatchTree.build(patch, instanceMap, { "Property", "Old", "New" })

		local rows, elapsed = renderRows(patchTree)
		expect(elapsed < 1).to.equal(true)

		-- The Parts are the deepest rows; every ancestor above them is its
		-- parent's only child.
		local deepest = 0
		for _, row in rows do
			deepest = math.max(deepest, row.depth)
		end
		local parts, finals, lastPart = 0, 0, nil
		for index, row in rows do
			if row.depth == deepest then
				expect(row.name).to.equal("Part")
				parts += 1
				lastPart = index
				if row.isFinalChild then
					finals += 1
				end
			else
				expect(row.isFinalChild).to.equal(true)
			end
		end
		expect(parts).to.equal(count)
		expect(finals).to.equal(1)
		expect(rows[lastPart].isFinalChild).to.equal(true)
		expect(rows[#rows].isFinalElement).to.equal(true)

		instanceMap:stop()
		container:Destroy()
	end)
end
