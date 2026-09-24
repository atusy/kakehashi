# Handle Unusable Startup Configuration by Frontend

| | |
| --- | --- |
| **Status** | accepted |
| **Date** | 2026-09-25 |
| **Decision-makers** | kakehashi maintainers through PR review |
| **Consulted** | [Issue #731](https://github.com/atusy/kakehashi/issues/731) |
| **Informed** | LSP and CLI users |

## Context and Problem Statement

Startup could silently ignore malformed discovered user/project files or a
misspelled explicit path and run with defaults. A failure was at most a
`window/logMessage` warning, which users rarely see, and nothing at all in the
`format`/`diagnose` CLI. The earlier
[configuration-merging-strategy](configuration-merging-strategy.md#file-loading-behavior)
tolerated discovered files deliberately — a half-edited `kakehashi.toml` must
not leave an editor without a server — but rejected an unusable explicit file
even in the editor. This decision replaces that startup policy to resolve #731;
it does not supersede the merging algorithm or runtime update policy in that
ADR.

## Decision Drivers

- A file the user configured must not silently disappear from effective settings.
- The CLI runs unattended, often in CI: a clean result computed on programmed
  defaults would be believed.
- An editor is better served by a server running on part of its configuration
  than by no server.
- Missing default locations must preserve zero-config operation.

## Considered Options

1. Keep tolerant discovered-file loading, strict explicit files, and optional
   explicit overlays.
2. Reject every unusable startup file in every frontend.
3. Let the frontend choose: the CLI rejects every unusable startup file; the LSP
   server reports each one visibly and starts without it.

## Decision Outcome

Choose option 3. Strictness is owned by the frontend, not by how a file was
selected.

A startup file is *unusable* when it is an explicit `--config-file` entry that
does not exist; or, whether explicit or discovered, when it is unreadable,
malformed, oversized, or carries invalid file settings (an unexpandable path,
an unresolvable directory, nested or too many bases, an unusable base); or when
a discovered location sits beneath a broken symbolic link. A missing base file
and a missing default location are not unusable. Unknown keys remain warnings.

- **`format` and `diagnose`**: the first unusable file, or a merged file
  configuration that violates a cross-field invariant, prints the path and
  reason and exits 2 before producing output. Reading stops at the first
  failure, so a later path naming a stream cannot hang a run already doomed.
- **LSP server**: each unusable file is reported as an error
  (`window/showMessage`) naming the path, and skipped. An unusable base is
  skipped on its own and its entry still applies, as discovered entries always
  treated their bases; an entry over the 64-base limit loads its first 64. The
  remaining files load. A merged configuration that is
  invalid is left to the ordinary merge, which reports it once and falls back to
  programmed defaults. `initialize` always succeeds on file grounds.

Startup reads the selected files once, before latching initialization state,
and carries the layers into the settings merge without re-reading them. Only
explicit stacks are retained for later replay — under the LSP policy, only the
entries that loaded, so repairing a skipped file takes a restart. Implicit files
are still rediscovered on later workspace-root changes using the existing
tolerant runtime policy. Client initializationOptions retain their existing
nonfatal validation policy.

### Consequences

- Configuration mistakes surface at their source rather than as missing
  features: as an exit status in CI, as an error popup in the editor.
- CLI invocations that list absent overlays must omit those arguments or create
  the files, and a half-edited discovered file fails a CLI run until fixed.
- An editor session whose `--config-file` is unusable now starts without it
  instead of refusing to initialize.
- Missing default locations still permit startup without configuration.

### Confirmation

CLI e2e tests exercise a missing explicit path, an invalid explicit file, an
invalid discovered file, and a broken discovered symlink, verifying exit 2 and
empty stdout. LSP e2e tests verify that initialize succeeds, that exactly one
error popup names each failing path, and that the valid layers still apply
(a broken user config leaves the project config in force, and vice versa).
Unit tests pin that a failing base costs only itself and that the strict read
stops at the first failure. Existing merge-order, path anchoring, unknown-key,
and runtime reload tests continue to pass.

## Pros and Cons of the Options

### Keep the earlier policy

Keeps an editor running through a broken discovered file, but reports it only
in a log, reports nothing in the CLI, and lets a misspelled overlay path pass
silently — #731 stays unresolved.

### Reject everywhere

Consistent, but a half-edited `kakehashi.toml` leaves the editor with no server
at all, and most editors do not retry a rejected `initialize`.

### Frontend chooses

Fails CI loudly and keeps editors working with a visible error. The same file
can therefore stop a CLI run yet only warn in the editor, which is deliberate:
the two frontends differ in who is watching.
