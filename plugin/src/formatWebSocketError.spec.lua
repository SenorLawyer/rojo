return function()
	local formatWebSocketError = require(script.Parent.formatWebSocketError)
	local lostConnection = "Lost connection to the Rojo server. Make sure `rojo serve` is still running."

	it("explains known lost-connection errors", function()
		for _, message in
			{
				"Failed ws recv - err: 0",
				"Failure when receiving data from the peer",
				"Connection reset by peer",
			}
		do
			expect(formatWebSocketError(400, message)).to.equal(lostConnection)
		end
	end)

	it("preserves the details of unrelated errors", function()
		expect(formatWebSocketError(500, "connection reset")).to.equal("WebSocket error: 500 - connection reset")
		expect(formatWebSocketError(400, "unexpected")).to.equal("WebSocket error: 400 - unexpected")
		expect(formatWebSocketError(500, nil)).to.equal("WebSocket error: 500 - ")
	end)
end
