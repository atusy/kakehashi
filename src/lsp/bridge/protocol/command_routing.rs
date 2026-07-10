//! Command-name routing for bridged `workspace/executeCommand` (#568 PR 6).
//!
//! A downstream `Command` (bare, or embedded in a `CodeAction`) is executed by
//! the client via `workspace/executeCommand`, which carries only `command` +
//! `arguments` — no `data` envelope. So the origin server can't ride along in
//! `data` the way `codeAction/resolve` routes (that path DOES get `data`).
//!
//! Instead the command NAME itself is the routing key: when the bridge surfaces
//! a downstream command it rewrites the name to encode the origin server AND the
//! host document it came from, and the bridged `executeCommand` decodes both to
//! reach the exact `(server, root)` connection that has the (virtual or host)
//! document open. The `arguments` stay verbatim — they are the downstream's own
//! coordinate system (checklist §10). Rewriting the name (which the bridge
//! minted when it surfaced the action) does not violate that.
//!
//! Encoding is stateless (no cross-request registry to populate/evict) and
//! therefore collision-safe: two servers advertising the same command name
//! (realistic once concatenated aggregation lands, #568 PR 7) encode to
//! distinct names and route correctly.
//!
//! The separator is the ASCII Unit Separator (`0x1f`). The routing invariant is
//! that it must not appear in the FIRST two segments — `origin` (a TOML config
//! key) and `host_uri` (a URL) — so those boundaries are unambiguous. `host_uri`
//! is control-char-free by construction (a URL), but `origin` is an unvalidated
//! config key, so [`encode_command`] ENFORCES the invariant: it fails closed
//! (`None`) rather than mint an ambiguous name, and the caller drops the
//! unroutable command. The `command` is the final segment and taken as the
//! `splitn(3)` remainder, so even an (LSP-legal but unheard-of) separator inside
//! a command id round-trips faithfully. Decoding is total: any name that isn't a
//! well-formed bridge command yields `None`, and the handler fails that command
//! soft rather than panicking.
//!
//! NOTE: this reaches only commands surfaced through a bridged action. Commands
//! the client fires WITHOUT an action context (from a palette, keyed off the
//! advertised `executeCommandProvider.commands` list) need real advertised
//! names + dynamic registration and are a deliberate follow-up (see #568 PR 6).

/// Marker that identifies a bridge-minted command name.
const COMMAND_PREFIX: &str = "kakehashi";
/// ASCII Unit Separator — cannot occur in the `origin` (a config server name)
/// or `host_uri` segments, which is the only invariant the split needs (the
/// `command` is the `splitn(3)` remainder, so a separator there is harmless).
const SEP: char = '\u{1f}';

/// A decoded bridge command name: the origin server, the host document it was
/// surfaced from, and the downstream's own command id (forwarded verbatim).
pub(crate) struct CommandRoute<'a> {
    pub(crate) origin: &'a str,
    pub(crate) host_uri: &'a str,
    pub(crate) command: &'a str,
}

/// Encode `command` as a bridge-routed name carrying its `origin` server and
/// `host_uri`: `"kakehashi\u{1f}{origin}\u{1f}{host_uri}\u{1f}{command}"`.
///
/// Fails closed (`None`) when `origin` or `host_uri` contains the separator: the
/// `splitn(3)` decode relies on those two segments being separator-free. A
/// `host_uri` is a URL (control chars are percent-encoded), but `origin` is an
/// unvalidated TOML config key — a stray separator there would make
/// [`decode_command`] mis-split and route execution to the WRONG server/root.
/// Better to drop the (unroutable) command at the call site than to mint an
/// ambiguous name. `command` may contain the separator freely (it's the
/// remainder).
pub(crate) fn encode_command(origin: &str, host_uri: &str, command: &str) -> Option<String> {
    if origin.contains(SEP) || host_uri.contains(SEP) {
        // Not a transient: the origin is a TOML config key, so an affected
        // config drops this server's commands on EVERY request. One warn here
        // covers all four call sites (initial bare/embedded command, virt and
        // host resolve), which fail closed on the `None`.
        log::warn!(
            target: "kakehashi::bridge",
            "cannot mint a routing name for command '{command}': the server name \
             ('{origin}') or host URI contains the reserved separator; the command \
             is dropped — rename the server in languageServers to fix this"
        );
        return None;
    }
    Some(format!(
        "{COMMAND_PREFIX}{SEP}{origin}{SEP}{host_uri}{SEP}{command}"
    ))
}

/// Decode a bridge-routed command name, or `None` if `name` was not minted by
/// [`encode_command`]. Total: never panics, so a malformed or foreign command
/// name fails soft at the call site.
pub(crate) fn decode_command(name: &str) -> Option<CommandRoute<'_>> {
    let rest = name.strip_prefix(COMMAND_PREFIX)?.strip_prefix(SEP)?;
    // Split into exactly three fields: origin, host_uri, and the command as the
    // REMAINDER. Only the origin/host_uri boundaries need to be separator-free
    // (they are — a config key and a URL); a separator inside the command id
    // rejoins into the remainder unharmed.
    let mut parts = rest.splitn(3, SEP);
    let origin = parts.next()?;
    let host_uri = parts.next()?;
    let command = parts.next()?;
    Some(CommandRoute {
        origin,
        host_uri,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_three_fields() {
        let encoded =
            encode_command("ruff", "file:///w/a.md", "ruff.applyOrganizeImports").expect("valid");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.origin, "ruff");
        assert_eq!(route.host_uri, "file:///w/a.md");
        assert_eq!(route.command, "ruff.applyOrganizeImports");
    }

    #[test]
    fn round_trips_a_server_name_with_dots_and_a_command_with_colons() {
        // Config server names are arbitrary TOML keys and command ids often
        // contain colons/dots; the separator must not collide with any of them.
        let encoded = encode_command("py.ruff-lsp", "file:///x", "cmd:with:colons").expect("valid");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.origin, "py.ruff-lsp");
        assert_eq!(route.host_uri, "file:///x");
        assert_eq!(route.command, "cmd:with:colons");
    }

    #[test]
    fn a_separator_inside_the_command_segment_round_trips() {
        // LSP command ids are arbitrary strings; a (theoretical) separator in
        // the command is harmless because it's the splitn(3) remainder — only
        // the origin/host_uri boundaries must be separator-free.
        let encoded = encode_command("srv", "file:///x", "weird\u{1f}cmd").expect("valid");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.origin, "srv");
        assert_eq!(route.host_uri, "file:///x");
        assert_eq!(route.command, "weird\u{1f}cmd");
    }

    #[test]
    fn encode_fails_closed_when_origin_or_host_uri_contains_the_separator() {
        // A separator in `origin` or `host_uri` would make `decode_command`
        // mis-split and route to the wrong server/root, so refuse to encode.
        assert!(encode_command("sr\u{1f}v", "file:///x", "cmd").is_none());
        assert!(encode_command("srv", "file:///x\u{1f}y", "cmd").is_none());
        // The command segment may contain it freely (it's the remainder).
        assert!(encode_command("srv", "file:///x", "c\u{1f}md").is_some());
    }

    #[test]
    fn decode_rejects_a_foreign_or_truncated_command_name() {
        // A command the bridge did not mint (e.g. a client-side command) must
        // not be mistaken for a routed one, nor may a truncated one panic.
        assert!(decode_command("rust-analyzer.runSingle").is_none());
        assert!(decode_command("kakehashi").is_none());
        assert!(decode_command("kakehashi\u{1f}only-server").is_none());
        assert!(decode_command("kakehashi\u{1f}server\u{1f}uri-no-command").is_none());
        assert!(decode_command("").is_none());
    }
}
