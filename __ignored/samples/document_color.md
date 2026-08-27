# Document color host/virtual layer sample

Run kakehashi with `KAKEHASHI_EXPERIMENTAL=true`, a Markdown language server
that provides document colors, and a CSS language server for embedded blocks.
Configure the Markdown language with:

```toml
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/documentColor"]
strategy = "concatenated"
priorities = ["virt", "host"]
```

A host-language provider can mark this prose color: `#ff0000`.

```css
.sample {
  color: #00ff00;
}
```

In Neovim, inspect both results. The host range must remain on the prose line,
while the CSS range must point inside the fenced block:

```vim
:lua local c=vim.lsp.get_clients({bufnr=0})[1]; vim.lsp.buf_request(0, 'textDocument/documentColor', {textDocument={uri=vim.uri_from_bufnr(0)}}, function(e,r) assert(not e, vim.inspect(e)); vim.print(r) end)
```

Requesting `textDocument/colorPresentation` for the prose result should route
back through the Markdown server; requesting it for the CSS result should use
the embedded CSS server and translate only the safe edits back to this buffer.
