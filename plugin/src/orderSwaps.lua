--[[
	Determines the order in which `ServeSession:__replaceInstances` should swap
	instances so that sibling order is preserved.

	Roblox appends to `GetChildren()` on every reparent, so the order in which we
	re-parent replacements determines their final sibling order. To rebuild
	`GetChildren()` exactly as it was before the swap we must:

	* process ancestors before descendants, so each replacement's parent already
	  exists when we re-parent the replacement, and
	* process siblings in their original `GetChildren()` order.

	`swaps` is an array of `{ id, replacement, oldInstance }` entries. This sorts
	the array in place (annotating each entry with `depth`/`siblingIndex`) and
	returns it.
]]
local function orderSwaps(swaps)
	-- Each parent's children are listed once. Calling `GetChildren` and
	-- searching it for every swap is quadratic in the number of siblings, which
	-- froze Studio for many seconds when a large flat model fell back to
	-- replacement.
	local siblingIndexes = {}

	for _, swap in swaps do
		local depth = 0
		local ancestor = swap.oldInstance.Parent
		while ancestor ~= nil do
			depth += 1
			ancestor = ancestor.Parent
		end
		swap.depth = depth

		local siblingIndex = 0
		local parent = swap.oldInstance.Parent
		if parent ~= nil then
			local indexes = siblingIndexes[parent]
			if indexes == nil then
				indexes = {}
				for index, child in parent:GetChildren() do
					indexes[child] = index
				end
				siblingIndexes[parent] = indexes
			end
			siblingIndex = indexes[swap.oldInstance] or 0
		end
		swap.siblingIndex = siblingIndex
	end

	table.sort(swaps, function(a, b)
		if a.depth ~= b.depth then
			return a.depth < b.depth
		end
		return a.siblingIndex < b.siblingIndex
	end)

	return swaps
end

return orderSwaps
