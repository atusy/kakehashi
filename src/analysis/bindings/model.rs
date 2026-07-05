//! Scope tree, binding registration, and reference resolution — the
//! resolution algorithm of the lexical-name-resolution ADR, section
//! "Resolution algorithm (the engine's entire language model)".
//!
//! The engine implements a universal lexical-scoping model; everything
//! language-specific arrives pre-declared in the [`super::collect`] records.

use std::collections::HashMap;
use std::ops::Range;

use super::collect::{
    Collection, DefinitionCapture, Inherits, Rebind, RedirectTarget, ReferenceCapture, ScopeTarget,
    Visibility,
};

/// Index into [`BindingsModel::bindings`].
pub(crate) type BindingId = usize;

type NameKey = (String, String); // (name, namespace)

#[derive(Debug)]
struct Scope {
    byte_range: Range<usize>,
    label: Option<String>,
    inherits: Inherits,
    visible_to_nested: bool,
    parent: Option<usize>,
    depth: usize,
    /// Bindings registered in this scope, in registration order per name.
    bindings: HashMap<NameKey, Vec<BindingId>>,
    /// `@redirect` directives governing a `(name, namespace)` in this scope.
    redirects: HashMap<NameKey, RedirectTarget>,
}

/// One definition site of a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Site {
    /// The definition node's byte range — what navigation reports.
    pub byte_range: Range<usize>,
    /// The byte from which this site makes the binding visible.
    pub visibility_start: usize,
}

#[derive(Debug)]
struct Binding {
    sites: Vec<Site>,
}

/// The resolved lexical structure of one layer tree: pure function of
/// (layer tree, bindings query), computed once per parsed version.
#[derive(Debug)]
pub(crate) struct BindingsModel {
    scopes: Vec<Scope>,
    bindings: Vec<Binding>,
    references: Vec<ReferenceCapture>,
}

impl BindingsModel {
    pub(crate) fn build(collection: Collection) -> Self {
        let mut model = BindingsModel {
            scopes: build_scope_tree(&collection),
            bindings: Vec::new(),
            references: collection.references,
        };

        // Attach redirect directives to their innermost enclosing scope.
        for redirect in &collection.redirects {
            let scope = model.innermost_scope_at(redirect.byte_range.start);
            model.scopes[scope]
                .redirects
                .entry((redirect.name.clone(), redirect.namespace.clone()))
                .or_insert(redirect.target.clone());
        }

        // Register definitions scope by scope, outermost first; document
        // order within a scope. Outer bindings therefore exist before any
        // inner scope is processed (`outer-or-local` and `nearest-binding`
        // rely on this).
        let mut by_scope: HashMap<usize, Vec<&DefinitionCapture>> = HashMap::new();
        for def in &collection.definitions {
            let scope = model.innermost_scope_at(def.byte_range.start);
            by_scope.entry(scope).or_default().push(def);
        }
        let mut scope_order: Vec<usize> = (0..model.scopes.len()).collect();
        scope_order.sort_by_key(|&s| (model.scopes[s].depth, model.scopes[s].byte_range.start));
        for scope in scope_order {
            let Some(mut defs) = by_scope.remove(&scope) else {
                continue;
            };
            defs.sort_by_key(|d| d.byte_range.start);
            for def in defs {
                model.register(scope, def);
            }
        }

        model
    }

    /// Register one definition whose innermost enclosing scope is
    /// `containing`, applying redirect routing, the `definition.scope` lift,
    /// and `definition.rebind`.
    fn register(&mut self, containing: usize, def: &DefinitionCapture) {
        // A redirect governs the whole (name, namespace) in its scope:
        // definitions route to its target as merge sites regardless of their
        // own definition.scope/rebind.
        let key = (def.name.clone(), def.namespace.clone());
        if let Some(target) = self.scopes[containing].redirects.get(&key).cloned() {
            let Some(target_scope) = self.resolve_redirect_target(containing, &target, &key) else {
                // No qualifying ancestor: silence, not a local binding.
                return;
            };
            self.register_into(target_scope, def, Rebind::Merge);
            return;
        }

        let Some(target_scope) = self.resolve_scope_target(containing, def) else {
            return;
        };
        self.register_into(target_scope, def, def.rebind);
    }

    fn resolve_redirect_target(
        &self,
        containing: usize,
        target: &RedirectTarget,
        key: &NameKey,
    ) -> Option<usize> {
        match target {
            RedirectTarget::Global => Some(0),
            RedirectTarget::Nearest(label) => self.nearest_labeled_ancestor(containing, label),
            RedirectTarget::NearestBinding(label) => {
                let mut current = self.scopes[containing].parent;
                while let Some(scope) = current {
                    if self.scopes[scope].label.as_deref() == Some(label.as_str())
                        && self.scopes[scope]
                            .bindings
                            .get(key)
                            .is_some_and(|b| !b.is_empty())
                    {
                        return Some(scope);
                    }
                    current = self.scopes[scope].parent;
                }
                None
            }
        }
    }

    /// The closest strict ancestor of `scope` carrying `label`.
    fn nearest_labeled_ancestor(&self, scope: usize, label: &str) -> Option<usize> {
        let mut current = self.scopes[scope].parent;
        while let Some(s) = current {
            if self.scopes[s].label.as_deref() == Some(label) {
                return Some(s);
            }
            current = self.scopes[s].parent;
        }
        None
    }

    /// Apply the `definition.scope` lift. `None` means the definition is not
    /// registered (absent or duplicated same-match label — silence).
    fn resolve_scope_target(&self, containing: usize, def: &DefinitionCapture) -> Option<usize> {
        match &def.scope_target {
            ScopeTarget::Local => Some(containing),
            // `parent` in the root scope clamps to the root.
            ScopeTarget::Parent => Some(self.scopes[containing].parent.unwrap_or(0)),
            ScopeTarget::Global => Some(0),
            ScopeTarget::Label(label) => {
                let range = def.labeled_scopes.get(label)?.clone()?;
                self.scope_by_range(&range)
            }
            ScopeTarget::Nearest(label) => {
                // Walk the definition's ancestor scopes outward, innermost
                // enclosing scope included; the layer root if none carries
                // the label.
                let mut current = Some(containing);
                while let Some(scope) = current {
                    if self.scopes[scope].label.as_deref() == Some(label.as_str()) {
                        return Some(scope);
                    }
                    current = self.scopes[scope].parent;
                }
                Some(0)
            }
        }
    }

    fn scope_by_range(&self, range: &Range<usize>) -> Option<usize> {
        self.scopes.iter().position(|s| s.byte_range == *range)
    }

    fn register_into(&mut self, target_scope: usize, def: &DefinitionCapture, rebind: Rebind) {
        let key = (def.name.clone(), def.namespace.clone());
        let site = Site {
            byte_range: def.byte_range.clone(),
            visibility_start: match def.visibility {
                Visibility::Scope => self.scopes[target_scope].byte_range.start,
                Visibility::After => def.match_end,
                Visibility::Declaration => def.byte_range.start,
            },
        };
        let p = def.byte_range.start;

        let binding_id = match rebind {
            Rebind::Fresh => None,
            Rebind::Merge => self.select_in_scope(target_scope, &key, p),
            Rebind::OuterOrLocal => {
                // An enclosing binding visible at the definition's start
                // byte, located by the resolution walk over scopes outside
                // the registering one; else merge locally.
                let namespaces = [def.namespace.clone()];
                self.walk(target_scope, false, &def.name, &namespaces, p)
                    .or_else(|| self.select_in_scope(target_scope, &key, p))
            }
        };

        match binding_id {
            Some(id) => self.bindings[id].sites.push(site),
            None => {
                let id = self.bindings.len();
                self.bindings.push(Binding { sites: vec![site] });
                self.scopes[target_scope]
                    .bindings
                    .entry(key)
                    .or_default()
                    .push(id);
            }
        }
    }

    /// The innermost scope containing byte `p` (the root contains
    /// everything, including positions outside its node range — an injected
    /// layer's cursor is clamped by construction).
    fn innermost_scope_at(&self, p: usize) -> usize {
        self.scopes
            .iter()
            .enumerate()
            .filter(|(i, s)| *i == 0 || (s.byte_range.start <= p && p < s.byte_range.end))
            .max_by_key(|(_, s)| s.depth)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// In-scope selection: among the scope's bindings for `key`, the one with
    /// the latest visibility start at or before `p`; ties broken by the later
    /// definition node.
    fn select_in_scope(&self, scope: usize, key: &NameKey, p: usize) -> Option<BindingId> {
        let candidates = self.scopes[scope].bindings.get(key)?;
        candidates
            .iter()
            .filter_map(|&id| {
                self.bindings[id]
                    .sites
                    .iter()
                    .filter(|s| s.visibility_start <= p)
                    .map(|s| (s.visibility_start, s.byte_range.start))
                    .max()
                    .map(|best| (best, id))
            })
            .max_by_key(|(best, _)| *best)
            .map(|(_, id)| id)
    }

    /// The resolution walk: scopes innermost → outermost from `start`, each
    /// namespace in order within a scope; `visible-to-nested false` scopes
    /// are skipped unless they are the innermost scope (`start_is_innermost`
    /// distinguishes a reference's own scope from registration-time probes,
    /// which always arrive "from a nested scope"); every scope's `inherits`
    /// gate applies on the way out, skipped or not.
    fn walk(
        &self,
        start: usize,
        start_is_innermost: bool,
        name: &str,
        namespaces: &[String],
        p: usize,
    ) -> Option<BindingId> {
        // Lookup keys are allocated once up front — the name/namespaces do
        // not change during the walk, only the active subset shrinks.
        let mut active: Vec<NameKey> = namespaces
            .iter()
            .map(|ns| (name.to_string(), ns.clone()))
            .collect();
        let mut current = Some(start);
        let mut innermost = start_is_innermost;
        while let Some(scope) = current {
            if innermost || self.scopes[scope].visible_to_nested {
                for key in &active {
                    if let Some(id) = self.select_in_scope(scope, key, p) {
                        return Some(id);
                    }
                }
            }
            match &self.scopes[scope].inherits {
                Inherits::All => {}
                Inherits::None => return None,
                Inherits::Namespaces(kept) => {
                    active.retain(|(_, ns)| kept.contains(ns));
                    if active.is_empty() {
                        return None;
                    }
                }
            }
            current = self.scopes[scope].parent;
            innermost = false;
        }
        None
    }

    /// Resolve one reference to its binding, if any. Unresolved is a
    /// first-class outcome.
    fn resolve_reference(&self, reference: &ReferenceCapture) -> Option<BindingId> {
        let p = reference.byte_range.start;
        let start = self.innermost_scope_at(p);
        self.walk(start, true, &reference.name, &reference.namespaces, p)
    }

    /// The binding the cursor identifies: a definition site directly, or a
    /// resolved reference. `byte` may be anywhere within the node.
    pub(crate) fn binding_at(&self, byte: usize) -> Option<BindingId> {
        if let Some((id, _)) = self.site_containing(byte) {
            return Some(id);
        }
        self.reference_containing(byte)
            .and_then(|r| self.resolve_reference(r))
    }

    /// The definition range navigation reports for a cursor at `byte`:
    /// on a definition node, that node; on a resolved reference, the site
    /// chosen by the visibility rules at the reference's position.
    pub(crate) fn definition_range_at(&self, byte: usize) -> Option<Range<usize>> {
        if let Some((_, site)) = self.site_containing(byte) {
            return Some(site.byte_range.clone());
        }
        let reference = self.reference_containing(byte)?;
        let binding = self.resolve_reference(reference)?;
        self.site_for(binding, reference.byte_range.start)
            .map(|s| s.byte_range.clone())
    }

    /// All definition sites of a binding, in registration order.
    pub(crate) fn sites(&self, id: BindingId) -> &[Site] {
        &self.bindings[id].sites
    }

    /// Byte ranges of every reference in the layer resolving to `id`.
    pub(crate) fn references_resolving_to(&self, id: BindingId) -> Vec<Range<usize>> {
        self.references
            .iter()
            .filter(|r| self.resolve_reference(r) == Some(id))
            .map(|r| r.byte_range.clone())
            .collect()
    }

    /// The identifier node the cursor is on, when it identifies a resolvable
    /// binding: a definition site's own range, or a reference's range when
    /// the reference resolves. This is `prepareRename`'s gate — nothing is
    /// offered for an identifier the resolver cannot ground.
    pub(crate) fn resolvable_identifier_at(&self, byte: usize) -> Option<Range<usize>> {
        if let Some((_, site)) = self.site_containing(byte) {
            return Some(site.byte_range.clone());
        }
        let reference = self.reference_containing(byte)?;
        self.resolve_reference(reference)?;
        Some(reference.byte_range.clone())
    }

    /// The definition site reported for `binding` as seen from position `p`:
    /// among sites visible at `p`, the one whose node starts latest at or
    /// before `p`; when every visible site starts after `p` (hoisting), the
    /// earliest one.
    fn site_for(&self, binding: BindingId, p: usize) -> Option<&Site> {
        let visible: Vec<&Site> = self.bindings[binding]
            .sites
            .iter()
            .filter(|s| s.visibility_start <= p)
            .collect();
        visible
            .iter()
            .filter(|s| s.byte_range.start <= p)
            .max_by_key(|s| s.byte_range.start)
            .or_else(|| visible.iter().min_by_key(|s| s.byte_range.start))
            .copied()
    }

    fn site_containing(&self, byte: usize) -> Option<(BindingId, &Site)> {
        // A node registered as sites of several bindings (e.g. the same node
        // captured twice with `fresh`) identifies the later binding — the one
        // that shadows.
        self.bindings
            .iter()
            .enumerate()
            .rev()
            .find_map(|(id, binding)| {
                binding
                    .sites
                    .iter()
                    .find(|s| s.byte_range.start <= byte && byte < s.byte_range.end)
                    .map(|s| (id, s))
            })
    }

    fn reference_containing(&self, byte: usize) -> Option<&ReferenceCapture> {
        self.references
            .iter()
            .find(|r| r.byte_range.start <= byte && byte < r.byte_range.end)
    }
}

/// Build the scope tree: dedupe captures by byte range (merging properties),
/// prepend the implicit root scope, and nest by containment.
fn build_scope_tree(collection: &Collection) -> Vec<Scope> {
    let mut root = Scope {
        byte_range: collection.root_range.clone(),
        label: None,
        inherits: Inherits::All,
        visible_to_nested: true,
        parent: None,
        depth: 0,
        bindings: HashMap::new(),
        redirects: HashMap::new(),
    };

    // Dedupe by range; a node captured as @scope by several patterns merges:
    // first label wins, first non-default inherits wins, visible-to-nested
    // ANDs (any pattern hiding the scope hides it).
    let mut deduped: Vec<Scope> = Vec::new();
    for capture in &collection.scopes {
        let merge_into = if capture.byte_range == root.byte_range {
            &mut root
        } else if let Some(existing) = deduped
            .iter_mut()
            .find(|s| s.byte_range == capture.byte_range)
        {
            existing
        } else {
            deduped.push(Scope {
                byte_range: capture.byte_range.clone(),
                label: capture.label.clone(),
                inherits: capture.inherits.clone(),
                visible_to_nested: capture.visible_to_nested,
                parent: None,
                depth: 0,
                bindings: HashMap::new(),
                redirects: HashMap::new(),
            });
            continue;
        };
        if merge_into.label.is_none() {
            merge_into.label = capture.label.clone();
        }
        if merge_into.inherits == Inherits::All {
            merge_into.inherits = capture.inherits.clone();
        }
        merge_into.visible_to_nested &= capture.visible_to_nested;
    }

    // Sort outer-first so each scope's parent precedes it; tree-sitter node
    // ranges nest or are disjoint, so the innermost strictly-containing
    // predecessor is the parent.
    deduped.sort_by(|a, b| {
        a.byte_range
            .start
            .cmp(&b.byte_range.start)
            .then(b.byte_range.end.cmp(&a.byte_range.end))
    });

    let mut scopes = vec![root];
    for mut scope in deduped {
        let parent = scopes
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, s)| {
                s.byte_range.start <= scope.byte_range.start
                    && scope.byte_range.end <= s.byte_range.end
            })
            .max_by_key(|(_, s)| s.depth)
            .map(|(i, _)| i)
            .unwrap_or(0);
        scope.parent = Some(parent);
        scope.depth = scopes[parent].depth + 1;
        scopes.push(scope);
    }
    scopes
}

#[cfg(test)]
mod tests {
    use super::super::collect::collect;
    use super::*;
    use tree_sitter::{Parser, Query};

    fn model_for(text: &str, query: &str) -> BindingsModel {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(text, None).unwrap();
        let query = Query::new(&tree_sitter_rust::LANGUAGE.into(), query).unwrap();
        BindingsModel::build(collect(text, tree.root_node(), &query))
    }

    /// Byte offset of the `n`th occurrence (0-based) of `needle` in `text`.
    fn nth(text: &str, needle: &str, n: usize) -> usize {
        let mut from = 0;
        for _ in 0..=n {
            let at = text[from..].find(needle).expect("needle occurrence") + from;
            from = at + 1;
            if from > text.len() {
                panic!("needle occurrence");
            }
        }
        from - 1
    }

    const BASIC: &str = r#"
        (block) @scope
        (let_declaration pattern: (identifier) @definition)
        (identifier) @reference
    "#;

    #[test]
    fn resolves_local_definition_from_nested_scope() {
        let text = "fn main() { let x = 1; { x; } }";
        let model = model_for(text, BASIC);
        let def_at = nth(text, "x", 0);
        let use_at = nth(text, "x", 1);
        assert_eq!(model.definition_range_at(use_at), Some(def_at..def_at + 1));
    }

    #[test]
    fn unresolved_is_first_class() {
        let text = "fn main() { let x = 1; y; }";
        let model = model_for(text, BASIC);
        assert_eq!(model.definition_range_at(nth(text, "y", 0)), None);
        assert_eq!(model.binding_at(nth(text, "y", 0)), None);
    }

    #[test]
    fn inner_scope_shadows_outer() {
        let text = "fn main() { let x = 1; { let x = 2; x; } }";
        let model = model_for(text, BASIC);
        let inner_def = nth(text, "x", 1);
        let use_at = nth(text, "x", 2);
        assert_eq!(
            model.definition_range_at(use_at),
            Some(inner_def..inner_def + 1)
        );
    }

    #[test]
    fn cursor_on_definition_identifies_its_binding() {
        let text = "fn main() { let x = 1; x; }";
        let model = model_for(text, BASIC);
        let def_at = nth(text, "x", 0);
        assert_eq!(model.definition_range_at(def_at), Some(def_at..def_at + 1));
        let binding = model.binding_at(def_at).unwrap();
        assert_eq!(
            model.references_resolving_to(binding),
            vec![nth(text, "x", 1)..nth(text, "x", 1) + 1]
        );
    }

    #[test]
    fn scope_visibility_hoists_and_reports_earliest_site() {
        // Python-assignment model: visible in the whole scope, so a
        // pre-definition reference binds locally (UnboundLocalError shape).
        let text = "fn main() { x; let x = 1; }";
        let model = model_for(text, BASIC);
        let use_at = nth(text, "x", 0);
        let def_at = nth(text, "x", 1);
        assert_eq!(model.definition_range_at(use_at), Some(def_at..def_at + 1));
    }

    const AFTER: &str = r#"
        (block) @scope
        ((let_declaration pattern: (identifier) @definition) @_decl
         (#set! definition.visibility "after"))
        (identifier) @reference
    "#;

    #[test]
    fn after_visibility_lets_initializer_read_outer_binding() {
        // Lua's `local x = x`: the right-hand x precedes the match end and
        // resolves outward.
        let text = "fn main() { let x = 1; { let x = x; } }";
        let model = model_for(text, AFTER);
        let outer_def = nth(text, "x", 0);
        let rhs = nth(text, "x", 2);
        assert_eq!(
            model.definition_range_at(rhs),
            Some(outer_def..outer_def + 1)
        );
    }

    #[test]
    fn after_visibility_pre_declaration_reference_is_unresolved() {
        let text = "fn main() { x; let x = 1; }";
        let model = model_for(text, AFTER);
        assert_eq!(model.definition_range_at(nth(text, "x", 0)), None);
    }

    const DECLARATION: &str = r#"
        (function_item) @scope
        ((function_item name: (identifier) @definition)
         (#set! definition.visibility "declaration"))
        (identifier) @reference
    "#;

    #[test]
    fn declaration_visibility_allows_recursion_but_not_earlier_use() {
        // Lua's `local function f` shape: visible in its own body, invisible
        // above the declaration.
        let text = "fn a() { rec(); } fn rec() { rec(); }";
        let model = model_for(text, DECLARATION);
        let early_use = nth(text, "rec", 0);
        let decl = nth(text, "rec", 1);
        let recursive_use = nth(text, "rec", 2);
        assert_eq!(model.definition_range_at(early_use), None);
        assert_eq!(
            model.definition_range_at(recursive_use),
            Some(decl..decl + 3)
        );
    }

    #[test]
    fn merge_rebind_coalesces_reassignment() {
        let text = "fn main() { let x = 1; x; let x = 2; x; }";
        let model = model_for(text, BASIC);
        let def1 = nth(text, "x", 0);
        let use1 = nth(text, "x", 1);
        let def2 = nth(text, "x", 2);
        let use2 = nth(text, "x", 3);

        // One binding, two sites: both references identify it.
        let binding = model.binding_at(use1).unwrap();
        assert_eq!(model.binding_at(use2), Some(binding));
        assert_eq!(model.sites(binding).len(), 2);

        // Each reference reports the nearest preceding site.
        assert_eq!(model.definition_range_at(use1), Some(def1..def1 + 1));
        assert_eq!(model.definition_range_at(use2), Some(def2..def2 + 1));

        // references/rename span every site of the name.
        assert_eq!(model.references_resolving_to(binding).len(), 2);
    }

    const FRESH_AFTER: &str = r#"
        (block) @scope
        ((let_declaration pattern: (identifier) @definition) @_decl
         (#set! definition.visibility "after")
         (#set! definition.rebind "fresh"))
        (identifier) @reference
    "#;

    #[test]
    fn fresh_rebind_keeps_shadows_distinct() {
        // Rust/ML `let x = x + 1`: reads the prior x, but references never
        // cross the shadow boundary.
        let text = "fn main() { let x = 1; x; let x = x; x; }";
        let model = model_for(text, FRESH_AFTER);
        let use1 = nth(text, "x", 1);
        let rhs = nth(text, "x", 3);
        let use2 = nth(text, "x", 4);

        let first = model.binding_at(use1).unwrap();
        let second = model.binding_at(use2).unwrap();
        assert_ne!(first, second);

        // The shadowing let's right-hand side still reads the first binding.
        assert_eq!(model.binding_at(rhs), Some(first));

        assert_eq!(model.references_resolving_to(first).len(), 2); // use1 + rhs
        assert_eq!(model.references_resolving_to(second).len(), 1); // use2
    }

    const PARENT_LIFT: &str = r#"
        (function_item) @scope
        ((function_item name: (identifier) @definition)
         (#set! definition.scope "parent"))
        (identifier) @reference
    "#;

    #[test]
    fn parent_lift_registers_function_name_outside_its_scope() {
        let text = "fn outer() { fn inner() {} inner(); }";
        let model = model_for(text, PARENT_LIFT);
        let decl = nth(text, "inner", 0);
        let call = nth(text, "inner", 1);
        assert_eq!(model.definition_range_at(call), Some(decl..decl + 5));
    }

    #[test]
    fn parent_lift_clamps_to_root() {
        let text = "fn top() {} fn main() { top(); }";
        let model = model_for(text, PARENT_LIFT);
        let decl = nth(text, "top", 0);
        let call = nth(text, "top", 1);
        assert_eq!(model.definition_range_at(call), Some(decl..decl + 3));
    }

    const GLOBAL_LIFT: &str = r#"
        (block) @scope
        ((let_declaration pattern: (identifier) @definition)
         (#set! definition.scope "global"))
        (identifier) @reference
    "#;

    #[test]
    fn global_lift_registers_at_layer_root() {
        // The dynamic-scoping approximation: every reference in the document
        // resolves to the textual definition site.
        let text = "fn a() { let g = 1; } fn b() { g; }";
        let model = model_for(text, GLOBAL_LIFT);
        let def = nth(text, "g", 0);
        let use_elsewhere = nth(text, "g", 1);
        assert_eq!(model.definition_range_at(use_elsewhere), Some(def..def + 1));
    }

    const BRANCH_TARGET: &str = r#"
        (block) @scope
        ((if_expression
           condition: (let_condition pattern: (identifier) @definition)
           consequence: (block) @scope.then)
         (#set! definition.scope "then"))
        (identifier) @reference
    "#;

    #[test]
    fn label_target_binds_into_branch_scope_only() {
        // `if let` shape: the pattern's name lands in the then-branch alone;
        // the definition node itself sits outside the scope it registers in.
        let text = "fn main() { if let v = w { v; } else { v; } }";
        let model = model_for(text, BRANCH_TARGET);
        let def = nth(text, "v", 0);
        let then_use = nth(text, "v", 1);
        let else_use = nth(text, "v", 2);
        assert_eq!(model.definition_range_at(then_use), Some(def..def + 1));
        assert_eq!(model.definition_range_at(else_use), None);
    }

    const NEAREST: &str = r#"
        ((function_item) @scope.function)
        (block) @scope
        ((let_declaration pattern: (identifier) @definition)
         (#set! definition.scope "nearest:function"))
        (identifier) @reference
    "#;

    #[test]
    fn nearest_label_hoists_to_enclosing_function() {
        // JS `var` shape: a declaration deep in nested blocks belongs to the
        // enclosing function.
        let text = "fn f() { { let x = 1; } x; }";
        let model = model_for(text, NEAREST);
        let def = nth(text, "x", 0);
        let use_after_block = nth(text, "x", 1);
        assert_eq!(
            model.definition_range_at(use_after_block),
            Some(def..def + 1)
        );
    }

    const INHERITS_FALSE: &str = r#"
        ((function_item) @scope (#set! scope.inherits "false"))
        (let_declaration pattern: (identifier) @definition)
        (identifier) @reference
    "#;

    #[test]
    fn inherits_false_stops_the_walk() {
        // PHP-function shape: inner functions don't capture outer locals.
        let text = "fn outer() { let x = 1; fn inner() { x; } }";
        let model = model_for(text, INHERITS_FALSE);
        let inner_use = nth(text, "x", 1);
        assert_eq!(model.definition_range_at(inner_use), None);
        // ...but a reference in the same scope as the definition resolves.
        let text2 = "fn outer() { let x = 1; x; }";
        let model2 = model_for(text2, INHERITS_FALSE);
        let def = nth(text2, "x", 0);
        assert_eq!(
            model2.definition_range_at(nth(text2, "x", 1)),
            Some(def..def + 1)
        );
    }

    const INHERITS_LIST: &str = r#"
        ((function_item) @scope (#set! scope.inherits "function"))
        ((static_item name: (identifier) @definition)
         (#set! definition.namespace "function"))
        (let_declaration pattern: (identifier) @definition)
        ((identifier) @reference (#set! reference.namespace "function default"))
    "#;

    #[test]
    fn inherits_list_filters_namespaces() {
        // PHP shape: globals in the listed namespaces stay reachable through
        // the function boundary; `default` locals do not.
        let text = "static F: u32 = 0; fn outer() { let x = 1; fn inner() { F; x; } }";
        let model = model_for(text, INHERITS_LIST);
        let f_def = nth(text, "F", 0);
        let f_use = nth(text, "F", 1);
        let x_use = nth(text, "x", 1);
        assert_eq!(model.definition_range_at(f_use), Some(f_def..f_def + 1));
        assert_eq!(model.definition_range_at(x_use), None);
    }

    const CLASS_BODY: &str = r#"
        ((function_item body: (block) @scope)
         (#set! scope.visible-to-nested "false"))
        (block) @scope
        (let_declaration pattern: (identifier) @definition)
        (identifier) @reference
    "#;

    #[test]
    fn visible_to_nested_false_hides_from_nested_scopes_only() {
        // Python-class-body shape: statements in the body see its names;
        // nested scopes skip them.
        let text = "fn main() { let x = 1; x; { x; } }";
        let model = model_for(text, CLASS_BODY);
        let def = nth(text, "x", 0);
        let direct_use = nth(text, "x", 1);
        let nested_use = nth(text, "x", 2);
        assert_eq!(model.definition_range_at(direct_use), Some(def..def + 1));
        assert_eq!(model.definition_range_at(nested_use), None);
    }

    const REDIRECT_GLOBAL: &str = r#"
        ((function_item) @scope)
        ((unary_expression (identifier) @redirect)
         (#set! redirect.target "global"))
        (let_declaration pattern: (identifier) @definition)
        (identifier) @reference
    "#;

    #[test]
    fn redirect_global_routes_definitions_to_layer_root() {
        // Python `global x` shape (the unary minus is the contrived marker):
        // the local assignment registers at the root, so references anywhere
        // resolve to it.
        let text = "fn f() { -x; let x = 1; } fn g() { x; }";
        let model = model_for(text, REDIRECT_GLOBAL);
        let def = nth(text, "x", 1);
        let elsewhere = nth(text, "x", 2);
        assert_eq!(model.definition_range_at(elsewhere), Some(def..def + 1));
    }

    const REDIRECT_NONLOCAL: &str = r#"
        ((function_item) @scope.function)
        ((unary_expression (identifier) @redirect)
         (#set! redirect.target "nearest-binding:function"))
        (let_declaration pattern: (identifier) @definition)
        (identifier) @reference
    "#;

    #[test]
    fn redirect_nearest_binding_skips_scopes_without_the_name() {
        // Python `nonlocal x`: binds to the nearest enclosing function that
        // has the name, skipping intermediate functions that don't.
        let text = "fn a() { let x = 1; fn b() { fn c() { -x; let x = 2; x; } } }";
        let model = model_for(text, REDIRECT_NONLOCAL);
        let outer_def = nth(text, "x", 0);
        let inner_def = nth(text, "x", 2);
        let use_in_c = nth(text, "x", 3);

        // c's definition merged into a's binding: one binding, two sites.
        let binding = model.binding_at(use_in_c).unwrap();
        assert_eq!(model.binding_at(outer_def), Some(binding));
        assert_eq!(model.sites(binding).len(), 2);
        // The reference reports the nearest preceding site (c's own).
        assert_eq!(
            model.definition_range_at(use_in_c),
            Some(inner_def..inner_def + 1)
        );
    }

    #[test]
    fn redirect_nearest_binding_without_qualifying_ancestor_is_silent() {
        let text = "fn a() { fn c() { -y; let y = 2; y; } }";
        let model = model_for(text, REDIRECT_NONLOCAL);
        assert_eq!(model.definition_range_at(nth(text, "y", 2)), None);
    }

    const OUTER_OR_LOCAL: &str = r#"
        (block) @scope
        ((let_declaration pattern: (identifier) @definition)
         (#set! definition.rebind "outer-or-local"))
        (identifier) @reference
    "#;

    #[test]
    fn outer_or_local_writes_outer_binding_when_visible() {
        // Ruby block assignment: writes the outer local when one exists.
        let text = "fn main() { let x = 1; { let x = 2; x; } }";
        let model = model_for(text, OUTER_OR_LOCAL);
        let use_inner = nth(text, "x", 2);
        let binding = model.binding_at(use_inner).unwrap();
        assert_eq!(model.sites(binding).len(), 2);
        assert_eq!(model.binding_at(nth(text, "x", 0)), Some(binding));
    }

    #[test]
    fn outer_or_local_introduces_local_when_no_outer_exists() {
        let text = "fn main() { { let y = 2; y; } y; }";
        let model = model_for(text, OUTER_OR_LOCAL);
        let def = nth(text, "y", 0);
        assert_eq!(
            model.definition_range_at(nth(text, "y", 1)),
            Some(def..def + 1)
        );
        // Outside the block the name does not exist.
        assert_eq!(model.definition_range_at(nth(text, "y", 2)), None);
    }

    const NAMESPACES: &str = r#"
        ((struct_item name: (type_identifier) @definition)
         (#set! definition.namespace "type"))
        (let_declaration pattern: (identifier) @definition)
        ((type_identifier) @reference (#set! reference.namespace "type"))
        (identifier) @reference
    "#;

    #[test]
    fn namespaces_match_by_equality() {
        let text = "struct T; fn f() { let v: T = T; }";
        let model = model_for(text, NAMESPACES);
        let type_def = nth(text, "T", 0);
        let type_use = nth(text, "T", 1); // the annotation, a type_identifier
        let value_use = nth(text, "T", 2); // an identifier expression

        assert_eq!(
            model.definition_range_at(type_use),
            Some(type_def..type_def + 1)
        );
        // An unannotated (default-namespace) reference does not match an
        // annotated definition: silence over a wrong answer.
        assert_eq!(model.definition_range_at(value_use), None);
    }

    #[test]
    fn scope_capture_on_root_range_merges_into_root() {
        // A @scope capturing the whole source_file must not create a second
        // root-sized scope.
        let text = "fn main() { x; }";
        let model = model_for(
            text,
            r#"
            (source_file) @scope
            (identifier) @reference
            "#,
        );
        assert_eq!(model.scopes.len(), 1);
        assert_eq!(model.scopes[0].parent, None);
    }
}
