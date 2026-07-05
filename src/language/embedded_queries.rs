//! Kakehashi-authored query assets embedded at build time.
//!
//! The `bindings.scm` corpus has no upstream source (the vocabulary is
//! kakehashi-owned — lexical-name-resolution ADR, "Asset distribution"), so
//! the assets live in-repo under `assets/queries/<lang>/bindings.scm` and are
//! compiled into the binary. They serve as the fallback behind `searchPaths`:
//! a user-provided file wins, per file, including for `; inherits:` parents.

/// The embedded `bindings.scm` for a language, if kakehashi ships one.
pub(crate) fn embedded_bindings_query(lang_name: &str) -> Option<&'static str> {
    match lang_name {
        "bash" => Some(include_str!("../../assets/queries/bash/bindings.scm")),
        "go" => Some(include_str!("../../assets/queries/go/bindings.scm")),
        "javascript" => Some(include_str!("../../assets/queries/javascript/bindings.scm")),
        "lua" => Some(include_str!("../../assets/queries/lua/bindings.scm")),
        "python" => Some(include_str!("../../assets/queries/python/bindings.scm")),
        "rust" => Some(include_str!("../../assets/queries/rust/bindings.scm")),
        "typescript" => Some(include_str!("../../assets/queries/typescript/bindings.scm")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::bindings::collect::collect;
    use crate::analysis::bindings::model::BindingsModel;

    fn language_of(name: &str) -> tree_sitter::Language {
        match name {
            "bash" => tree_sitter_bash::LANGUAGE.into(),
            "go" => tree_sitter_go::LANGUAGE.into(),
            "javascript" => tree_sitter_javascript::LANGUAGE.into(),
            "lua" => tree_sitter_lua::LANGUAGE.into(),
            "python" => tree_sitter_python::LANGUAGE.into(),
            "rust" => tree_sitter_rust::LANGUAGE.into(),
            "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            other => panic!("no grammar for {other}"),
        }
    }

    /// The asset source with `; inherits:` parents concatenated from the
    /// embedded corpus, mirroring the loader's resolution.
    fn resolved_source(lang: &str) -> String {
        let source = embedded_bindings_query(lang).expect("asset is embedded");
        let mut combined = String::new();
        if let Some(first_line) = source.lines().next()
            && let Some(parents) = first_line.strip_prefix("; inherits:")
        {
            for parent in parents.split(',') {
                combined.push_str(&resolved_source(parent.trim()));
                combined.push('\n');
            }
        }
        combined.push_str(source);
        combined
    }

    /// Every embedded asset must compile in full against the grammar it
    /// targets — a pattern silently dropped by tolerant compilation would be
    /// a dead rule nobody notices. TypeScript is the sanctioned exception:
    /// the inherited JS class pattern is impossible against the TS grammar.
    #[test]
    fn embedded_assets_compile_without_skipped_patterns() {
        for lang in ["bash", "go", "javascript", "lua", "python", "rust"] {
            let source = resolved_source(lang);
            tree_sitter::Query::new(&language_of(lang), &source).unwrap_or_else(|e| {
                panic!("embedded {lang} bindings.scm must compile in full: {e}")
            });
        }
        // TypeScript compiles through the tolerant path: the inherited
        // JavaScript class/parameter patterns are impossible against the TS
        // grammar and drop out, but every TypeScript-authored pattern (after
        // the inherited prefix) must survive.
        let js_lines = resolved_source("javascript").lines().count();
        let source = resolved_source("typescript");
        let parsed = crate::language::query_loader::QueryLoader::parse_query(
            &language_of("typescript"),
            &source,
            true,
        );
        let query = parsed.query.expect("typescript asset must compile");
        assert!(
            query.pattern_count() > 10,
            "typescript corpus is non-trivial"
        );
        let dead_ts_patterns: Vec<_> = parsed
            .skipped
            .iter()
            .filter(|s| s.start_line > js_lines)
            .collect();
        assert!(
            dead_ts_patterns.is_empty(),
            "unexpected dead typescript patterns: {dead_ts_patterns:?}"
        );
    }

    fn model_for(lang: &str, text: &str) -> BindingsModel {
        let language = language_of(lang);
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(text, None).unwrap();
        let source = resolved_source(lang);
        let parsed =
            crate::language::query_loader::QueryLoader::parse_query(&language, &source, true);
        let query = parsed.query.expect("asset compiles");
        BindingsModel::build(collect(text, tree.root_node(), &query))
    }

    fn nth(text: &str, needle: &str, n: usize) -> usize {
        let mut from = 0;
        for _ in 0..=n {
            let at = text[from..].find(needle).expect("needle") + from;
            from = at + 1;
        }
        from - 1
    }

    /// `definition_range_at(at(use))` must be the needle at definition index.
    fn assert_resolves(
        model: &BindingsModel,
        text: &str,
        needle: &str,
        use_index: usize,
        def_index: usize,
    ) {
        let use_at = nth(text, needle, use_index);
        let def_at = nth(text, needle, def_index);
        assert_eq!(
            model.definition_range_at(use_at),
            Some(def_at..def_at + needle.len()),
            "{needle}[{use_index}] must resolve to {needle}[{def_index}]"
        );
    }

    /// Fixture resolution through the real shipped assets: the query is the
    /// language spec, so exercise the idioms it declares.
    mod rust_fixtures {
        use super::*;

        fn model(text: &str) -> BindingsModel {
            model_for("rust", text)
        }

        #[test]
        fn let_shadowing_reads_prior_binding() {
            let text = "fn main() { let count = 1; let count = count + 1; count; }";
            let m = model(text);
            let first = nth(text, "count", 0);
            let second = nth(text, "count", 1);
            let rhs = nth(text, "count", 2);
            let last = nth(text, "count", 3);
            // The shadowing initializer reads the first binding...
            assert_eq!(m.definition_range_at(rhs), Some(first..first + 5));
            // ...and the trailing use reads the shadow.
            assert_eq!(m.definition_range_at(last), Some(second..second + 5));
        }

        #[test]
        fn function_names_hoist_and_params_resolve() {
            let text = "fn caller() { helper(2); } fn helper(amount: u32) { amount; }";
            let m = model(text);
            let call = nth(text, "helper", 0);
            let decl = nth(text, "helper", 1);
            assert_eq!(m.definition_range_at(call), Some(decl..decl + 6));

            let param = nth(text, "amount", 0);
            let use_ = nth(text, "amount", 1);
            assert_eq!(m.definition_range_at(use_), Some(param..param + 6));
        }

        #[test]
        fn if_let_binds_then_branch_only() {
            let text = "fn f(w: Option<u32>) { if let Some(v) = w { v; } else { v; } }";
            let m = model(text);
            let def = nth(text, "v)", 0);
            let then_use = nth(text, "v;", 0);
            let else_use = nth(text, "v;", 1);
            assert_eq!(m.definition_range_at(then_use), Some(def..def + 1));
            assert_eq!(m.definition_range_at(else_use), None);
        }

        #[test]
        fn match_arm_binding_stays_in_its_arm_and_variants_stay_free() {
            let text = "fn f(w: Option<u32>) { match w { Some(inner) => { inner; } None => {} } }";
            let m = model(text);
            let def = nth(text, "inner", 0);
            let use_ = nth(text, "inner", 1);
            assert_eq!(m.definition_range_at(use_), Some(def..def + 5));
            // `None` must not be read as a binding (lowercase guard).
            let none_at = nth(text, "None", 0);
            assert_eq!(m.definition_range_at(none_at), None);
        }

        #[test]
        fn type_namespace_separates_types_from_values() {
            let text = "struct Config; fn f(c: Config) { c; }";
            let m = model(text);
            let type_def = nth(text, "Config", 0);
            let type_use = nth(text, "Config", 1);
            assert_eq!(
                m.definition_range_at(type_use),
                Some(type_def..type_def + 6)
            );
        }
    }

    mod lua_fixtures {
        use super::*;

        #[test]
        fn local_x_reads_outer_x() {
            let text = "local acc = 1\ndo\n  local acc = acc\n  print(acc)\nend\n";
            let m = model_for("lua", text);
            // The shadowing initializer reads the outer local...
            assert_resolves(&m, text, "acc", 2, 0);
            // ...and the body use reads the shadow.
            assert_resolves(&m, text, "acc", 3, 1);
        }

        #[test]
        fn local_function_recurses_but_is_invisible_above() {
            let text = "walk()\nlocal function walk(n) return walk(n) end\n";
            let m = model_for("lua", text);
            assert_eq!(
                m.definition_range_at(nth(text, "walk", 0)),
                None,
                "a call above `local function` must not resolve"
            );
            assert_resolves(&m, text, "walk", 2, 1);
        }

        #[test]
        fn local_function_is_callable_after_its_body() {
            let text = "local function f()\n  return 1\nend\nf()\n";
            let m = model_for("lua", text);
            let def = nth(text, "f(", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "f(", 1)),
                Some(def..def + 1),
                "a call after `end` must resolve to the local function"
            );
        }

        #[test]
        fn global_function_is_visible_across_scopes() {
            let text = "local function a() helper() end\nfunction helper() end\n";
            let m = model_for("lua", text);
            assert_resolves(&m, text, "helper", 0, 1);
        }

        #[test]
        fn parameters_and_loop_variables_resolve() {
            let text =
                "local function f(count)\n  for i = 1, count do print(i) end\nend\nprint(i)\n";
            let m = model_for("lua", text);
            assert_resolves(&m, text, "count", 1, 0);
            let loop_def = nth(text, "i =", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "i)", 0)),
                Some(loop_def..loop_def + 1)
            );
            // The loop variable is confined to the loop body.
            assert_eq!(m.definition_range_at(nth(text, "i)", 1)), None);
        }

        #[test]
        fn member_fields_are_not_lexical_references() {
            let text = "local x = 1\nlocal t = {}\nprint(t.x)\n";
            let m = model_for("lua", text);
            let field = nth(text, "x)", 0);
            assert_eq!(
                m.definition_range_at(field),
                None,
                "t.x's field must not resolve to the local x"
            );
            // The table itself is a reference.
            let t_def = nth(text, "t =", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "t.x", 0)),
                Some(t_def..t_def + 1)
            );
        }

        #[test]
        fn assignment_writes_visible_local() {
            let text = "local n = 1\ndo\n  n = 2\nend\nprint(n)\n";
            let m = model_for("lua", text);
            let binding = m.binding_at(nth(text, "n", 0)).unwrap();
            assert_eq!(m.binding_at(nth(text, "n", 1)), Some(binding));
            assert_eq!(m.sites(binding).len(), 2, "assignment merged as a site");
        }
    }

    mod python_fixtures {
        use super::*;

        #[test]
        fn assignments_are_scope_visible_and_params_resolve() {
            let text = "def f(width):\n    total = width\n    return total\n";
            let m = model_for("python", text);
            assert_resolves(&m, text, "width", 1, 0);
            assert_resolves(&m, text, "total", 1, 0);
        }

        #[test]
        fn global_statement_routes_assignment_to_module() {
            let text = "counter = 0\ndef bump():\n    global counter\n    counter = 1\ncounter\n";
            let m = model_for("python", text);
            // One module-level binding: the def inside bump merged into it.
            let module_binding = m.binding_at(nth(text, "counter", 0)).unwrap();
            assert_eq!(m.binding_at(nth(text, "counter", 3)), Some(module_binding));
            assert_eq!(m.sites(module_binding).len(), 2);
        }

        #[test]
        fn nonlocal_binds_to_nearest_enclosing_function_with_the_name() {
            let text = "def outer():\n    state = 1\n    def inner():\n        nonlocal state\n        state = 2\n    return state\n";
            let m = model_for("python", text);
            let outer_binding = m.binding_at(nth(text, "state", 0)).unwrap();
            assert_eq!(m.binding_at(nth(text, "state", 2)), Some(outer_binding));
            assert_eq!(m.sites(outer_binding).len(), 2);
        }

        #[test]
        fn class_body_names_are_invisible_to_methods() {
            let text =
                "class K:\n    size = 1\n    area = size\n    def m(self):\n        return size\n";
            let m = model_for("python", text);
            // Statements directly in the body see class-level names...
            assert_resolves(&m, text, "size", 1, 0);
            // ...but methods do not.
            assert_eq!(m.definition_range_at(nth(text, "size", 2)), None);
        }

        #[test]
        fn attributes_and_keywords_are_not_references() {
            let text = "value = 1\nobj.value\nf(value=2)\n";
            let m = model_for("python", text);
            let attr = nth(text, "value", 1);
            assert_eq!(
                m.definition_range_at(attr),
                None,
                "obj.value is member access"
            );
            let keyword = nth(text, "value", 2);
            assert_eq!(
                m.definition_range_at(keyword),
                None,
                "f(value=...) names a parameter, not the variable"
            );
        }

        #[test]
        fn imports_and_with_aliases_bind() {
            let text = "import os\nfrom pkg import thing as alias\nwith open(p) as fh:\n    fh\nos\nalias\n";
            let m = model_for("python", text);
            assert_resolves(&m, text, "os", 1, 0);
            assert_resolves(&m, text, "alias", 1, 0);
            assert_resolves(&m, text, "fh", 1, 0);
        }

        #[test]
        fn comprehension_variable_is_confined() {
            let text = "rows = [item for item in xs]\nitem\n";
            let m = model_for("python", text);
            assert_resolves(&m, text, "item", 0, 1);
            assert_eq!(m.definition_range_at(nth(text, "item", 2)), None);
        }
    }

    mod javascript_fixtures {
        use super::*;

        #[test]
        fn var_hoists_to_function_let_stays_in_block() {
            let text = "function f() { { var v = 1; let l = 2; } v; l; }";
            let m = model_for("javascript", text);
            let v_def = nth(text, "v =", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "v;", 0)),
                Some(v_def..v_def + 1),
                "var must hoist out of the block to the function"
            );
            assert_eq!(
                m.definition_range_at(nth(text, "l;", 0)),
                None,
                "let must stay confined to its block"
            );
        }

        #[test]
        fn function_declarations_hoist() {
            let text = "before();\nfunction before() {}\n";
            let m = model_for("javascript", text);
            assert_resolves(&m, text, "before", 0, 1);
        }

        #[test]
        fn classes_are_not_hoisted() {
            let text = "new Widget();\nclass Widget {}\nnew Widget();\n";
            let m = model_for("javascript", text);
            assert_eq!(m.definition_range_at(nth(text, "Widget", 0)), None);
            assert_resolves(&m, text, "Widget", 2, 1);
        }

        #[test]
        fn destructuring_imports_catch_and_shorthand() {
            let text = "import def1, {a as b} from 'm';\nconst {c, d: e} = o;\ntry {} catch (err) { err; }\n({c});\nb; e2e(e); def1;\n";
            let m = model_for("javascript", text);
            assert_resolves(&m, text, "err", 1, 0);
            // The import alias binds; the last-line uses resolve to it.
            let alias_def = nth(text, "b}", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "b;", 0)),
                Some(alias_def..alias_def + 1)
            );
            // Renamed destructuring `{d: e}` binds e.
            let e_def = nth(text, "e}", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "(e)", 0) + 1),
                Some(e_def..e_def + 1)
            );
            assert_resolves(&m, text, "def1", 1, 0);
            // Object shorthand `{c}` reads the destructured c.
            let c_def = nth(text, "c,", 0);
            assert_eq!(
                m.definition_range_at(nth(text, "c}", 0)),
                Some(c_def..c_def + 1)
            );
        }

        #[test]
        fn property_names_are_not_references() {
            let text = "const prop = 1;\nobj.prop;\n";
            let m = model_for("javascript", text);
            assert_eq!(m.definition_range_at(nth(text, "prop", 1)), None);
        }
    }

    mod typescript_fixtures {
        use super::*;

        #[test]
        fn inherited_javascript_rules_apply() {
            let text = "function f(size: number) { return size; }";
            let m = model_for("typescript", text);
            assert_resolves(&m, text, "size", 1, 0);
        }

        #[test]
        fn type_namespace_definitions_resolve_type_references() {
            let text = "interface Shape {}\nlet s: Shape;\n";
            let m = model_for("typescript", text);
            assert_resolves(&m, text, "Shape", 1, 0);
        }

        #[test]
        fn classes_are_both_type_and_value() {
            let text = "class Box {}\nlet b: Box = new Box();\n";
            let m = model_for("typescript", text);
            assert_resolves(&m, text, "Box", 1, 0);
            assert_resolves(&m, text, "Box", 2, 0);
        }
    }

    mod bash_fixtures {
        use super::*;

        #[test]
        fn variables_flatten_to_the_document() {
            let text = "greeting=hello\nsay() {\n  echo \"$greeting\"\n}\n";
            let m = model_for("bash", text);
            assert_resolves(&m, text, "greeting", 1, 0);
        }

        #[test]
        fn function_calls_resolve_in_their_own_namespace() {
            let text = "deploy=1\ndeploy() { :; }\ndeploy\n";
            let m = model_for("bash", text);
            // The bare command resolves to the function, not the variable.
            assert_resolves(&m, text, "deploy", 2, 1);
            // And $deploy reads the variable.
            let text2 = "deploy=1\ndeploy() { :; }\necho \"$deploy\"\n";
            let m2 = model_for("bash", text2);
            assert_resolves(&m2, text2, "deploy", 2, 0);
        }

        #[test]
        fn for_variable_binds() {
            let text = "for item in a b; do echo \"$item\"; done\n";
            let m = model_for("bash", text);
            assert_resolves(&m, text, "item", 1, 0);
        }
    }

    mod go_fixtures {
        use super::*;

        #[test]
        fn short_var_declarations_are_sequential() {
            let text = "package m\nfunc f() {\n\ttotal := 1\n\ttotal = total + 1\n\t{ total := 2; _ = total }\n}\n";
            let m = model_for("go", text);
            assert_resolves(&m, text, "total", 1, 0);
            // The nested := is a new block-local declaration.
            assert_resolves(&m, text, "total", 4, 3);
        }

        #[test]
        fn package_level_names_are_order_independent() {
            let text = "package m\nfunc f() Widget { return maker() }\ntype Widget struct{}\nfunc maker() Widget { return Widget{} }\n";
            let m = model_for("go", text);
            assert_resolves(&m, text, "Widget", 0, 1);
            assert_resolves(&m, text, "maker", 0, 1);
        }

        #[test]
        fn params_receivers_and_selectors() {
            let text =
                "package m\nfunc f(count int) int { return count }\nfunc g(o O) { o.count = 1 }\n";
            let m = model_for("go", text);
            assert_resolves(&m, text, "count", 1, 0);
            // Selector fields are not lexical references.
            assert_eq!(m.definition_range_at(nth(text, "count", 2)), None);
        }

        #[test]
        fn if_init_binding_is_scoped_to_the_if() {
            let text =
                "package m\nfunc f(a int) {\n\tif w := a; w > 0 {\n\t\t_ = w\n\t}\n\t_ = w\n}\n";
            let m = model_for("go", text);
            assert_resolves(&m, text, "w", 1, 0);
            assert_resolves(&m, text, "w", 2, 0);
            assert_eq!(m.definition_range_at(nth(text, "w", 3)), None);
        }
    }
}
