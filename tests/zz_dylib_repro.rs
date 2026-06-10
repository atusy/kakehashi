//! Issue #348 discriminator: reproduce the crash with the INSTALLED dylibs
//! loaded via libloading (as kakehashi does), without the server.
#![cfg(feature = "e2e")]

use std::path::PathBuf;

fn load(lang: &str) -> (libloading::Library, tree_sitter::Language) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("deps/test/kakehashi/parser")
        .join(format!("{lang}.dylib"));
    let lib = unsafe { libloading::Library::new(&path) }.expect("dlopen");
    let sym = format!("tree_sitter_{lang}");
    let f: libloading::Symbol<unsafe extern "C" fn() -> tree_sitter::Language> =
        unsafe { lib.get(sym.as_bytes()) }.expect("symbol");
    let language = unsafe { f() };
    (lib, language)
}

const TEXT: &str = "# Title\n\n```python\ndef f():\n    pass\n```\n";

#[test]
fn installed_dylibs_inline_then_incremental_reparse() {
    let (_lib_md, md) = load("markdown");
    let (_lib_inline, inline) = load("markdown_inline");
    eprintln!(
        "ABI markdown={} inline={}",
        md.abi_version(),
        inline.abi_version()
    );

    let mut host_parser = tree_sitter::Parser::new();
    host_parser.set_language(&md).unwrap();
    let host_tree = host_parser.parse(TEXT, None).unwrap();

    // Inline layer over the heading content "Title" (bytes 2..7), as the
    // injection machinery would.
    let mut inline_parser = tree_sitter::Parser::new();
    inline_parser.set_language(&inline).unwrap();
    inline_parser
        .set_included_ranges(&[tree_sitter::Range {
            start_byte: 2,
            end_byte: 7,
            start_point: tree_sitter::Point { row: 0, column: 2 },
            end_point: tree_sitter::Point { row: 0, column: 7 },
        }])
        .unwrap();
    let _inline_tree = inline_parser.parse(TEXT, None).unwrap();

    // Edit + incremental host reparse with the SAME parser (pool reuse).
    let edited = format!("{TEXT}\nmore text\n");
    let mut old = host_tree.clone();
    old.edit(&tree_sitter::InputEdit {
        start_byte: TEXT.len(),
        old_end_byte: TEXT.len(),
        new_end_byte: edited.len(),
        start_position: tree_sitter::Point { row: 6, column: 0 },
        old_end_position: tree_sitter::Point { row: 6, column: 0 },
        new_end_position: tree_sitter::Point { row: 8, column: 0 },
    });
    let new_tree = host_parser.parse(&edited, Some(&old)).unwrap();
    eprintln!("reparse ok, root kind={}", new_tree.root_node().kind());
}
