# Bridged full semantic tokens sample

Configure Lua as an injected bridge language and optionally enable a Markdown
`bridge._self` server, then open this file as Markdown:

hostword

> ```lua
> local answer = 42
> print(answer)
> ```

Ask kakehashi for the full token set from Neovim and inspect the host-coordinate
delta stream:

```vim
:lua local b=0; local p=vim.lsp.util.make_text_document_params(b); vim.lsp.buf_request(b, 'textDocument/semanticTokens/full', p, function(e,r) assert(not e, vim.inspect(e)); vim.print(r and r.data) end)
```

Tokens from the Lua server should use the Markdown line numbers and include the
`> ` column prefix. Uncovered spans remain highlighted by kakehashi's built-in
Tree-sitter layer; an enabled Markdown host server can refine them further.
