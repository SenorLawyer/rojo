--[[
	Hashes script source the same way the server does (`src/web/verify.rs`), so
	`rojo studio verify` can compare Studio with the files on disk without
	sending every script's source over the bridge.

	The hashes are 32-bit FNV-1a and 32-bit djb2 (xor variant) over the source
	with CRLF line endings turned into LF, since Studio may store either for a
	file that has CRLF endings on disk. Together with the length they tell
	versions of a script apart, but they are not a defense against sources made
	to collide on purpose; `verify --fix` also compares the exact source it saw.
]]

local OFFSET_BASIS = 2166136261
local TWO_POW_32 = 4294967296

local function hashSource(source: string): (number, number, number)
	local normalized = string.gsub(source, "\r\n", "\n")
	local hash = OFFSET_BASIS
	local second = 5381
	for index = 1, #normalized do
		local byte = string.byte(normalized, index)
		hash = bit32.bxor(hash, byte)
		-- The FNV prime is 2^24 + 403. Multiplying by it directly would exceed
		-- the 53 bits a double holds exactly, so the two parts are added
		-- separately, each of which stays exact.
		hash = (hash * 403 + bit32.lshift(hash, 24)) % TWO_POW_32
		-- second * 33 stays below 2^38, which a double holds exactly.
		second = bit32.bxor((second * 33) % TWO_POW_32, byte)
	end
	return hash, #normalized, second
end

return hashSource
