# Type hierarchy bridge sample

Open this file through kakehashi with a TypeScript language server configured,
then put the cursor on `GuideDog` inside the embedded block.

```typescript
interface Animal {
  name: string;
}

class Dog implements Animal {
  name = "dog";
}

class GuideDog extends Dog {}
```

Prepare an item and retain it in `g:type_hierarchy_item`:

```vim
:lua local c=vim.lsp.get_clients({bufnr=0})[1]; vim.lsp.buf_request(0, 'textDocument/prepareTypeHierarchy', vim.lsp.util.make_position_params(0, c.offset_encoding), function(e,r) assert(not e, vim.inspect(e)); vim.g.type_hierarchy_item=r and r[1]; vim.print(r) end)
```

From the prepared `GuideDog`, inspect its supertypes:

```vim
:lua vim.lsp.buf_request(0, 'typeHierarchy/supertypes', {item=vim.g.type_hierarchy_item}, function(e,r) assert(not e, vim.inspect(e)); vim.g.type_hierarchy_parent=r and r[1]; vim.print(r) end)
```

Then expand the returned `Dog` item in the opposite direction. The result
should contain `GuideDog`, with ranges translated to this Markdown buffer:

```vim
:lua vim.lsp.buf_request(0, 'typeHierarchy/subtypes', {item=vim.g.type_hierarchy_parent}, function(e,r) assert(not e, vim.inspect(e)); vim.print(r) end)
```
