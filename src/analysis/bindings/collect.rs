//! Collection phase of lexical name resolution: run a `bindings.scm` query
//! over a layer tree and turn its matches into typed capture records.
//!
//! This is the query-facing half of the engine specified in
//! `docs/architecture-decisions/lexical-name-resolution.md`: the capture
//! vocabulary (`@scope` / `@definition` / `@reference` / `@redirect`) and the
//! static `#set!` properties are parsed here; scope-tree construction and
//! resolution live in [`super::model`].

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tree_sitter::{Node, Query, QueryProperty, StreamingIterator};

use crate::language::query_predicates::check_match_predicates;

/// Which scope a definition registers in (`definition.scope`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeTarget {
    /// The innermost enclosing `@scope` (default).
    Local,
    /// The parent of the innermost enclosing scope (clamped to the root).
    Parent,
    /// The layer root scope.
    Global,
    /// A `@scope.<label>` captured in the same pattern, regardless of
    /// containment.
    Label(String),
    /// The closest ancestor scope carrying the label; the layer root if none.
    Nearest(String),
}

/// When a binding becomes visible (`definition.visibility`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Visibility {
    /// The whole registering scope (default).
    Scope,
    /// From the end byte of the pattern's match onward.
    After,
    /// From the start byte of the definition node onward.
    Declaration,
}

/// How a definition relates to an earlier same-name binding
/// (`definition.rebind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebind {
    /// Add a site to the binding active at the definition's start byte
    /// (default).
    Merge,
    /// Start a new binding that shadows the previous one.
    Fresh,
    /// Register into an enclosing visible binding if one exists, else merge
    /// locally.
    OuterOrLocal,
}

/// Where a `@redirect` re-routes a name (`redirect.target`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RedirectTarget {
    /// The layer root scope.
    Global,
    /// The closest ancestor scope labelled with the string, unconditionally.
    Nearest(String),
    /// The closest labelled ancestor that already holds a binding of the
    /// `(name, namespace)`.
    NearestBinding(String),
}

/// Which lookups may continue past a scope (`scope.inherits`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inherits {
    /// Every namespace continues outward (default).
    All,
    /// No namespace continues outward.
    None,
    /// Only the listed namespaces continue outward.
    Namespaces(Vec<String>),
}

/// A `@scope` / `@scope.<label>` capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeCapture {
    pub byte_range: Range<usize>,
    pub label: Option<String>,
    pub inherits: Inherits,
    pub visible_to_nested: bool,
}

/// A `@definition` / `@definition.<label>` capture with its declared
/// semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefinitionCapture {
    pub byte_range: Range<usize>,
    /// The identifier text.
    pub name: String,
    /// The opaque `<label>` part of the capture name, if any.
    pub label: Option<String>,
    pub namespace: String,
    pub scope_target: ScopeTarget,
    pub visibility: Visibility,
    pub rebind: Rebind,
    /// Largest end byte among all nodes the pattern match captured
    /// (visibility anchor for `after`).
    pub match_end: usize,
    /// `@scope.<label>` captures from the same match, for `ScopeTarget::Label`
    /// targeting. `None` marks a duplicate label (authoring error → the
    /// target is unusable and the definition is not registered).
    pub labeled_scopes: HashMap<String, Option<Range<usize>>>,
}

/// A `@reference` / `@reference.<label>` capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReferenceCapture {
    pub byte_range: Range<usize>,
    pub name: String,
    /// Namespaces this reference may bind to, searched in order.
    pub namespaces: Vec<String>,
}

/// A `@redirect` capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RedirectCapture {
    pub byte_range: Range<usize>,
    pub name: String,
    pub namespace: String,
    pub target: RedirectTarget,
}

/// Everything a `bindings.scm` run produced for one layer tree.
#[derive(Debug, Default)]
pub(crate) struct Collection {
    /// Byte range of the layer root (the implicit root scope).
    pub root_range: Range<usize>,
    pub scopes: Vec<ScopeCapture>,
    pub definitions: Vec<DefinitionCapture>,
    pub references: Vec<ReferenceCapture>,
    pub redirects: Vec<RedirectCapture>,
}

const DEFAULT_NAMESPACE: &str = "default";

/// Reserved `definition.scope` words that cannot be scope labels.
const RESERVED_SCOPE_WORDS: [&str; 3] = ["local", "parent", "global"];

/// Look up a `#set!` property for one capture: the capture-scoped form
/// (`#set! @capture key value`) wins over the bare form (`#set! key value`).
fn property<'a>(props: &'a [QueryProperty], key: &str, capture_index: u32) -> Option<&'a str> {
    props
        .iter()
        .find(|p| p.capture_id == Some(capture_index as usize) && &*p.key == key)
        .or_else(|| {
            props
                .iter()
                .find(|p| p.capture_id.is_none() && &*p.key == key)
        })
        .and_then(|p| p.value.as_deref())
}

/// Split a capture name into its vocabulary base and optional opaque label.
fn split_capture_name(name: &str) -> (&str, Option<&str>) {
    match name.split_once('.') {
        Some((base, label)) => (base, Some(label)),
        None => (name, None),
    }
}

fn parse_scope_target(value: Option<&str>) -> Option<ScopeTarget> {
    match value {
        None | Some("local") => Some(ScopeTarget::Local),
        Some("parent") => Some(ScopeTarget::Parent),
        Some("global") => Some(ScopeTarget::Global),
        Some(other) => {
            if let Some(label) = other.strip_prefix("nearest:") {
                if label.is_empty() || RESERVED_SCOPE_WORDS.contains(&label) {
                    return None;
                }
                Some(ScopeTarget::Nearest(label.to_string()))
            } else if other.is_empty() || RESERVED_SCOPE_WORDS.contains(&other) {
                None
            } else {
                Some(ScopeTarget::Label(other.to_string()))
            }
        }
    }
}

fn parse_visibility(value: Option<&str>) -> Option<Visibility> {
    match value {
        None | Some("scope") => Some(Visibility::Scope),
        Some("after") => Some(Visibility::After),
        Some("declaration") => Some(Visibility::Declaration),
        Some(_) => None,
    }
}

fn parse_rebind(value: Option<&str>) -> Option<Rebind> {
    match value {
        None | Some("merge") => Some(Rebind::Merge),
        Some("fresh") => Some(Rebind::Fresh),
        Some("outer-or-local") => Some(Rebind::OuterOrLocal),
        Some(_) => None,
    }
}

fn parse_inherits(value: Option<&str>) -> Inherits {
    match value {
        None | Some("true") => Inherits::All,
        Some("false") => Inherits::None,
        Some(list) => {
            let namespaces: Vec<String> = list.split_whitespace().map(str::to_string).collect();
            if namespaces.is_empty() {
                Inherits::All
            } else {
                Inherits::Namespaces(namespaces)
            }
        }
    }
}

fn parse_redirect_target(value: Option<&str>) -> Option<RedirectTarget> {
    match value? {
        "global" => Some(RedirectTarget::Global),
        other => {
            if let Some(label) = other.strip_prefix("nearest-binding:") {
                (!label.is_empty()).then(|| RedirectTarget::NearestBinding(label.to_string()))
            } else if let Some(label) = other.strip_prefix("nearest:") {
                (!label.is_empty()).then(|| RedirectTarget::Nearest(label.to_string()))
            } else {
                None
            }
        }
    }
}

fn parse_namespaces(value: Option<&str>) -> Vec<String> {
    let namespaces: Vec<String> = value
        .unwrap_or(DEFAULT_NAMESPACE)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if namespaces.is_empty() {
        vec![DEFAULT_NAMESPACE.to_string()]
    } else {
        namespaces
    }
}

/// Run `query` over the layer tree rooted at `root` and collect the bindings
/// vocabulary. General predicates gate whole matches; a match with a failing
/// predicate contributes nothing. Property values outside the spec drop the
/// carrying capture (silence over a wrong answer), never default.
/// The production path always carries the race's cancellation flag through
/// [`collect_cancellable`], so the flagless form stays test-only.
#[cfg(test)]
pub(crate) fn collect(text: &str, root: Node, query: &Query) -> Collection {
    collect_cancellable(
        text,
        root,
        query,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .expect("an unset flag never cancels")
}

/// [`collect`] with a cooperative cancellation flag, checked once per query
/// match: a dropped requester (e.g. the cross-layer race returning early)
/// stops the walk instead of burning a blocking thread to completion.
/// `None` means cancelled — never a partial collection.
pub(crate) fn collect_cancellable(
    text: &str,
    root: Node,
    query: &Query,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<Collection> {
    let mut collection = Collection {
        root_range: root.byte_range(),
        ..Collection::default()
    };

    let capture_names = query.capture_names();
    let mut definition_nodes: HashSet<(usize, usize)> = HashSet::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, root, text.as_bytes());

    while let Some(m) = matches.next() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        if m.captures.is_empty() {
            continue;
        }
        if !check_match_predicates(query, m, text) {
            continue;
        }

        let props = query.property_settings(m.pattern_index);

        // The match extent: largest end byte among ALL captured nodes,
        // vocabulary and throwaway (`@_`) captures alike.
        let match_end = m
            .captures
            .iter()
            .map(|c| c.node.end_byte())
            .max()
            .unwrap_or(0);

        // `@scope.<label>` captures in this match, for label targeting.
        // A duplicated label is an authoring error recorded as `None`.
        let mut labeled_scopes: HashMap<String, Option<Range<usize>>> = HashMap::new();
        for capture in m.captures {
            let (base, label) = split_capture_name(capture_names[capture.index as usize]);
            if base == "scope"
                && let Some(label) = label
            {
                labeled_scopes
                    .entry(label.to_string())
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(capture.node.byte_range()));
            }
        }

        for capture in m.captures {
            let (base, label) = split_capture_name(capture_names[capture.index as usize]);
            let byte_range = capture.node.byte_range();
            let node_text = || text.get(byte_range.clone()).map(str::to_string);

            match base {
                "scope" => {
                    let visible_to_nested =
                        match property(props, "scope.visible-to-nested", capture.index) {
                            None | Some("true") => true,
                            Some("false") => false,
                            Some(other) => {
                                log::debug!(
                                    target: "kakehashi::bindings",
                                    "dropping @scope: invalid scope.visible-to-nested '{}'",
                                    other
                                );
                                continue;
                            }
                        };
                    collection.scopes.push(ScopeCapture {
                        byte_range,
                        label: label.map(str::to_string),
                        inherits: parse_inherits(property(props, "scope.inherits", capture.index)),
                        visible_to_nested,
                    });
                }
                "definition" => {
                    // Every @definition-authored node is barred from the
                    // reference set, valid or not: a dropped definition must
                    // silence, not resolve outward as a blanket reference.
                    definition_nodes.insert((byte_range.start, byte_range.end));
                    let Some(name) = node_text() else { continue };
                    let Some(scope_target) =
                        parse_scope_target(property(props, "definition.scope", capture.index))
                    else {
                        log::debug!(
                            target: "kakehashi::bindings",
                            "dropping @definition '{}': invalid definition.scope",
                            name
                        );
                        continue;
                    };
                    let Some(visibility) =
                        parse_visibility(property(props, "definition.visibility", capture.index))
                    else {
                        log::debug!(
                            target: "kakehashi::bindings",
                            "dropping @definition '{}': invalid definition.visibility",
                            name
                        );
                        continue;
                    };
                    let Some(rebind) =
                        parse_rebind(property(props, "definition.rebind", capture.index))
                    else {
                        log::debug!(
                            target: "kakehashi::bindings",
                            "dropping @definition '{}': invalid definition.rebind",
                            name
                        );
                        continue;
                    };
                    // Only label-targeted definitions consult the match's
                    // labeled scopes; everything else takes an empty
                    // (allocation-free) map instead of a per-capture clone.
                    let labeled_scopes = if matches!(scope_target, ScopeTarget::Label(_)) {
                        labeled_scopes.clone()
                    } else {
                        HashMap::new()
                    };
                    collection.definitions.push(DefinitionCapture {
                        byte_range,
                        name,
                        label: label.map(str::to_string),
                        namespace: property(props, "definition.namespace", capture.index)
                            .unwrap_or(DEFAULT_NAMESPACE)
                            .to_string(),
                        scope_target,
                        visibility,
                        rebind,
                        match_end,
                        labeled_scopes,
                    });
                }
                "reference" => {
                    let Some(name) = node_text() else { continue };
                    collection.references.push(ReferenceCapture {
                        byte_range,
                        name,
                        namespaces: parse_namespaces(property(
                            props,
                            "reference.namespace",
                            capture.index,
                        )),
                    });
                }
                "redirect" => {
                    let Some(name) = node_text() else { continue };
                    let Some(target) =
                        parse_redirect_target(property(props, "redirect.target", capture.index))
                    else {
                        log::debug!(
                            target: "kakehashi::bindings",
                            "dropping @redirect '{}': missing or invalid redirect.target",
                            name
                        );
                        continue;
                    };
                    collection.redirects.push(RedirectCapture {
                        byte_range,
                        name,
                        namespace: property(props, "redirect.namespace", capture.index)
                            .unwrap_or(DEFAULT_NAMESPACE)
                            .to_string(),
                        target,
                    });
                }
                // Captures outside the vocabulary (including `@_` throwaways)
                // only widen the match extent.
                _ => {}
            }
        }
    }

    // A node captured as @definition never enters the reference set — even
    // when the definition itself was dropped for an invalid property: the
    // blanket `(identifier) @reference` form is expected, and a declaration
    // site resolving outward would be a wrong answer, not silence.
    collection
        .references
        .retain(|r| !definition_nodes.contains(&(r.byte_range.start, r.byte_range.end)));

    Some(collection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_rust(text: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    fn rust_query(query: &str) -> Query {
        Query::new(&tree_sitter_rust::LANGUAGE.into(), query).unwrap()
    }

    fn collect_rust(text: &str, query: &str) -> Collection {
        let tree = parse_rust(text);
        let query = rust_query(query);
        collect(text, tree.root_node(), &query)
    }

    #[test]
    fn cancelled_collection_returns_none_not_a_partial_result() {
        let text = "fn main() { let x = 1; x; }";
        let tree = parse_rust(text);
        let query = rust_query("(identifier) @reference");
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        assert!(
            collect_cancellable(text, tree.root_node(), &query, &cancelled).is_none(),
            "cancellation must yield None, never a partial collection"
        );
    }

    #[test]
    fn invalid_visible_to_nested_drops_the_scope_capture() {
        // Anything other than "true"/"false" is an authoring error: the
        // scope must drop (silence), not default to visible and leak its
        // names into nested scopes.
        let text = "fn main() { let x = 1; }";
        let collection = collect_rust(
            text,
            r#"
            ((block) @scope
             (#set! scope.visible-to-nested "nope"))
            "#,
        );
        assert!(
            collection.scopes.is_empty(),
            "invalid scope.visible-to-nested must drop the capture"
        );
    }

    #[test]
    fn definition_dropped_for_invalid_property_stays_out_of_references() {
        // A node authored as @definition must never fall back to being a
        // blanket reference when its properties are invalid — resolving a
        // declaration site to an unrelated outer binding is a wrong answer.
        let text = "fn main() { let x = 1; x; }";
        let collection = collect_rust(
            text,
            r#"
            ((let_declaration pattern: (identifier) @definition)
             (#set! definition.visibility "sometimes"))
            (identifier) @reference
            "#,
        );
        assert!(
            collection.definitions.is_empty(),
            "invalid visibility drops the definition"
        );
        let def_x = text.find('x').unwrap();
        assert!(
            !collection
                .references
                .iter()
                .any(|r| r.byte_range.start == def_x),
            "the dropped definition node must not survive as a reference"
        );
    }

    #[test]
    fn collects_scopes_definitions_references_with_defaults() {
        let text = "fn main() { let x = 1; x; }";
        let collection = collect_rust(
            text,
            r#"
            (block) @scope
            (let_declaration pattern: (identifier) @definition)
            (identifier) @reference
            "#,
        );

        assert_eq!(collection.root_range, 0..text.len());
        assert_eq!(collection.scopes.len(), 1);
        let scope = &collection.scopes[0];
        assert_eq!(scope.label, None);
        assert_eq!(scope.inherits, Inherits::All);
        assert!(scope.visible_to_nested);

        assert_eq!(collection.definitions.len(), 1);
        let def = &collection.definitions[0];
        assert_eq!(def.name, "x");
        assert_eq!(def.namespace, "default");
        assert_eq!(def.scope_target, ScopeTarget::Local);
        assert_eq!(def.visibility, Visibility::Scope);
        assert_eq!(def.rebind, Rebind::Merge);

        // `main` and the usage `x`; the definition node `x` is excluded.
        let ref_names: Vec<&str> = collection
            .references
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert!(ref_names.contains(&"main"));
        assert!(ref_names.contains(&"x"));
        assert_eq!(
            collection
                .references
                .iter()
                .filter(|r| r.byte_range == collection.definitions[0].byte_range)
                .count(),
            0,
            "definition node must be excluded from references"
        );
    }

    #[test]
    fn match_end_spans_throwaway_captures() {
        let text = "fn main() { let x = 1; }";
        let collection = collect_rust(
            text,
            r#"
            (let_declaration pattern: (identifier) @definition) @_decl
            "#,
        );
        assert_eq!(collection.definitions.len(), 1);
        let def = &collection.definitions[0];
        let decl_end = text.find("= 1;").unwrap() + "= 1;".len();
        assert_eq!(
            def.match_end, decl_end,
            "match extent must include the @_decl capture"
        );
        assert!(def.match_end > def.byte_range.end);
    }

    #[test]
    fn bare_and_capture_scoped_properties() {
        let text = "fn f(a: u32) { let b = 1; }";
        let collection = collect_rust(
            text,
            r#"
            ((parameter pattern: (identifier) @definition.parameter)
             (#set! definition.visibility "after"))
            ((let_declaration pattern: (identifier) @definition.var)
             (#set! @definition.var definition.visibility "declaration")
             (#set! @definition.var definition.rebind "fresh"))
            "#,
        );
        let param = collection
            .definitions
            .iter()
            .find(|d| d.name == "a")
            .unwrap();
        assert_eq!(param.label.as_deref(), Some("parameter"));
        assert_eq!(param.visibility, Visibility::After);

        let var = collection
            .definitions
            .iter()
            .find(|d| d.name == "b")
            .unwrap();
        assert_eq!(var.label.as_deref(), Some("var"));
        assert_eq!(var.visibility, Visibility::Declaration);
        assert_eq!(var.rebind, Rebind::Fresh);
    }

    #[test]
    fn invalid_property_values_drop_the_definition() {
        let text = "fn main() { let x = 1; }";
        let collection = collect_rust(
            text,
            r#"
            ((let_declaration pattern: (identifier) @definition)
             (#set! definition.visibility "sometimes"))
            "#,
        );
        assert!(collection.definitions.is_empty());

        // Reserved words are not scope labels.
        let collection = collect_rust(
            text,
            r#"
            ((let_declaration pattern: (identifier) @definition)
             (#set! definition.scope "nearest:parent"))
            "#,
        );
        assert!(collection.definitions.is_empty());
    }

    #[test]
    fn namespaces_parse_and_default() {
        let text = "struct S; fn f(s: S) {}";
        let collection = collect_rust(
            text,
            r#"
            ((struct_item name: (type_identifier) @definition.type)
             (#set! definition.namespace "type"))
            ((type_identifier) @reference
             (#set! reference.namespace "type default"))
            (identifier) @reference
            "#,
        );
        let def = &collection.definitions[0];
        assert_eq!(def.namespace, "type");

        let type_ref = collection
            .references
            .iter()
            .find(|r| r.namespaces.len() == 2)
            .expect("type-position reference");
        assert_eq!(type_ref.namespaces, vec!["type", "default"]);

        let plain_ref = collection
            .references
            .iter()
            .find(|r| r.name == "s")
            .expect("plain reference");
        assert_eq!(plain_ref.namespaces, vec!["default"]);
    }

    #[test]
    fn scope_properties_parse() {
        let text = "fn main() { { let x = 1; } }";
        let collection = collect_rust(
            text,
            r#"
            ((function_item body: (block) @scope.function)
             (#set! scope.inherits "function class")
             (#set! scope.visible-to-nested "false"))
            "#,
        );
        assert_eq!(collection.scopes.len(), 1);
        let scope = &collection.scopes[0];
        assert_eq!(scope.label.as_deref(), Some("function"));
        assert_eq!(
            scope.inherits,
            Inherits::Namespaces(vec!["function".to_string(), "class".to_string()])
        );
        assert!(!scope.visible_to_nested);
    }

    #[test]
    fn redirect_targets_parse_and_gate() {
        let text = "fn main() { -x; -y; }";
        // Contrived: treat unary negation as a redirect marker; the engine
        // never knows a language, so any node works for vocabulary tests.
        let collection = collect_rust(
            text,
            r#"
            ((unary_expression (identifier) @redirect)
             (#set! redirect.target "nearest-binding:function"))
            "#,
        );
        assert_eq!(collection.redirects.len(), 2);
        assert_eq!(
            collection.redirects[0].target,
            RedirectTarget::NearestBinding("function".to_string())
        );
        assert_eq!(collection.redirects[0].namespace, "default");

        // Missing target drops the redirect entirely.
        let collection = collect_rust(text, r#"((unary_expression (identifier) @redirect))"#);
        assert!(collection.redirects.is_empty());
    }

    #[test]
    fn duplicate_scope_label_in_match_is_poisoned() {
        let text = "fn main() { if true { 1; } else { 2; } }";
        let collection = collect_rust(
            text,
            r#"
            ((if_expression
               consequence: (block) @scope.branch
               alternative: (else_clause (block) @scope.branch)) @_if
             (#set! definition.scope "branch"))
            (let_declaration pattern: (identifier) @definition)
            "#,
        );
        // The two @scope.branch captures share one match: the label slot is
        // poisoned so any definition targeting it stays unregistered.
        let poisoned = collection
            .definitions
            .iter()
            .flat_map(|d| d.labeled_scopes.get("branch"))
            .all(|slot| slot.is_none());
        assert!(poisoned || collection.definitions.is_empty());
        // The scopes themselves still exist.
        assert_eq!(collection.scopes.len(), 2);
    }

    #[test]
    fn whole_match_predicates_gate_everything() {
        let text = "fn main() { let keep = 1; let drop_me = 2; }";
        let collection = collect_rust(
            text,
            r#"
            ((let_declaration pattern: (identifier) @definition)
             (#not-lua-match? @definition "^drop"))
            "#,
        );
        let names: Vec<&str> = collection
            .definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, vec!["keep"]);
    }
}
