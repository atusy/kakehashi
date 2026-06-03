# ADR-0001: Hierarchical CLI Subcommands

## Status

Accepted

## Context

kakehashi started with flat CLI commands:

```
kakehashi install <language>
kakehashi list-languages
```

As the project grows, we need to add more commands:
- Status checking for installed languages
- Uninstalling languages
- Configuration file generation
- Potentially cache management, diagnostics, etc.

A flat command structure leads to:
1. **Naming conflicts**: `status` could mean language status or server status
2. **Poor discoverability**: Users can't easily find related commands
3. **Inconsistent naming**: `list-languages` vs `install` (noun-verb vs verb)
4. **Limited scalability**: Adding new features requires creative naming

## Decision

Adopt a hierarchical subcommand structure following the `<resource> <action>` pattern:

```
kakehashi language install <lang>
kakehashi language list
kakehashi language status
kakehashi language uninstall <lang>
kakehashi config init
```

This follows established CLI patterns from:
- **kubectl**: `kubectl get pods`, `kubectl delete deployment`
- **docker**: `docker container run`, `docker image list`
- **gh (GitHub CLI)**: `gh repo clone`, `gh pr create`

### Command Structure

```
kakehashi
├── language          # Language/parser management
│   ├── install       # Install parser and queries
│   ├── list          # List available languages
│   ├── status        # Show installed languages
│   └── uninstall     # Remove parser and queries
├── config            # Configuration management
│   └── init          # Generate default config file
└── (no subcommand)   # Start LSP server (default)
```

## Consequences

### Positive

- **Discoverability**: `kakehashi language --help` shows all language-related commands
- **Scalability**: Easy to add new resource groups (e.g., `cache`, `debug`)
- **Consistency**: All commands follow the same `<resource> <action>` pattern
- **Tab completion**: Shell completion can suggest resources first, then actions
- **Grouping**: Related functionality is clearly grouped together

### Negative

- **Breaking change**: Existing `install` and `list-languages` commands will stop working
- **Verbosity**: Commands are longer (`language install` vs `install`)
- **Learning curve**: Users familiar with old commands need to update scripts

### Neutral

- The default behavior (starting LSP server with no arguments) remains unchanged
- This is an early-stage project with few users, minimizing migration impact
