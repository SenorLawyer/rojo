return function()
	local SpecRunner = require(script.Parent.SpecRunner)

	local fixture
	local fixturePath = "game/ReplicatedStorage/__RojoSpecRunnerSpec"

	local function module(name, parent)
		local instance = Instance.new("ModuleScript")
		instance.Name = name
		instance.Parent = parent
		return instance
	end

	local function folder(name, parent)
		local instance = Instance.new("Folder")
		instance.Name = name
		instance.Parent = parent
		return instance
	end

	local function node(nodeType, phrase, status, errors, children)
		return {
			planNode = { type = nodeType, phrase = phrase },
			status = status,
			errors = errors or {},
			children = children or {},
		}
	end

	local function failures(count)
		local children = {}
		for index = 1, count do
			table.insert(children, node("It", "fails " .. index, "Failure", { "boom " .. index }))
		end
		return { children = { node("Describe", "Many", "Failure", nil, children) } }
	end

	beforeEach(function()
		fixture = folder("__RojoSpecRunnerSpec", game:GetService("ReplicatedStorage"))
	end)

	afterEach(function()
		fixture:Destroy()
	end)

	describe("normalizeOptions", function()
		it("fills in defaults and keeps default roots unset", function()
			local options = SpecRunner.normalizeOptions(nil)
			expect(options.roots).to.equal(nil)
			expect(options.framework).to.equal("auto")
			expect(options.module).to.equal(nil)
			expect(options.filter).to.equal(nil)
			expect(options.timeoutSeconds).to.equal(60)
		end)

		it("gives the same options when normalized twice", function()
			local options = SpecRunner.normalizeOptions({
				roots = { "game/ReplicatedStorage/Shared" },
				framework = "testez",
				module = "game/ReplicatedStorage/Packages/TestEZ",
				filter = "adds",
				timeoutSeconds = 5,
			})
			local again = SpecRunner.normalizeOptions(options)
			expect(again.roots[1]).to.equal("game/ReplicatedStorage/Shared")
			expect(#again.roots).to.equal(1)
			expect(again.framework).to.equal("testez")
			expect(again.module).to.equal("game/ReplicatedStorage/Packages/TestEZ")
			expect(again.filter).to.equal("adds")
			expect(again.timeoutSeconds).to.equal(5)
		end)

		it("treats an empty filter as no filter", function()
			expect(SpecRunner.normalizeOptions({ filter = "" }).filter).to.equal(nil)
		end)

		it("rejects options outside their bounds", function()
			local tooManyRoots = {}
			for index = 1, SpecRunner.MAX_ROOTS + 1 do
				tooManyRoots[index] = "game/ReplicatedStorage"
			end
			for _, case in
				{
					{ args = "roots", reason = "options must be an object" },
					{ args = { timeoutSeconds = 0 }, reason = "timeoutSeconds" },
					{ args = { timeoutSeconds = 301 }, reason = "timeoutSeconds" },
					{ args = { timeoutSeconds = 1.5 }, reason = "timeoutSeconds" },
					{ args = { timeoutSeconds = "60" }, reason = "timeoutSeconds" },
					{ args = { roots = {} }, reason = "roots" },
					{ args = { roots = tooManyRoots }, reason = "roots" },
					{ args = { roots = { 5 } }, reason = "root" },
					{ args = { roots = { "" } }, reason = "root" },
					{ args = { framework = "mocha" }, reason = "framework" },
					{ args = { module = 5 }, reason = "module" },
					{ args = { filter = string.rep("a", SpecRunner.MAX_FILTER_BYTES + 1) }, reason = "filter" },
				}
			do
				local ok, err = pcall(SpecRunner.normalizeOptions, case.args)
				expect(ok).to.equal(false)
				expect(string.find(tostring(err), case.reason, 1, true)).to.be.ok()
			end
		end)
	end)

	describe("resolvePath", function()
		it("resolves escaped names and rejects missing, ambiguous and relative paths", function()
			local slashed = folder("a/b", fixture)
			expect(SpecRunner.resolvePath(game, fixturePath .. "/a\\/b")).to.equal(slashed)
			expect(SpecRunner.resolvePath(game, "game")).to.equal(game)

			folder("Twin", fixture)
			folder("Twin", fixture)
			for _, case in
				{
					{ path = fixturePath .. "/Missing", reason = "does not exist" },
					{ path = fixturePath .. "/Twin", reason = "Ambiguous" },
					{ path = "ReplicatedStorage", reason = "start with game/" },
				}
			do
				local ok, err = pcall(SpecRunner.resolvePath, game, case.path)
				expect(ok).to.equal(false)
				expect(string.find(tostring(err), case.reason, 1, true)).to.be.ok()
			end
		end)
	end)

	describe("prepare", function()
		it("finds specs once each and skips vendored packages and the framework", function()
			local testEZ = module("TestEZ", fixture)
			module("Inner.spec", testEZ)
			local shared = folder("Shared", fixture)
			local mathSpec = module("Math.spec", shared)
			module("Helper", shared)
			local index = folder("_Index", fixture)
			module("Dependency.spec", folder("pkg", index))
			module("Pesde.spec", folder(".pesde", fixture))

			local prepared = SpecRunner.prepare(
				SpecRunner.normalizeOptions({
					-- Overlapping roots must not run a spec twice.
					roots = { fixturePath, fixturePath .. "/Shared" },
					module = fixturePath .. "/TestEZ",
				}),
				game
			)
			expect(prepared.framework).to.equal("testez")
			expect(prepared.module).to.equal(testEZ)
			expect(#prepared.specs).to.equal(1)
			expect(prepared.specs[1].instance).to.equal(mathSpec)
		end)

		it("skips the specs inside Roblox's default PlayerModule", function()
			module("TestEZ", fixture)
			local scripts = folder("StarterPlayerScripts", fixture)
			local playerModule = folder("PlayerModule", scripts)
			module("CameraWrapper.spec", playerModule)
			local own = module("Controls.spec", scripts)
			local prepared = SpecRunner.prepare(
				SpecRunner.normalizeOptions({ roots = { fixturePath }, module = fixturePath .. "/TestEZ" }),
				game
			)
			expect(#prepared.specs).to.equal(1)
			expect(prepared.specs[1].instance).to.equal(own)
		end)

		it("runs vendored specs when a root points inside the package folder", function()
			local index = folder("_Index", fixture)
			local dependency = module("Dependency.spec", index)
			module("TestEZ", fixture)
			local prepared = SpecRunner.prepare(
				SpecRunner.normalizeOptions({
					roots = { fixturePath .. "/_Index" },
					module = fixturePath .. "/TestEZ",
				}),
				game
			)
			expect(#prepared.specs).to.equal(1)
			expect(prepared.specs[1].instance).to.equal(dependency)
		end)

		it("detects Jest from the module name and finds Jest projects", function()
			local jest = module("Jest", fixture)
			local project = folder("Project", fixture)
			module("jest.config", project)
			local prepared = SpecRunner.prepare(
				SpecRunner.normalizeOptions({ roots = { fixturePath }, module = fixturePath .. "/Jest" }),
				game
			)
			expect(prepared.framework).to.equal("jest")
			expect(prepared.module).to.equal(jest)
			expect(#prepared.projects).to.equal(1)
			expect(prepared.projects[1]).to.equal(project)
		end)

		it("fails clearly when there is nothing to run", function()
			module("TestEZ", fixture)
			local ok, err = pcall(
				SpecRunner.prepare,
				SpecRunner.normalizeOptions({ roots = { fixturePath }, module = fixturePath .. "/TestEZ" }),
				game
			)
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "No *.spec ModuleScripts found beneath " .. fixturePath, 1, true)).to.be.ok()

			folder("NotAModule", fixture)
			ok, err = pcall(
				SpecRunner.prepare,
				SpecRunner.normalizeOptions({ roots = { fixturePath }, module = fixturePath .. "/NotAModule" }),
				game
			)
			expect(ok).to.equal(false)
			expect(string.find(tostring(err), "must be a ModuleScript", 1, true)).to.be.ok()
		end)
	end)

	describe("collectTestEZ", function()
		it("counts results and reports each failure with its full path", function()
			local report = SpecRunner.collectTestEZ({
				children = {
					node("Describe", "ReplicatedStorage", "Failure", nil, {
						node("Describe", "Math", "Failure", nil, {
							node("It", "adds", "Success"),
							node("It", "subtracts", "Failure", { "expected 1, got 2" }),
							node("It", "divides", "Skipped"),
						}),
						-- A describe body that errored while planning.
						node("Describe", "Broken", "Failure", { "Error during planning: oops" }),
					}),
				},
			})
			expect(report.status).to.equal("failed")
			expect(report.passed).to.equal(1)
			expect(report.failed).to.equal(2)
			expect(report.skipped).to.equal(1)
			expect(table.concat(report.failures[1].path, " > ")).to.equal("ReplicatedStorage > Math > subtracts")
			expect(report.failures[1].messages[1]).to.equal("expected 1, got 2")
			expect(table.concat(report.failures[2].path, " > ")).to.equal("ReplicatedStorage > Broken")
			expect(report.budget).to.equal(nil)
		end)

		it("passes when nothing failed", function()
			local report = SpecRunner.collectTestEZ({
				children = { node("Describe", "Shared", "Success", nil, { node("It", "works", "Success") }) },
			})
			expect(report.status).to.equal("passed")
			expect(report.passed).to.equal(1)
			expect(#report.failures).to.equal(0)
		end)

		it("bounds the number of failures it lists", function()
			local report = SpecRunner.collectTestEZ(failures(SpecRunner.MAX_FAILURES + 3))
			expect(report.failed).to.equal(SpecRunner.MAX_FAILURES + 3)
			expect(#report.failures).to.equal(SpecRunner.MAX_FAILURES)
			expect(report.omittedFailures).to.equal(3)
		end)

		it("truncates long messages on codepoint boundaries and repairs invalid UTF-8", function()
			local long = string.rep("a", SpecRunner.MAX_MESSAGE_BYTES - 1) .. "é" .. "tail"
			local report = SpecRunner.collectTestEZ({
				children = {
					node("It", "long", "Failure", { long }),
					node("It", "bytes", "Failure", { "bad \255 byte" }),
				},
			})
			local message = report.failures[1].messages[1]
			expect(#message).to.equal(SpecRunner.MAX_MESSAGE_BYTES - 1)
			expect(utf8.len(message)).to.be.ok()
			expect(report.truncatedMessages).to.equal(1)
			expect(utf8.len(report.failures[2].messages[1])).to.be.ok()
		end)

		it("keeps at most a fixed number of messages per failure", function()
			local messages = {}
			for index = 1, SpecRunner.MAX_MESSAGES_PER_FAILURE + 2 do
				messages[index] = "message " .. index
			end
			local report = SpecRunner.collectTestEZ({ children = { node("It", "noisy", "Failure", messages) } })
			expect(#report.failures[1].messages).to.equal(SpecRunner.MAX_MESSAGES_PER_FAILURE)
			expect(report.truncatedMessages).to.equal(2)
		end)

		it("bounds the total text in a report", function()
			local children = {}
			local big = string.rep("x", SpecRunner.MAX_MESSAGE_BYTES)
			for index = 1, SpecRunner.MAX_FAILURES do
				table.insert(
					children,
					node("It", "fails " .. index, "Failure", { big, big, big, big, big, big, big, big })
				)
			end
			local report = SpecRunner.collectTestEZ({ children = children })
			local total = 0
			for _, failure in report.failures do
				for _, message in failure.messages do
					total += #message
				end
			end
			expect(total <= SpecRunner.MAX_TEXT_BYTES).to.equal(true)
			expect(report.truncatedMessages > 0).to.equal(true)
		end)
	end)

	describe("collectJest", function()
		it("maps Jest's aggregated result and strips terminal colours", function()
			local report = SpecRunner.collectJest({
				testResults = {
					{
						testFilePath = "ReplicatedStorage/Project/Math.spec",
						testResults = {
							{ status = "passed", title = "adds", ancestorTitles = { "Math" } },
							{
								status = "failed",
								title = "subtracts",
								ancestorTitles = { "Math" },
								failureMessages = { "\27[31mexpected\27[39m 1" },
							},
							{ status = "pending", title = "later", ancestorTitles = { "Math" } },
						},
					},
					{
						testFilePath = "ReplicatedStorage/Project/Broken.spec",
						testResults = {},
						testExecError = { message = "Requested module experienced an error while loading" },
					},
				},
			})
			expect(report.framework).to.equal("jest")
			expect(report.status).to.equal("failed")
			expect(report.passed).to.equal(1)
			expect(report.failed).to.equal(1)
			expect(report.skipped).to.equal(1)
			expect(report.modules).to.equal(2)
			expect(table.concat(report.failures[1].path, " > ")).to.equal("Math > subtracts")
			expect(report.failures[1].messages[1]).to.equal("expected 1")
			expect(report.loadErrors[1].module).to.equal("ReplicatedStorage/Project/Broken.spec")
		end)
	end)

	describe("run", function()
		local function fakeTestEZ(results)
			local fake = { planned = nil }
			fake.TestPlanner = {
				createPlan = function(modules, pattern)
					fake.planned = { modules = modules, pattern = pattern }
					return fake.planned
				end,
			}
			fake.TestRunner = {
				runPlan = function()
					return results
				end,
			}
			return fake
		end

		it("loads each spec separately and reports the ones that fail to load", function()
			local testEZModule = module("TestEZ", fixture)
			local shared = folder("Shared", fixture)
			local good = module("Good.spec", shared)
			local broken = module("Broken.spec", shared)
			local notFunction = module("Value.spec", shared)
			local fake = fakeTestEZ({
				children = { node("It", "works", "Success") },
			})
			local function load(instance)
				if instance == testEZModule then
					return fake
				elseif instance == broken then
					error("Broken.spec:1: syntax error")
				elseif instance == notFunction then
					return 5
				elseif instance == good then
					return function() end
				end
				error("unexpected require")
			end
			local report = SpecRunner.run({
				roots = { fixturePath .. "/Shared" },
				module = fixturePath .. "/TestEZ",
				filter = "works",
			}, game, load)
			expect(fake.planned.pattern).to.equal("works")
			expect(#fake.planned.modules).to.equal(1)
			-- TestEZ's path: the spec first, then its parents up to the root.
			expect(fake.planned.modules[1].path[1]).to.equal("Good")
			expect(fake.planned.modules[1].path[2]).to.equal("Shared")
			expect(fake.planned.modules[1].pathStringForSorting).to.equal("shared good")
			expect(report.modules).to.equal(1)
			expect(report.passed).to.equal(1)
			expect(#report.loadErrors).to.equal(2)
			expect(report.status).to.equal("failed")
			local messages = report.loadErrors[1].message .. report.loadErrors[2].message
			expect(string.find(messages, "syntax error", 1, true)).to.be.ok()
			expect(string.find(messages, "must return a function", 1, true)).to.be.ok()
			expect(type(report.durationSeconds)).to.equal("number")
		end)

		it("returns an error report instead of throwing", function()
			local report = SpecRunner.run({ roots = { fixturePath }, module = fixturePath .. "/Missing" }, game)
			expect(report.status).to.equal("error")
			expect(string.find(report.error, "does not exist", 1, true)).to.be.ok()
			expect(report.passed).to.equal(0)

			report = SpecRunner.run({ timeoutSeconds = 0 }, game)
			expect(report.status).to.equal("error")
		end)

		it("reports a TestEZ module without a planner as an error", function()
			local testEZModule = module("TestEZ", fixture)
			local spec = module("Only.spec", fixture)
			local report = SpecRunner.run(
				{ roots = { fixturePath }, module = fixturePath .. "/TestEZ" },
				game,
				function(instance)
					if instance == testEZModule then
						return {}
					end
					expect(instance).to.equal(spec)
					return function() end
				end
			)
			expect(report.status).to.equal("error")
			expect(string.find(report.error, "TestPlanner", 1, true)).to.be.ok()
		end)

		it("gives up on specs that do not finish within the timeout", function()
			local testEZModule = module("TestEZ", fixture)
			module("Slow.spec", fixture)
			local fake = fakeTestEZ(nil)
			fake.TestRunner.runPlan = function()
				task.wait(5)
				return { children = {} }
			end
			local started = os.clock()
			local report = SpecRunner.runWithTimeout(
				{
					roots = { fixturePath },
					module = fixturePath .. "/TestEZ",
					timeoutSeconds = 1,
				},
				game,
				function(instance)
					if instance == testEZModule then
						return fake
					end
					return function() end
				end
			)
			expect(os.clock() - started < 3).to.equal(true)
			expect(report.status).to.equal("timedOut")
			expect(string.find(report.error, "1 seconds", 1, true)).to.be.ok()
		end)
	end)
end
