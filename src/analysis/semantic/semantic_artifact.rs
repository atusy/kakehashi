use crate::analysis::SemanticSnapshotIdentity;
use std::sync::Arc;
use tower_lsp_server::ls_types::{SemanticTokens, SemanticTokensResult};
use url::Url;

/// All request-independent inputs that identify one semantic-token artifact.
///
/// The artifact remains scoped to one immutable parse snapshot. Response-local
/// state such as an LSP `resultId` is deliberately not part of this identity.
#[derive(Clone, Debug, Eq, PartialEq)]
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
}

/// Complete immutable semantic output for one [`SemanticArtifactIdentity`].
///
/// Construction accepts only a complete full result. The data stays private
/// until a request materializes it and supplies its own LSP `resultId`.
pub(crate) struct SemanticArtifact {
    identity: Arc<SemanticArtifactIdentity>,
    tokens: SemanticTokens,
}

impl SemanticArtifact {
    pub(crate) fn from_full_result(
        identity: Arc<SemanticArtifactIdentity>,
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
        expected_identity: &SemanticArtifactIdentity,
        result_id: Option<String>,
    ) -> Option<SemanticTokens> {
        if self.identity.as_ref() != expected_identity {
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
    use std::sync::Arc;
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
            Arc::new(identity()),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: Some("compute-local".into()),
                data: vec![token],
            }),
        )
        .expect("complete result");

        let materialized = artifact
            .materialize_full(&identity(), Some("request-42".into()))
            .expect("matching identity");
        assert_eq!(materialized.result_id.as_deref(), Some("request-42"));
        assert_eq!(materialized.data, vec![token]);
    }

    #[test]
    fn mismatched_identity_cannot_materialize_artifact() {
        let artifact = SemanticArtifact::from_full_result(
            Arc::new(identity()),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            }),
        )
        .expect("complete result");
        let mut different = identity();
        different.snapshot.generation += 1;

        assert!(artifact.materialize_full(&different, None).is_none());
    }

    #[test]
    fn partial_result_cannot_become_visible_artifact() {
        let artifact = SemanticArtifact::from_full_result(
            Arc::new(identity()),
            SemanticTokensResult::Partial(SemanticTokensPartialResult { data: vec![] }),
        );

        assert!(artifact.is_none());
    }
}
