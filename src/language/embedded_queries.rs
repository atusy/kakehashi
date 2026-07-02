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
        "rust" => Some(include_str!("../../assets/queries/rust/bindings.scm")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded rust asset must compile in full against the grammar it
    /// targets — a pattern silently dropped by tolerant compilation would be
    /// a dead rule nobody notices.
    #[test]
    fn rust_bindings_asset_compiles_without_skipped_patterns() {
        let source = embedded_bindings_query("rust").expect("rust asset is embedded");
        tree_sitter::Query::new(&tree_sitter_rust::LANGUAGE.into(), source)
            .expect("embedded rust bindings.scm must compile in full");
    }

    /// Fixture resolution through the real shipped asset: the query is the
    /// language spec, so exercise the idioms it declares.
    mod rust_fixtures {
        use super::*;
        use crate::analysis::bindings::collect::collect;
        use crate::analysis::bindings::model::BindingsModel;

        fn model(text: &str) -> BindingsModel {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(text, None).unwrap();
            let query = tree_sitter::Query::new(
                &tree_sitter_rust::LANGUAGE.into(),
                embedded_bindings_query("rust").unwrap(),
            )
            .unwrap();
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
}
