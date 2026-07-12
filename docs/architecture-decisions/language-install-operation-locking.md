# Coordinate Language Mutations with a Data-Directory Lock

## Context

Parser and query installs already serialize same-language replacement, but
`language uninstall --all` first discovers a set of names and then removes that
snapshot. A different process can publish another language after discovery, so
the command can report that everything was removed while an install remains.
The protocol must cover CLI and LSP auto-install processes without preventing
independent language installs from running concurrently.

## Decision

Each data directory owns a persistent advisory operation-lock file.

- Parser install and top-level query install phases acquire a shared lock.
- Targeted uninstall holds a shared lock across query and parser removal.
- `uninstall --all` asks for confirmation before locking, then acquires the
  exclusive lock, repeats recovery and discovery, and holds the lock through
  every removal and the final verification.
- The global operation lock is always acquired before any per-language parser
  or query replacement lock. Code holding a per-language lock must never try to
  acquire the global lock.

The successful bulk uninstall linearizes at its verified-empty state while the
exclusive lock is held. An install phase that acquires its shared lock after
that point is ordered after the uninstall and may legitimately create a new
artifact.

## Considered Options

1. A shared/exclusive advisory lock per data directory.
2. Repeated unlocked discovery/removal until a scan happens to be empty.
3. A persistent uninstall generation/tombstone checked by every installer.

Option 1 is chosen. It gives an explicit cross-process order and composes with
the existing filesystem locks. Option 2 can starve and never proves that an
installer is not between its own check and publish. Option 3 can encode order,
but requires transactional generation metadata and recovery in every parser
and query publication path.

## Consequences

### Positive

- Successful `uninstall --all` has a precise, testable linearization point.
- Separate language installs retain shared-lock concurrency.
- The protocol works across CLI and LSP server processes.

### Negative

- Bulk uninstall waits for every in-flight install phase and temporarily blocks
  new ones.
- All new filesystem mutation entry points must participate and preserve the
  global-before-language lock order.
- Advisory locks only coordinate cooperating kakehashi processes; arbitrary
  external filesystem writers remain outside the guarantee.

### Neutral

- The lock file remains in the data directory and is not an installed-language
  artifact.

## Confirmation

- Cross-process tests prove an exclusive bulk operation waits for shared
  installers and prevents new shared publishers until it releases.
- CLI regression coverage pauses after the preliminary snapshot, starts a
  cooperating install, and proves the authoritative locked rescan removes it
  or orders its publication after the uninstall.
- Review verifies every parser/query install and targeted/bulk uninstall path
  follows the documented lock order.

