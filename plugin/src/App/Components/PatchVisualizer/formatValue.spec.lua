return function()
	local formatValue = require(script.Parent.formatValue)

	it("shows position, orientation and units in a CFrame patch", function()
		expect(formatValue(CFrame.new(1, 2, 3))).to.equal("(1, 2, 3), (0 deg, 0 deg, 0 deg)")
	end)
	it("preserves sub-millistud differences", function()
		expect(formatValue(Vector3.new(0, 0, 0)) == formatValue(Vector3.new(0, 0.0001, 0))).to.equal(false)
	end)
end
