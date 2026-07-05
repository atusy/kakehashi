//! Native lexical-resolution layer (lexical-name-resolution ADR).
//!
//! Serves definition / references / documentHighlight / rename from the
//! tree-sitter tree alone via the generic bindings engine, for layers whose
//! language has a `bindings.scm` — typically ones with no bridge server
//! configured. Feeds the `native` slot of the cross-layer walk; the miss
//! policy is silence, so an unresolved cursor contributes nothing and the
//! bridge/aggregation path owns the answer.
//!
//! Layers: the host document and offset-free injected regions (the region's
//! tree is parsed with included ranges; results map back through the
//! region's content offset). `#offset!`-shifted regions stay bridge-only.

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
/// cursor's byte offset **within the layer**, the host-text mapper, and the
/// layer's byte offset into the host text (0 for the host layer).
pub(crate) struct NativeBindingsContext<'a> {
    pub(crate) model: &'a BindingsModel,
    pub(crate) byte: usize,
    pub(crate) mapper: &'a PositionMapper,
    pub(crate) layer_offset: usize,
}

impl NativeBindingsContext<'_> {
    /// A layer-relative byte range as a host-document LSP range.
    fn to_host_range(&self, range: &Range<usize>) -> Option<tower_lsp_server::ls_types::Range> {
        Some(tower_lsp_server::ls_types::Range {
            start: self
                .mapper
                .byte_to_position(range.start + self.layer_offset)?,
            end: self
                .mapper
                .byte_to_position(range.end + self.layer_offset)?,
        })
    }
}

/// The bindings inputs of the layer under the cursor: the layer's query, its
/// text (`Arc<str>` so the host layer shares the document text instead of
/// copying it into the blocking task), its tree, and its byte offset into
/// the host text.
type LayerInputs = (
    std::sync::Arc<tree_sitter::Query>,
    std::sync::Arc<str>,
    tree_sitter::Tree,
    usize,
);

impl Kakehashi {
    /// Build the bindings model for the layer under the cursor — the injected
    /// region's tree when the cursor sits inside one, the host tree otherwise
    /// — and answer with `f`. `Ok(None)` — the layer contributes nothing —
    /// when the layer's language has no bindings query, the document is
    /// unavailable, or `f` itself answers `None` (miss policy: silence).
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

        // Wait for / trigger the off-ingress parse, then snapshot text and
        // tree without holding the store Ref across compute. The text Arc
        // doubles as the staleness witness for the publish-time check below
        // (every edit and reopen installs a fresh allocation).
        self.ensure_document_parsed(&uri).await;
        let Some((text, tree)) = ({
            let doc = self.documents.get(&uri);
            doc.and_then(|doc| {
                let tree = doc.tree()?.clone();
                Some((doc.text_arc(), tree))
            })
        }) else {
            return Ok(None);
        };

        // Strict conversion: a client-supplied `character` past its line
        // end must silence, not spill onto a later line's identifier.
        let mapper = PositionMapper::new(&text);
        let Some(byte) = mapper.position_to_byte_strict(position) else {
            return Ok(None);
        };

        // Scope trees are per layer and resolution never crosses layer
        // boundaries: a cursor inside an injected region resolves in that
        // region's layer alone.
        let (query, layer_text, layer_tree, layer_offset) = match self
            .injected_bindings_layer(&uri, &text, &tree, &language, byte)
            .await
        {
            Some(Some(layer)) => layer,
            // Inside a region the resolver cannot serve: silence, never
            // a host-layer answer for an injected-code cursor.
            Some(None) => return Ok(None),
            None => {
                let Some(query) = self.language.bindings_query(&language) else {
                    return Ok(None);
                };
                (query, std::sync::Arc::clone(&text), tree.clone(), 0)
            }
        };

        // The query walk and model build are CPU work proportional to the
        // layer, not the request: keep them off the async worker like every
        // other tree-CPU site. A failed task degrades to silence.
        let model = match tokio::task::spawn_blocking(move || {
            BindingsModel::build(collect(&layer_text, layer_tree.root_node(), &query))
        })
        .await
        {
            Ok(model) => model,
            Err(join_error) => {
                log::warn!(
                    target: "kakehashi::bindings",
                    "bindings model task failed for {uri}: {join_error}"
                );
                return Ok(None);
            }
        };
        let answer = f(NativeBindingsContext {
            model: &model,
            byte: byte - layer_offset,
            mapper: &mapper,
            layer_offset,
        });

        // A didChange between the snapshot and here makes every computed
        // range stale — worst case a rename WorkspaceEdit applied to newer
        // text. Publish only while the snapshot's text is still current.
        let unchanged = self
            .documents
            .get(&uri)
            .is_some_and(|doc| std::sync::Arc::ptr_eq(&doc.text_arc(), &text));
        if !unchanged {
            return Ok(None);
        }
        Ok(answer)
    }

    /// The injected layer under the cursor, prepared for resolution.
    ///
    /// - `None`: the cursor is not inside any injection region → host layer.
    /// - `Some(None)`: inside a region, but the layer cannot answer (no
    ///   resolvable language, no bindings query, offset-shifted region, or
    ///   parse failure) → silence.
    /// - `Some(Some(inputs))`: the region's layer, parsed with included
    ///   ranges so node offsets are relative to the region's content start.
    async fn injected_bindings_layer(
        &self,
        uri: &url::Url,
        text: &str,
        tree: &tree_sitter::Tree,
        host_language: &str,
        byte: usize,
    ) -> Option<Option<LayerInputs>> {
        use crate::language::injection::{
            collect_all_injections, compute_included_ranges, parse_with_ranges,
        };

        let injection_query = self.language.injection_query(host_language)?;
        let regions = collect_all_injections(&tree.root_node(), text, Some(&injection_query))?;
        // Innermost containing region: under nesting (an include-children
        // outer region wrapping a fence) the smallest layer owns the cursor.
        let region = regions
            .iter()
            .filter(|r| r.content_node.start_byte() <= byte && byte < r.content_node.end_byte())
            .min_by_key(|r| r.content_node.end_byte() - r.content_node.start_byte())?;

        // From here on the cursor IS in a region: every bail is silence.
        if region.offset.is_some() {
            // `#offset!`-shifted regions need window clipping the native
            // path does not do yet; the bridge keeps owning them.
            return Some(None);
        }
        let content_range = region.content_node.byte_range();
        let Some(content_text) = text.get(content_range.clone()).map(str::to_string) else {
            return Some(None);
        };
        let Some((layer_language, load)) = self
            .language
            .resolve_injection_language(&region.language, &content_text)
        else {
            return Some(None);
        };
        if !load.success {
            return Some(None);
        }
        let Some(query) = self.language.bindings_query(&layer_language) else {
            return Some(None);
        };

        let included_ranges =
            compute_included_ranges(&region.content_node, region.include_children);
        // The pooled spawn_blocking + timeout protocol every parse site
        // uses: a pathological region cannot pin an async worker.
        let lang_for_parse = layer_language.clone();
        let parsed = self
            .parse_coordinator()
            .parse_with_pool(
                &layer_language,
                uri,
                content_text.len(),
                move |mut parser| {
                    let tree = parse_with_ranges(
                        &mut parser,
                        &content_text,
                        included_ranges.as_deref(),
                        "kakehashi::bindings",
                        &lang_for_parse,
                    );
                    (parser, tree.map(|tree| (tree, content_text)))
                },
            )
            .await;

        match parsed {
            Some((layer_tree, content_text)) => Some(Some((
                query,
                std::sync::Arc::from(content_text),
                layer_tree,
                content_range.start,
            ))),
            None => Some(None),
        }
    }
}

/// `textDocument/definition`: the definition site the resolution rules report
/// for the cursor's identifier.
pub(crate) fn native_definition(
    ctx: NativeBindingsContext<'_>,
    lsp_uri: &Uri,
) -> Option<Vec<LocationLink>> {
    let target = ctx.model.definition_range_at(ctx.byte)?;
    let range = ctx.to_host_range(&target)?;
    Some(vec![LocationLink {
        origin_selection_range: ctx
            .model
            .resolvable_identifier_at(ctx.byte)
            .and_then(|r| ctx.to_host_range(&r)),
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
        .filter_map(|r| ctx.to_host_range(r))
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
        .filter_map(|r| ctx.to_host_range(r))
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
    // The engine carries no per-language identifier grammar: accept only
    // the conservative intersection every shipped asset's language treats
    // as one identifier ([A-Za-z_][A-Za-z0-9_]*). Anything else — however
    // valid in some grammar — silences, and the bridge (if any) owns the
    // request; writing a syntax error would be a wrong answer.
    if !is_conservative_identifier(new_name) {
        return None;
    }
    let ranges = binding_ranges(&ctx, true)?;
    let edits: Vec<TextEdit> = ranges
        .iter()
        .filter_map(|r| ctx.to_host_range(r))
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

/// The identifier shape every shipped asset's language accepts:
/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_conservative_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `textDocument/prepareRename`: answers only when the cursor's identifier
/// resolves natively; otherwise silence and the bridge owns the request.
pub(crate) fn native_prepare_rename(
    ctx: NativeBindingsContext<'_>,
) -> Option<PrepareRenameResponse> {
    let range = ctx.model.resolvable_identifier_at(ctx.byte)?;
    ctx.to_host_range(&range).map(PrepareRenameResponse::Range)
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
    async fn native_rename_refuses_names_that_cannot_be_identifiers() {
        // The engine has no per-language identifier grammar, but an empty
        // or whitespace-carrying name corrupts the buffer in every
        // language: silence, and the bridge (if any) owns the request.
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        for bad in ["", "a b", "x\n", " lead"] {
            let edit = server
                .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                    native_rename(ctx, &uri, bad)
                })
                .await
                .unwrap();
            assert!(edit.is_none(), "{bad:?} must not produce a WorkspaceEdit");
        }
    }

    #[tokio::test]
    async fn out_of_bounds_position_is_silence_not_a_spilled_byte() {
        // A `character` past the end of its line must not spill onto a
        // later line's identifier: byte 0 + 39 would land exactly on the
        // second `target` here under lenient conversion.
        let text = "//abcdefgh\nfn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        let links = server
            .native_bindings_answer(&uri, Position::new(0, 39), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap();
        assert!(
            links.is_none(),
            "invalid coordinates must silence: {links:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_edit_after_the_snapshot_silences_the_answer() {
        // A didChange landing between the text/tree snapshot and the answer
        // makes every computed range stale — worst case a rename edit
        // applied to newer text. The answer closure runs at publish time,
        // so mutating the document inside it simulates exactly that window.
        let text = "fn main() { let target = 1; target; }";
        let (service, url, uri) = server_with_doc(text);
        let server = service.inner();

        let edit = server
            .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                server
                    .documents
                    .update_document(url.clone(), "fn main() {}".to_string(), None);
                native_rename(ctx, &uri, "renamed")
            })
            .await
            .unwrap();
        assert!(
            edit.is_none(),
            "a stale answer must be dropped once the document moved on"
        );
    }

    #[tokio::test]
    async fn native_rename_refuses_non_identifier_names() {
        // Beyond whitespace: punctuation-carrying names produce
        // syntactically invalid edits in every shipped asset's language.
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        for bad in ["a.b", "x;drop", "!", "123abc", "café"] {
            let edit = server
                .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                    native_rename(ctx, &uri, bad)
                })
                .await
                .unwrap();
            assert!(edit.is_none(), "{bad:?} must not produce a WorkspaceEdit");
        }
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

    /// A server with markdown (host, injection query, no bindings) and lua
    /// (embedded bindings asset) registered, and one open markdown document.
    fn server_with_markdown_doc(text: &str) -> (LspService<Kakehashi>, Uri) {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_md::LANGUAGE.into());
        server
            .language
            .language_registry_for_parallel()
            .register("lua".to_string(), tree_sitter_lua::LANGUAGE.into());
        let injection_query = Query::new(
            &tree_sitter_md::LANGUAGE.into(),
            r#"
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
            "#,
        )
        .unwrap();
        server
            .language
            .query_store()
            .insert_injection_query("markdown".to_string(), Arc::new(injection_query));
        let lua_bindings_source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/queries/lua/bindings.scm"
        ))
        .unwrap();
        let lua_bindings =
            Query::new(&tree_sitter_lua::LANGUAGE.into(), &lua_bindings_source).unwrap();
        server
            .language
            .query_store()
            .insert_bindings_query("lua".to_string(), Arc::new(lua_bindings));

        let url = Url::parse("file:///test/native_bindings.md").unwrap();
        let uri = Uri::from_str(url.as_str()).unwrap();
        server.documents.insert(
            url.clone(),
            text.to_string(),
            Some("markdown".to_string()),
            None,
        );
        (service, uri)
    }

    #[tokio::test]
    async fn nested_regions_resolve_in_the_innermost_layer() {
        // An outer include-children region spans the whole document and the
        // lua fence nests inside it: the cursor must resolve in the
        // innermost layer, not the first region in document order.
        let text = "# t\n\n```lua\nlocal v = 1\nprint(v)\n```\n";
        let (service, uri) = server_with_markdown_doc(text);
        let server = service.inner();
        let nested_query = Query::new(
            &tree_sitter_md::LANGUAGE.into(),
            r#"
            ((document) @injection.content
             (#set! injection.language "markdown")
             (#set! injection.include-children))
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
            "#,
        )
        .unwrap();
        server
            .language
            .query_store()
            .insert_injection_query("markdown".to_string(), Arc::new(nested_query));

        let links = server
            .native_bindings_answer(&uri, Position::new(4, 6), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap()
            .expect("the innermost (lua) layer must answer, not the outer region");
        assert_eq!(links[0].target_range.start, Position::new(3, 6));
    }

    #[tokio::test]
    async fn native_definition_resolves_inside_an_injected_layer() {
        // line 3: `local v = 1`; line 4: `print(v)`.
        let text = "# t\n\n```lua\nlocal v = 1\nprint(v)\n```\n";
        let (service, uri) = server_with_markdown_doc(text);
        let server = service.inner();

        let links = server
            .native_bindings_answer(&uri, Position::new(4, 6), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap()
            .expect("the injected-layer reference must resolve natively");
        assert_eq!(
            links[0].target_range.start,
            Position::new(3, 6),
            "definition is `local v` in host coordinates"
        );
    }

    #[tokio::test]
    async fn injected_rename_edits_both_sites_in_host_coordinates() {
        // line 3: `local v = 1`; line 4: `print(v)`.
        let text = "# t\n\n```lua\nlocal v = 1\nprint(v)\n```\n";
        let (service, uri) = server_with_markdown_doc(text);
        let server = service.inner();

        let edit = server
            .native_bindings_answer(&uri, Position::new(4, 6), |ctx| {
                native_rename(ctx, &uri, "renamed")
            })
            .await
            .unwrap()
            .expect("injected-layer rename must be offered");
        let edits = &edit.changes.unwrap()[&uri];
        let starts: Vec<Position> = edits.iter().map(|e| e.range.start).collect();
        assert!(
            starts.contains(&Position::new(3, 6)) && starts.contains(&Position::new(4, 6)),
            "definition and reference in host coordinates: {starts:?}"
        );
    }

    #[tokio::test]
    async fn references_without_declaration_exclude_the_definition_site() {
        let text = "fn main() { let target = 1; target; }";
        let (service, _url, uri) = server_with_doc(text);
        let server = service.inner();

        let refs = server
            .native_bindings_answer(&uri, Position::new(0, 28), |ctx| {
                native_references(ctx, &uri, false)
            })
            .await
            .unwrap()
            .expect("the use site is still a reference");
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].range.start,
            Position::new(0, 28),
            "only the use site; the `let target` declaration is excluded"
        );
    }

    #[tokio::test]
    async fn injected_layers_never_cross_regions() {
        // Two lua blocks: the second reads a name only the first defines.
        let text = "```lua\nlocal shared = 1\n```\n\n```lua\nprint(shared)\n```\n";
        let (service, uri) = server_with_markdown_doc(text);
        let server = service.inner();

        let links = server
            .native_bindings_answer(&uri, Position::new(5, 8), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap();
        assert!(
            links.is_none(),
            "cross-region resolution is out of scope for v1: {links:?}"
        );
    }

    #[tokio::test]
    async fn injected_cursor_never_gets_a_host_layer_answer() {
        // The host (markdown) has no bindings query; even if it did, an
        // injected-code cursor must resolve in its own layer only. Here the
        // injected language (yaml-ish unknown) has no bindings query either:
        // silence.
        let text = "```nosuchlang\nkey\n```\n";
        let (service, uri) = server_with_markdown_doc(text);
        let server = service.inner();

        let links = server
            .native_bindings_answer(&uri, Position::new(1, 1), |ctx| {
                native_definition(ctx, &uri)
            })
            .await
            .unwrap();
        assert!(links.is_none());
    }
}
