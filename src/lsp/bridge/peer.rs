//! Downstream-facing `kakehashi/bridge/peer*` request handlers.
//!
//! A downstream language server uses these methods to discover and request
//! another downstream connection through kakehashi. Unlike the editor-facing
//! `kakehashi/bridge/client*` family, these handlers are registered only on a
//! downstream connection's reader.

use super::pool::ConnectionKey;

#[cfg(test)]
fn peer_keys<'a>(
    keys: impl IntoIterator<Item = &'a ConnectionKey>,
    origin: &ConnectionKey,
) -> Vec<ConnectionKey> {
    keys.into_iter()
        .filter(|key| *key != origin)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_list_excludes_only_the_origin_connection() {
        let origin = ConnectionKey::new("tsudoi", Some("file:///repo/a".to_string()));
        let same_server_other_root =
            ConnectionKey::new("tsudoi", Some("file:///repo/b".to_string()));
        let formatter = ConnectionKey::new("denols", Some("file:///repo/a".to_string()));
        let keys = [
            origin.clone(),
            same_server_other_root.clone(),
            formatter.clone(),
        ];

        assert_eq!(
            peer_keys(keys.iter(), &origin),
            vec![same_server_other_root, formatter]
        );
    }
}
