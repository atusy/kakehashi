# Bridged semantic token range sample

Configure Lua as an injected bridge language, open this file as Markdown, and
place the cursor on `local answer = 42`:

> ```lua
> local answer = 42
> print(answer)
> ```

The following Neovim command asks only for the current quoted line. Replace the
configured Lua server as needed, then inspect the delta-encoded host-coordinate
tokens:

```vim
:lua local b=0; local l=vim.api.nvim_win_get_cursor(0)[1]-1; local p=vim.lsp.util.make_text_document_params(b); p.range={start={line=l,character=2},["end"]={line=l,character=19}}; vim.lsp.buf_request(b, 'textDocument/semanticTokens/range', p, function(e,r) assert(not e, vim.inspect(e)); vim.print(r and r.data) end)
```

The first token's `deltaLine` should be the Markdown host line and its
`deltaStart` should include the `> ` prefix. A range extending outside the Lua
region falls through to an enabled Markdown `bridge._self` server, then to
kakehashi's built-in Tree-sitter tokens.
