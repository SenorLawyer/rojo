--[[
	Persistent plugin settings.
]]

local plugin = plugin or script:FindFirstAncestorWhichIsA("Plugin")
local Rojo = script:FindFirstAncestor("Rojo")
local Packages = Rojo.Packages

local Log = require(Packages.Log)
local Roact = require(Packages.Roact)

local defaultSettings = {
	openScriptsExternally = false,
	twoWaySync = false,
	autoReconnect = false,
	showNotifications = true,
	enableSyncFallback = true,
	syncReminderMode = "Notify" :: "None" | "Notify" | "Fullscreen",
	syncReminderPolling = true,
	checkForUpdates = true,
	checkForPrereleases = false,
	autoConnectPlaytestServer = false,
	confirmationBehavior = "Initial" :: "Never" | "Initial" | "Large Changes" | "Unlisted PlaceId",
	largeChangesConfirmationThreshold = 5,
	playSounds = true,
	typecheckingEnabled = false,
	logLevel = "Info",
	timingLogsEnabled = false,
	agentControlEnabled = true,
	agentControlCanChangeConfirmation = true,
	studioBridgeAutoEnable = true,
	guardScriptWipes = true,
	priorEndpoints = {},
}

local Settings = {}

-- Keep store loading separate so tests can verify upgrades without modifying
-- the developer's actual plugin preferences.
function Settings._loadValues(store)
	local values = table.clone(defaultSettings)
	if not store then
		return values
	end
	for name, defaultValue in pairs(values) do
		local savedValue = store:GetSetting("Rojo_" .. name)

		if savedValue == nil then
			-- plugin:SetSetting hits disc instead of memory, so it can be slow. Spawn so we don't hang.
			task.spawn(store.SetSetting, store, "Rojo_" .. name, defaultValue)
		else
			values[name] = savedValue
		end
	end
	Log.trace("Loaded settings from plugin store")
	return values
end

Settings._values = Settings._loadValues(plugin)
Settings._updateListeners = {}
Settings._bindings = {}

function Settings:get(name)
	if defaultSettings[name] == nil then
		error("Invalid setings name " .. tostring(name), 2)
	end

	return self._values[name]
end

function Settings:set(name, value)
	self._values[name] = value
	if self._bindings[name] then
		self._bindings[name].set(value)
	end

	if plugin then
		-- plugin:SetSetting hits disc instead of memory, so it can be slow. Spawn so we don't hang.
		task.spawn(plugin.SetSetting, plugin, "Rojo_" .. name, value)
	end

	if self._updateListeners[name] then
		for callback in pairs(self._updateListeners[name]) do
			task.spawn(callback, value)
		end
	end

	Log.trace(string.format("Set setting '%s' to '%s'", name, tostring(value)))
end

function Settings:onChanged(name, callback)
	local listeners = self._updateListeners[name]
	if listeners == nil then
		listeners = {}
		self._updateListeners[name] = listeners
	end
	listeners[callback] = true

	Log.trace(string.format("Added listener for setting '%s' changes", name))

	return function()
		listeners[callback] = nil
		Log.trace(string.format("Removed listener for setting '%s' changes", name))
	end
end

function Settings:getBinding(name)
	local cached = self._bindings[name]
	if cached then
		return cached.bind
	end

	local bind, set = Roact.createBinding(self._values[name])
	self._bindings[name] = {
		bind = bind,
		set = set,
	}

	Log.trace(string.format("Created binding for setting '%s'", name))

	return bind
end

function Settings:getBindings(...: string)
	local bindings = {}
	for i = 1, select("#", ...) do
		local source = select(i, ...)
		bindings[source] = self:getBinding(source)
	end

	return Roact.joinBindings(bindings)
end

return Settings
