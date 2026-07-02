//! Native lexical-resolution layer (lexical-name-resolution ADR).
//!
//! Serves definition / references / documentHighlight / rename from the
//! tree-sitter tree alone via the generic bindings engine, for layers whose
//! language has a `bindings.scm` — typically ones with no bridge server
//! configured. Feeds the `native` slot of the cross-layer walk; the miss
//! policy is silence, so an unresolved cursor contributes nothing and the
//! bridge/aggregation path owns the answer.
//!
//! v1 scope: the host layer only. Injected layers resolve through the bridge
//! until the native path learns injection coordinates (tracked follow-up).

use std::ops::Range;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DocumentHighlight, DocumentHighlightKind, Location, LocationLink, Position,
    PrepareRenameResponse, TextEdit, Uri, WorkspaceEdit,
};

use super::super::{Kakehashi, uri_to_url};
use crate::analysis::bindings::collect::collect;
use crate::analysis::bindings::model::BindingsModel;
use crate::text::PositionMapper;

/// Everything a native answer is computed from: the resolved model, the
/// cursor's byte offset, and the mapper for byte→position conversion.
pub(crate) struct NativeBindingsContext<'a> {
    pub(crate) model: &'a BindingsModel,
    pub(crate) byte: usize,
    pub(crate) mapper: &'a PositionMapper,
}

impl Kakehashi {
    /// Build the bindings model for the host layer under the cursor and
    /// answer with `f`. `Ok(None)` — the layer contributes nothing — when the
    /// language has no bindings query, the document is unavailable, or `f`
    /// itself answers `None` (miss policy: silence).
    pub(crate) async fn native_bindings_answer<R>(
        &self,
        lsp_uri: &Uri,
        position: Position,
        f: impl FnOnce(NativeBindingsContext<'_>) -> Option<R>,
    ) -> Result<Option<R>> {
        let Ok(uri) = uri_to_url(lsp_uri) else {
            return Ok(None);
        };
        let Some(language) = self.document_language(&uri) else {
            return Ok(None);
        };
        if !self.language.ensure_language_loaded(&language).success {
            return Ok(None);
        }
        let Some(query) = self.language.bindings_query(&language) else {
            return Ok(None);
        };

        // Wait for / trigger the off-ingress parse, then snapshot text and
        // tree without holding the store Ref across compute.
        self.ensure_document_parsed(&uri).await;
        let Some((text, tree)) = ({
            let doc = self.documents.get(&uri);
            doc.and_then(|doc| {
                let tree = doc.tree()?.clone();
                Some((doc.text().to_string(), tree))
            })
        }) else {
            return Ok(None);
        };

        let mapper = PositionMapper::new(&text);
        let Some(byte) = mapper.position_to_byte(position) else {
            return Ok(None);
        };

        let model = BindingsModel::build(collect(&text, tree.root_node(), &query));
        Ok(f(NativeBindingsContext {
            model: &model,
            byte,
            mapper: &mapper,
        }))
    }
}

fn to_lsp_range(
    mapper: &PositionMapper,
    range: &Range<usize>,
) -> Option<tower_lsp_server::ls_types::Range> {
    Some(tower_lsp_server::ls_types::Range {
        start: mapper.byte_to_position(range.start)?,
        end: mapper.byte_to_position(range.end)?,
    })
}

/// `textDocument/definition`: the definition site the resolution rules report
/// for the cursor's identifier.
pub(crate) fn native_definition(
    ctx: NativeBindingsContext<'_>,
    lsp_uri: &Uri,
) -> Option<Vec<LocationLink>> {
    let target = ctx.model.definition_range_at(ctx.byte)?;
    let range = to_lsp_range(ctx.mapper, &target)?;
    Some(vec![LocationLink {
        origin_selection_range: ctx
            .model
            .resolvable_identifier_at(ctx.byte)
            .and_then(|r| to_lsp_range(ctx.mapper, &r)),
        target_uri: lsp_uri.clone(),
        target_range: range,
        target_selection_range: range,
    }])
}

/// `textDocument/references`: every reference resolving to the cursor's
/// binding; definition sites included per `includeDeclaration`.
pub(crate) fn native_references(
    ctx: NativeBindingsContext<'_>,
    lsp_uri: &Uri,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let ranges = binding_ranges(&ctx, include_declaration)?;
    let locations: Vec<Location> = ranges
        .iter()
        .filter_map(|r| to_lsp_range(ctx.mapper, r))
        .map(|range| Location {
            uri: lsp_uri.clone(),
            range,
        })
        .collect();
    (!locations.is_empty()).then_some(locations)
}

/// `textDocument/documentHighlight`: the references set with definition
/// sites always included (the request has no `includeDeclaration`), kind
/// `Text` — Read/Write would require language knowledge the engine refuses.
pub(crate) fn native_document_highlight(
    ctx: NativeBindingsContext<'_>,
) -> Option<Vec<DocumentHighlight>> {
    let ranges = binding_ranges(&ctx, true)?;
    let highlights: Vec<DocumentHighlight> = ranges
        .iter()
        .filter_map(|r| to_lsp_range(ctx.mapper, r))
        .map(|range| DocumentHighlight {
            range,
            kind: Some(DocumentHighlightKind::TEXT),
        })
        .collect();
    (!highlights.is_empty()).then_some(highlights)
}

/// `textDocument/rename`: rename every definition site and every reference
/// resolving to the cursor's binding — layer-confined by construction,
/// best-effort by design.
pub(crate) fn native_rename(
    ctx: NativeBindingsContext<'_>,
    lsp_uri: &Uri,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let ranges = binding_ranges(&ctx, true)?;
    let edits: Vec<TextEdit> = ranges
        .iter()
        .filter_map(|r| to_lsp_range(ctx.mapper, r))
        .map(|range| TextEdit {
            range,
            new_text: new_name.to_string(),
        })
        .collect();
    if edits.is_empty() {
        return None;
    }
    let mut changes = std::collections::HashMap::new();
    changes.insert(lsp_uri.clone(), edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        ..WorkspaceEdit::default()
    })
}

/// `textDocument/prepareRename`: answers only when the cursor's identifier
/// resolves natively; otherwise silence and the bridge owns the request.
pub(crate) fn native_prepare_rename(
    ctx: NativeBindingsContext<'_>,
) -> Option<PrepareRenameResponse> {
    let range = ctx.model.resolvable_identifier_at(ctx.byte)?;
    to_lsp_range(ctx.mapper, &range).map(PrepareRenameResponse::Range)
}

/// The byte ranges of the cursor binding's references (and, when included,
/// its definition sites), sorted for deterministic responses.
fn binding_ranges(
    ctx: &NativeBindingsContext<'_>,
    include_definitions: bool,
) -> Option<Vec<Range<usize>>> {
    let binding = ctx.model.binding_at(ctx.byte)?;
    let mut ranges = ctx.model.references_resolving_to(binding);
    if include_definitions {
        ranges.extend(
            ctx.model
                .sites(binding)
                .iter()
                .map(|s| s.byte_range.clone()),
        );
    }
    ranges.sort_by_key(|r| (r.start, r.end));
    ranges.dedup();
    Some(ranges)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::Position;
    use tree_sitter::Query;
    use url::Url;

    use super::super::super::Kakehashi;
    use super::*;

    const RUST_BINDINGS: &str = r#"
        (block) @scope
        (let_declaration pattern: (identifier) @definition)
        (identifier) @reference
    "#;

    /// A server with the rust grammar registered, a bindings query in the
    /// store, and one open (unparsed) rust document — the on-demand parse in
    /// `native_bindings_answer` supplies the tree.
    fn server_with_doc(text: &str) -> (LspService<Kakehashi>, Url, Uri) {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let query = Query::new(&tree_sitter_rust::LANGUAGE.into(), RUST_BINDINGS).unwrap();
        server
            .language
            .query_store()
            .insert_bindings_query("rust".to_string(), Arc::new(query));

        let url = Url::parse("file:///test/native_bindings.rs").unwrap();
        let uri = Uri::from_str(url.as_str()).unwrap();
        server.documents.insert(
            url.clone(),
            text.to_string(),
            Some("rust".to_string()),
            None,
        );
        (service, url, uri)
    }

    #[tokio::test]
    async fn native_definition_resolves_through_the_full_stack() {
        // "fn main() { let target = 1; target; }"
        //  byte 16: definition; byte 28: reference.
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        let links = server
            .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap()
            .expect("the reference must resolve natively");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_range.start, Position::new(0, 16));
        assert_eq!(links[0].target_range.end, Position::new(0, 22));
    }

    #[tokio::test]
    async fn native_rename_edits_every_site_and_reference() {
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        let edit = server
            .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                native_rename(ctx, &uri, "renamed")
            })
            .await
            .unwrap()
            .expect("rename must be offered for a resolved identifier");
        let edits = &edit.changes.unwrap()[&uri];
        assert_eq!(edits.len(), 2, "definition site + reference");
        assert!(edits.iter().all(|e| e.new_text == "renamed"));
    }

    #[tokio::test]
    async fn native_prepare_rename_stays_silent_on_unresolved_identifier() {
        // `missing` resolves nowhere: the native layer must contribute
        // nothing so the bridge owns the request.
        let text = "fn main() { missing; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        let response = server
            .native_bindings_answer(&uri, Position::new(0, 14), native_prepare_rename)
            .await
            .unwrap();
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn language_without_bindings_query_contributes_nothing() {
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();
        // Drop the query: the layer must answer None, not error.
        server.language.query_store().remove_queries("rust");

        let links = server
            .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap();
        assert!(links.is_none());
    }

    #[tokio::test]
    async fn native_document_highlight_includes_definition_sites() {
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        let highlights = server
            .native_bindings_answer(&uri, Position::new(0, 18), native_document_highlight)
            .await
            .unwrap()
            .expect("cursor on the definition identifies its binding");
        assert_eq!(highlights.len(), 2);
        assert!(
            highlights
                .iter()
                .all(|h| h.kind == Some(DocumentHighlightKind::TEXT))
        );
    }
}
