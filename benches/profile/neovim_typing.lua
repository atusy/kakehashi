-- Run with: env -u NVIM_LISTEN_ADDRESS nvim --clean --headless -l this-file.lua
-- Required environment: PROBE_BIN, PROBE_FILE, PROBE_CONFIG, PROBE_OUTPUT.
-- Set KAKEHASHI_DATA_DIR to the same attested runtime used by the server.
-- Uses Neovim's private semantic-token state (tested with 0.13-dev); fails if
-- that API changes. This measures client token conversion, not visible UI paint.
-- PROBE_LINE is a zero-based line whose first token must shift with inserted
-- spaces (default 1, suitable for the generated Rust fixture's use statement).
local edit_line = tonumber(vim.env.PROBE_LINE or "1")
local count = tonumber(vim.env.PROBE_SAMPLES or "8")
local burst = tonumber(vim.env.PROBE_BURST or "1")
local interval = tonumber(vim.env.PROBE_INTERVAL_MS or "20")
assert(burst and burst > 0 and burst % 1 == 0, "invalid PROBE_BURST")
assert(interval and interval >= 0, "invalid PROBE_INTERVAL_MS")
assert(edit_line and edit_line >= 0 and edit_line % 1 == 0, "invalid PROBE_LINE")
assert(count and count > 0 and count % 1 == 0, "invalid PROBE_SAMPLES")
for _, key in ipairs({ "PROBE_BIN", "PROBE_FILE", "PROBE_CONFIG", "PROBE_OUTPUT" }) do
	assert(vim.env[key] and vim.env[key] ~= "", key .. " is required")
end
local uv = vim.uv
local samples, active = {}, nil
local response_samples = {}
local function now()
	return uv.hrtime() / 1e6
end
local sem = vim.lsp.semantic_tokens
local cls = sem.__STHighlighter
local original = cls.process_response
cls.process_response = function(self, response, client, request_id, version, is_range)
	local origin = response_samples[request_id]
	local sample = origin and origin.sample
	if sample then
		sample.response_ms = now() - origin.started
		table.insert(sample.events, { event = "response", ms = sample.response_ms, id = request_id, version = version })
	end
	original(self, response, client, request_id, version, is_range)
	if
		sample
		and sample == active
		and sample.ready_ms == nil
		and version == vim.lsp.util.buf_versions[self.bufnr]
		and not is_range
		and self.client_state[client.id].current_result.version == vim.lsp.util.buf_versions[self.bufnr]
	then
		sample.ready_ms = now() - origin.started
		sample.response_version = version
	end
end
vim.cmd.edit(vim.env.PROBE_FILE)
local buf = vim.api.nvim_get_current_buf()
vim.bo[buf].filetype = vim.env.PROBE_LANG or "rust"
local id = vim.lsp.start({
	name = "kakehashi-probe",
	cmd = { vim.env.PROBE_BIN, "--config-file", vim.env.PROBE_CONFIG },
	root_dir = vim.fn.getcwd(),
	handlers = {
		["workspace/semanticTokens/refresh"] = function(err, result, ctx, config)
			if active then
				table.insert(active.events, { event = "refresh", ms = now() - active.started })
			end
			return vim.lsp.handlers["workspace/semanticTokens/refresh"](err, result, ctx, config)
		end,
	},
	on_init = function(client)
		local rpc_request = client.rpc.request
		client.rpc.request = function(method, params, callback, notify_reply_callback, ...)
			local sample = active and method:find("semanticTokens", 1, true) and active or nil
			local origin = sample and { sample = sample, started = sample.started } or nil
			local sent
			if sample then
				sample.wire_request_ms = now() - origin.started
				sent = { event = method, ms = sample.wire_request_ms }
				table.insert(sample.events, sent)
			end
			local success, request_id = rpc_request(method, params, function(err, result, reply_id)
				if sample then
					table.insert(sample.events, {
						event = "wire_response",
						method = method,
						id = reply_id,
						ms = now() - origin.started,
						error = err,
						null = result == nil or result == vim.NIL,
					})
				end
				-- process_response may yield; it captures this origin before yielding.
				response_samples[reply_id] = origin
				local returned = vim.F.pack_len(callback(err, result, reply_id))
				response_samples[reply_id] = nil
				return vim.F.unpack_len(returned)
			end, function(reply_id)
				if sample then
					table.insert(sample.events, {
						event = "wire_reply",
						method = method,
						id = reply_id,
						ms = now() - origin.started,
					})
				end
				if notify_reply_callback then
					return notify_reply_callback(reply_id)
				end
			end, ...)
			if sent then
				sent.id = request_id
			end
			return success, request_id
		end
		local notify = client.rpc.notify
		client.rpc.notify = function(method, params)
			if active then
				table.insert(active.events, { event = method, ms = now() - active.started })
			end
			if active and method == "textDocument/didChange" then
				active.did_change_ms = now() - active.started
			end
			return notify(method, params)
		end
		local request = client.request
		client.request = function(self, method, ...)
			if active and method:find("semanticTokens", 1, true) then
				active.request_ms = now() - active.started
				active.method = method
			end
			return request(self, method, ...)
		end
	end,
})
local function current()
	local h = cls.active[buf]
	local state = h and h.client_state[id]
	return state and state.has_full_result and state.current_result.version == vim.lsp.util.buf_versions[buf]
end
assert(vim.wait(60000, current, 10), "initial tokens timed out")
local function tracked_start()
	local result = cls.active[buf].client_state[id].current_result
	local row, column = 0, 0
	for i = 1, #result.tokens, 5 do
		local delta = result.tokens[i]
		row = row + delta
		column = delta == 0 and column + result.tokens[i + 1] or result.tokens[i + 1]
		if row == edit_line then
			return column
		end
		if row > edit_line then
			break
		end
	end
	error("PROBE_LINE has no semantic token")
end
local expected = tracked_start()
for i = 1, count do
	active = { started = now(), iteration = i, events = {} }
	for edit = 1, burst do
		active.ready_ms = nil
		active.last_edit_ms = now() - active.started
		vim.api.nvim_buf_set_text(buf, edit_line, 0, edit_line, 0, { " " })
		if edit < burst and interval > 0 then
			vim.wait(interval)
		end
	end
	active.edit_call_ms = now() - active.started
	if not vim.wait(60000, function()
		return active.ready_ms ~= nil
	end, 5) then
		local state = cls.active[buf] and cls.active[buf].client_state[id]
		vim.fn.writefile({
			vim.json.encode({
				error = "edit tokens timed out",
				samples = samples,
				failed_sample = active,
				buffer_version = vim.lsp.util.buf_versions[buf],
				current_version = state and state.current_result.version,
				active_request = state and state.active_request,
			}),
		}, vim.env.PROBE_OUTPUT)
		error("edit tokens timed out; partial trace saved to PROBE_OUTPUT")
	end
	expected = expected + burst
	assert(tracked_start() == expected, "response does not match the latest unique edit")
	active.follow_ms = active.ready_ms - active.last_edit_ms
	active.tracked_start = expected
	active.token_count = #cls.active[buf].client_state[id].current_result.tokens / 5
	active.started = nil
	samples[#samples + 1] = active
	active = nil
end
vim.fn.writefile({
	vim.json.encode({
		samples = samples,
		final_token_sha256 = vim.fn.sha256(vim.json.encode(cls.active[buf].client_state[id].current_result.tokens)),
		burst = burst,
		interval_ms = interval,
		nvim = vim.version(),
		file = vim.env.PROBE_FILE,
		note = "ready is client token conversion completion, not visible UI paint",
	}),
}, vim.env.PROBE_OUTPUT)
vim.lsp.get_client_by_id(id):stop(true)
vim.cmd("qa!")
