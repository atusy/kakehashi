# Inline value host/virtual layer sample

Configure a debugger-aware language server for Lua and, if desired, a Markdown
server through `bridge._self`. The default `preferred` layer order uses the Lua
server for a request whose visible range and stopped location are both inside
this block:

> ```lua
> local answer = 42
> print(answer)
> ```

Place the cursor on `local answer = 42`, replace `frameId = 1` below with a live
DAP stack frame id, and inspect the translated result in Neovim:

```vim
:lua local b=0; local l=vim.api.nvim_win_get_cursor(0)[1]-1; local p=vim.lsp.util.make_text_document_params(b); p.range={start={line=l,character=2},["end"]={line=l+1,character=15}}; p.context={frameId=1,stoppedLocation={start={line=l+1,character=2},["end"]={line=l+1,character=15}}}; vim.lsp.buf_request(b, 'textDocument/inlineValue', p, function(e,r) assert(not e, vim.inspect(e)); vim.print(r) end)
```

The returned ranges should point at the quoted host lines (including the `> `
column offset), never at virtual line zero. Requests outside the block fall back
to the Markdown host server only when `bridge._self.enabled = true`.
