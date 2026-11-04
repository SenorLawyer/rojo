--[[
	Datatype components such as Vector3.X are stored as float32, so printing
	them at double precision shows noise like 9.999999747378752e-05 for 0.0001.
	Instead, print the shortest text that reads back as the same float32. This
	still shows any difference float32 can represent, so small changes remain
	visible.
]]
local function toFloat32(value: number): number
	return Vector3.new(value, 0, 0).X
end

local function formatComponent(value: number): string
	if value ~= value or value == math.huge or value == -math.huge then
		return tostring(value)
	end
	-- Adding zero turns negative zero into zero.
	value += 0

	local text = string.format("%.9g", value)
	for precision = 1, 8 do
		local candidate = string.format(`%.{precision}g`, value)
		if toFloat32(tonumber(candidate) :: number) == toFloat32(value) then
			text = candidate
			break
		end
	end

	-- float32 values this large are whole numbers, which read better without
	-- an exponent.
	if string.find(text, "e+", 1, true) and math.abs(value) < 2 ^ 53 then
		return string.format("%.0f", value)
	end
	return text
end

return function(value)
	local kind = typeof(value)
	if kind == "Vector3" then
		return `({formatComponent(value.X)}, {formatComponent(value.Y)}, {formatComponent(value.Z)})`
	elseif kind == "Vector2" then
		return `({formatComponent(value.X)}, {formatComponent(value.Y)})`
	elseif kind == "UDim2" then
		return string.format(
			"{%s, %s}, {%s, %s}",
			formatComponent(value.X.Scale),
			value.X.Offset,
			formatComponent(value.Y.Scale),
			value.Y.Offset
		)
	elseif kind == "CFrame" then
		local x, y, z = value:ToOrientation()
		return `({formatComponent(value.X)}, {formatComponent(value.Y)}, {formatComponent(value.Z)}), ({formatComponent(
			math.deg(x)
		)} deg, {formatComponent(math.deg(y))} deg, {formatComponent(math.deg(z))} deg)`
	elseif kind == "NumberRange" then
		return `[{formatComponent(value.Min)}, {formatComponent(value.Max)}]`
	end
	-- Plain numbers may be float64 properties, so they keep tostring's precision.
	local text = string.gsub(tostring(value), "%s", " ")
	return if kind == "string" then '"' .. text .. '"' else text
end
