--!strict
local Items = require(script.Parent.Items)
local Levels = require(script.Parent.Levels)
local Settings = require(script.Parent.Settings)

local function price(item: Items.Row): number
	return item.cost
end

local first: Levels.Row? = Levels[1]
local music: string? = if first then first.music else nil

return {
	price = price(Items.sword),
	music = music,
	maxPlayers = Settings.maxPlayers,
}
