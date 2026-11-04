--[[
	Runs a place's TestEZ or Jest-Lua specs and returns a bounded report made
	of plain JSON data.

	The Studio bridge's `spec` action clones this module into the temporary
	runner script it inserts, so the module runs inside the Run-mode
	DataModel, where none of the plugin's packages exist. It therefore
	requires nothing outside itself and never reads `script`, which also lets
	the same source run through Open Cloud's Luau Execution.

	Report fields:
		framework     "testez" or "jest"
		status        "passed", "failed", "error" or "timedOut"
		passed, failed, skipped, modules
		failures      { { path = { phrase, ... }, messages = { text, ... } } }
		loadErrors    { { module = "game/...", message = text } }
		omittedFailures, omittedLoadErrors, truncatedMessages
		error         why the run itself failed (status "error" or "timedOut")
		durationSeconds
]]

local SpecRunner = {}

SpecRunner.MIN_TIMEOUT_SECONDS = 1
SpecRunner.MAX_TIMEOUT_SECONDS = 300
SpecRunner.DEFAULT_TIMEOUT_SECONDS = 60
SpecRunner.MAX_ROOTS = 32
SpecRunner.MAX_PATH_BYTES = 2048
SpecRunner.MAX_FILTER_BYTES = 256
-- A report has to fit the bridge's 8 MiB bodies (and Luau Execution's output
-- limit) with room to spare, so failures, messages and phrases are each
-- bounded, and so is the text they carry in total.
SpecRunner.MAX_FAILURES = 500
SpecRunner.MAX_LOAD_ERRORS = 100
SpecRunner.MAX_MESSAGES_PER_FAILURE = 8
SpecRunner.MAX_MESSAGE_BYTES = 4096
SpecRunner.MAX_PHRASE_BYTES = 256
SpecRunner.MAX_PATH_DEPTH = 32
SpecRunner.MAX_TEXT_BYTES = 2 * 1024 * 1024

-- Searched for specs when no roots are given: the services that hold a
-- place's code. Workspace is left out because it is mostly parts.
SpecRunner.DEFAULT_SERVICES = {
	"ReplicatedFirst",
	"ReplicatedStorage",
	"ServerScriptService",
	"ServerStorage",
	"StarterGui",
	"StarterPack",
	"StarterPlayer",
}

-- Searched for the framework module when none is given. Wally and pesde
-- projects usually map their (dev) packages into one of these.
SpecRunner.FRAMEWORK_SERVICES = { "ReplicatedStorage", "ServerStorage", "ServerScriptService" }

-- Package managers install dependencies, with their own specs, beneath these
-- folders. Running a dependency's specs would report failures the project
-- cannot fix, so they are skipped unless a root points inside them.
local VENDORED = { ["_Index"] = true, [".pesde"] = true }

-- Roblox inserts its default player scripts at run time when a place does not
-- provide its own, and PlayerModule ships internal `*.spec` modules that only
-- load inside Roblox's own test setup. They are skipped the same way.
local function isRobloxDefaultScripts(instance)
	local parent = instance.Parent
	return instance.Name == "PlayerModule" and parent ~= nil and parent.Name == "StarterPlayerScripts"
end

local REPLACEMENT_CHARACTER = utf8.char(0xFFFD)

-- Cut on a codepoint boundary so truncating cannot split a multibyte character.
local function truncateUtf8(value, maxBytes)
	if #value <= maxBytes then
		return value
	end
	local cut = maxBytes
	while cut > 0 do
		local nextByte = string.byte(value, cut + 1)
		if nextByte < 0x80 or nextByte >= 0xC0 then
			break
		end
		cut -= 1
	end
	return string.sub(value, 1, cut)
end

-- Error messages can carry any bytes a script produced, and JSON text must be
-- valid UTF-8.
local function toValidUtf8(value)
	if utf8.len(value) then
		return value
	end
	local parts = {}
	local position = 1
	while position <= #value do
		local length, invalid = utf8.len(value, position)
		if length then
			table.insert(parts, string.sub(value, position))
			break
		end
		table.insert(parts, string.sub(value, position, invalid - 1))
		table.insert(parts, REPLACEMENT_CHARACTER)
		position = invalid + 1
	end
	return table.concat(parts)
end

SpecRunner.truncateUtf8 = truncateUtf8
SpecRunner.toValidUtf8 = toValidUtf8

local function text(value, maxBytes)
	return truncateUtf8(toValidUtf8(tostring(value)), maxBytes)
end

--[[
	Checks the options a caller passed and fills in defaults. The result has
	the same shape as its input (roots stay nil when the defaults apply), so
	normalizing twice gives the same options.
]]
function SpecRunner.normalizeOptions(args)
	assert(args == nil or type(args) == "table", "Spec options must be an object")
	args = args or {}
	local roots = nil
	if args.roots ~= nil then
		assert(
			type(args.roots) == "table" and #args.roots >= 1 and #args.roots <= SpecRunner.MAX_ROOTS,
			"roots must list 1 to " .. SpecRunner.MAX_ROOTS .. " instance paths"
		)
		roots = {}
		for index = 1, #args.roots do
			local root = args.roots[index]
			assert(
				type(root) == "string" and #root > 0 and #root <= SpecRunner.MAX_PATH_BYTES,
				"Each root must be a game/... instance path"
			)
			table.insert(roots, root)
		end
	end
	local framework = if args.framework == nil then "auto" else args.framework
	assert(
		framework == "auto" or framework == "testez" or framework == "jest",
		"framework must be auto, testez or jest"
	)
	local module = args.module
	assert(
		module == nil or (type(module) == "string" and #module > 0 and #module <= SpecRunner.MAX_PATH_BYTES),
		"module must be a game/... instance path"
	)
	local filter = args.filter
	assert(
		filter == nil or (type(filter) == "string" and #filter <= SpecRunner.MAX_FILTER_BYTES),
		"filter must be a string of at most " .. SpecRunner.MAX_FILTER_BYTES .. " bytes"
	)
	if filter == "" then
		filter = nil
	end
	local timeoutSeconds = if args.timeoutSeconds == nil
		then SpecRunner.DEFAULT_TIMEOUT_SECONDS
		else args.timeoutSeconds
	assert(
		type(timeoutSeconds) == "number"
			and timeoutSeconds % 1 == 0
			and timeoutSeconds >= SpecRunner.MIN_TIMEOUT_SECONDS
			and timeoutSeconds <= SpecRunner.MAX_TIMEOUT_SECONDS,
		"timeoutSeconds must be a whole number from "
			.. SpecRunner.MIN_TIMEOUT_SECONDS
			.. " to "
			.. SpecRunner.MAX_TIMEOUT_SECONDS
	)
	return {
		roots = roots,
		framework = framework,
		module = module,
		filter = filter,
		timeoutSeconds = timeoutSeconds,
	}
end

-- The bridge's path syntax: '/'-joined names starting at `game`, where '\'
-- escapes a '/' or '\' inside a name.
local function splitPath(reference)
	local parts = {}
	local current = {}
	local index = 1
	while index <= #reference do
		local character = string.sub(reference, index, index)
		if character == "\\" and index < #reference then
			index += 1
			table.insert(current, string.sub(reference, index, index))
		elseif character == "/" then
			table.insert(parts, table.concat(current))
			current = {}
		else
			table.insert(current, character)
		end
		index += 1
	end
	table.insert(parts, table.concat(current))
	return parts
end

function SpecRunner.resolvePath(dataModel, reference)
	local parts = splitPath(reference)
	assert(parts[1] == "game", "Instance paths start with game/, such as game/ReplicatedStorage/Shared: " .. reference)
	local current = dataModel
	for index = 2, #parts do
		local found = nil
		for _, child in current:GetChildren() do
			if child.Name == parts[index] then
				assert(not found, "Ambiguous instance path, since two siblings share a name: " .. reference)
				found = child
			end
		end
		assert(found, "Instance path does not exist: " .. reference)
		current = found
	end
	return current
end

local function pathOf(instance, dataModel)
	local names = {}
	local current = instance
	while current and current ~= dataModel do
		table.insert(names, 1, (string.gsub(current.Name, "[\\/]", "\\%0")))
		current = current.Parent
	end
	table.insert(names, 1, "game")
	return table.concat(names, "/")
end

SpecRunner.pathOf = pathOf

local function services(dataModel, names)
	local found = {}
	for _, name in names do
		local ok, service = pcall(dataModel.FindService, dataModel, name)
		if ok and service then
			table.insert(found, service)
		end
	end
	return found
end

-- The shallowest ModuleScript with this (case-insensitive) name. A Wally
-- project has both `Packages.TestEZ`, which requires the real module, and the
-- real module deeper in `Packages._Index`; either works.
local function findModule(dataModel, lowerName)
	local best, bestDepth = nil, math.huge
	for _, service in services(dataModel, SpecRunner.FRAMEWORK_SERVICES) do
		for _, descendant in service:GetDescendants() do
			if descendant:IsA("ModuleScript") and string.lower(descendant.Name) == lowerName then
				local depth = 0
				local current = descendant
				while current ~= service do
					depth += 1
					current = current.Parent
				end
				if depth < bestDepth then
					best, bestDepth = descendant, depth
				end
			end
		end
	end
	return best
end

local function isVendored(instance, root)
	local current = instance.Parent
	while current and current ~= root do
		if VENDORED[current.Name] or isRobloxDefaultScripts(current) then
			return true
		end
		current = current.Parent
	end
	return false
end

local function isSpec(instance)
	return instance:IsA("ModuleScript") and string.match(instance.Name, "%.spec$") ~= nil
end

--[[
	Every `*.spec` ModuleScript beneath the roots, once each, skipping vendored
	packages and anything inside the framework module itself.
]]
function SpecRunner.findSpecs(roots, frameworkModule)
	local found = {}
	local seen = {}
	for _, root in roots do
		local candidates = root:GetDescendants()
		table.insert(candidates, 1, root)
		for _, instance in candidates do
			if
				not seen[instance]
				and isSpec(instance)
				and not isVendored(instance, root)
				and not (frameworkModule and (instance == frameworkModule or instance:IsDescendantOf(frameworkModule)))
			then
				seen[instance] = true
				table.insert(found, { instance = instance, root = root })
			end
		end
	end
	return found
end

-- Jest-Lua runs projects, each a folder holding a `jest.config` module.
local function findJestProjects(roots)
	local projects = {}
	local seen = {}
	for _, root in roots do
		local candidates = root:GetDescendants()
		table.insert(candidates, 1, root)
		for _, instance in candidates do
			local parent = instance.Parent
			if
				instance:IsA("ModuleScript")
				and instance.Name == "jest.config"
				and parent
				and not seen[parent]
				and not isVendored(instance, root)
			then
				seen[parent] = true
				table.insert(projects, parent)
			end
		end
	end
	return projects
end

--[[
	Resolves the roots and the framework module. Called in Edit mode before
	Run mode starts, so a wrong path fails fast, and again inside Run mode.
]]
function SpecRunner.prepare(options, dataModel)
	local roots
	if options.roots then
		roots = {}
		for _, path in options.roots do
			table.insert(roots, SpecRunner.resolvePath(dataModel, path))
		end
	else
		roots = services(dataModel, SpecRunner.DEFAULT_SERVICES)
	end

	local framework = options.framework
	local module = nil
	if options.module then
		module = SpecRunner.resolvePath(dataModel, options.module)
		assert(module:IsA("ModuleScript"), "The framework module must be a ModuleScript: " .. options.module)
		if framework == "auto" then
			framework = if string.lower(module.Name) == "jest" then "jest" else "testez"
		end
	else
		if framework ~= "jest" then
			module = findModule(dataModel, "testez")
			if module then
				framework = "testez"
			end
		end
		if not module and framework ~= "testez" then
			module = findModule(dataModel, "jest")
			if module then
				framework = "jest"
			end
		end
		if not module then
			local wanted = if framework == "testez"
				then "TestEZ"
				elseif framework == "jest" then "Jest"
				else "TestEZ or Jest"
			error(
				"Could not find a ModuleScript named "
					.. wanted
					.. " in "
					.. table.concat(SpecRunner.FRAMEWORK_SERVICES, ", ")
					.. ". Install it (for example roblox/testez with Wally) and sync it, or pass its game/... path as the module option.",
				0
			)
		end
	end

	local prepared = { roots = roots, framework = framework, module = module }
	if framework == "jest" then
		prepared.projects = findJestProjects(roots)
		assert(#prepared.projects > 0, "No jest.config ModuleScript found beneath the roots")
	else
		prepared.specs = SpecRunner.findSpecs(roots, module)
		local searched = {}
		for _, root in roots do
			table.insert(searched, pathOf(root, dataModel))
		end
		assert(#prepared.specs > 0, "No *.spec ModuleScripts found beneath " .. table.concat(searched, ", "))
	end
	return prepared
end

local function newReport(framework)
	return {
		framework = framework,
		status = "passed",
		passed = 0,
		failed = 0,
		skipped = 0,
		modules = 0,
		failures = {},
		loadErrors = {},
		omittedFailures = 0,
		omittedLoadErrors = 0,
		truncatedMessages = 0,
		-- Bytes of text still allowed in this report. Not part of the result.
		budget = SpecRunner.MAX_TEXT_BYTES,
	}
end

local function addText(report, value, maxBytes)
	local full = toValidUtf8(tostring(value))
	local limited = truncateUtf8(full, math.min(maxBytes, report.budget))
	if #limited < #full then
		report.truncatedMessages += 1
	end
	report.budget -= #limited
	return limited
end

-- `path` lists phrases from the outermost describe to the test.
local function addFailure(report, path, messages)
	report.failed += 1
	if #report.failures >= SpecRunner.MAX_FAILURES then
		report.omittedFailures += 1
		return
	end
	local phrases = {}
	-- Keep the innermost phrases, which name the test, when a path is deep.
	local first = math.max(1, #path - SpecRunner.MAX_PATH_DEPTH + 1)
	for index = first, #path do
		table.insert(phrases, text(path[index], SpecRunner.MAX_PHRASE_BYTES))
	end
	local kept = {}
	for index, message in messages do
		-- Messages past the per-failure limit or the report's text budget
		-- are dropped and counted.
		if index <= SpecRunner.MAX_MESSAGES_PER_FAILURE and report.budget > 0 then
			table.insert(kept, addText(report, message, SpecRunner.MAX_MESSAGE_BYTES))
		else
			report.truncatedMessages += 1
		end
	end
	table.insert(report.failures, { path = phrases, messages = kept })
end

local function addLoadError(report, module, message)
	if #report.loadErrors >= SpecRunner.MAX_LOAD_ERRORS then
		report.omittedLoadErrors += 1
		return
	end
	table.insert(report.loadErrors, {
		module = text(module, SpecRunner.MAX_PATH_BYTES),
		message = addText(report, message, SpecRunner.MAX_MESSAGE_BYTES),
	})
end

local function finish(report)
	report.budget = nil
	if report.status == "passed" and (report.failed > 0 or #report.loadErrors + report.omittedLoadErrors > 0) then
		report.status = "failed"
	end
	return report
end

function SpecRunner.errorReport(framework, message)
	local report = newReport(framework)
	report.status = "error"
	report.error = text(message, SpecRunner.MAX_MESSAGE_BYTES)
	report.budget = nil
	return report
end

--[[
	Turns a TestEZ results tree into a report. A node with errors is one
	failure: a failed `it`, a failing beforeAll/afterAll hook (TestEZ adds an
	`it` for those) or a describe block whose body errored while planning,
	which TestEZ's own failure count leaves out.
]]
function SpecRunner.collectTestEZ(results, report)
	report = report or newReport("testez")
	local path = {}
	local function visit(node)
		local planNode = node.planNode or {}
		table.insert(path, if planNode.phrase == nil then "" else planNode.phrase)
		local errors = node.errors or {}
		if #errors > 0 then
			addFailure(report, path, errors)
		elseif planNode.type == "It" then
			if node.status == "Success" then
				report.passed += 1
			elseif node.status == "Skipped" then
				report.skipped += 1
			elseif node.status == "Failure" then
				addFailure(report, path, { "Test failed without an error message" })
			end
		end
		for _, child in node.children or {} do
			visit(child)
		end
		table.remove(path)
	end
	for _, child in results.children or {} do
		visit(child)
	end
	return finish(report)
end

-- TestEZ's path for a spec: names from the spec up to its root, with `.spec`
-- removed, where an `init.spec` module stands for its parent.
local function testEZPath(instance, root)
	local path = {}
	local current = instance
	if current.Name == "init.spec" then
		current = current.Parent
	end
	while current ~= nil and current ~= root do
		table.insert(path, (string.gsub(current.Name, "%.spec$", "")))
		current = current.Parent
	end
	table.insert(path, (string.gsub(root.Name, "%.spec$", "")))
	return path
end

function SpecRunner.runTestEZ(TestEZ, prepared, options, dataModel, load)
	assert(
		type(TestEZ) == "table" and type(TestEZ.TestPlanner) == "table" and type(TestEZ.TestRunner) == "table",
		"The TestEZ module does not expose TestPlanner and TestRunner"
	)
	local report = newReport("testez")
	-- TestEZ's bootstrap requires every spec itself, and one spec that fails
	-- to load stops the whole run, so specs are loaded here one at a time.
	local modules = {}
	for _, spec in prepared.specs do
		local ok, method = pcall(load, spec.instance)
		if ok and type(method) ~= "function" then
			ok, method = false, "Spec module must return a function, got " .. typeof(method)
		end
		if ok then
			local path = testEZPath(spec.instance, spec.root)
			local pathString = ""
			for index = #path, 1, -1 do
				pathString ..= (if index == #path then "" else " ") .. path[index]
			end
			table.insert(modules, {
				method = method,
				path = path,
				pathStringForSorting = string.lower(pathString),
			})
		else
			addLoadError(report, pathOf(spec.instance, dataModel), method)
		end
	end
	report.modules = #modules
	local plan = TestEZ.TestPlanner.createPlan(modules, options.filter, {})
	local results = TestEZ.TestRunner.runPlan(plan)
	return SpecRunner.collectTestEZ(results, report)
end

-- Jest colours its messages for terminals.
local function stripAnsi(value)
	return (string.gsub(tostring(value), "\27%[[%d;]*m", ""))
end

--[[
	Turns Jest-Lua's aggregated result (`runCLI(...)`'s `results`) into a
	report. A test file that failed to run counts as a load error.
]]
function SpecRunner.collectJest(aggregated, report)
	report = report or newReport("jest")
	for _, file in aggregated.testResults or {} do
		report.modules += 1
		local failedTests = 0
		for _, test in file.testResults or {} do
			if test.status == "passed" then
				report.passed += 1
			elseif test.status == "failed" then
				failedTests += 1
				local path = table.clone(test.ancestorTitles or {})
				table.insert(path, test.title or test.fullName or "")
				local messages = {}
				for _, message in test.failureMessages or {} do
					table.insert(messages, stripAnsi(message))
				end
				addFailure(report, path, messages)
			else
				report.skipped += 1
			end
		end
		local suiteError = file.testExecError or (failedTests == 0 and file.failureMessage)
		if suiteError then
			local message = if type(suiteError) == "table" then suiteError.message else suiteError
			addLoadError(report, file.testFilePath or "unknown test file", stripAnsi(message))
		end
	end
	return finish(report)
end

function SpecRunner.runJest(Jest, prepared, options)
	assert(type(Jest) == "table" and type(Jest.runCLI) == "function", "The Jest module does not expose runCLI")
	local argv = { ci = true, verbose = false }
	if options.filter then
		argv.testNamePattern = options.filter
	end
	local ok, output = Jest.runCLI(prepared.projects[1], argv, prepared.projects):await()
	assert(ok, tostring(output))
	assert(type(output) == "table" and type(output.results) == "table", "Jest returned no results")
	return SpecRunner.collectJest(output.results)
end

--[[
	Runs the specs and always returns a report, never an error. `load`
	requires a module and is replaced by tests.
]]
function SpecRunner.run(args, dataModel, load)
	dataModel = dataModel or game
	load = load or require
	local started = os.clock()
	local framework = if type(args) == "table" and args.framework == "jest" then "jest" else "testez"
	local ok, report = pcall(function()
		local options = SpecRunner.normalizeOptions(args)
		local prepared = SpecRunner.prepare(options, dataModel)
		framework = prepared.framework
		local module = load(prepared.module)
		if framework == "jest" then
			return SpecRunner.runJest(module, prepared, options)
		end
		return SpecRunner.runTestEZ(module, prepared, options, dataModel, load)
	end)
	if not ok then
		report = SpecRunner.errorReport(framework, report)
	end
	report.durationSeconds = os.clock() - started
	return report
end

--[[
	Runs the specs on their own thread and gives up after the timeout, so a
	spec that yields forever still produces a report. A spec that never
	yields cannot be interrupted, but Roblox stops it once it exhausts the
	script execution timeout.
]]
function SpecRunner.runWithTimeout(args, dataModel, load)
	local ok, options = pcall(SpecRunner.normalizeOptions, args)
	if not ok then
		return SpecRunner.errorReport("testez", options)
	end
	local finished, report = false, nil
	local thread = task.spawn(function()
		report = SpecRunner.run(options, dataModel, load)
		finished = true
	end)
	local deadline = os.clock() + options.timeoutSeconds
	while not finished and os.clock() < deadline do
		task.wait(0.05)
	end
	if finished then
		return report
	end
	pcall(task.cancel, thread)
	report = SpecRunner.errorReport(
		if options.framework == "jest" then "jest" else "testez",
		"Specs did not finish within " .. options.timeoutSeconds .. " seconds"
	)
	report.status = "timedOut"
	report.durationSeconds = options.timeoutSeconds
	return report
end

return SpecRunner
