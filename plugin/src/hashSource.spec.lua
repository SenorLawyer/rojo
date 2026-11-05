return function()
	local hashSource = require(script.Parent.hashSource)

	it("should match standard FNV-1a test vectors", function()
		expect(hashSource("")).to.equal(0x811c9dc5)
		expect(hashSource("a")).to.equal(0xe40c292c)
		expect(hashSource("foobar")).to.equal(0xbf9cf968)
	end)

	it("should match the server's hash of a script", function()
		-- Same vector as `hashes_match_the_plugin` in src/web/verify.rs.
		local hash, length = hashSource('print("Hello, Rojo!")\n')
		expect(hash).to.equal(0x8d5c0b3a)
		expect(length).to.equal(22)
	end)

	it("should treat CRLF and LF line endings the same", function()
		local crlf, crlfLength = hashSource("local a = 1\r\nreturn a\r\n")
		local lf, lfLength = hashSource("local a = 1\nreturn a\n")
		expect(crlf).to.equal(lf)
		expect(crlfLength).to.equal(lfLength)
	end)

	it("should match the server's second hash", function()
		-- Same vectors as `hashes_match_the_plugin` in src/web/verify.rs.
		local _, _, empty = hashSource("")
		local _, _, a = hashSource("a")
		local _, _, sample = hashSource('print("Hello, Rojo!")\n')
		expect(empty).to.equal(5381)
		expect(a).to.equal(177604)
		expect(sample).to.equal(911966760)
	end)

	it("should tell apart sources whose first hash collides", function()
		local first, length, secondA = hashSource("return '07ilh49'")
		local otherFirst, otherLength, secondB = hashSource("return '0v3kbm8'")
		expect(first).to.equal(otherFirst)
		expect(length).to.equal(otherLength)
		expect(secondA).never.to.equal(secondB)
	end)

	it("should tell different sources apart", function()
		expect(hashSource("return 1")).never.to.equal(hashSource("return 2"))
	end)
end
