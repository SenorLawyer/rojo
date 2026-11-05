--[[
	Hashes script source the same way the server does (`src/web/verify.rs`), so
	`rojo studio verify` can compare Studio with the files on disk without
	sending every script's source over the bridge.

	The hash is 32-bit FNV-1a over the source with CRLF line endings turned into
	LF, since Studio may store either for a file that has CRLF endings on disk.
	It only has to tell apart versions of the same script, so a 32-bit hash
	together with the byte length is enough.
]]

local OFFSET_BASIS = 2166136261
local TWO_POW_32 = 4294967296

local function hashSource(source: string): (number, number)
	local normalized = string.gsub(source, "\r\n", "\n")
	local hash = OFFSET_BASIS
	for index = 1, #normalized do
		hash = bit32.bxor(hash, string.byte(normalized, index))
		-- The FNV prime is 2^24 + 403. Multiplying by it directly would exceed
		-- the 53 bits a double holds exactly, so the two parts are added
		-- separately, each of which stays exact.
		hash = (hash * 403 + bit32.lshift(hash, 24)) % TWO_POW_32
	end
	return hash, #normalized
end

return hashSource
