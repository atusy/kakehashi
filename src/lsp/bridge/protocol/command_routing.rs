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
//! The separator is the ASCII Unit Separator (`0x1f`), which cannot appear in a
//! TOML config server name, a `file://` URI, or a real command identifier, so
//! the split is unambiguous. Decoding is total: any name that isn't a
//! well-formed bridge command yields `None`, and the handler fails that command
//! soft rather than panicking.
//!
//! NOTE: this reaches only commands surfaced through a bridged action. Commands
//! the client fires WITHOUT an action context (from a palette, keyed off the
//! advertised `executeCommandProvider.commands` list) need real advertised
//! names + dynamic registration and are a deliberate follow-up (see #568 PR 6).

/// Marker that identifies a bridge-minted command name.
const COMMAND_PREFIX: &str = "kakehashi";
/// ASCII Unit Separator — cannot occur in a config server name, a URI, or a
/// command id.
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
pub(crate) fn encode_command(origin: &str, host_uri: &str, command: &str) -> String {
    format!("{COMMAND_PREFIX}{SEP}{origin}{SEP}{host_uri}{SEP}{command}")
}

/// Decode a bridge-routed command name, or `None` if `name` was not minted by
/// [`encode_command`]. Total: never panics, so a malformed or foreign command
/// name fails soft at the call site.
pub(crate) fn decode_command(name: &str) -> Option<CommandRoute<'_>> {
    let rest = name.strip_prefix(COMMAND_PREFIX)?.strip_prefix(SEP)?;
    // Exactly three fields; the command id is the remainder (it may contain
    // anything except the separator, which the encoder never emits inside it).
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
        let encoded = encode_command("ruff", "file:///w/a.md", "ruff.applyOrganizeImports");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.origin, "ruff");
        assert_eq!(route.host_uri, "file:///w/a.md");
        assert_eq!(route.command, "ruff.applyOrganizeImports");
    }

    #[test]
    fn round_trips_a_server_name_with_dots_and_a_command_with_colons() {
        // Config server names are arbitrary TOML keys and command ids often
        // contain colons/dots; the separator must not collide with any of them.
        let encoded = encode_command("py.ruff-lsp", "file:///x", "cmd:with:colons");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.origin, "py.ruff-lsp");
        assert_eq!(route.host_uri, "file:///x");
        assert_eq!(route.command, "cmd:with:colons");
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
