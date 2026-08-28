//! Inline value method for Kakehashi.
//!
//! The preferred layer wins. Host servers receive the original request
//! verbatim; virtual servers receive both request ranges in virtual coordinates
//! and their result ranges are translated back to the host document.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    InlineValue, InlineValueContext, InlineValueParams, NumberOrString, Range, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::bridge::RegionOffset;
use crate::lsp::lsp_impl::bridge_context::{normalize_range_endpoints, parse_host_verbatim};
use crate::text::PositionMapper;

const METHOD: &str = "textDocument/inlineValue";

impl Kakehashi {
    pub(crate) async fn inline_value_impl(
        &self,
        params: InlineValueParams,
    ) -> Result<Option<Vec<InlineValue>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let context = params.context;
        let progress_token = params.work_done_progress_params.work_done_token;

        let virt = self.inline_value_virt_layer(&lsp_uri, range, context, progress_token);
        let host = self.inline_value_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |values: &Vec<InlineValue>| !values.is_empty(),
        )
        .await
    }

    async fn inline_value_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<InlineValue>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let incarnation = ctx.incarnation;
        let content_version = ctx.content_version;
        if !self.inline_value_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let fan_in = dispatch_host_preferred(
            &ctx,
            self.bridge.pool_arc(),
            move |t: HostFanOutTask| {
                let params = raw_params.clone();
                async move {
                    let raw = t
                        .pool
                        .send_host_raw_request_for_incarnation(
                            &t.server_name,
                            &t.server_config,
                            &HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                            },
                            METHOD,
                            params,
                            t.upstream_id,
                            incarnation,
                        )
                        .await?;
                    Ok(raw
                        .and_then(|raw| parse_host_verbatim::<Vec<InlineValue>>(raw.value))
                        .filter(|values| !values.is_empty()))
                }
            },
            |opt| matches!(opt, Some(values) if !values.is_empty()),
            cancel_rx,
        )
        .await;
        let values = self.host_layer_result(fan_in, METHOD, |won| won).await?;
        if !self.inline_value_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        Ok(values)
    }

    async fn inline_value_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        mut context: InlineValueContext,
        progress_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<InlineValue>>> {
        // `range` is the editor's visible span and commonly starts in host
        // prose. The debugger stop identifies the one language region that
        // owns this request; resolve by that location, then intersect the
        // visible span with the resolved region.
        let Some(mut ctx) = self
            .resolve_bridge_contexts_for_range(lsp_uri, context.stopped_location, METHOD)
            .await
        else {
            return Ok(None);
        };
        let offset = RegionOffset::with_per_line_offsets(
            ctx.document.resolved.region.line_range.start,
            ctx.document.resolved.line_column_offsets.clone(),
        );
        let region_start = tower_lsp_server::ls_types::Position {
            line: offset.line(),
            character: offset.column_for_line(0),
        };
        let Some(region_end) = ctx.document.region_end else {
            return Ok(None);
        };
        let Some(range) = self.normalize_inline_value_range_if_current(
            lsp_uri,
            ctx.incarnation,
            ctx.content_version,
            range,
        ) else {
            return Ok(None);
        };
        let Some(range) = clamp_visible_range_to_region(range, region_start, region_end) else {
            return Ok(None);
        };
        context.stopped_location = ctx.range;
        ctx.document.client_progress_token = progress_token;
        let incarnation = ctx.incarnation;
        let content_version = ctx.content_version;
        #[cfg(feature = "e2e")]
        wait_for_inline_value_admission_release().await;
        if !self.inline_value_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());
        let result = dispatch_preferred(
            &ctx.document,
            self.bridge.pool_arc(),
            |t| {
                let context = context.clone();
                async move {
                    t.pool
                        .send_inline_value_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            range,
                            context,
                            t.region_end(),
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                            t.client_progress_token,
                            incarnation,
                        )
                        .await
                }
            },
            |opt| matches!(opt, Some(values) if !values.is_empty()),
            cancel_rx,
        )
        .await;

        let values = result
            .handle(&self.notifier(), "inline value", None, Ok)
            .await?;
        if !self.inline_value_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        Ok(values)
    }

    fn inline_value_snapshot_is_current(
        &self,
        lsp_uri: &Uri,
        incarnation: u64,
        content_version: u64,
    ) -> bool {
        super::super::uri_to_url(lsp_uri)
            .ok()
            .and_then(|uri| {
                self.documents.get(&uri).map(|document| {
                    document.incarnation() == incarnation
                        && document.content_version() == content_version
                })
            })
            .unwrap_or(false)
    }

    fn normalize_inline_value_range_if_current(
        &self,
        lsp_uri: &Uri,
        incarnation: u64,
        content_version: u64,
        range: Range,
    ) -> Option<Range> {
        let uri = super::super::uri_to_url(lsp_uri).ok()?;
        let document = self.documents.get(&uri)?;
        (document.incarnation() == incarnation && document.content_version() == content_version)
            .then(|| normalize_range_endpoints(&PositionMapper::new(document.text()), range))?
    }
}

fn clamp_visible_range_to_region(
    range: Range,
    region_start: tower_lsp_server::ls_types::Position,
    region_end: tower_lsp_server::ls_types::Position,
) -> Option<Range> {
    if range.start > range.end {
        return None;
    }
    let intersects = if range.start == range.end {
        region_start <= range.start && range.start < region_end
    } else {
        range.start < region_end && region_start < range.end
    };
    intersects.then(|| Range {
        start: range.start.max(region_start),
        end: range.end.min(region_end),
    })
}

#[cfg(feature = "e2e")]
async fn wait_for_inline_value_admission_release() {
    let Ok(dir) = std::env::var("KAKEHASHI_E2E_INLINE_VALUE_BARRIER_DIR") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err()
        || std::fs::write(dir.join("captured"), b"captured").is_err()
    {
        return;
    }
    let release = dir.join("release");
    for _ in 0..300 {
        if release.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::Position;

    #[tokio::test]
    async fn snapshot_freshness_rejects_edits_and_reopens() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = url::Url::parse("file:///inline-value-freshness.md").unwrap();
        let lsp_uri = super::super::super::url_to_uri(&uri).unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "old".to_string(),
            Some("markdown".to_string()),
            None,
        );
        let content_version = server.documents.get(&uri).unwrap().content_version();

        assert!(server.inline_value_snapshot_is_current(&lsp_uri, incarnation, content_version));
        server
            .documents
            .apply_edit_clearing_tree(&uri, "edited".to_string(), &[]);
        assert!(!server.inline_value_snapshot_is_current(&lsp_uri, incarnation, content_version));

        server.documents.remove(&uri);
        server.documents.insert(
            uri.clone(),
            "reopened".to_string(),
            Some("markdown".to_string()),
            None,
        );
        assert!(!server.inline_value_snapshot_is_current(&lsp_uri, incarnation, content_version));
    }

    #[test]
    fn visible_range_is_clamped_to_the_stopped_region() {
        assert_eq!(
            clamp_visible_range_to_region(
                Range::new(Position::new(0, 0), Position::new(3, 6)),
                Position::new(3, 2),
                Position::new(3, 6),
            ),
            Some(Range::new(Position::new(3, 2), Position::new(3, 6)))
        );
    }

    #[test]
    fn visible_range_without_region_overlap_is_rejected() {
        assert_eq!(
            clamp_visible_range_to_region(
                Range::new(Position::new(0, 0), Position::new(1, 0)),
                Position::new(3, 2),
                Position::new(3, 6),
            ),
            None
        );
    }
}
