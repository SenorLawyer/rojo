local LOST_CONNECTION_MESSAGE = "Lost connection to the Rojo server. Make sure `rojo serve` is still running."

return function(code, message): string
	local detail = if message == nil then "" else tostring(message)
	if tostring(code) == "400" then
		local normalized = string.lower(detail)
		for _, fragment in { "failed ws recv", "failure when receiving data from the peer", "connection reset" } do
			if string.find(normalized, fragment, 1, true) then
				return LOST_CONNECTION_MESSAGE
			end
		end
	end

	return ("WebSocket error: %s - %s"):format(tostring(code), detail)
end
