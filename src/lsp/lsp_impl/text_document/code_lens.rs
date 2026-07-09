//! Code lens methods for Kakehashi: the whole-document fan-out and the
//! `codeLens/resolve` round-trip (#355).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CodeLens, CodeLensParams};
use ulid::Ulid;
use url::Url;

use super::super::Kakehashi;
use crate::lsp::bridge::{CodeLensEnvelope, extract_code_lens_envelope};
use crate::text::PositionMapper;

impl Kakehashi {
    pub(crate) async fn code_lens_impl(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token;
        self.whole_document_fan_out(
            &params.text_document.uri,
            "textDocument/codeLens",
            raw_params,
            work_done_token,
            |t| async move {
                t.pool
                    .send_code_lens_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                        t.client_progress_token,
                    )
                    .await
            },
        )
        .await
    }

    /// `codeLens/resolve`: route the lens back to the downstream server that
    /// produced it, identified by the envelope in `lens.data` (#355).
    ///
    /// Fails soft at every step: a lens without an envelope (host-layer or
    /// foreign) passes through unchanged, and a stale region returns the lens
    /// unresolved with its envelope intact — clients re-request lenses on
    /// change, so the staleness window is short.
    pub(crate) async fn code_lens_resolve_impl(&self, lens: CodeLens) -> Result<CodeLens> {
        let Some(envelope) = extract_code_lens_envelope(&lens) else {
            return Ok(lens);
        };

        // Fail-soft staleness gate: resolving against a moved or invalidated
        // region would translate coordinates with a stale offset and bind the
        // lens to content the user has since edited.
        if !self.code_lens_region_is_fresh(&envelope) {
            log::debug!(
                target: "kakehashi::bridge",
                "codeLens/resolve: region {} is stale; returning lens unresolved",
                envelope.region_id
            );
            return Ok(lens);
        }

        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();
        let pool = self.bridge.pool_arc();
        Ok(pool
            .dispatch_code_lens_resolve(lens, &settings, upstream_id)
            .await)
    }

    /// Whether the envelope's injection region still exists at the position it
    /// had when the lens was minted.
    ///
    /// The node tracker's edit-adjusted lookup answers "does the region still
    /// exist" (a `None` covers invalidation, close, and unknown ULIDs alike);
    /// comparing the region's CURRENT start position against the envelope's
    /// offset snapshot additionally catches regions that survived an edit but
    /// moved — the lens coordinates (and the snapshot offset) are stale then,
    /// so resolution must fail soft rather than translate wrongly.
    ///
    /// Known limitation: for injections whose queries apply `#offset!` (today
    /// only YAML/TOML frontmatter in the bundled markdown queries), the
    /// envelope offset is `#offset!`-adjusted while the tracker stores the raw
    /// content-node position, so this comparison never matches and resolve
    /// always fails soft for those regions. That errs in the safe direction
    /// (lenses stay unresolved — still strictly better than the pre-#355
    /// behavior of dropping them) and frontmatter code lenses have no known
    /// real-world producer; revisit if one appears.
    fn code_lens_region_is_fresh(&self, envelope: &CodeLensEnvelope) -> bool {
        let Ok(uri) = Url::parse(&envelope.host_uri) else {
            return false;
        };
        let Ok(ulid) = envelope.region_id.parse::<Ulid>() else {
            return false;
        };
        let Some((start_byte, _end, _kind)) =
            self.bridge.node_tracker().lookup_position(&uri, &ulid)
        else {
            return false;
        };
        let Some(doc) = self.documents.get(&uri) else {
            return false;
        };
        let mapper = PositionMapper::new(doc.text());
        let Some(position) = mapper.byte_to_position(start_byte) else {
            return false;
        };
        position.line == envelope.offset.line && position.character == envelope.offset.column
    }
}
