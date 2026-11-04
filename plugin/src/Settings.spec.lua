return function()
	local Settings = require(script.Parent.Settings)

	local function makeStore(saved)
		local store = { saved = saved, writes = {} }
		function store:GetSetting(name)
			return self.saved[name]
		end
		function store:SetSetting(name, value)
			self.saved[name] = value
			self.writes[name] = value
		end
		return store
	end

	it("enables agent confirmation in a fresh store and preserves it on reload", function()
		local store = makeStore({})
		local values = Settings._loadValues(store)
		expect(values.agentControlCanChangeConfirmation).to.equal(true)
		expect(values.twoWaySync).to.equal(false)
		expect(values.confirmationBehavior).to.equal("Initial")
		task.wait()
		expect(store.saved.Rojo_agentControlCanChangeConfirmation).to.equal(true)
		table.clear(store.writes)
		local reloaded = Settings._loadValues(store)
		task.wait()
		expect(reloaded.agentControlCanChangeConfirmation).to.equal(true)
		expect(next(store.writes)).to.equal(nil)
	end)

	it("preserves saved agent confirmation choices without overwriting them", function()
		for _, choice in { false, true } do
			local store = makeStore({ Rojo_agentControlCanChangeConfirmation = choice })
			local values = Settings._loadValues(store)
			task.wait()
			expect(values.agentControlCanChangeConfirmation).to.equal(choice)
			expect(store.saved.Rojo_agentControlCanChangeConfirmation).to.equal(choice)
			expect(store.writes.Rojo_agentControlCanChangeConfirmation).to.equal(nil)
			local reloaded = Settings._loadValues(store)
			task.wait()
			expect(reloaded.agentControlCanChangeConfirmation).to.equal(choice)
		end
	end)
end
