//! Command-name routing for bridged `workspace/executeCommand` (#568 PR 6,
//! reshaped by execute-command-routing-token).
//!
//! `workspace/executeCommand` carries only `command` + `arguments` — no `data`
//! envelope, so the origin cannot ride along the way `codeAction/resolve` routes
//! (that path DOES get `data`). The command NAME is the only channel, so when
//! the bridge surfaces a downstream command it rewrites the name to encode the
//! CONNECTION the command must run on. The `arguments` stay verbatim — they are
//! the downstream's own coordinate system (checklist §10). Rewriting a name the
//! bridge itself minted does not violate that.
//!
//! The token names a connection, not a document. A downstream command needs the
//! process that holds its state, and that process is identified by
//! `(server, root)` — the pool's own [`ConnectionKey`]. Encoding the key rather
//! than a host document keeps the name stable across edits and makes the set of
//! routable names finite (one per `(server, root, command)`), which a
//! per-document name could never be.
//!
//! ```text
//! kakehashi|{root_tag}|{server}|{root}|{command}
//! ```
//!
//! `root_tag` is the rooting mode; `root` is the marker root URI, empty for the
//! two root-less modes; `command` is the downstream's own id, taken as the
//! `splitn(4)` remainder so it needs no constraint at all.
//!
//! Encoding is stateless (no cross-request registry to populate or evict) and
//! collision-safe: two servers advertising the same command name — realistic
//! under concatenated aggregation, the codeAction default — encode to distinct
//! names, as do two roots of one server.
//!
//! The split boundaries are separator-free **by construction**: `server` and
//! `root` are escaped on the way in and unescaped on the way out, so a config
//! key containing the separator round-trips instead of being refused. Only `%`
//! and the separator itself are escaped — a root that already contains percent
//! escapes gets them doubled (`%20` → `%2520`), which is correct but not pretty.
//!
//! Decoding is total: any name that isn't a well-formed bridge command yields
//! `None` — the handler then tries the raw palette-command registry, and only a
//! command matching neither route fails soft (never a panic).
//!
//! TRUST BOUNDARY. A decoded name is EDITOR input: kakehashi mints it, hands it
//! to the client, and the client hands it back. It is not authenticated, so a
//! client can decode to any `(server, root)` it likes — including a configured
//! server at a root no open document sits under, which the previous encoding
//! could not express (its root came from a marker walk over a live document).
//!
//! That is deliberate, not an oversight. `server` still has to name an EXACT
//! `languageServers` entry — `is_server_spawnable` does a plain `get(name)`, so
//! a `_` wildcard does NOT make an invented server spawnable — and the client
//! that could forge a name is the same one that supplies `languageServers`
//! (with its `cmd`) via `initializationOptions`. It already decides what runs
//! and where; a forged root grants nothing further. Anything that changes that
//! premise — a token reaching kakehashi from a source other than the editor it
//! was minted for — would need the token authenticated or checked against a
//! registry of keys the pool actually spawned.
//!
//! NOTE: this encoding covers commands surfaced through a bridged action.
//! Commands the client fires WITHOUT an action context (from a palette) route by
//! their RAW advertised names instead, via the pool's command-origin registry +
//! dynamic `workspace/executeCommand` registration (see
//! `dispatch_palette_command` in `src/lsp/bridge/workspace/execute_command.rs`).

use crate::lsp::bridge::pool::ConnectionKey;

/// Marker that identifies a bridge-minted command name.
const COMMAND_PREFIX: &str = "kakehashi";
/// Field separator. Printable, unlike the 0x1f control character it replaced:
/// these names reach logs, editor UIs, and JSON (where RFC 8259 forces a
/// `\u001f` escape for the old separator), so a legible byte is strictly easier
/// to transport and debug. Unambiguity comes from ESCAPING the segments, not
/// from the character being exotic.
const SEP: char = '|';
/// Rooting-mode tags. A tag rather than an empty-root sentinel because the two
/// root-less modes are genuinely different connections: routing a shared
/// instance to the client-root fallback would run the command in the wrong
/// workspace.
const TAG_MARKER: &str = "m";
const TAG_CLIENT_FALLBACK: &str = "c";
const TAG_SHARED: &str = "s";

/// A decoded bridge command name: the connection to run on, and the
/// downstream's own command id (forwarded verbatim).
pub(crate) struct CommandRoute<'a> {
    pub(crate) key: ConnectionKey,
    pub(crate) command: &'a str,
}

/// Escape `%` and the separator so a segment cannot break the split.
///
/// `%` must be escaped FIRST, otherwise unescaping could not tell a literal
/// `%7C` in the input from an escaped separator.
fn escape_segment(segment: &str) -> String {
    segment.replace('%', "%25").replace(SEP, "%7C")
}

/// Inverse of [`escape_segment`], or `None` for a segment `escape_segment` could
/// never have produced. A single left-to-right pass, so the `%25` produced for a
/// literal `%` is not re-read as the start of another escape.
///
/// Fails CLOSED on a stray `%`: `escape_segment` turns every `%` into `%25`, so
/// any other `%` sequence means the name was not bridge-minted. Decoding it
/// leniently would route a forged name to a real connection; rejecting sends it
/// to the palette registry instead, which is where a non-bridge name belongs.
fn unescape_segment(segment: &str) -> Option<String> {
    let mut out = String::with_capacity(segment.len());
    let mut rest = segment;
    while let Some(index) = rest.find('%') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        if let Some(stripped) = tail.strip_prefix("%25") {
            out.push('%');
            rest = stripped;
        } else {
            // `escape_segment` emits exactly two escapes, so this `?` IS the
            // fail-closed branch: anything that is not `%7C` here was not
            // bridge-minted.
            rest = tail.strip_prefix("%7C")?;
            out.push(SEP);
        }
    }
    out.push_str(rest);
    Some(out)
}

/// Encode `command` as a bridge-routed name carrying the connection it must run
/// on.
///
/// Total: unlike the host-URI encoding this replaced, there is no input that
/// fails to encode. That scheme had to refuse (and the caller had to drop the
/// command) when a config key contained the separator, because the split relied
/// on the separator's absence; escaping makes the invariant structural.
pub(crate) fn encode_command(key: &ConnectionKey, command: &str) -> String {
    let (tag, root) = if let Some(root) = key.marker_root() {
        (TAG_MARKER, root)
    } else if key.is_shared() {
        (TAG_SHARED, "")
    } else {
        (TAG_CLIENT_FALLBACK, "")
    };
    format!(
        "{COMMAND_PREFIX}{SEP}{tag}{SEP}{}{SEP}{}{SEP}{command}",
        escape_segment(key.server()),
        escape_segment(root)
    )
}

/// Decode a bridge-routed command name, or `None` if `name` was not minted by
/// [`encode_command`]. Total: never panics, so a malformed or foreign command
/// name fails soft at the call site.
pub(crate) fn decode_command(name: &str) -> Option<CommandRoute<'_>> {
    let rest = name.strip_prefix(COMMAND_PREFIX)?.strip_prefix(SEP)?;
    // Four fields: tag, server, root, and the command as the REMAINDER. Only the
    // first three boundaries must be separator-free, which escaping guarantees;
    // a separator inside the command id rejoins into the remainder unharmed.
    let mut parts = rest.splitn(4, SEP);
    let tag = parts.next()?;
    let server = unescape_segment(parts.next()?)?;
    let root = unescape_segment(parts.next()?)?;
    let command = parts.next()?;
    // Every arm rejects a root the corresponding `encode_command` branch could
    // not have produced. A marker tag with no root would rebuild as the client
    // fallback, and a root-less tag WITH a root is not a shape we mint at all —
    // accepting either would let a forged name pick a rooting mode by hand.
    let key = match tag {
        TAG_MARKER if !root.is_empty() => ConnectionKey::new(server, Some(root)),
        TAG_CLIENT_FALLBACK if root.is_empty() => ConnectionKey::new(server, None),
        TAG_SHARED if root.is_empty() => ConnectionKey::shared(server),
        _ => return None,
    };
    Some(CommandRoute { key, command })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_key(server: &str, root: &str) -> ConnectionKey {
        ConnectionKey::new(server, Some(root.to_string()))
    }

    #[test]
    fn round_trips_a_marker_rooted_connection() {
        let key = marker_key("ruff", "file:///w");
        let encoded = encode_command(&key, "ruff.applyOrganizeImports");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.key, key);
        assert_eq!(route.command, "ruff.applyOrganizeImports");
    }

    #[test]
    fn the_three_rooting_modes_round_trip_distinctly() {
        // Each mode is a different downstream process. Collapsing any pair would
        // run the command in the wrong workspace.
        for key in [
            marker_key("tsgo", "file:///repo/a"),
            ConnectionKey::new("tsgo", None),
            ConnectionKey::shared("tsgo"),
        ] {
            let encoded = encode_command(&key, "cmd");
            let route = decode_command(&encoded).expect("well-formed");
            assert_eq!(route.key, key, "rooting mode must survive the round trip");
        }
    }

    #[test]
    fn two_roots_of_one_server_encode_distinctly() {
        // The whole point of keying on the connection: the same command id under
        // two roots must reach two different processes.
        let a = encode_command(&marker_key("tsgo", "file:///repo/a"), "tsgo.restart");
        let b = encode_command(&marker_key("tsgo", "file:///repo/b"), "tsgo.restart");
        assert_ne!(a, b);
    }

    #[test]
    fn a_separator_in_the_server_name_round_trips_instead_of_failing() {
        // The host-URI encoding had to DROP this command (it could not mint an
        // unambiguous name). Escaping makes it routable.
        let key = ConnectionKey::new("we|ird", Some("file:///w|x".to_string()));
        let encoded = encode_command(&key, "cmd");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.key, key);
        assert_eq!(route.command, "cmd");
    }

    #[test]
    fn a_percent_in_a_segment_round_trips() {
        // Percent-escaped roots are ordinary (a URL with an encoded space), and a
        // literal `%25` in the input must not be confused with our own escape.
        let key = ConnectionKey::new("srv%25", Some("file:///a%20b".to_string()));
        let encoded = encode_command(&key, "cmd");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.key, key);
    }

    #[test]
    fn a_separator_inside_the_command_segment_round_trips() {
        // LSP command ids are arbitrary strings; a separator in the command is
        // harmless because it is the splitn(4) remainder.
        let key = marker_key("srv", "file:///x");
        let encoded = encode_command(&key, "weird|cmd");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.command, "weird|cmd");
    }

    #[test]
    fn round_trips_a_server_name_with_dots_and_a_command_with_colons() {
        // Config server names are arbitrary TOML keys and command ids often
        // contain colons/dots; the separator must not collide with any of them.
        let key = marker_key("py.ruff-lsp", "file:///x");
        let encoded = encode_command(&key, "cmd:with:colons");
        let route = decode_command(&encoded).expect("well-formed");
        assert_eq!(route.key, key);
        assert_eq!(route.command, "cmd:with:colons");
    }

    #[test]
    fn decode_rejects_a_foreign_or_truncated_command_name() {
        // A command the bridge did not mint (e.g. a client-side command) must not
        // be mistaken for a routed one, nor may a truncated one panic.
        assert!(decode_command("rust-analyzer.runSingle").is_none());
        assert!(decode_command("kakehashi").is_none());
        assert!(decode_command("kakehashi|m").is_none());
        assert!(decode_command("kakehashi|m|server").is_none());
        assert!(decode_command("kakehashi|m|server|file:///x").is_none());
        assert!(decode_command("").is_none());
    }

    #[test]
    fn decode_rejects_an_unknown_rooting_tag() {
        // A tag from a future (or corrupt) encoding must fail soft rather than
        // default to a rooting mode and reach the wrong process.
        assert!(decode_command("kakehashi|z|server|file:///x|cmd").is_none());
    }

    #[test]
    fn decode_rejects_a_stray_percent_escape() {
        // `escape_segment` emits `%` only as `%25`/`%7C`, so any other sequence
        // means the name was not bridge-minted. Decoding it leniently would let a
        // forged name route to a real connection.
        assert!(decode_command("kakehashi|c|sr%v||cmd").is_none());
        assert!(decode_command("kakehashi|c|srv%||cmd").is_none());
        assert!(decode_command("kakehashi|m|srv|file:///a%20b|cmd").is_none());
        // The canonical form of that last root DOES decode.
        let key = ConnectionKey::new("srv", Some("file:///a%20b".to_string()));
        let encoded = encode_command(&key, "cmd");
        assert_eq!(decode_command(&encoded).expect("canonical").key, key);
    }

    #[test]
    fn decode_rejects_a_root_less_tag_that_carries_a_root() {
        // `encode_command` writes an empty root for both root-less modes, so a
        // populated one is a shape we never mint — accepting it would let a
        // forged name pick a rooting mode by hand.
        assert!(decode_command("kakehashi|c|srv|file:///w|cmd").is_none());
        assert!(decode_command("kakehashi|s|srv|file:///w|cmd").is_none());
    }

    #[test]
    fn decode_rejects_a_marker_tag_with_an_empty_root() {
        // `ConnectionKey::new(server, Some(""))` is a marker key rooted at the
        // empty string, which matches no live connection; and treating it as the
        // fallback would run the command in the wrong workspace.
        assert!(decode_command("kakehashi|m|server||cmd").is_none());
    }
}
