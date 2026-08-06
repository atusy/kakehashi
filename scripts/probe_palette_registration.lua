-- Manual probe for the palette-command registration gate in
-- docs/architecture-decisions/execute-command-routing-token.md.
--
-- The two questions that ADR deferred can only be answered by a real client:
--   Q1 does the editor tolerate a `client/registerCapability` for
--      `workspace/executeCommand` arriving repeatedly, as workspace roots are
--      discovered lazily through a session?
--   Q2 is a machine-generated command id usable where the client surfaces it?
--
-- Run from the repo root, after `cargo build --features e2e`:
--
--   PROBE_ROOT=/path/with/repo-a-and-repo-b \
--   KAKEHASHI_BIN=target/debug/kakehashi \
--   MOCK_BIN=target/debug/mock-lsp-formatter \
--   FORCE_EXEC_DYNREG=1 \
--     nvim --headless --noplugin -u NONE -l scripts/probe_palette_registration.lua
--
-- PROBE_ROOT must hold `repo-a/` and `repo-b/`, each with a `.git` directory
-- and a `doc.md` containing a lua fence. Two marker roots is the whole point:
-- per-root pooling is on by default, so ONE kakehashi spawns TWO connections of
-- one downstream server -- the shape that makes a raw palette name ambiguous
-- (#823).
--
-- FORCE_EXEC_DYNREG exists because Neovim does NOT advertise
-- `workspace.executeCommand.dynamicRegistration` on its own, so without it
-- kakehashi registers nothing and the probe measures nothing. That is itself a
-- finding, recorded in the ADR.

local root = vim.fn.fnamemodify(vim.env.PROBE_ROOT or vim.uv.cwd(), ":p"):gsub("/$", "")
local kakehashi = vim.env.KAKEHASHI_BIN
local mock = vim.env.MOCK_BIN

local log = {}
local function say(fmt, ...)
  local line = select("#", ...) > 0 and string.format(fmt, ...) or fmt
  table.insert(log, line)
end

-- Capture every registerCapability / unregisterCapability the server sends,
-- then hand off to Neovim's own handler so we also learn whether IT accepts them.
local registrations = {}
local unregistrations = {}
local handler_errors = {}

local orig_register = vim.lsp.handlers["client/registerCapability"]
vim.lsp.handlers["client/registerCapability"] = function(err, result, ctx, config)
  local copy = vim.deepcopy(result)
  copy.__client = ctx and ctx.client_id
  table.insert(registrations, copy)
  local ok, res = pcall(orig_register, err, result, ctx, config)
  if not ok then
    table.insert(handler_errors, "register: " .. tostring(res))
    return vim.NIL
  end
  return res
end

local orig_unregister = vim.lsp.handlers["client/unregisterCapability"]
vim.lsp.handlers["client/unregisterCapability"] = function(err, result, ctx, config)
  table.insert(unregistrations, vim.deepcopy(result))
  local ok, res = pcall(orig_unregister, err, result, ctx, config)
  if not ok then
    table.insert(handler_errors, "unregister: " .. tostring(res))
    return vim.NIL
  end
  return res
end

local caps = vim.lsp.protocol.make_client_capabilities()
if vim.env.FORCE_EXEC_DYNREG == "1" then
  caps.workspace = caps.workspace or {}
  caps.workspace.executeCommand = { dynamicRegistration = true }
end

vim.lsp.config["kakehashi"] = {
  cmd = { kakehashi },
  capabilities = caps,
  filetypes = { "markdown" },
  -- ONE kakehashi for both repos: its own per-root pooling (workspaceMarkers
  -- defaults to ) is what spawns a downstream connection per repo. A
  -- root_dir per file would start two kakehashi processes instead, which is a
  -- different question entirely.
  root_dir = function(_, on_dir)
    on_dir(root)
  end,
  init_options = {
    languages = { markdown = { bridge = { lua = { enabled = true } } } },
    languageServers = {
      ["mock-codeaction"] = { cmd = { mock, "code-action" }, languages = { "lua" } },
    },
  },
}
vim.lsp.enable("kakehashi")

local function wait(ms)
  vim.wait(ms, function() return false end, 50)
end

local function open(path)
  vim.cmd.edit(path)
  vim.bo.filetype = "markdown"
  wait(4000)
end

-- 1st root
open(root .. "/repo-a/doc.md")
local after_a = #registrations
say("== after opening repo-a ==")
say("registerCapability requests: %d", after_a)

-- 2nd root: the lazily-discovered one Q1 is about
open(root .. "/repo-b/doc.md")
say("== after opening repo-b ==")
say("registerCapability requests: %d (delta %d)", #registrations, #registrations - after_a)

say("")
say("== every executeCommand registration, in arrival order ==")
local ids = {}
local dup_ids = {}
for i, params in ipairs(registrations) do
  for _, reg in ipairs(params.registrations or {}) do
    if reg.method == "workspace/executeCommand" then
      local cmds = (reg.registerOptions or {}).commands or {}
      say("  [req %d client %s] id=%s", i, tostring(params.__client), reg.id)
      if ids[reg.id] then table.insert(dup_ids, reg.id) end
      ids[reg.id] = true
    end
  end
end
say("duplicate registration ids across the session: %s",
  #dup_ids == 0 and "none" or vim.inspect(dup_ids))
say("unregisterCapability requests: %d", #unregistrations)
say("handler errors: %s", #handler_errors == 0 and "none" or vim.inspect(handler_errors))

say("")
say("== what the client exposes (Q2) ==")
local client = vim.lsp.get_clients({ name = "kakehashi" })[1]
if not client then
  say("  NO CLIENT — kakehashi did not attach")
else
  say("  kakehashi clients attached: %d", #vim.lsp.get_clients({ name = "kakehashi" }))
  say("  server_capabilities.executeCommandProvider = %s",
    vim.inspect(client.server_capabilities.executeCommandProvider):gsub("%s+", " "))
  -- Neovim keys dynamic registrations by CAPABILITY name, not by method.
  local regs = (rawget(client, "registrations") or {})["executeCommandProvider"] or {}
  local offered = {}
  for _, reg in ipairs(regs) do
    for _, c in ipairs((reg.registerOptions or {}).commands or {}) do
      table.insert(offered, c)
    end
  end
  table.sort(offered)
  say("  commands the client now holds (%d):", #offered)
  for _, c in ipairs(offered) do
    say("    %s", (c:gsub(vim.pesc(root), "<root>")))
  end

  say("")
  say("== end to end: raw vs routed (Q2's real substance) ==")
  local messages = {}
  local orig_log = vim.lsp.handlers["window/logMessage"]
  vim.lsp.handlers["window/logMessage"] = function(err, result, ctx, cfg)
    if result and result.message and result.message:match("kakehashi") then
      table.insert(messages, result.message)
    end
    return orig_log and orig_log(err, result, ctx, cfg)
  end

  local function run(name, label)
    local before = #messages
    -- Wait for the CALLBACK, not a fixed interval: `client:request` is async, so
    -- a timer that expires first would report a half-finished request as if it
    -- were the result.
    local settled = false
    client:request("workspace/executeCommand", { command = name, arguments = {} },
      function(rerr, rres)
        say("    %s -> err=%s result=%s", label, vim.inspect(rerr):gsub("%s+", " "),
          vim.inspect(rres):gsub("%s+", " "))
        settled = true
      end)
    if not vim.wait(2500, function() return settled end, 50) then
      say("    %s -> request never came back", label)
    end
    -- The refusal notification is a separate message and can land just after the
    -- response. Give it a moment, or it gets attributed to the NEXT command.
    vim.wait(500, function() return #messages > before end, 20)
    local said = {}
    for i = before + 1, #messages do table.insert(said, messages[i]) end
    say("    %s -> editor was told: %s", label,
      #said == 0 and "(nothing)" or vim.inspect(said):gsub("%s+", " "):sub(1, 260))
  end

  run("mock.run", "RAW  mock.run")
  local routed_b
  for _, c in ipairs(offered) do
    if c:match("repo%-b") then routed_b = c end
  end
  if routed_b then
    run(routed_b, "ROUTED repo-b")
  else
    say("    ROUTED repo-b -> no routed entry found to try")
  end

  say("  supports_method('workspace/executeCommand') = %s",
    tostring(client:supports_method("workspace/executeCommand")))
end

io.write(table.concat(log, "\n"), "\n")
vim.cmd("qall!")
