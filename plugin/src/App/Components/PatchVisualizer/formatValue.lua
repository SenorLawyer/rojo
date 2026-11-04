-- Keep the same precision as tostring for scalar fields so small changes remain visible.
return function(value)
	local kind = typeof(value)
	if kind == "Vector3" then
		return `({value.X}, {value.Y}, {value.Z})`
	elseif kind == "Vector2" then
		return `({value.X}, {value.Y})`
	elseif kind == "UDim2" then
		return string.format("{%s, %s}, {%s, %s}", value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset)
	elseif kind == "CFrame" then
		local x, y, z = value:ToOrientation()
		-- ToOrientation can return negative zero for the identity rotation.
		return `({value.X}, {value.Y}, {value.Z}), ({math.deg(x) + 0} deg, {math.deg(y) + 0} deg, {math.deg(z) + 0} deg)`
	elseif kind == "NumberRange" then
		return `[{value.Min}, {value.Max}]`
	end
	local text = string.gsub(tostring(value), "%s", " ")
	return if kind == "string" then '"' .. text .. '"' else text
end
