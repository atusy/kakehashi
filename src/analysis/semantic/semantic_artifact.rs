use crate::analysis::SemanticSnapshotIdentity;
use tower_lsp_server::ls_types::{SemanticTokens, SemanticTokensResult};
use url::Url;

/// All request-independent inputs that identify one semantic-token artifact.
///
/// The artifact remains scoped to one immutable parse snapshot. Response-local
/// state such as an LSP `resultId` is deliberately not part of this identity.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SemanticArtifactIdentity {
    uri: Url,
    language: String,
    snapshot: SemanticSnapshotIdentity,
    supports_multiline: bool,
}

impl SemanticArtifactIdentity {
    pub(crate) fn new(
        uri: Url,
        language: String,
        snapshot: SemanticSnapshotIdentity,
        supports_multiline: bool,
    ) -> Self {
        Self {
            uri,
            language,
            snapshot,
            supports_multiline,
        }
    }

    pub(crate) fn expected<'a>(
        uri: &'a Url,
        language: &'a str,
        snapshot: SemanticSnapshotIdentity,
        supports_multiline: bool,
    ) -> SemanticArtifactIdentityRef<'a> {
        SemanticArtifactIdentityRef {
            uri,
            language,
            snapshot,
            supports_multiline,
        }
    }

    #[cfg(test)]
    fn as_ref(&self) -> SemanticArtifactIdentityRef<'_> {
        Self::expected(
            &self.uri,
            &self.language,
            self.snapshot,
            self.supports_multiline,
        )
    }

    fn matches(&self, expected: SemanticArtifactIdentityRef<'_>) -> bool {
        self.uri == *expected.uri
            && self.language == expected.language
            && self.snapshot == expected.snapshot
            && self.supports_multiline == expected.supports_multiline
    }
}

/// Allocation-free request view used to validate a reusable artifact.
///
/// Stage 2 constructs the artifact locally, so this comparison is expected to
/// succeed. Keeping the authoritative request inputs separate makes the same
/// check non-tautological when Stage 3 retrieves an artifact from a snapshot
/// slot, without cloning its URI or language for every lookup.
#[derive(Clone, Copy)]
pub(crate) struct SemanticArtifactIdentityRef<'a> {
    uri: &'a Url,
    language: &'a str,
    snapshot: SemanticSnapshotIdentity,
    supports_multiline: bool,
}

impl SemanticArtifactIdentityRef<'_> {
    pub(crate) fn to_owned(self) -> SemanticArtifactIdentity {
        SemanticArtifactIdentity::new(
            self.uri.clone(),
            self.language.to_owned(),
            self.snapshot,
            self.supports_multiline,
        )
    }
}

/// Complete immutable semantic output for one [`SemanticArtifactIdentity`].
///
/// Construction accepts only a complete full result. The data stays private
/// until a request materializes it and supplies its own LSP `resultId`.
pub(crate) struct SemanticArtifact {
    identity: SemanticArtifactIdentity,
    tokens: SemanticTokens,
}

impl SemanticArtifact {
    pub(crate) fn from_full_result(
        identity: SemanticArtifactIdentity,
        result: SemanticTokensResult,
    ) -> Option<Self> {
        let SemanticTokensResult::Tokens(mut tokens) = result else {
            return None;
        };
        tokens.result_id = None;
        Some(Self { identity, tokens })
    }

    pub(crate) fn materialize_full(
        mut self,
        expected_identity: SemanticArtifactIdentityRef<'_>,
        result_id: Option<String>,
    ) -> Option<SemanticTokens> {
        if !self.identity.matches(expected_identity) {
            return None;
        }
        self.tokens.result_id = result_id;
        Some(self.tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::{SemanticArtifact, SemanticArtifactIdentity};
    use crate::analysis::SemanticSnapshotIdentity;
    use tower_lsp_server::ls_types::{
        SemanticToken, SemanticTokens, SemanticTokensPartialResult, SemanticTokensResult,
    };
    use url::Url;

    fn identity() -> SemanticArtifactIdentity {
        SemanticArtifactIdentity::new(
            Url::parse("file:///workspace/main.rs").unwrap(),
            "rust".into(),
            SemanticSnapshotIdentity {
                parsed_version: 7,
                incarnation: 3,
                generation: 11,
            },
            true,
        )
    }

    #[test]
    fn artifact_identity_includes_every_output_input() {
        let identity = identity();

        assert_eq!(identity.uri.as_str(), "file:///workspace/main.rs");
        assert_eq!(identity.language, "rust");
        assert_eq!(identity.snapshot.parsed_version, 7);
        assert_eq!(identity.snapshot.incarnation, 3);
        assert_eq!(identity.snapshot.generation, 11);
        assert!(identity.supports_multiline);
    }

    #[test]
    fn artifact_owns_complete_tokens_without_request_result_id() {
        let token = SemanticToken {
            delta_line: 1,
            delta_start: 2,
            length: 3,
            token_type: 4,
            token_modifiers_bitset: 5,
        };
        let artifact = SemanticArtifact::from_full_result(
            identity(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: Some("compute-local".into()),
                data: vec![token],
            }),
        )
        .expect("complete result");

        let materialized = artifact
            .materialize_full(identity().as_ref(), Some("request-42".into()))
            .expect("matching identity");
        assert_eq!(materialized.result_id.as_deref(), Some("request-42"));
        assert_eq!(materialized.data, vec![token]);
    }

    #[test]
    fn every_identity_component_discriminates_materialization() {
        let base = identity();
        let other_uri = Url::parse("file:///workspace/other.rs").unwrap();
        let different_parsed_version = SemanticSnapshotIdentity {
            parsed_version: base.snapshot.parsed_version + 1,
            ..base.snapshot
        };
        let different_incarnation = SemanticSnapshotIdentity {
            incarnation: base.snapshot.incarnation + 1,
            ..base.snapshot
        };
        let different_generation = SemanticSnapshotIdentity {
            generation: base.snapshot.generation + 1,
            ..base.snapshot
        };
        let mismatches = [
            SemanticArtifactIdentity::expected(
                &other_uri,
                &base.language,
                base.snapshot,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                "python",
                base.snapshot,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                different_parsed_version,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                different_incarnation,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                different_generation,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                base.snapshot,
                !base.supports_multiline,
            ),
        ];

        for mismatch in mismatches {
            let artifact = SemanticArtifact::from_full_result(
                identity(),
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: vec![],
                }),
            )
            .expect("complete result");

            assert!(artifact.materialize_full(mismatch, None).is_none());
        }
    }

    #[test]
    fn partial_result_cannot_become_visible_artifact() {
        let artifact = SemanticArtifact::from_full_result(
            identity(),
            SemanticTokensResult::Partial(SemanticTokensPartialResult { data: vec![] }),
        );

        assert!(artifact.is_none());
    }
}
