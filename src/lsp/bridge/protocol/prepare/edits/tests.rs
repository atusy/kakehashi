use super::super::{
    LineMap, PrepareResult, PreparedDocument, PreparedMap, SegmentKind, VirtualLayout,
    apply_prepare_result,
};
use super::*;
use serde_json::json;
use tower_lsp_server::ls_types::Position;

impl PreparedMap {
    /// One edit keeping its extent.
    fn map_one_with_extent(&self, edit: &TextEdit) -> Option<TextEdit> {
        self.map_edits(std::slice::from_ref(edit), EditMode::Extent)
            .map(|mut edits| edits.remove(0))
    }
}

/// How many cases a property defaulting to `cases` runs: as many, or
/// `KAKEHASHI_PROPTEST_CASES`.
fn property_cases(cases: u32) -> u32 {
    std::env::var("KAKEHASHI_PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(cases)
}

/// A property's runner over `cases` cases, from a fixed seed: a run is
/// repeatable, and a failure it finds found again. `PROPTEST_RNG_SEED` picks
/// another seed, `KAKEHASHI_PROPTEST_RANDOM` set a random one, and
/// `KAKEHASHI_PROPTEST_CASES` another number of cases.
fn property_runner(cases: u32) -> proptest::test_runner::TestRunner {
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    let cases = property_cases(cases);
    let rng_seed = if std::env::var_os("KAKEHASHI_PROPTEST_RANDOM").is_some() {
        RngSeed::Random
    } else if std::env::var_os("PROPTEST_RNG_SEED").is_some() {
        Config::default().rng_seed
    } else {
        RngSeed::Fixed(0x6b61_6b65)
    };
    TestRunner::new(Config {
        cases,
        failure_persistence: None,
        rng_seed,
        ..Config::default()
    })
}

fn result(value: serde_json::Value) -> Option<PrepareResult> {
    serde_json::from_value(value).unwrap()
}

fn pos(line: u32, character: u32) -> Position {
    Position::new(line, character)
}

fn edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> TextEdit {
    TextEdit {
        range: LspRange::new(pos(start.0, start.1), pos(end.0, end.1)),
        new_text: new_text.to_string(),
    }
}

/// `text` with `edits` applied (a column past a line's end meaning its end).
pub(super) fn apply_to(text: &str, edits: &[TextEdit]) -> String {
    let lines = LineMap::new(text.to_string());
    let (ranges, order) = sorted_extents(&lines, edits, true).expect("edits apply");
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for index in order {
        output.push_str(&text[cursor..ranges[index].start]);
        output.push_str(&edits[index].new_text);
        cursor = ranges[index].end;
    }
    output.push_str(&text[cursor..]);
    output
}

fn prepare(virtual_text: &str, changes: serde_json::Value) -> PreparedMap {
    let prepared = apply_prepare_result(
        virtual_text,
        &VirtualLayout::single(virtual_text),
        result(json!({"segments": [{"type": "content", "changes": changes}]})),
    )
    .unwrap();
    std::sync::Arc::try_unwrap(prepared.map).unwrap()
}

fn prepare_pieces(
    virtual_text: &str,
    pieces: impl IntoIterator<Item = (SegmentKind, Range<usize>, String)>,
    answer: serde_json::Value,
) -> PreparedDocument {
    apply_prepare_result(
        virtual_text,
        &VirtualLayout::from_pieces(virtual_text, pieces),
        result(answer),
    )
    .unwrap()
}

/// Two lines indented by two spaces, dedented by the peer.
fn dedented() -> (String, PreparedMap) {
    let virtual_text = "  if x:\n    y\n".to_string();
    let map = prepare(
        &virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]),
    );
    (virtual_text, map)
}

fn dedent_by_two(virtual_text: &str) -> PreparedMap {
    let changes: Vec<_> = virtual_text
        .split('\n')
        .enumerate()
        .filter(|(_, line)| line.starts_with("  "))
        .map(|(line, _)| json!({
            "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 2}},
            "newText": ""
        }))
        .collect();
    prepare(virtual_text, json!(changes))
}

/// `\n  a\n  b\n`: the leading blank line deleted, both lines dedented.
fn leading_blank_dedented() -> (String, PreparedMap) {
    let virtual_text = "\n  a\n  b\n".to_string();
    let map = prepare(
        &virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    (virtual_text, map)
}

/// `  a\n  \n\n`: dedented, the closing blank lines deleted.
fn trailing_blank_dedented() -> (String, PreparedMap) {
    let virtual_text = "  a\n  \n\n".to_string();
    let map = prepare(
        &virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
        ]),
    );
    (virtual_text, map)
}

/// `x = ${a}\nprint(x)\n`, the interpolation prepared as `None`.
fn interpolated() -> (String, PreparedMap) {
    let virtual_text = "x =     \nprint(x)\n".to_string();
    let prepared = prepare_pieces(
        &virtual_text,
        [
            (SegmentKind::Content, 0..4, String::new()),
            (SegmentKind::Gap, 4..8, "${a}".to_string()),
            (SegmentKind::Content, 8..18, String::new()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}]}),
    );
    (
        virtual_text,
        std::sync::Arc::try_unwrap(prepared.map).unwrap(),
    )
}

/// `ab${x}cd` with the interpolation (a gap) replaced by nothing.
fn emptied_gap() -> (String, PreparedMap) {
    let virtual_text = "ab    cd".to_string();
    let prepared = prepare_pieces(
        &virtual_text,
        [
            (SegmentKind::Content, 0..2, String::new()),
            (SegmentKind::Gap, 2..6, "${x}".to_string()),
            (SegmentKind::Content, 6..8, String::new()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}]}),
    );
    assert_eq!(prepared.text, "abcd");
    (
        virtual_text,
        std::sync::Arc::try_unwrap(prepared.map).unwrap(),
    )
}

#[test]
fn edits_touching_where_one_bears_on_the_other_are_refused() {
    // How each regains a removed indent depends on the other's text: two
    // insertions at one place, or an edit ending where another starts.
    let (_, map) = dedented();
    for edits in [
        [edit((0, 1), (0, 1), "\n"), edit((0, 1), (0, 1), "b")],
        [edit((0, 0), (0, 0), "a"), edit((0, 0), (0, 2), "if")],
        [edit((0, 0), (1, 0), "z\n"), edit((1, 0), (2, 0), "")],
        [edit((1, 0), (1, 1), "w"), edit((0, 0), (1, 0), "")],
        // Lines inserted before a change taking the line's indent along.
        [edit((1, 0), (1, 0), "w\n"), edit((1, 0), (2, 0), "")],
    ] {
        assert_eq!(map.map_edits(&edits, EditMode::Extent), None, "{edits:?}");
    }
    // Apart, they map.
    let edits = [edit((0, 0), (0, 2), "if"), edit((1, 0), (1, 1), "w")];
    assert!(map.map_edits(&edits, EditMode::Extent).is_some());
}

#[test]
fn lines_inserted_where_a_placeholder_breaks_a_host_line_touch_nothing() {
    // `  a${x}\n`, the interpolation shown as `x\n`: P's line after it is
    // the rest of the host line, no line start of the host's.
    let virtual_text = "  a    \n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..3, String::new()),
            (SegmentKind::Gap, 3..7, "${x}".to_string()),
            (SegmentKind::Content, 7..8, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": "x\n"},
            {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "ax\n\n");
    let edits = [edit((1, 0), (1, 0), "\n"), edit((1, 0), (2, 0), "z\n")];
    assert_eq!(prepared.map.map_edits(&edits, EditMode::Extent), None);
}

#[test]
fn lines_inserted_before_an_edit_on_the_same_line_map() {
    // A completion rewriting `pa` on the region's first line with an import
    // added above it.
    let virtual_text = "  pa\n  x\n";
    let map = dedent_by_two(virtual_text);
    let edits = [
        edit((0, 0), (0, 2), "Path"),
        edit((0, 0), (0, 0), "from pathlib import Path\n"),
    ];
    let mapped = map.map_edits(&edits, EditMode::Extent).unwrap();
    assert_eq!(
        apply_to(virtual_text, &mapped),
        "  from pathlib import Path\n  Path\n  x\n"
    );
    // Two imports at one place, in the order sent.
    let edits = [
        edit((0, 0), (0, 0), "import a\n"),
        edit((0, 0), (0, 0), "import b\n"),
    ];
    let mapped = map.map_edits(&edits, EditMode::Extent).unwrap();
    assert_eq!(
        apply_to(virtual_text, &mapped),
        "  import a\n  import b\n  pa\n  x\n"
    );
}

#[test]
fn a_reversed_edit_past_the_end_is_refused() {
    let map = prepare("a", json!([]));
    assert_eq!(map.map_one_with_extent(&edit((2, 0), (1, 0), "X")), None);
}

#[test]
fn an_edit_on_a_line_past_the_prepared_text_is_refused() {
    // P `if x:\n  y\n` has lines 0 to 2 (the last empty).
    let (virtual_text, map) = dedented();
    assert_eq!(
        map.map_one_with_extent(&edit((7, 0), (7, 0), "import x\n")),
        None
    );
    assert_eq!(map.map_one_with_extent(&edit((1, 0), (3, 0), "")), None);
    let appended = map
        .map_one_with_extent(&edit((2, 0), (2, 0), "z\n"))
        .unwrap();
    assert_eq!(
        apply_to(&virtual_text, &[appended]),
        "  if x:\n    y\n  z\n"
    );
    // A column past a line is its end.
    let completed = map.map_one_with_extent(&edit((1, 3), (1, 9), "!")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[completed]), "  if x:\n    y!\n");
}

#[test]
fn edits_overlapping_in_the_prepared_text_are_refused() {
    let (_, map) = dedented();
    assert_eq!(
        map.map_edits(
            &[edit((0, 0), (0, 3), "a"), edit((0, 2), (0, 4), "b")],
            EditMode::Extent
        ),
        None
    );
}

#[test]
fn a_change_keeps_the_line_break_before_a_gap_starting_a_line() {
    // Two combined fenced blocks: the closing fence, the prose and the
    // opening fence between them are a gap starting a line.
    let virtual_text = "foo()\n\n\n\nbar()\n";
    for gap in [
        json!({"type": "gap"}),
        json!({"type": "gap", "content": ""}),
    ] {
        let prepared = prepare_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..6, String::new()),
                (SegmentKind::Gap, 6..9, "```\ntext\n```lua\n".to_string()),
                (SegmentKind::Content, 9..15, String::new()),
            ],
            json!({"segments": [{"type": "content"}, gap, {"type": "content"}]}),
        );
        let map = prepared.map;
        // Joining `foo()` onto the closing fence.
        assert_eq!(map.map_one_with_extent(&edit((0, 5), (1, 0), "")), None);
        assert_eq!(map.map_one_with_extent(&edit((0, 5), (1, 0), ";")), None);
        // Whole lines, or text ending with a line break, keep it.
        assert!(map.map_one_with_extent(&edit((0, 0), (1, 0), "")).is_some());
        assert!(
            map.map_one_with_extent(&edit((0, 0), (1, 0), "baz()\n"))
                .is_some()
        );
        assert!(
            map.map_one_with_extent(&edit((0, 5), (0, 5), ";"))
                .is_some()
        );
    }
}

#[test]
fn edits_keep_deleted_trailing_blank_lines() {
    let (virtual_text, map) = trailing_blank_dedented();
    // A change reaching P's end stops before the deleted lines.
    let changed = map
        .map_one_with_extent(&edit((0, 0), (1, 0), "b\n"))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[changed]), "  b\n  \n\n");
    // An append at P's end goes before them, indented.
    let appended = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "c\n"))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[appended]), "  a\n  c\n  \n\n");
    // A whole-document replacement re-indents under them.
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (1, 0), "if a:\n  b\n"))
        .unwrap();
    assert_eq!(
        apply_to(&virtual_text, &[replaced]),
        "  if a:\n    b\n  \n\n"
    );
}

#[test]
fn edits_keep_a_deleted_leading_blank_line() {
    let (virtual_text, map) = leading_blank_dedented();
    // Replacing the whole document re-indents under the host's line break.
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (2, 0), "if a:\n  b\n"))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[replaced]), "\n  if a:\n    b\n");
    // Replacing P's whole first line restores that line's indent only.
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (1, 0), "c\n"))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[replaced]), "\n  c\n  b\n");
    // A line inserted before P's first goes after the deleted line.
    let inserted = map
        .map_one_with_extent(&edit((0, 0), (0, 0), "z\n"))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[inserted]), "\n  z\n  a\n  b\n");
}

#[test]
fn trailing_blank_lines_go_before_a_closing_indent() {
    // A Nix `''` string ending in a blank line, then the indentation before
    // the closing `''`.
    let virtual_text = "  a\n\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "a\n");
    let changed = map
        .map_one_with_extent(&edit((0, 0), (1, 0), "b\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[changed]), "  b\n\n  ");
}

#[test]
fn a_line_appended_above_a_deleted_whitespace_line_takes_no_tail() {
    // The deleted closing line holds whitespace, and the closing indent
    // after it is deleted too: a line appended at P's end goes above that
    // line, which keeps its own text.
    let virtual_text = "  a\n   \n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "a\n");
    let appended = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "b\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[appended]), "  a\n  b\n   \n  ");
}

#[test]
fn a_deleted_closing_indent_is_no_dedent() {
    // The indent before a closing `''` is usually shallower than the
    // content's, and deleting it removes no indent from a content line: new
    // lines still regain the content's.
    let virtual_text = "\n    a\n\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "a\n");
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (1, 0), "a\nb\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "\n    a\n    b\n\n  ");
    // Without a dedent, a new line gains none either.
    let virtual_text = "  a\n\n\t\t";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    let appended = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "b\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[appended]), "  a\nb\n\n\t\t");
}

#[test]
fn a_whitespace_only_document_keeps_its_dedent() {
    // With no content line, the only line's indent is all a new line can
    // regain.
    let virtual_text = "  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    let inserted = map
        .map_one_with_extent(&edit((0, 0), (0, 0), "if True:\n  pass\nprint(1)"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[inserted]),
        "  if True:\n    pass\n  print(1)"
    );
}

#[test]
fn a_change_at_p_end_must_end_its_line() {
    // The deleted closing lines start a line in V, as P's end does: a change
    // ending there without a line break would join the host's first blank
    // line (its whitespace onto a heredoc terminator, say).
    let virtual_text = "  cat <<'EOF'\n  \n\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "cat <<'EOF'\n");
    for refused in [
        edit((1, 0), (1, 0), "hello\nEOF"),
        edit((0, 0), (1, 0), "x"),
    ] {
        assert_eq!(map.map_one_with_extent(&refused), None, "{refused:?}");
    }
    // Ending its line, or deleting whole lines, is fine.
    let appended = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "EOF\n"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[appended]),
        "  cat <<'EOF'\n  EOF\n  \n\n"
    );
    let deleted = map.map_one_with_extent(&edit((0, 0), (1, 0), "")).unwrap();
    assert_eq!(apply_to(virtual_text, &[deleted]), "  \n\n");
}

#[test]
fn a_blank_last_segment_after_content_takes_no_closing_dedent() {
    // `''a${x}\n\n  ''`: the last segment is blank, but the document has
    // content, so its closing indent is no dedent.
    let virtual_text = "a    \n\n  ";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..5, "${x}".to_string()),
            (SegmentKind::Content, 5..9, String::new()),
        ],
        json!({"segments": [
            {"type": "content"},
            {"type": "gap", "content": "x"},
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}
        ]}),
    );
    assert_eq!(prepared.text, "ax\n");
    let appended = prepared
        .map
        .map_one_with_extent(&edit((1, 0), (1, 0), "b\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[appended]), "a    \nb\n\n  ");
}

#[test]
fn a_blank_last_segment_of_one_line_keeps_its_dedent() {
    // A last segment holding only an indented line of its own (no line
    // break before it in the segment) is no closing indent after content
    // lines: its indent is what new lines there regain.
    let virtual_text = "a\n\n\n  ";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..2, String::new()),
            (SegmentKind::Gap, 2..4, "```\n```py\n".to_string()),
            (SegmentKind::Content, 4..6, String::new()),
        ],
        json!({"segments": [
            {"type": "content"},
            {"type": "gap"},
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]}
        ]}),
    );
    let inserted = prepared
        .map
        .map_one_with_extent(&edit((3, 0), (3, 0), "if True:\n  pass\nprint(1)"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[inserted]),
        "a\n\n\n  if True:\n    pass\n  print(1)"
    );
    // Nor is one after a blank line in it, when no closing lines go.
    let virtual_text = "a\n\n\n\n  ";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..2, String::new()),
            (SegmentKind::Gap, 2..4, "```\n```py\n".to_string()),
            (SegmentKind::Content, 4..7, String::new()),
        ],
        json!({"segments": [
            {"type": "content"},
            {"type": "gap"},
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}
        ]}),
    );
    let inserted = prepared
        .map
        .map_one_with_extent(&edit((4, 0), (4, 0), "if True:\n  pass\nprint(1)"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[inserted]),
        "a\n\n\n\n  if True:\n    pass\n  print(1)"
    );
}

#[test]
fn a_change_fusing_a_cr_and_lf_across_deleted_lines_is_refused() {
    // A CR the change ends with, before a deleted line's LF, or an LF it
    // starts with, after a deleted line's CR, would make one CRLF of two
    // line breaks and drop a host line.
    let map = prepare(
        "a\n\n",
        json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]),
    );
    assert_eq!(map.map_one_with_extent(&edit((1, 0), (1, 0), "b\r")), None);
    let map = prepare(
        "\ra",
        json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]),
    );
    assert_eq!(map.map_one_with_extent(&edit((0, 0), (0, 0), "\nb")), None);
    // Deleting what keeps a hidden CR from a visible LF fuses them too.
    let map = prepare(
        "\ra\n",
        json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]),
    );
    assert_eq!(map.map_one_with_extent(&edit((0, 0), (0, 1), "")), None);
    // A CRLF the edit makes from line breaks P has is the edit's own.
    let map = prepare("a\n", json!([]));
    let crlf = map
        .map_one_with_extent(&edit((0, 1), (0, 1), "\r"))
        .unwrap();
    assert_eq!(apply_to("a\n", &[crlf]), "a\r\n");
}

#[test]
fn a_line_break_the_prepared_text_shows_is_the_servers_to_join() {
    // `a${x}b`, the interpolation's host text ending with a CR, replaced by
    // text ending with a CR too: the server sees that CR.
    let virtual_text = "a \rb";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..3, "x\r".to_string()),
            (SegmentKind::Content, 3..4, String::new()),
        ],
        json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": "y\r"}, {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "ay\rb");
    let mapped = prepared
        .map
        .map_edits(&[edit((1, 0), (1, 0), "\n")], EditMode::Extent)
        .unwrap();
    assert_eq!(apply_to(virtual_text, &mapped), "a \r\nb");
}

#[test]
fn an_emptied_gaps_line_break_stays_hidden_beside_content_showing_one() {
    // `a\r${x\r}b`, the gap emptied: P shows the content's CR, never the
    // gap's, which an LF right after the gap would fuse with.
    let virtual_text = "a\r \rb";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..2, String::new()),
            (SegmentKind::Gap, 2..4, "x\r".to_string()),
            (SegmentKind::Content, 4..5, String::new()),
        ],
        json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "a\rb");
    assert_eq!(
        prepared
            .map
            .map_edits(&[edit((1, 0), (1, 1), "\nc")], EditMode::Extent),
        None
    );
}

#[test]
fn an_emptied_gaps_leading_lf_stays_hidden_beside_content_showing_one() {
    // `a${\nx}\nb`, the gap emptied: P shows the content's LF after `a`,
    // never the gap's, which a CR inserted before the gap would fuse with.
    let virtual_text = "a\n \nb";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..3, "\nx".to_string()),
            (SegmentKind::Content, 3..5, String::new()),
        ],
        json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "a\nb");
    assert_eq!(
        prepared
            .map
            .map_edits(&[edit((0, 1), (0, 1), "\r")], EditMode::Extent),
        None
    );
}

#[test]
fn edits_fusing_a_cr_and_lf_only_together_are_refused_together() {
    // A completion's main edit and an additional one: each alone leaves a
    // character between the hidden CR and the LF, together they leave none.
    let virtual_text = "\rab\n";
    let map = prepare(
        virtual_text,
        json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]),
    );
    let first = edit((0, 0), (0, 1), "");
    let second = edit((0, 1), (0, 2), "");
    for alone in [&first, &second] {
        let mapped = map
            .map_edits(std::slice::from_ref(alone), EditMode::Extent)
            .unwrap();
        assert_eq!(apply_to(virtual_text, &mapped).len(), 3);
    }
    assert_eq!(map.map_edits(&[first, second], EditMode::Extent), None);
}

#[test]
fn an_edit_into_a_gap_is_refused_and_one_beside_it_kept() {
    let (virtual_text, map) = interpolated();
    // Touching the placeholder would edit host-owned text.
    assert_eq!(map.map_one_with_extent(&edit((0, 4), (0, 8), "1")), None);
    assert_eq!(map.map_one_with_extent(&edit((0, 5), (0, 6), "1")), None);
    assert_eq!(
        map.map_one_with_extent(&edit((0, 0), (0, 8), "y = None")),
        None,
        "spanning the placeholder, even rewriting it unchanged"
    );
    // Beside it, the content is the server's.
    let renamed = map.map_one_with_extent(&edit((0, 0), (0, 1), "y")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[renamed]), "y =     \nprint(x)\n");
    let appended = map.map_one_with_extent(&edit((0, 8), (0, 8), "!")).unwrap();
    assert_eq!(
        apply_to(&virtual_text, &[appended]),
        "x =     !\nprint(x)\n"
    );
}

#[test]
fn exact_edit_keeps_its_extent_and_reindents() {
    let (virtual_text, map) = dedented();
    // A completion replacing `y` (P line 1, columns 2..3) keeps its range.
    let completed = map
        .map_one_with_extent(&edit((1, 2), (1, 3), "yes"))
        .unwrap();
    assert_eq!(completed.range, LspRange::new(pos(1, 4), pos(1, 5)));
    // An inserted block gains the host indent on each new line.
    let inserted = map
        .map_one_with_extent(&edit((1, 3), (1, 3), "\n  z"))
        .unwrap();
    assert_eq!(
        apply_to(&virtual_text, &[inserted]),
        "  if x:\n    y\n    z\n"
    );
}

#[test]
fn replacing_the_whole_document_reindents_new_lines() {
    let (virtual_text, map) = dedented();
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (2, 0), "if x:\n  y\n  z\n"))
        .unwrap();
    assert_eq!(
        apply_to(&virtual_text, &[replaced]),
        "  if x:\n    y\n    z\n"
    );
}

#[test]
fn an_edit_joining_lines_drops_the_inner_indent() {
    let (virtual_text, map) = dedented();
    let joined = map.map_one_with_extent(&edit((0, 5), (1, 0), " ")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[joined]), "  if x:   y\n");
}

#[test]
fn an_edit_indenting_a_line_keeps_the_host_indent() {
    let (virtual_text, map) = dedented();
    let indented = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "  "))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[indented]), "  if x:\n      y\n");
}

#[test]
fn a_change_before_an_emptied_gap_keeps_the_gap() {
    let (virtual_text, map) = emptied_gap();
    // Deleting `b` must not reach over the gap's host text.
    let deleted = map.map_one_with_extent(&edit((0, 1), (0, 2), "")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[deleted]), "a    cd");
    let replaced = map.map_one_with_extent(&edit((0, 1), (0, 2), "B")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[replaced]), "aB    cd");
    // An insertion there stays an insertion, before the gap.
    let inserted = map.map_one_with_extent(&edit((0, 2), (0, 2), "!")).unwrap();
    assert_eq!(inserted.range.start, inserted.range.end);
    assert_eq!(apply_to(&virtual_text, &[inserted]), "ab!    cd");
    // A change across its position would remove the host text around it.
    assert_eq!(map.map_one_with_extent(&edit((0, 1), (0, 3), "")), None);
}

#[test]
fn an_undedented_line_stays_unindented() {
    let virtual_text = "  a\nb\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    let replaced = map.map_one_with_extent(&edit((1, 0), (1, 1), "B")).unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  a\nB\n");
}

#[test]
fn replacing_a_dedented_lines_first_word_keeps_its_indent() {
    let (virtual_text, map) = dedented();
    // A completion's replace range over `if` (insert range empty there).
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (0, 2), "while"))
        .unwrap();
    let inserted = map
        .map_one_with_extent(&edit((0, 0), (0, 0), "while"))
        .unwrap();
    assert_eq!(replaced.new_text, inserted.new_text);
    assert_eq!(replaced.range.start, inserted.range.start);
    assert_eq!(apply_to(&virtual_text, &[replaced]), "  while x:\n    y\n");
}

#[test]
fn inserting_on_an_indented_last_line_keeps_one_indent() {
    let virtual_text = "  a\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "a\n");
    let inserted = map.map_one_with_extent(&edit((1, 0), (1, 0), "b")).unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "  a\n  b");
}

#[test]
fn deleting_a_line_keeps_the_next_lines_own_indent() {
    // Lines indented differently, each fully dedented by the peer.
    let virtual_text = "  a\n    b\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
        ]),
    );
    let removed = map.map_one_with_extent(&edit((0, 0), (1, 0), "")).unwrap();
    assert_eq!(apply_to(virtual_text, &[removed]), "    b\n");
}

#[test]
fn a_line_created_where_the_indent_removed_differs_is_refused() {
    let virtual_text = "  a\n    b\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
        ]),
    );
    // Same-line edits still map…
    assert!(
        map.map_one_with_extent(&edit((0, 0), (0, 1), "A"))
            .is_some()
    );
    // …but a new line has no single right indentation.
    assert_eq!(map.map_one_with_extent(&edit((0, 1), (0, 1), "\nc")), None);
}

#[test]
fn blank_lines_created_where_the_indent_removed_differs_map() {
    let virtual_text = "  a\n    b\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
        ]),
    );
    // A blank line needs no indentation, and the existing line after it
    // keeps its own.
    for blank in [edit((1, 0), (1, 0), "\n"), edit((0, 1), (0, 1), "\n")] {
        let inserted = map.map_one_with_extent(&blank).unwrap();
        assert_eq!(
            apply_to(virtual_text, &[inserted]),
            "  a\n\n    b\n",
            "{blank:?}"
        );
    }
    // A created line of text still has no single right indentation.
    for text in [
        edit((1, 0), (1, 0), "x\n"),
        edit((1, 0), (1, 0), "x\ny\n"),
        edit((0, 1), (0, 1), "\nc"),
        edit((1, 0), (1, 0), "\nx\n"),
    ] {
        assert_eq!(map.map_one_with_extent(&text), None, "{text:?}");
    }
}

#[test]
fn inserted_blank_lines_carry_no_indent() {
    let imports = "  import os\n  x = 1\n";
    let map = dedent_by_two(imports);
    let inserted = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "\n"))
        .unwrap();
    assert_eq!(apply_to(imports, &[inserted]), "  import os\n\n  x = 1\n");
}

#[test]
fn a_line_the_peer_left_alone_gains_no_indent() {
    // Only the first line was dedented.
    let virtual_text = "  a\nb\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    let inserted = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "x\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "  a\n  x\nb\n");
}

#[test]
fn removing_a_line_keeps_the_next_lines_indent_once() {
    let virtual_text = "  a\n  b\n";
    let map = dedent_by_two(virtual_text);
    let removed = map.map_one_with_extent(&edit((0, 0), (1, 0), "")).unwrap();
    assert_eq!(apply_to(virtual_text, &[removed]), "  b\n");
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (1, 0), "x\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  b\n");

    let imports = "  import os\n  import sys\n\n\n\n  x = 1\n";
    let map = dedent_by_two(imports);
    let fixed = map.map_one_with_extent(&edit((0, 0), (1, 0), "")).unwrap();
    assert_eq!(apply_to(imports, &[fixed]), "  import sys\n\n\n\n  x = 1\n");

    let blank = "  a\n\n  b\n";
    let map = dedent_by_two(blank);
    let removed = map.map_one_with_extent(&edit((1, 0), (2, 0), "")).unwrap();
    assert_eq!(apply_to(blank, &[removed]), "  a\n  b\n");
}

#[test]
fn an_emptied_gap_beside_a_removed_indent_survives_line_edits() {
    // Gap then indent: `  a\n` + gap (host fences) + `  b\n`, all emptied or
    // dedented.
    let virtual_text = "  a\n\n\n  b\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..4, String::new()),
            (SegmentKind::Gap, 4..6, "```\n```py\n".to_string()),
            (SegmentKind::Content, 6..10, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}]},
            {"type": "gap", "content": ""},
            {"type": "content", "changes": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}]}
        ]}),
    );
    assert_eq!(prepared.text, "a\nb\n");
    let removed = prepared
        .map
        .map_one_with_extent(&edit((0, 0), (1, 0), ""))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[removed]), "\n\n  b\n");

    // Indent then gap: `\n  foo\n  ${x}b\n` with `${x}` emptied.
    let virtual_text = "\n  foo\n      b\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..9, String::new()),
            (SegmentKind::Gap, 9..13, "${x}".to_string()),
            (SegmentKind::Content, 13..15, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": ""},
            {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "\nfoo\nb\n");
    let replaced = prepared
        .map
        .map_one_with_extent(&edit((2, 0), (2, 1), "z"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "\n  foo\n      z\n");
}

#[test]
fn deleting_a_dedented_line_takes_its_indent() {
    let (virtual_text, map) = dedented();
    let deleted = map.map_one_with_extent(&edit((1, 0), (2, 0), "")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[deleted]), "  if x:\n");
    let cleared = map.map_one_with_extent(&edit((1, 0), (1, 3), "")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[cleared]), "  if x:\n\n");
    let replaced = map.map_one_with_extent(&edit((1, 0), (1, 3), "z")).unwrap();
    assert_eq!(apply_to(&virtual_text, &[replaced]), "  if x:\n  z\n");
}

#[test]
fn crlf_dedent_maps_and_reindents_with_crlf() {
    let virtual_text = "  a\r\n  b\r\n";
    let map = dedent_by_two(virtual_text);
    assert_eq!(map.prepared_lines.text, "a\r\nb\r\n");
    let inserted = map
        .map_one_with_extent(&edit((1, 1), (1, 1), "\r\nc"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "  a\r\n  b\r\n  c\r\n");
}

#[test]
fn lone_cr_lines_regain_their_indent() {
    let virtual_text = "  a\r  b";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "a\rb");
    let inserted = map
        .map_one_with_extent(&edit((1, 1), (1, 1), "\rc"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "  a\r  b\r  c");
}

#[test]
fn a_range_reaching_into_a_gap_is_in_it() {
    let (_, map) = interpolated();
    assert!(map.virtual_range_in_gap(LspRange::new(pos(0, 4), pos(0, 8))));
    assert!(map.virtual_range_in_gap(LspRange::new(pos(0, 0), pos(0, 5))));
    assert!(map.virtual_range_in_gap(LspRange::new(pos(0, 6), pos(0, 6))));
    assert!(!map.virtual_range_in_gap(LspRange::new(pos(0, 0), pos(0, 4))));
    assert!(!map.virtual_range_in_gap(LspRange::new(pos(0, 8), pos(1, 2))));
    assert!(!map.virtual_range_in_gap(LspRange::new(pos(0, 4), pos(0, 4))));
}

#[test]
fn a_caret_on_the_first_deleted_closing_line_is_in_removed_indent() {
    let (_, map) = trailing_blank_dedented();
    // V (1, 0) is where the deleted lines start, and P's end maps there, but
    // what the client inserts at a caret there joins the first of those
    // lines, which P does not have.
    assert!(!map.virtual_position_in_removed_indent(pos(0, 3)));
    assert!(map.virtual_position_in_removed_indent(pos(1, 0)));
    assert!(map.virtual_position_in_removed_indent(pos(1, 1)));
    assert!(map.virtual_position_in_removed_indent(pos(2, 0)));
}

#[test]
fn a_caret_at_the_end_of_a_deleted_closing_indent_is_in_removed_indent() {
    // `''\n  a\n\n  ''`'s value ends after `a`: a caret right before the
    // closing `''` stands for P's end, which maps above the blank line.
    let map = prepare(
        "  a\n\n  ",
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    assert!(!map.virtual_position_in_removed_indent(pos(0, 3)));
    assert!(map.virtual_position_in_removed_indent(pos(1, 0)));
    assert!(map.virtual_position_in_removed_indent(pos(2, 0)));
    assert!(map.virtual_position_in_removed_indent(pos(2, 2)));
}

#[test]
fn a_caret_in_a_removed_indent_or_a_deleted_leading_line_is_in_removed_indent() {
    let (_, map) = leading_blank_dedented();
    // P has no position on the deleted line nor inside the indent: a caret
    // there stands in for one at the line's content.
    assert!(map.virtual_position_in_removed_indent(pos(0, 0)));
    assert!(map.virtual_position_in_removed_indent(pos(1, 0)));
    assert!(map.virtual_position_in_removed_indent(pos(1, 1)));
    assert!(!map.virtual_position_in_removed_indent(pos(1, 2)));
    assert!(!map.virtual_position_in_removed_indent(pos(1, 3)));
}

// =============================================================================
// Property: mapped edits keep the host's text and the server's content
// =============================================================================

mod properties {
    use super::super::super::{Run, is_line_start, splits_crlf};
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    use std::cell::Cell;

    /// One line of a generated virtual document.
    #[derive(Debug, Clone)]
    struct LineSpec {
        indent: usize,
        body: usize,
        eol: usize,
        /// 0: content only; 1: an interpolation (a gap) after the body; 2:
        /// the whole line is host text (a gap such as a fence).
        gap: usize,
        /// Whether content follows an interpolation on its line.
        after_gap: bool,
    }

    const INDENTS: [&str; 4] = ["", "  ", "    ", "\t"];
    const BODIES: [&str; 5] = ["", "a", "é", "𝄞x", "b c"];
    const EOLS: [&str; 4] = ["\n", "\r\n", "\r", ""];
    const PLACEHOLDERS: [Option<&str>; 6] = [
        None,
        Some(""),
        Some("1"),
        Some("x\n"),
        Some("\r"),
        Some("  "),
    ];
    const INSERTS: [&str; 10] = [
        "", "z", "\n", "q\n", "\nw", "\r\n", "\r", "é𝄞", "  ", "\n\n",
    ];

    fn line_spec() -> impl Strategy<Value = LineSpec> {
        (
            0..INDENTS.len(),
            0..BODIES.len(),
            0..EOLS.len(),
            0..3usize,
            any::<bool>(),
        )
            .prop_map(|(indent, body, eol, gap, after_gap)| LineSpec {
                indent,
                body,
                eol,
                gap,
                after_gap,
            })
    }

    /// An edit to P: two char-boundary picks, a column overshoot, a text.
    type EditSpec = (u16, u16, bool, usize);

    fn edit_spec() -> impl Strategy<Value = EditSpec> {
        (
            any::<u16>(),
            any::<u16>(),
            prop::bool::weighted(0.1),
            0..INSERTS.len(),
        )
    }

    /// The virtual text and layout `lines` describe.
    fn document(lines: &[LineSpec]) -> (String, VirtualLayout) {
        let mut text = String::new();
        let mut pieces = Vec::new();
        let content = |text: &mut String, piece: &str| {
            let start = text.len();
            text.push_str(piece);
            (SegmentKind::Content, start..text.len(), String::new())
        };
        for (index, line) in lines.iter().enumerate() {
            let last = index + 1 == lines.len();
            // Only the last line may end without a line break.
            let eol = match EOLS[line.eol] {
                "" if !last => "\n",
                eol => eol,
            };
            if line.gap == 2 {
                // Host lines masked by line breaks, as between two fences.
                let start = text.len();
                let eol = if eol.is_empty() { "\n" } else { eol };
                text.push_str(eol);
                pieces.push((SegmentKind::Gap, start..text.len(), format!("```{eol}")));
                continue;
            }
            let head = format!("{}{}", INDENTS[line.indent], BODIES[line.body]);
            pieces.push(content(&mut text, &head));
            if line.gap == 1 {
                let start = text.len();
                text.push_str("    ");
                pieces.push((SegmentKind::Gap, start..text.len(), "${x}".to_string()));
                if line.after_gap {
                    pieces.push(content(&mut text, "y"));
                }
            }
            pieces.push(content(&mut text, eol));
        }
        let layout = VirtualLayout::from_pieces(&text, pieces);
        (text, layout)
    }

    /// An answer for `layout`: per content segment, the leading whitespace
    /// of the lines `dedent` picks and the blank edge lines `edges` picks
    /// deleted; per gap, a placeholder.
    fn answer(
        virtual_text: &str,
        layout: &VirtualLayout,
        dedent: &[bool],
        edges: (bool, bool),
        placeholders: &[usize],
    ) -> serde_json::Value {
        let segments = layout.segments();
        let last = segments.len() - 1;
        let mut picks = dedent.iter().cycle();
        let mut gaps = placeholders.iter().cycle();
        let answered: Vec<serde_json::Value> = segments
            .iter()
            .enumerate()
            .map(|(index, segment)| match segment.kind {
                SegmentKind::Gap => match PLACEHOLDERS[*gaps.next().unwrap_or(&0)] {
                    None => json!({"type": "gap"}),
                    Some(content) => json!({"type": "gap", "content": content}),
                },
                SegmentKind::Content => {
                    let range = segment.virtual_range.clone();
                    let text = &virtual_text[range.clone()];
                    let lines = LineMap::new(text.to_string());
                    let change = |start: usize, end: usize| {
                        json!({"range": {
                            "start": lines.position(start),
                            "end": lines.position(end)
                        }, "newText": ""})
                    };
                    let line_starts: Vec<usize> = (0..text.len())
                        .filter(|&at| {
                            text.is_char_boundary(at)
                                && is_line_start(virtual_text, range.start + at)
                                && !splits_crlf(virtual_text, range.start + at)
                        })
                        .collect();
                    let line_end = |at: usize| {
                        text[at..].find(['\n', '\r']).map_or(text.len(), |end| {
                            let end = at + end;
                            end + if text[end..].starts_with("\r\n") {
                                2
                            } else {
                                1
                            }
                        })
                    };
                    let blank = |at: usize| text[at..line_end(at)].trim().is_empty();
                    let mut changes = Vec::new();
                    let mut deleted_lines = Vec::new();
                    if index == 0 && edges.0 {
                        for &at in &line_starts {
                            let end = line_end(at);
                            if !blank(at) || !text[at..end].contains(['\n', '\r']) {
                                break;
                            }
                            deleted_lines.push(at..end);
                        }
                    }
                    if index == last && edges.1 {
                        let mut trailing = Vec::new();
                        for &at in line_starts.iter().rev() {
                            let end = line_end(at);
                            if !blank(at) {
                                break;
                            }
                            if text[at..end].contains(['\n', '\r']) {
                                trailing.push(at..end);
                            }
                        }
                        deleted_lines.extend(trailing);
                    }
                    for line in &deleted_lines {
                        changes.push(change(line.start, line.end));
                    }
                    for &at in &line_starts {
                        if deleted_lines.iter().any(|line| line.contains(&at)) {
                            continue;
                        }
                        let indent =
                            text[at..].len() - text[at..].trim_start_matches([' ', '\t']).len();
                        if indent > 0 && *picks.next().unwrap_or(&false) {
                            changes.push(change(at, at + indent));
                        }
                    }
                    json!({"type": "content", "changes": changes})
                }
            })
            .collect();
        json!({ "segments": answered })
    }

    /// P edits from their specs.
    fn edits(map: &PreparedMap, specs: &[EditSpec]) -> Vec<TextEdit> {
        let prepared = &map.prepared_lines.text;
        let boundaries: Vec<usize> = (0..=prepared.len())
            .filter(|&at| prepared.is_char_boundary(at))
            .collect();
        specs
            .iter()
            .map(|&(a, b, overshoot, text)| {
                let a = boundaries[a as usize % boundaries.len()];
                let b = boundaries[b as usize % boundaries.len()];
                let start = map.prepared_lines.position(a.min(b));
                let mut end = map.prepared_lines.position(a.max(b));
                if overshoot {
                    end.character += 3;
                }
                TextEdit {
                    range: LspRange::new(start, end),
                    new_text: INSERTS[text].to_string(),
                }
            })
            .collect()
    }

    /// The host-owned runs: text V holds that P does not show as content.
    fn host_owned(map: &PreparedMap) -> Vec<&Run> {
        map.runs
            .iter()
            .enumerate()
            .filter(|(index, run)| {
                matches!(
                    run.kind,
                    RunKind::Gap | RunKind::LeadingLines | RunKind::TrailingLines
                ) || (run.kind == RunKind::Deleted
                    && index.checked_sub(1).is_some_and(|previous| {
                        map.runs[previous].kind == RunKind::TrailingLines
                            && map.runs[previous].virtual_.end == run.virtual_.start
                    }))
            })
            .map(|(_, run)| run)
            .filter(|run| !run.virtual_.is_empty())
            .collect()
    }

    fn strip(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// Where `offset`, outside every edit, lands once `ranges` (with the
    /// lengths of their new texts) are applied: after the edits ending
    /// before it, and those ending at it unless `after` says insertions
    /// there go after it.
    fn shifted(ranges: &[(Range<usize>, usize)], offset: usize, after: bool) -> usize {
        ranges
            .iter()
            .filter(|(range, _)| {
                range.end < offset || (range.end == offset && !(after && range.is_empty()))
            })
            .fold(offset, |at, (range, len)| at + len - range.len())
    }

    fn check(
        virtual_text: &str,
        map: &PreparedMap,
        edits: &[TextEdit],
        mapped: &[TextEdit],
    ) -> Result<(), TestCaseError> {
        let edited = apply_to(virtual_text, mapped);
        let ranges: Vec<(Range<usize>, usize)> = mapped
            .iter()
            .map(|edit| {
                (
                    map.virtual_lines.offset_clamped(edit.range.start)
                        ..map.virtual_lines.offset_clamped(edit.range.end),
                    edit.new_text.len(),
                )
            })
            .collect();
        // (a) Host-owned runs survive byte for byte, still starting a line
        // where they did, and no line break at their edges fuses with one
        // beside it unless V or P already had them meet.
        for run in host_owned(map) {
            let text = &virtual_text[run.virtual_.clone()];
            for (range, _) in &ranges {
                let inside = if range.is_empty() {
                    run.virtual_.start < range.start && range.start < run.virtual_.end
                } else {
                    range.start < run.virtual_.end && run.virtual_.start < range.end
                };
                prop_assert!(!inside, "an edit {range:?} reaches into {run:?}");
            }
            let start = shifted(&ranges, run.virtual_.start, false);
            let end = start + text.len();
            prop_assert_eq!(&edited[start..end], text, "{:?} moved or changed", run);
            if is_line_start(virtual_text, run.virtual_.start)
                && !splits_crlf(virtual_text, run.virtual_.start)
            {
                prop_assert!(
                    is_line_start(&edited, start),
                    "{:?} no longer starts a line",
                    run
                );
            }
            let shown = &map.prepared_lines.text[run.prepared.clone()];
            prop_assert!(
                !splits_crlf(&edited, start)
                    || splits_crlf(virtual_text, run.virtual_.start)
                    || shown.starts_with('\n'),
                "a CR fused with {:?}'s LF",
                run
            );
            prop_assert!(
                !splits_crlf(&edited, end)
                    || splits_crlf(virtual_text, run.virtual_.end)
                    || shown.ends_with('\r'),
                "{:?}'s CR fused with an LF",
                run
            );
        }
        // (c) The edits add or remove as many lines in V as in P.
        let line_breaks = |text: &str| {
            text.matches('\n').count() + text.matches('\r').count() - text.matches("\r\n").count()
        };
        prop_assert_eq!(
            line_breaks(&edited) as isize - line_breaks(virtual_text) as isize,
            line_breaks(&apply_to(&map.prepared_lines.text, edits)) as isize
                - line_breaks(&map.prepared_lines.text) as isize,
            "line count"
        );
        // (b) Whitespace aside, the edited V's content between the gaps is
        // the edited P's between them.
        let prepared = &map.prepared_lines.text;
        let edited_prepared = apply_to(prepared, edits);
        let prepared_ranges: Vec<(Range<usize>, usize)> = edits
            .iter()
            .map(|edit| {
                (
                    map.prepared_lines.offset_clamped(edit.range.start)
                        ..map.prepared_lines.offset_clamped(edit.range.end),
                    edit.new_text.len(),
                )
            })
            .collect();
        let gaps: Vec<&Run> = map
            .runs
            .iter()
            .filter(|run| run.kind == RunKind::Gap)
            .collect();
        let (mut virtual_cursor, mut prepared_cursor) = (0, 0);
        for gap in gaps.iter().map(Some).chain([None]) {
            let (virtual_split, prepared_split) = match gap {
                Some(gap) => (
                    (
                        shifted(&ranges, gap.virtual_.start, false),
                        shifted(&ranges, gap.virtual_.start, false) + gap.virtual_.len(),
                    ),
                    (
                        shifted(&prepared_ranges, gap.prepared.start, false),
                        shifted(&prepared_ranges, gap.prepared.end, !gap.prepared.is_empty()),
                    ),
                ),
                None => (
                    (edited.len(), edited.len()),
                    (edited_prepared.len(), edited_prepared.len()),
                ),
            };
            prop_assert_eq!(
                strip(&edited[virtual_cursor..virtual_split.0]),
                strip(&edited_prepared[prepared_cursor..prepared_split.0]),
                "content before {:?}",
                gap
            );
            (virtual_cursor, prepared_cursor) = (virtual_split.1, prepared_split.1);
        }
        Ok(())
    }

    #[test]
    fn mapped_edits_keep_host_text_and_the_servers_content() {
        let mut runner = property_runner(4000);
        let (answered, mapped_sets) = (Cell::new(0), Cell::new(0));
        let strategy = (
            prop::collection::vec(line_spec(), 1..7),
            prop::collection::vec(any::<bool>(), 1..8),
            (any::<bool>(), any::<bool>()),
            prop::collection::vec(0..PLACEHOLDERS.len(), 1..4),
            prop::collection::vec(edit_spec(), 1..4),
        );
        runner
            .run(&strategy, |(lines, dedent, edges, placeholders, specs)| {
                let (virtual_text, layout) = document(&lines);
                let answer = answer(&virtual_text, &layout, &dedent, edges, &placeholders);
                let Ok(prepared) = apply_prepare_result(
                    &virtual_text,
                    &layout,
                    serde_json::from_value(answer).unwrap(),
                ) else {
                    return Ok(());
                };
                answered.set(answered.get() + 1);
                let map = &prepared.map;
                let edits = edits(map, &specs);
                let Some(mapped) = map.map_edits(&edits, EditMode::Extent) else {
                    return Ok(());
                };
                mapped_sets.set(mapped_sets.get() + 1);
                prop_assert_eq!(mapped.len(), edits.len());
                check(&virtual_text, map, &edits, &mapped)
            })
            .unwrap();
        // Not vacuous: most documents are prepared, and many edit sets map.
        assert!(answered.get() > 2000, "answered {}", answered.get());
        // Measured over 40 runs: mean 605, sd 22, min 561.
        assert!(mapped_sets.get() > 450, "mapped {}", mapped_sets.get());
    }

    /// Every non-empty line of `prepared` indented by `indent`.
    fn indent_lines(prepared: &str, indent: &str) -> String {
        let mut output = String::new();
        let mut at_line_start = true;
        let mut chars = prepared.chars().peekable();
        while let Some(character) = chars.next() {
            if at_line_start && character != '\n' && character != '\r' {
                output.push_str(indent);
            }
            output.push(character);
            at_line_start = character == '\n' || (character == '\r' && chars.peek() != Some(&'\n'));
        }
        output
    }

    #[test]
    fn edits_to_a_uniformly_dedented_document_map_back_reindented() {
        // A document whose every non-empty line the peer dedented by one
        // indent: any edit set maps back to its result on P re-indented.
        let mut runner = property_runner(3000);
        let mapped_sets = Cell::new(0);
        let strategy = (
            prop::collection::vec((1..BODIES.len(), any::<bool>(), 0..3usize), 1..6),
            1..INDENTS.len(),
            prop::collection::vec(edit_spec(), 1..4),
        );
        runner
            .run(&strategy, |(lines, indent, specs)| {
                let indent = INDENTS[indent];
                let prepared: String = lines
                    .iter()
                    .map(|&(body, empty, eol)| {
                        format!("{}{}", if empty { "" } else { BODIES[body] }, EOLS[eol])
                    })
                    .collect();
                let virtual_text = indent_lines(&prepared, indent);
                let lines = LineMap::new(virtual_text.clone());
                let changes: Vec<_> = (0..virtual_text.len())
                    .filter(|&at| {
                        is_line_start(&virtual_text, at)
                            && !splits_crlf(&virtual_text, at)
                            && virtual_text[at..].starts_with(indent)
                    })
                    .map(|at| {
                        json!({"range": {
                            "start": lines.position(at),
                            "end": lines.position(at + indent.len())
                        }, "newText": ""})
                    })
                    .collect();
                if changes.is_empty() {
                    // No dedent to regain.
                    return Ok(());
                }
                let Ok(document) = apply_prepare_result(
                    &virtual_text,
                    &VirtualLayout::single(&virtual_text),
                    serde_json::from_value(
                        json!({"segments": [{"type": "content", "changes": changes}]}),
                    )
                    .unwrap(),
                ) else {
                    return Ok(());
                };
                prop_assert_eq!(&document.text, &prepared);
                let edits = edits(&document.map, &specs);
                let Some(mapped) = document.map.map_edits(&edits, EditMode::Extent) else {
                    return Ok(());
                };
                mapped_sets.set(mapped_sets.get() + 1);
                prop_assert_eq!(
                    apply_to(&virtual_text, &mapped),
                    indent_lines(&apply_to(&prepared, &edits), indent),
                    "{:?} on {:?}",
                    edits,
                    virtual_text
                );
                Ok(())
            })
            .unwrap();
        assert!(mapped_sets.get() > 600, "mapped {}", mapped_sets.get());
    }
}

#[test]
fn an_edit_ending_between_a_cr_and_a_gaps_lf_is_refused() {
    // Content ending with a CR, then a gap whose masked text is the LF: no
    // LSP position names the offset between them.
    let virtual_text = "\r\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..2, "```\n".to_string()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": ""}]}),
    );
    assert_eq!(prepared.text, "\r");
    assert_eq!(
        prepared.map.map_one_with_extent(&edit((0, 0), (1, 0), "q")),
        None
    );
}

#[test]
fn a_cr_meeting_an_lf_across_an_emptied_gap_is_refused() {
    // `é${x}\na`, the interpolation emptied and both lines dedented: a CR
    // inserted after `é` makes one CRLF in P, but the gap's host text keeps
    // it apart from the LF in V, which gains a line.
    let virtual_text = "  é    \n  a\n";
    let gap = "  é".len();
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..gap, String::new()),
            (SegmentKind::Gap, gap..gap + 4, "${x}".to_string()),
            (
                SegmentKind::Content,
                gap + 4..virtual_text.len(),
                String::new(),
            ),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": ""},
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}
        ]}),
    );
    assert_eq!(prepared.text, "é\na\n");
    let map = prepared.map;
    assert_eq!(map.map_one_with_extent(&edit((0, 1), (0, 1), "\r")), None);
    assert_eq!(map.map_one_with_extent(&edit((0, 0), (0, 1), "\r")), None);
    // A CR kept apart from the LF by text maps.
    let mapped = map
        .map_one_with_extent(&edit((0, 1), (0, 1), "\rb"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[mapped]), "  é\r  b    \n  a\n");
}

#[test]
fn a_cr_an_edit_inserts_before_an_lf_both_documents_show_joins_alike() {
    let (virtual_text, map) = dedented();
    let mapped = map
        .map_one_with_extent(&edit((0, 5), (0, 5), "\r"))
        .unwrap();
    assert_eq!(apply_to(&virtual_text, &[mapped]), "  if x:\r\n    y\n");
}

#[test]
fn an_empty_insertion_inserts_no_indent() {
    // An empty line, where text typed starts a line of the content.
    let virtual_text = "  a\n\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    let mapped = map.map_one_with_extent(&edit((1, 0), (1, 0), "")).unwrap();
    assert_eq!(mapped.new_text, "");
    // Text there regains the indent.
    let mapped = map.map_one_with_extent(&edit((1, 0), (1, 0), "x")).unwrap();
    assert_eq!(mapped.new_text, "  x");
}

#[test]
fn text_typed_on_a_whitespace_line_the_peer_kept_regains_nothing() {
    // The peer left `   ` alone, as it may leave any line: typed at its
    // start, text joins it as on a line left alone.
    let virtual_text = "  a\n   \n  b\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    let typed = map.map_one_with_extent(&edit((1, 0), (1, 0), "x")).unwrap();
    assert_eq!(apply_to(virtual_text, &[typed]), "  a\nx   \n  b\n");
}

#[test]
fn an_insertion_between_a_cr_and_a_gaps_lf_is_refused() {
    // Content ending with a CR, then a gap whose masked text is the LF,
    // emptied: an insertion after the CR maps before the gap, between the
    // CR and the LF, where no LSP position is.
    let prepared = prepare_pieces(
        "\r\n",
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..2, "```\n".to_string()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": ""}]}),
    );
    assert_eq!(
        prepared
            .map
            .map_one_with_extent(&edit((1, 0), (1, 0), "\n")),
        None
    );
}

#[test]
fn a_whole_line_replaced_before_a_blank_line_keeps_its_indent() {
    let virtual_text = "  a\n\n  b\n";
    let map = dedent_by_two(virtual_text);
    let replaced = map.map_one_with_extent(&edit((0, 0), (1, 0), "x")).unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  b\n");
}

#[test]
fn whole_line_edits_beside_an_emptied_gap_after_a_removed_indent_map() {
    // `\n  foo\n  ${x}b\n` with `${x}` emptied: P's last line starts with
    // the indent, then the gap.
    let virtual_text = "\n  foo\n      b\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..9, String::new()),
            (SegmentKind::Gap, 9..13, "${x}".to_string()),
            (SegmentKind::Content, 13..15, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": ""},
            {"type": "content"}
        ]}),
    );
    let deleted = prepared
        .map
        .map_one_with_extent(&edit((2, 0), (3, 0), ""))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[deleted]), "\n  foo\n      ");
    let replaced = prepared
        .map
        .map_one_with_extent(&edit((2, 0), (3, 0), "q\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "\n  foo\n      q\n");
}

#[test]
fn line_breaks_that_meet_across_a_host_runs_edge_in_v_stay_met() {
    // `a${x}b`, the gap's masked text ending with the CR of a CRLF whose LF
    // opens the next content, the gap shown as `y`: an edit elsewhere keeps
    // that CRLF as it was.
    let virtual_text = "a \r\nb";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..3, "x\r".to_string()),
            (SegmentKind::Content, 3..5, String::new()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": "y"}, {"type": "content"}]}),
    );
    assert_eq!(prepared.text, "ay\nb");
    let mapped = prepared
        .map
        .map_one_with_extent(&edit((1, 0), (1, 1), "c"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[mapped]), "a \r\nc");
    // The mirror: content ending with the CR, the gap's masked text opening
    // with the LF.
    let virtual_text = "a\r\n b";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..2, String::new()),
            (SegmentKind::Gap, 2..4, "\nx".to_string()),
            (SegmentKind::Content, 4..5, String::new()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": "y"}, {"type": "content"}]}),
    );
    assert_eq!(prepared.text, "a\ryb");
    let mapped = prepared
        .map
        .map_one_with_extent(&edit((0, 0), (0, 1), "c"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[mapped]), "c\r\n b");
}

#[test]
fn a_cr_before_a_gap_shown_opening_with_its_lf_is_the_servers_to_join() {
    // `a${\nx}b`, the gap shown as `\ny`: P shows the LF, so a CR inserted
    // before it joins it in both documents.
    let virtual_text = "a\n b";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..1, String::new()),
            (SegmentKind::Gap, 1..3, "\nx".to_string()),
            (SegmentKind::Content, 3..4, String::new()),
        ],
        json!({"segments": [{"type": "content"}, {"type": "gap", "content": "\ny"}, {"type": "content"}]}),
    );
    assert_eq!(prepared.text, "a\nyb");
    let mapped = prepared
        .map
        .map_one_with_extent(&edit((0, 1), (0, 1), "\r"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[mapped]), "a\r\n b");
}

// =============================================================================
// Property: mapped edits give exactly the host text a line-based reference
// builds from the edited prepared text
// =============================================================================

mod exact_reference {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    use std::cell::Cell;

    /// One line of a generated document.
    #[derive(Debug, Clone)]
    enum Line {
        /// Indented content (`body` from [`BODIES`]), with an interpolation
        /// after it and text after that when `gap` says so.
        Content { body: usize, gap: Option<bool> },
        /// An empty line inside the content.
        Empty,
        /// A whitespace-only line inside the content, which the peer
        /// empties: holding the indent (removed with the content's), or
        /// when `short`, a single space (removed whole).
        Blank { short: bool },
        /// Host lines between two fences: a gap starting a line.
        Fence,
    }

    /// Line bodies, shared between lines and with the inserted texts, so
    /// that an edit's text can repeat the lines around it (as a formatter's
    /// or a completion's does).
    const BODIES: [&str; 7] = ["a", "é", "𝄞x", "b c", "}", "a = 1", "f('a')"];
    const PLACEHOLDERS: [Option<&str>; 4] = [None, Some(""), Some("1"), Some("x\n")];
    const INSERTS: [&str; 13] = [
        "",
        "z",
        "\n",
        "q\n",
        "\nw",
        "é𝄞",
        "  ",
        "\n\n",
        "\nw\n",
        "a = 1,",
        "\n}",
        "f('a')\n",
        "a = 1\na = 1,",
    ];

    /// A generated document: its pieces and the text the reference puts
    /// back around the edited content.
    struct Document {
        virtual_text: String,
        pieces: Vec<(SegmentKind, Range<usize>, String)>,
        /// Whether each gap is a fence (starts a line), in order.
        fences: Vec<bool>,
        changes: Vec<serde_json::Value>,
        /// Text before the content: the deleted leading blank lines.
        leading: String,
        /// Text after the content: the deleted closing lines, then the
        /// closing indent deleted with them.
        closing: String,
        /// Whether the last line is an indentation-only line the peer
        /// deleted with no closing lines before it: a content line of its
        /// own (see the ADR), whose indent comes back even when empty.
        indent_only_last_line: bool,
    }

    fn line() -> impl Strategy<Value = Line> {
        prop_oneof![
            4 => (0..BODIES.len(), prop::option::of(any::<bool>()))
                .prop_map(|(body, gap)| Line::Content { body, gap }),
            1 => Just(Line::Empty),
            1 => any::<bool>().prop_map(|short| Line::Blank { short }),
            1 => Just(Line::Fence),
        ]
    }

    /// The document `lines` describe, indented by `indent`, between
    /// `leading` blank lines and `trailing` ones, optionally ending with a
    /// closing indent; `None` for a shape the peer could not answer for
    /// (a fence first or last, a block between fences with no content line).
    fn document(
        lines: &[Line],
        indent: &str,
        leading: usize,
        trailing: usize,
        closing_indent: bool,
    ) -> Option<Document> {
        if matches!(lines.first(), Some(Line::Fence) | None)
            || matches!(lines.last(), Some(Line::Fence))
        {
            return None;
        }
        // Every block between fences holds a content line: a block of only
        // empty lines says nothing of the indentation lines there regain.
        let mut has_content = false;
        for line in lines {
            match line {
                Line::Content { .. } => has_content = true,
                Line::Fence if !has_content => return None,
                Line::Fence => has_content = false,
                Line::Empty | Line::Blank { .. } => {}
            }
        }
        if !has_content {
            return None;
        }
        let mut text = String::new();
        let mut pieces = Vec::new();
        let mut fences = Vec::new();
        let mut changes = Vec::new();
        fn push_content(
            text: &mut String,
            pieces: &mut Vec<(SegmentKind, Range<usize>, String)>,
            piece: &str,
        ) {
            let start = text.len();
            text.push_str(piece);
            pieces.push((SegmentKind::Content, start..text.len(), String::new()));
        }
        let leading_text = "\n".repeat(leading);
        push_content(&mut text, &mut pieces, &leading_text);
        for line in lines {
            match line {
                Line::Empty => push_content(&mut text, &mut pieces, "\n"),
                Line::Blank { short } => push_content(
                    &mut text,
                    &mut pieces,
                    &format!("{}\n", if *short { " " } else { indent }),
                ),
                Line::Fence => {
                    let start = text.len();
                    text.push('\n');
                    pieces.push((SegmentKind::Gap, start..text.len(), "```\n".to_string()));
                    fences.push(true);
                }
                Line::Content { body, gap } => {
                    push_content(
                        &mut text,
                        &mut pieces,
                        &format!("{indent}{}", BODIES[*body]),
                    );
                    if let Some(after) = gap {
                        let start = text.len();
                        text.push_str("    ");
                        pieces.push((SegmentKind::Gap, start..text.len(), "${x}".to_string()));
                        fences.push(false);
                        if *after {
                            push_content(&mut text, &mut pieces, " y");
                        }
                    }
                    push_content(&mut text, &mut pieces, "\n");
                }
            }
        }
        let content_end = text.len();
        push_content(&mut text, &mut pieces, &"\n".repeat(trailing));
        if closing_indent {
            push_content(&mut text, &mut pieces, indent);
        }
        // The answer, segment by segment: built after the layout merges
        // the pieces.
        let layout = VirtualLayout::from_pieces(&text, pieces.clone());
        let segments = layout.segments();
        let last = segments.len() - 1;
        for (index, segment) in segments.iter().enumerate() {
            let mut segment_changes = Vec::new();
            if segment.kind == SegmentKind::Content {
                let range = segment.virtual_range.clone();
                let segment_text = &text[range.clone()];
                let lines = LineMap::new(segment_text.to_string());
                let change = |start: usize, end: usize| {
                    json!({"range": {
                        "start": lines.position(start - range.start),
                        "end": lines.position(end - range.start)
                    }, "newText": ""})
                };
                for at in range.clone() {
                    if !text.is_char_boundary(at) || !super::super::super::is_line_start(&text, at)
                    {
                        continue;
                    }
                    // Edge blank lines go whole, one change each.
                    let edge_line = (index == 0 && at < leading)
                        || (index == last && at >= content_end && at < content_end + trailing);
                    let line_text = text[at..range.end].split('\n').next().unwrap_or_default();
                    if edge_line {
                        segment_changes.push(change(at, at + 1));
                    } else if !line_text.is_empty()
                        && !line_text.starts_with(indent)
                        && line_text.chars().all(|c| c == ' ')
                    {
                        // A short blank line, emptied whole.
                        segment_changes.push(change(at, at + line_text.len()));
                    } else if text[at..range.end].starts_with(indent) {
                        segment_changes.push(change(at, at + indent.len()));
                    }
                }
            }
            changes.push(segment_changes);
        }
        let indent_only_last_line = closing_indent && trailing == 0;
        Some(Document {
            leading: leading_text,
            closing: if indent_only_last_line {
                String::new()
            } else {
                format!(
                    "{}{}",
                    "\n".repeat(trailing),
                    if closing_indent { indent } else { "" }
                )
            },
            indent_only_last_line,
            virtual_text: text,
            pieces,
            fences,
            changes: changes.into_iter().map(serde_json::Value::from).collect(),
        })
    }

    /// The host text the edited P stands for, built line by line: the gaps'
    /// host text put back where they sit in the edited P, every line
    /// starting in content (not with a fence) indented, the deleted edge
    /// text put back around it. `None` where the edited P leaves a fence or
    /// the closing text not starting a line, which the host cannot take.
    fn reference(
        document: &Document,
        map: &PreparedMap,
        edits: &[TextEdit],
        indent: &str,
    ) -> Option<String> {
        let prepared = &map.prepared_lines.text;
        let edited = apply_to(prepared, edits);
        let ranges: Vec<(Range<usize>, usize)> = edits
            .iter()
            .map(|edit| {
                (
                    map.prepared_lines.offset_clamped(edit.range.start)
                        ..map.prepared_lines.offset_clamped(edit.range.end),
                    edit.new_text.len(),
                )
            })
            .collect();
        // Where a P offset outside every edit lands in the edited P, after
        // the edits ending at or before it (an insertion there included).
        let shifted = |offset: usize| {
            ranges
                .iter()
                .filter(|(range, _)| range.end <= offset)
                .fold(offset, |at, (range, len)| at + len - range.len())
        };
        // The edited P with each gap's prepared text swapped for its host
        // (V) text, and which characters a fence holds.
        let gaps: Vec<&super::super::super::Run> = map
            .runs
            .iter()
            .filter(|run| run.kind == RunKind::Gap)
            .collect();
        let mut text = String::new();
        let mut fence_starts = Vec::new();
        let mut cursor = 0;
        for (gap, fence) in gaps.iter().zip(&document.fences) {
            let start = shifted(gap.prepared.start);
            text.push_str(&edited[cursor..start]);
            if *fence {
                fence_starts.push(text.len());
            }
            text.push_str(&document.virtual_text[gap.virtual_.clone()]);
            cursor = start + gap.prepared.len();
        }
        text.push_str(&edited[cursor..]);
        // A fence starts a line.
        for &start in &fence_starts {
            if start > 0 && !text[..start].ends_with('\n') {
                return None;
            }
        }
        // A blank line whose indent the peer removed keeps it while no edit
        // reaches into it (on a document without gaps, where `text` is the
        // edited P itself).
        let kept_blank_lines: Vec<(usize, &str)> = if gaps.is_empty() {
            map.runs
                .iter()
                .filter(|run| {
                    run.kind == RunKind::Deleted
                        && run.prepared.is_empty()
                        && super::super::super::is_line_start(prepared, run.prepared.start)
                        && prepared[run.prepared.start..].starts_with('\n')
                })
                .filter(|run| {
                    !ranges.iter().any(|(range, _)| {
                        range.start <= run.prepared.start && run.prepared.start < range.end
                    })
                })
                .map(|run| {
                    (
                        shifted(run.prepared.start),
                        &document.virtual_text[run.virtual_.clone()],
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut indented = String::new();
        let mut at_line_start = true;
        for (offset, character) in text.char_indices() {
            if at_line_start && character == '\n' {
                if let Some((_, removed)) = kept_blank_lines.iter().find(|(at, _)| *at == offset) {
                    indented.push_str(removed);
                }
            } else if at_line_start && !fence_starts.contains(&offset) {
                indented.push_str(indent);
            }
            indented.push(character);
            at_line_start = character == '\n';
        }
        // The indentation-only last line keeps its indent, empty as it is.
        if document.indent_only_last_line && at_line_start {
            indented.push_str(indent);
        }
        // The closing text starts a line too.
        if !document.closing.is_empty() && !indented.is_empty() && !indented.ends_with('\n') {
            return None;
        }
        Some(format!(
            "{}{indented}{}",
            document.leading, document.closing
        ))
    }

    #[test]
    fn mapped_edits_give_exactly_the_reference_host_text() {
        let mut runner = property_runner(
            // Most generated documents bear gaps, skipped below.
            12000,
        );
        let (mapped_sets, refused_by_reference) = (Cell::new(0), Cell::new(0));
        let strategy = (
            prop::collection::vec(line(), 1..6),
            prop::sample::select(vec!["  ", "\t", "    "]),
            (0..3usize, 0..3usize, any::<bool>()),
            prop::collection::vec(0..PLACEHOLDERS.len(), 1..4),
            prop::collection::vec((any::<u16>(), any::<u16>(), 0..INSERTS.len()), 1..3),
        );
        runner
            .run(
                &strategy,
                |(lines, indent, (leading, trailing, closing), placeholders, specs)| {
                    let Some(document) = document(&lines, indent, leading, trailing, closing)
                    else {
                        return Ok(());
                    };
                    // Exactness is asserted on contiguous documents only:
                    // edit carriers open on no other, and on a map with gaps
                    // the indentation lines regain is not yet exact in some
                    // shapes (see `gap_map_defects` below). The host text a
                    // gap holds is kept whatever the map, which `properties`
                    // asserts with gaps included.
                    if !document.fences.is_empty() {
                        return Ok(());
                    }
                    let layout = VirtualLayout::from_pieces(
                        &document.virtual_text,
                        document.pieces.clone(),
                    );
                    let mut gaps = placeholders.iter().cycle();
                    let answer: Vec<serde_json::Value> = layout
                        .segments()
                        .iter()
                        .zip(&document.changes)
                        .map(|(segment, changes)| match segment.kind {
                            SegmentKind::Content => {
                                json!({"type": "content", "changes": changes})
                            }
                            SegmentKind::Gap => match PLACEHOLDERS[*gaps.next().unwrap()] {
                                None => json!({"type": "gap"}),
                                Some(content) => json!({"type": "gap", "content": content}),
                            },
                        })
                        .collect();
                    let Ok(prepared) = apply_prepare_result(
                        &document.virtual_text,
                        &layout,
                        serde_json::from_value(json!({ "segments": answer })).unwrap(),
                    ) else {
                        return Ok(());
                    };
                    let map = &prepared.map;
                    let text = &map.prepared_lines.text;
                    let boundaries: Vec<usize> = (0..=text.len())
                        .filter(|&at| text.is_char_boundary(at))
                        .collect();
                    let edits: Vec<TextEdit> = specs
                        .iter()
                        .map(|&(a, b, insert)| {
                            let a = boundaries[a as usize % boundaries.len()];
                            let b = boundaries[b as usize % boundaries.len()];
                            TextEdit {
                                range: LspRange::new(
                                    map.prepared_lines.position(a.min(b)),
                                    map.prepared_lines.position(a.max(b)),
                                ),
                                new_text: INSERTS[insert].to_string(),
                            }
                        })
                        .collect();
                    let Some(mapped) = map.map_edits(&edits, EditMode::Extent) else {
                        return Ok(());
                    };
                    mapped_sets.set(mapped_sets.get() + 1);
                    let got = apply_to(&document.virtual_text, &mapped);
                    match reference(&document, map, &edits, indent) {
                        Some(expected) => prop_assert_eq!(
                            &got,
                            &expected,
                            "{:?} on {:?} (P {:?})",
                            edits,
                            document.virtual_text,
                            text
                        ),
                        None => {
                            refused_by_reference.set(refused_by_reference.get() + 1);
                            prop_assert!(
                                false,
                                "mapped though the host cannot take it: {:?} on {:?} (P {:?}) -> {:?}",
                                edits,
                                document.virtual_text,
                                text,
                                got
                            );
                        }
                    }
                    Ok::<(), TestCaseError>(())
                },
            )
            .unwrap();
        assert!(mapped_sets.get() > 400, "mapped {}", mapped_sets.get());
    }

    /// Edits on maps with gaps whose result differs from the reference:
    /// known defects of gap-bearing maps, which no edit carrier reaches
    /// (they open on contiguous regions only). Fixing them is a
    /// prerequisite for opening edit carriers on noncontiguous regions.
    mod gap_map_defects {
        use super::*;

        /// Map `edits` on the document `lines` describe (as the property
        /// generates it, ending with a closing indent when `closing`, the
        /// gaps answered with `placeholders` in turn), against the
        /// reference.
        fn mapped_and_reference(
            lines: &[Line],
            indent: &str,
            closing: bool,
            placeholders: &[&str],
            edits: &[TextEdit],
        ) -> (String, Option<String>) {
            let document = document(lines, indent, 0, 0, closing).unwrap();
            let mut placeholders = placeholders.iter().cycle();
            let layout =
                VirtualLayout::from_pieces(&document.virtual_text, document.pieces.clone());
            let answer: Vec<serde_json::Value> = layout
                .segments()
                .iter()
                .zip(&document.changes)
                .map(|(segment, changes)| match segment.kind {
                    SegmentKind::Content => json!({"type": "content", "changes": changes}),
                    SegmentKind::Gap => {
                        json!({"type": "gap", "content": placeholders.next().unwrap()})
                    }
                })
                .collect();
            let prepared = apply_prepare_result(
                &document.virtual_text,
                &layout,
                serde_json::from_value(json!({ "segments": answer })).unwrap(),
            )
            .unwrap();
            let mapped = prepared.map.map_edits(edits, EditMode::Extent).unwrap();
            (
                apply_to(&document.virtual_text, &mapped),
                reference(&document, &prepared.map, edits, indent),
            )
        }

        /// `  a`, a fence shown as `1`, a blank line, `  a`, `  a`: two
        /// insertions right after the placeholder, a line break then
        /// text, and more text. The second takes the content's indent
        /// again instead of joining the line the first created.
        #[test]
        #[ignore = "known gap-map defect: unreachable while edit carriers stay contiguous"]
        fn insertions_after_a_fence_placeholder_without_a_line_break() {
            let (got, expected) = mapped_and_reference(
                &[
                    Line::Content { body: 0, gap: None },
                    Line::Fence,
                    Line::Empty,
                    Line::Content { body: 0, gap: None },
                    Line::Content { body: 0, gap: None },
                ],
                "  ",
                false,
                &["1"],
                &[edit((1, 1), (1, 1), "\nw"), edit((1, 1), (1, 1), "é𝄞")],
            );
            assert_eq!(expected.as_deref(), Some("  a\n\n\n  wé𝄞\n  a\n  a\n"));
            assert_eq!(Some(got), expected);
        }

        /// `  b c${x}`, a fence shown as `1`, a blank line, `  a`, then the
        /// indentation-only last line: deleting from the placeholder to P's
        /// end also deletes that last line's indent, which the host keeps.
        #[test]
        #[ignore = "known gap-map defect: unreachable while edit carriers stay contiguous"]
        fn deleting_from_a_fence_placeholder_to_the_end() {
            let (got, expected) = mapped_and_reference(
                &[
                    Line::Content {
                        body: 3,
                        gap: Some(false),
                    },
                    Line::Fence,
                    Line::Empty,
                    Line::Content { body: 0, gap: None },
                ],
                "  ",
                true,
                &["", "1"],
                &[edit((1, 1), (3, 0), "")],
            );
            assert_eq!(expected.as_deref(), Some("  b c    \n\n  "));
            assert_eq!(Some(got), expected);
        }
    }
}

#[test]
fn a_line_created_after_an_interpolation_takes_the_indent_of_its_line() {
    // `\n  foo ${x} bar\n`: the segment after the interpolation starts no
    // line of its own and removed no indent; a line created in it is
    // indented as the line it began on.
    let virtual_text = "\n  foo      bar\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..7, String::new()),
            (SegmentKind::Gap, 7..11, "${x}".to_string()),
            (SegmentKind::Content, 11..16, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": "1"},
            {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "\nfoo 1 bar\n");
    let appended = prepared
        .map
        .map_one_with_extent(&edit((1, 9), (1, 9), "\nbaz"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[appended]),
        "\n  foo      bar\n  baz\n"
    );
}

#[test]
fn the_indentation_only_last_line_keeps_its_indent() {
    // `\n  foo\n  `: the last line holds only the indent the peer removed (a
    // content line of its own, no closing lines deleted before it).
    let virtual_text = "\n  foo\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "foo\n");
    for (edit, expected) in [
        (edit((0, 0), (1, 0), "bar\n"), "\n  bar\n  "),
        (edit((0, 0), (1, 0), ""), "\n  "),
        (edit((1, 0), (1, 0), "bar\n"), "\n  foo\n  bar\n  "),
    ] {
        let mapped = map.map_one_with_extent(&edit).unwrap();
        assert_eq!(apply_to(virtual_text, &[mapped]), expected, "{edit:?}");
    }
}

#[test]
fn deleting_a_line_keeps_the_next_whitespace_only_lines_indent() {
    let virtual_text = "  a\n  \n  b\n";
    let map = dedent_by_two(virtual_text);
    let removed = map.map_one_with_extent(&edit((0, 0), (1, 0), "")).unwrap();
    assert_eq!(apply_to(virtual_text, &[removed]), "  \n  b\n");
    // Nothing inserted on it takes its indent away.
    let nothing = map.map_one_with_extent(&edit((1, 0), (1, 0), "")).unwrap();
    assert_eq!(apply_to(virtual_text, &[nothing]), virtual_text);
}

#[test]
fn a_line_left_starting_with_an_emptied_interpolation_keeps_its_indent() {
    // `\n  a${x} y\n`, the interpolation emptied: deleting `a` leaves the
    // line starting with the interpolation, after the host's indent.
    let virtual_text = "\n  a     y\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..4, String::new()),
            (SegmentKind::Gap, 4..8, "${x}".to_string()),
            (SegmentKind::Content, 8..11, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": ""},
            {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "\na y\n");
    let replaced = prepared
        .map
        .map_one_with_extent(&edit((0, 0), (1, 1), "\n\n"))
        .unwrap();
    // The indent, the interpolation's masked text, then ` y`.
    assert_eq!(apply_to(virtual_text, &[replaced]), "\n\n       y\n");
}

#[test]
fn a_line_break_a_placeholder_shows_mid_line_starts_no_existing_line() {
    // `  b c${x} y\n`, the interpolation shown as `x\n`: P's line ` y` is the
    // rest of the host line, which keeps no indent of its own.
    let virtual_text = "  b c     y\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..5, String::new()),
            (SegmentKind::Gap, 5..9, "${x}".to_string()),
            (SegmentKind::Content, 9..12, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": "x\n"},
            {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "b cx\n y\n");
    // A line break inserted before ` y` makes it a line of the content.
    let broken = prepared
        .map
        .map_one_with_extent(&edit((1, 0), (1, 0), "\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[broken]), "  b c    \n   y\n");
}

#[test]
fn text_after_a_fence_shown_without_its_line_break_starts_a_line() {
    // `  a\n` + a fence + `\n  b\n`, the fence shown as `1`: P joins it with
    // the blank line after it, which is a line of its own in the host.
    let virtual_text = "  a\n\n\n  b\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..4, String::new()),
            (SegmentKind::Gap, 4..5, "```\n".to_string()),
            (SegmentKind::Content, 5..10, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": "1"},
            {"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}
        ]}),
    );
    assert_eq!(prepared.text, "a\n1\nb\n");
    let typed = prepared
        .map
        .map_one_with_extent(&edit((1, 1), (1, 1), "z"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[typed]), "  a\n\n  z\n  b\n");
}

#[test]
fn a_line_left_alone_after_an_interpolation_does_not_unindent_new_lines() {
    // `  foo ${x} bar\nbaz\n`, `baz` left alone: as without the
    // interpolation, a line created on the first line regains its indent.
    let virtual_text = "  foo      bar\nbaz\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..6, String::new()),
            (SegmentKind::Gap, 6..10, "${x}".to_string()),
            (SegmentKind::Content, 10..19, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap", "content": "1"},
            {"type": "content"}
        ]}),
    );
    assert_eq!(prepared.text, "foo 1 bar\nbaz\n");
    let broken = prepared
        .map
        .map_one_with_extent(&edit((0, 9), (0, 9), "\nq"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[broken]),
        "  foo      bar\n  q\nbaz\n"
    );
}

#[test]
fn a_block_the_peer_left_unindented_regains_no_indent() {
    // `  a\n` + a fence + `b\n`: the block after the fence was not
    // dedented, so a line created in it regains nothing.
    let virtual_text = "  a\n\nb\n";
    let prepared = prepare_pieces(
        virtual_text,
        [
            (SegmentKind::Content, 0..4, String::new()),
            (SegmentKind::Gap, 4..5, "```\n".to_string()),
            (SegmentKind::Content, 5..7, String::new()),
        ],
        json!({"segments": [
            {"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]},
            {"type": "gap"},
            {"type": "content"}
        ]}),
    );
    let added = prepared
        .map
        .map_one_with_extent(&edit((2, 1), (2, 1), "\nc"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[added]), "  a\n\nb\nc\n");
}

#[test]
fn lines_created_above_a_shallower_closing_indent_regain_the_contents() {
    // The nixfmt layout: `''\n    foo\n  ''`, the closing indent shallower
    // than the content and deleted right after it.
    let virtual_text = "\n    foo\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "foo\n");
    let imported = map
        .map_one_with_extent(&edit((0, 0), (0, 0), "x\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[imported]), "\n    x\n    foo\n  ");
    let completed = map
        .map_one_with_extent(&edit((0, 0), (0, 3), "if a:\n  b"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[completed]),
        "\n    if a:\n      b\n  "
    );
}

#[test]
fn text_replacing_whole_lines_joins_the_next_as_that_line_is() {
    // Before a line of text, what replaces the lines joins it, keeping its
    // indent (none: `b` was left alone).
    let virtual_text = "  a\nb\n";
    let map = dedent_by_two(virtual_text);
    let joined = map.map_one_with_extent(&edit((0, 0), (1, 0), "x")).unwrap();
    assert_eq!(apply_to(virtual_text, &[joined]), "xb\n");
    // Before a blank line, it is that line's content instead, indented as
    // the replaced line was.
    let virtual_text = "  a\n\n";
    let map = dedent_by_two(virtual_text);
    let joined = map.map_one_with_extent(&edit((0, 0), (1, 0), "x")).unwrap();
    assert_eq!(apply_to(virtual_text, &[joined]), "  x\n");
}

#[test]
fn a_blank_lines_removed_indent_does_not_set_the_contents() {
    // `x` left alone, the blank line after it dedented: a line created
    // after `x` regains nothing, the blank line its own indent.
    let virtual_text = "x\n  \n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "x\n\n");
    let inserted = map
        .map_one_with_extent(&edit((0, 1), (0, 1), "\ny"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "x\ny\n  \n");
    // Content dedented by two, the blank line by four: the content's two
    // (the differing blank line no longer leaves no single indent).
    let virtual_text = "  x\n    \n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
        ]),
    );
    let inserted = map
        .map_one_with_extent(&edit((0, 1), (0, 1), "\ny"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "  x\n  y\n    \n");
    // Without content, the blank lines' removed indent is the one new
    // lines regain.
    let virtual_text = "  \n  \n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]),
    );
    let typed = map.map_one_with_extent(&edit((0, 0), (0, 0), "z")).unwrap();
    assert_eq!(apply_to(virtual_text, &[typed]), "  z\n  \n");
}

#[test]
fn text_replacing_lines_from_a_blank_one_takes_the_indent_of_where_it_lands() {
    // `  x`, a one-space line the peer emptied, `  c`, `  then`: replacing
    // from the blank line through `then` leaves one line, `then`'s place.
    let virtual_text = "  x\n \n  c\n  then\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "x\n\nc\nthen\n");
    let replaced = map
        .map_one_with_extent(&edit((1, 0), (3, 4), "end"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  end\n");
    // Partway into the line: the rest of it stays after the text.
    let replaced = map.map_one_with_extent(&edit((1, 0), (3, 2), "e")).unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  een\n");
    // Lines created from the blank line regain the content's indent, not
    // the blank line's.
    let replaced = map
        .map_one_with_extent(&edit((1, 0), (3, 0), "end\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  end\n  then\n");
    // Content left at column 0, the blank line's space removed: none.
    let virtual_text = "x\n \nc\nthen\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""}
        ]),
    );
    let replaced = map
        .map_one_with_extent(&edit((1, 0), (3, 4), "end"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "x\nend\n");
    // A whitespace line the peer kept: the text lands where `then` was.
    let virtual_text = "  x\n   \n  c\n  then\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "x\n   \nc\nthen\n");
    let replaced = map
        .map_one_with_extent(&edit((1, 0), (3, 4), "end"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  end\n");
}

#[test]
fn text_replacing_lines_from_a_kept_whitespace_line_keeps_the_indent_where_it_lands() {
    // A one-space line the peer kept, between dedented ones: replacing from
    // it to within the last line leaves that line's text, which regains its
    // removed indent, the line's own indent after it kept.
    let virtual_text = "  a = 1\n \n    foo(\n    f('a')\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "a = 1\n \n  foo(\n  f('a')\n");
    let mapped =
        |edit: TextEdit| apply_to(virtual_text, &[map.map_one_with_extent(&edit).unwrap()]);
    // To the end of that line (what replacing the whole text with
    // "    a = 1\na = 1,\n" leaves once common text is kept).
    assert_eq!(
        mapped(edit((1, 0), (3, 8), "a = 1,")),
        "  a = 1\n  a = 1,\n"
    );
    // Within its content, after its kept indent, and within that indent.
    assert_eq!(
        mapped(edit((1, 0), (3, 4), "a = 1,")),
        "  a = 1\n  a = 1,'a')\n"
    );
    assert_eq!(
        mapped(edit((1, 0), (3, 2), "a = 1,")),
        "  a = 1\n  a = 1,f('a')\n"
    );
    assert_eq!(
        mapped(edit((1, 0), (3, 1), "a = 1,")),
        "  a = 1\n  a = 1, f('a')\n"
    );
    // Lines before it regain the content's indent, as created lines do.
    assert_eq!(
        mapped(edit((1, 0), (3, 4), "x\na = 1,")),
        "  a = 1\n  x\n  a = 1,'a')\n"
    );
}

#[test]
fn text_replacing_lines_up_to_a_whitespace_only_rest_stays_the_start_lines() {
    // The later line keeps nothing but whitespace after the change (here
    // the indentation-only last line the peer kept): the text is the start
    // line's, regaining its removed indent.
    let virtual_text = " \tdef f(x):\r\n\t";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "def f(x):\r\n\t");
    let replaced = map
        .map_one_with_extent(&edit((0, 0), (1, 1), "end"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[replaced]), " \tend");
}

#[test]
fn text_typed_on_an_emptied_blank_line_is_content() {
    // A one-space line under a two-space indent, emptied by the peer: text
    // typed there is a line of the content, which its space says nothing
    // of; text joining it after a line break too.
    let virtual_text = "  a\n \n  b\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    let typed = map.map_one_with_extent(&edit((1, 0), (1, 0), "z")).unwrap();
    assert_eq!(apply_to(virtual_text, &[typed]), "  a\n  z\n  b\n");
    let joined = map
        .map_one_with_extent(&edit((0, 1), (1, 0), "\nw"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[joined]), "  a\n  w\n  b\n");
    // Its space kept when nothing is typed on it: a line break inserted
    // before it leaves it as it was.
    let broken = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "\n"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[broken]), "  a\n\n \n  b\n");
}

#[test]
fn only_spaces_and_tabs_are_blank() {
    // A line left alone holding an ideographic space is a line of text: what
    // is typed there joins it, regaining no indent.
    let virtual_text = "  a\n\u{3000}\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    let typed = map.map_one_with_extent(&edit((1, 0), (1, 0), "x")).unwrap();
    assert_eq!(apply_to(virtual_text, &[typed]), "  a\nx\u{3000}\n");
}

#[test]
fn text_joining_an_existing_line_takes_that_lines_indent() {
    // `b` left alone: the text after the inserted line break joins it, and
    // stays unindented with it.
    let virtual_text = "  a\nb\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    let inserted = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "x\ny"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "  a\n  x\nyb\n");
    // Joining the indentation-only last line, it keeps that line's indent.
    let virtual_text = "\t \n\tb c\n ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}}, "newText": ""}
        ]),
    );
    let joined = map
        .map_one_with_extent(&edit((2, 0), (2, 0), "\r\nw"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[joined]), "\t \n\tb c\n\r\n w");
}

#[test]
fn lines_inserted_before_a_shallower_closing_indent_are_content_lines() {
    // The nixfmt layout: a line inserted at the closing line's start is a
    // content line; the closing line keeps its own indent after it.
    let virtual_text = "\n    foo\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    let inserted = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "baz\n"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[inserted]),
        "\n    foo\n    baz\n  "
    );
}

#[test]
fn text_ending_mid_line_inserted_before_a_closing_indent_regains_it_once() {
    // Lines created at the closing line's start are content lines; the text
    // after the last line break joins the closing line, after its own
    // indent: the closing indent is regained once, not on both.
    let virtual_text = "b c\r\n    ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "b c\r\n");
    let inserted = |text| {
        let edit = map
            .map_one_with_extent(&edit((1, 0), (1, 0), text))
            .unwrap();
        apply_to(virtual_text, &[edit])
    };
    assert_eq!(inserted("if a:\n  b\nc"), "b c\r\nif a:\n  b\n    c");
    assert_eq!(inserted("x\n  y"), "b c\r\nx\n      y");
    // The nixfmt layout, the closing indent shallower than the content's.
    let virtual_text = "  foo(1)\r ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_lines.text, "foo(1)\r");
    let inserted = map
        .map_one_with_extent(&edit((1, 0), (1, 0), "if a:\n  b\nc"))
        .unwrap();
    assert_eq!(
        apply_to(virtual_text, &[inserted]),
        "  foo(1)\r  if a:\n    b\n c"
    );
}

#[test]
fn a_closing_indent_after_content_left_alone_is_no_indent_to_regain() {
    // `a` at column 0, only the closing indent deleted: a line created after
    // `a` regains no indent.
    let virtual_text = "a\n  ";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]),
    );
    let inserted = map
        .map_one_with_extent(&edit((0, 1), (0, 1), "\nz"))
        .unwrap();
    assert_eq!(apply_to(virtual_text, &[inserted]), "a\nz\n  ");
}

#[test]
fn many_edits_on_one_long_line_look_at_it_in_linear_work() {
    // Each edit asks whether the rest of its line is blank; asked by
    // scanning to the line's end, edits on one long line cost edits ×
    // line length.
    // Text, and text followed by a long run of spaces.
    for body in ["ab".repeat(4_000), format!("a{}", " ".repeat(7_999))] {
        let virtual_text = format!("  {body}\n");
        let map = dedent_by_two(&virtual_text);
        let edits: Vec<_> = (0..4_000u32)
            .map(|column| edit((0, 2 * column + 1), (0, 2 * column + 1), "x"))
            .collect();
        let blank_scanned = || super::super::tests::BLANK_SCANNED.with(std::cell::Cell::get);
        let before = blank_scanned();
        let mapped = map.map_edits(&edits, EditMode::Extent).unwrap();
        let scanned = blank_scanned() - before;
        assert_eq!(mapped.len(), edits.len());
        assert!(
            scanned <= 2 * virtual_text.len(),
            "{scanned} bytes scanned for a {}-byte line",
            virtual_text.len()
        );
    }
}

#[test]
fn an_edit_keeping_its_extent_over_whole_blank_lines_is_placed_as_it_says() {
    // Text replacing whole lines from a blank line: an edit keeping its
    // extent takes the indent its own rules give (formatting answers alone
    // read such text as a line of the content in the blank line's place).
    let changes = |removed: &[usize]| -> serde_json::Value {
        removed
            .iter()
            .enumerate()
            .filter(|&(_, &width)| width > 0)
            .map(|(line, &width)| {
                json!({"range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": width}}, "newText": ""})
            })
            .collect()
    };
    let map = prepare("\t \n\n    \t\n", changes(&[1, 0, 4]));
    assert_eq!(
        map.map_one_with_extent(&edit((0, 0), (2, 1), "\u{1D11E}")),
        Some(edit((0, 0), (2, 5), "\t\u{1D11E}"))
    );
    let map = prepare("    \r\na\n x\n      \n x\r\n", changes(&[3, 0, 0, 4, 1]));
    assert_eq!(
        map.map_one_with_extent(&edit((3, 0), (5, 1), "x")),
        Some(edit((3, 0), (5, 0), "    x"))
    );
}

mod formatting {
    use super::*;

    /// V formatted through `map` by a formatter answering `edits` to P, the
    /// diff given `budget` steps of work; `None` when the map refuses the
    /// answer.
    fn formatted(
        map: &PreparedMap,
        virtual_text: &str,
        edits: &[TextEdit],
        budget: usize,
    ) -> Option<String> {
        let mapped = map.map_formatting(edits, budget).ok()?;
        Some(apply_to(virtual_text, &mapped))
    }

    /// [`formatted`] with work enough; with none, an answer changing
    /// anything is refused.
    fn formatted_either_way(
        map: &PreparedMap,
        virtual_text: &str,
        edits: &[TextEdit],
    ) -> Option<String> {
        let precise = formatted(map, virtual_text, edits, usize::MAX);
        let coarse = formatted(map, virtual_text, edits, 0);
        if precise.as_deref() != Some(virtual_text) {
            assert_eq!(coarse, None, "a diff out of work is refused");
        }
        precise
    }

    #[test]
    fn a_whitespace_line_the_peer_kept_shorter_than_the_indent_keeps_it() {
        // A whitespace-only line the peer kept, shorter than the indent.
        let virtual_text = "    if x:\n \n";
        let map = prepare(
            virtual_text,
            json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}, "newText": ""}]),
        );
        let answer = [edit((0, 0), (2, 0), "    if x:\n    \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("        if x:\n    \n")
        );
    }

    /// `virtual_text` with the indent `removed` (`""` for none) deleted
    /// from each line.
    fn dedent_lines(virtual_text: &str, removed: &[usize]) -> PreparedMap {
        let changes: Vec<_> = removed
            .iter()
            .enumerate()
            .filter(|&(_, &width)| width > 0)
            .map(|(line, &width)| {
                json!({"range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": width}}, "newText": ""})
            })
            .collect();
        prepare(virtual_text, json!(changes))
    }

    #[test]
    fn a_line_inserted_before_a_kept_whitespace_line_takes_the_content_indent() {
        // The kept ` ` is a line of its own, not the start of the new one.
        let virtual_text = "  if x:\n \n";
        let map = dedent_lines(virtual_text, &[2]);
        let answer = [edit((0, 0), (2, 0), "if x:\n  w\n \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  if x:\n    w\n \n")
        );
    }

    #[test]
    fn text_where_blank_lines_were_takes_the_contents_indent_or_is_refused() {
        // An empty line had nothing to remove: text written where it was
        // is the content's, indented as it is. Text where the kept ` ` was
        // (typed after its whitespace or not) is refused: typed on it or
        // written in its place, a whole-text answer does not tell.
        let virtual_text = "  a\n\n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 2]);
        let answer = [edit((0, 0), (3, 0), "a\ny\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n  y\n  b\n")
        );
        let virtual_text = "  a\n \n\n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 0, 2]);
        let answer = [edit((0, 0), (4, 0), "a\n x\ny\nb\n")];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
    }

    #[rstest::rstest]
    // Seeds of a provenance oracle: text typed right after a kept
    // whitespace line's whitespace, a line inserted in its place as well.
    #[case("  1,\r   \r  \r ", &[2, 0, 2, 1], "1,\r\n   }\r\n")]
    #[case("\t\t)\n\t\t\n", &[2, 0], ")\r\n\t\t}\r\n")]
    #[case("    f(a,b,c)\t\r    \t\n", &[4, 0], "f(a,b,c)\r    \t}\n")]
    fn text_typed_after_a_kept_whitespace_line_is_refused(
        #[case] virtual_text: &str,
        #[case] removed: &[usize],
        #[case] answer: &str,
    ) {
        let map = dedent_lines(virtual_text, removed);
        let lines = map.prepared_lines.text.split(['\n', '\r']).count() as u32;
        let answer = [edit((0, 0), (lines, 0), answer)];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
    }

    #[test]
    fn lines_repeating_a_lines_start_are_each_indented() {
        // `x` begins two written lines: whichever the diff keeps as the
        // old one, every line written gains the indent, none twice.
        let virtual_text = "  x\n  y\n";
        let map = dedent_lines(virtual_text, &[2, 2]);
        let answer = [edit((0, 0), (2, 0), "xa\nq\nxb\ny\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  xa\n  q\n  xb\n  y\n")
        );
    }

    /// Minified JSON of at least `bytes`, and the same pretty-printed, two
    /// spaces a level: they differ only in whitespace.
    fn json_pair(bytes: usize) -> (String, String) {
        let mut items = Vec::new();
        let mut length = 0;
        while length < bytes {
            let index = items.len();
            let item = format!(
                "{{\"key{index}\":[{index},{},\"v{index}\"],\"n\":{{\"x\":{index}}}}}",
                index * 7
            );
            length += item.len() + 1;
            items.push(item);
        }
        let minified = format!("[{}]", items.join(","));
        let value: serde_json::Value = serde_json::from_str(&minified).unwrap();
        (minified, serde_json::to_string_pretty(&value).unwrap())
    }

    /// A one-line JSON document in an indented block, and an answer
    /// pretty-printing it: V, its map, the answer, and V formatted.
    fn pretty_printed(bytes: usize) -> (String, PreparedMap, [TextEdit; 1], String) {
        let (minified, pretty) = json_pair(bytes);
        let virtual_text = format!("  {minified}\n");
        let map = dedent_lines(&virtual_text, &[2]);
        let answer = [edit((0, 0), (1, 0), &format!("{pretty}\n"))];
        let expected = pretty.lines().map(|line| format!("  {line}\n")).collect();
        (virtual_text, map, answer, expected)
    }

    #[test]
    fn a_large_pretty_printed_answer_maps() {
        // Some 25 KB rewritten into thousands of lines: every character
        // other than whitespace is kept, so pairing those first finds them
        // along one diagonal, and only the whitespace between them is
        // diffed.
        let (virtual_text, map, answer, expected) = pretty_printed(25_000);
        assert!(virtual_text.len() > 25_000);
        assert_eq!(
            map.map_formatting_answer(&answer)
                .ok()
                .map(|mapped| apply_to(&virtual_text, &mapped)),
            Some(expected)
        );
    }

    #[test]
    fn whitespace_rewritten_costs_work_linear_in_the_texts() {
        let (virtual_text, map, answer, expected) = pretty_printed(25_000);
        let budget = 4 * (virtual_text.len() + expected.len());
        assert_eq!(
            formatted(&map, &virtual_text, &answer, budget),
            Some(expected)
        );
    }

    #[test]
    fn the_work_an_answer_takes_decides_it_alike_every_time() {
        // No clock: the least budget mapping an answer maps it on every
        // run, and one step less refuses it on every run.
        let (_, map, answer, _) = pretty_printed(2_000);
        let maps = |budget| map.map_formatting(&answer, budget).ok();
        let (mut refusing, mut mapping) = (0, FORMATTING_DIFF_BUDGET);
        let mapped = maps(mapping);
        assert!(mapped.is_some());
        assert_eq!(maps(refusing), None);
        while mapping - refusing > 1 {
            let between = refusing + (mapping - refusing) / 2;
            if maps(between).is_some() {
                mapping = between;
            } else {
                refusing = between;
            }
        }
        for _ in 0..5 {
            assert_eq!(maps(refusing), None);
            assert_eq!(maps(mapping), mapped);
        }
    }

    #[rstest::rstest]
    // A kept whitespace line deleted, the next line rewritten.
    #[case::kept("  a\n \n  b\n", &[2, 0, 2], "a\nB\n", "  a\n  B\n")]
    // A whitespace line the peer cleared, deleted the same way.
    #[case::cleared("  a\n   \n  b\n", &[2, 3, 2], "a\nB\n", "  a\n  B\n")]
    // And with two lines made one beside it.
    #[case::merged("  a\n \n  b\n  c\n", &[2, 0, 2, 2], "a\nB C\n", "  a\n  B C\n")]
    // A line split, its second half joined onto the next line across a
    // blank line the answer removed, a cleared one: in its place.
    #[case::rejoined_cleared(
        "    ={x}y  =\n    \t=\n          \n    \te {\n",
        &[4, 4, 10, 4],
        "= { x\n\t} y = =\n\te {\n",
        "    = { x\n    \t} y = =\n    \te {\n"
    )]
    fn text_is_never_written_on_a_whitespace_line_the_answer_removed(
        #[case] virtual_text: &str,
        #[case] removed: &[usize],
        #[case] answer: &str,
        #[case] expected: &str,
    ) {
        let map = dedent_lines(virtual_text, removed);
        let lines = map.prepared_lines.text.split_inclusive('\n').count() as u32;
        let answer = [edit((0, 0), (lines, 0), answer)];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn text_typed_after_a_kept_whitespace_line_beside_lines_moved_is_refused() {
        // `1a1` moved after `2a2` lands beside the kept ` `, and ` w3`
        // starts with its whitespace: more lines than blank lines there,
        // whether ` w3` was typed on it or written in its place no answer
        // tells.
        let virtual_text = "0a0\n  1a1\n  2a2\n \n";
        let map = dedent_lines(virtual_text, &[0, 2, 2, 0]);
        let answer = [edit((0, 0), (4, 0), "0a0\n2a2\n1a1\n w3\n")];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
    }

    #[test]
    fn alike_lines_with_the_same_indent_stay_mapped() {
        // Alike lines with the same indent stay mapped.
        let virtual_text = "  }\n  a\n  }\n";
        let map = dedent_lines(virtual_text, &[2, 2, 2]);
        let answer = [edit((0, 0), (3, 0), "}\nb\n}\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  }\n  b\n  }\n")
        );
    }

    #[rstest::rstest]
    #[case::cleared("  a\n   \n  b\n", &[2, 2, 2], "a\nc\n\nb\n", "  a\n  c\n\n  b\n")]
    #[case::comment(
        "  a = 1\n      \n  b = 2\n",
        &[2, 2, 2],
        "a = 1\n# x\n\nb = 2\n",
        "  a = 1\n  # x\n\n  b = 2\n"
    )]
    #[case::sorted(
        "  import c\n  import b\n  import a\n      \n  y = 1\n",
        &[2, 2, 2, 2, 2],
        "import a\nimport b\nimport c\n\ny = 1\n",
        "  import a\n  import b\n  import c\n\n  y = 1\n"
    )]
    fn a_line_written_before_a_dedented_whitespace_line_the_answer_clears_maps(
        #[case] virtual_text: &str,
        #[case] removed: &[usize],
        #[case] answer: &str,
        #[case] expected: &str,
    ) {
        let map = dedent_lines(virtual_text, removed);
        let lines = map.prepared_lines.text.split('\n').count() as u32;
        let answer = [edit((0, 0), (lines, 0), answer)];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some(expected)
        );
    }

    #[rstest::rstest]
    // The closing indent the peer removed with the last line.
    #[case::closing_indent("  a\n  ", json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
        {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
    ]), "a")]
    #[case::nixfmt("    a\n  ", json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}, "newText": ""},
        {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
    ]), "a\nb")]
    fn an_answer_dropping_ps_final_line_break_before_a_deleted_closing_is_told(
        #[case] virtual_text: &str,
        #[case] changes: serde_json::Value,
        #[case] answer: &str,
    ) {
        // Formatting answers keep P's edges, restored before mapping; an
        // on-type answer does not: one dropping P's final line break would
        // edit the closing the peer deleted (taking its indent off, or
        // writing a line on it), which the on-type transform refuses.
        let map = prepare(virtual_text, changes);
        assert_eq!(map.prepared_lines.text, "a\n");
        let answer = [edit((0, 0), (1, 0), answer)];
        assert!(map.drops_the_break_before_the_closing(&answer));
        // Keeping the break, it does not.
        let kept = [edit((0, 0), (1, 0), "b\n")];
        assert!(!map.drops_the_break_before_the_closing(&kept));
    }

    #[test]
    fn text_where_a_kept_whitespace_line_was_is_refused() {
        // A line split, its second half joined onto the next across a kept
        // `  \t` the answer replaced: written on it (no indent) or in its
        // place (the content's), no answer tells. Refused, not guessed.
        let virtual_text = "  )  } =  (y\n  \t)  \n  \t\n  ,  in\n";
        let map = dedent_lines(virtual_text, &[2, 2, 0, 2]);
        let answer = [edit((0, 0), (4, 0), ") } =\n\t( y )\n, in\n")];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
        // Typed right after the kept whitespace too: a whole-text answer
        // does not tell that from a line written in the kept line's place.
        let virtual_text = "  a\n \n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 2]);
        let answer = [edit((0, 0), (3, 0), "a\n x\nb\n")];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
    }

    #[rstest::rstest]
    #[case::quotes(|index: usize| format!("x = 'a{index}'"), |index: usize| format!("x = \"a{index}\""))]
    #[case::call(
        |index: usize| format!("foo('a{index}', \"b\")"),
        |index: usize| format!("foo(\"a{index}\", \"b\");")
    )]
    #[case::object(
        |index: usize| format!("\"k{index}\":[1,2,{{'a':'b'}}],"),
        |index: usize| format!("\"k{index}\": [1, 2, {{\"a\": \"b\"}}],")
    )]
    fn every_line_rewritten_in_place_maps_in_linear_work(
        #[case] old_line: fn(usize) -> String,
        #[case] new_line: fn(usize) -> String,
    ) {
        // No line alike to another but for whitespace, every one changed:
        // lines alike in their letters and digits pair in order, each
        // diffed on its own, however many there are. One line split in
        // two in the middle leaves the lines on either side paired.
        let count = 20_000;
        let indented = |line: String| format!("  {line}\n");
        let virtual_text: String = (0..count).map(old_line).map(indented).collect();
        let map = dedent_lines(&virtual_text, &vec![2; count]);
        let split = count / 2;
        let formatted: String = (0..count)
            .map(|index| {
                let line = new_line(index);
                if index == split {
                    let (head, tail) = line.split_at(line.len() / 2);
                    format!("{head}\n{tail}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        let answer = [edit((0, 0), (count as u32, 0), &formatted)];
        let expected: String = formatted
            .lines()
            .map(|line| indented(line.to_string()))
            .collect();
        assert_eq!(
            map.map_formatting_answer(&answer)
                .map(|mapped| apply_to(&virtual_text, &mapped)),
            Ok(expected)
        );
    }

    #[rstest::rstest]
    #[case::lf("  a\n  b", "  a b")]
    #[case::crlf("  a\r\n  b", "  a b")]
    fn joining_the_last_line_with_no_line_break_maps(
        #[case] virtual_text: &str,
        #[case] expected: &str,
    ) {
        // The line joined away ends the document with no line break: its
        // removal goes with the join, not apart from it.
        let map = dedent_lines(virtual_text, &[2, 2]);
        let answer = [edit((0, 0), (1, 1), "a b")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn every_line_rewritten_and_empty_lines_removed_maps_in_linear_work() {
        // An empty line after every tenth, all of them removed, every line
        // of text rewritten: the lines still pair in order.
        let count = 20_000;
        let virtual_text: String = (0..count)
            .map(|index| {
                let line = format!("  \"k{index}\":[1,2,{{'a':'b'}}],\n");
                if index % 10 == 9 {
                    format!("{line}\n")
                } else {
                    line
                }
            })
            .collect();
        let removed: Vec<usize> = virtual_text
            .lines()
            .map(|line| if line.is_empty() { 0 } else { 2 })
            .collect();
        let map = dedent_lines(&virtual_text, &removed);
        let formatted: String = (0..count)
            .map(|index| format!("\"k{index}\": [1, 2, {{\"a\": \"b\"}}],\n"))
            .collect();
        let answer = [edit((0, 0), (removed.len() as u32, 0), &formatted)];
        let expected: String = formatted
            .lines()
            .map(|line| format!("  {line}\n"))
            .collect();
        assert_eq!(
            map.map_formatting_answer(&answer)
                .map(|mapped| apply_to(&virtual_text, &mapped)),
            Ok(expected)
        );
    }

    #[test]
    fn blank_lines_removed_whole_go_where_the_answer_leaves_them() {
        // `   ` dedented to ` ` between two lines of the content.
        let virtual_text = "  a\n   \n  b\n";
        let map = dedent_lines(virtual_text, &[2, 2, 2]);
        // `a` rewritten and the blank line gone: the blank line goes
        // whole, before what follows, not into `xxxa`.
        let answer = [edit((0, 0), (3, 0), "xxxa\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  xxxa\n  b\n")
        );
        // Everything rewritten as one line: the blank line goes with the
        // rest, none of it left behind.
        let answer = [edit((0, 0), (3, 0), "x\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  x\n")
        );
        // Two lines where the blank line was, neither starting with its
        // whitespace: lines of the content, the blank line gone.
        let answer = [edit((0, 0), (3, 0), "a\n     x\ny\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n       x\n  y\n  b\n")
        );
    }

    #[test]
    fn lines_found_more_than_once_pair_only_once_the_rest_is_placed() {
        // Nothing dedented: `}` twice in P, the answer joining and
        // respacing around them. `e let y` is `e  let  y` respaced, given
        // no indent of another line's.
        let virtual_text = "}\n      \n  }  \n e  let  y z( \"s\"";
        let map = dedent_lines(virtual_text, &[0, 0, 0, 0]);
        let answer = [edit(
            (0, 0),
            (4, 0),
            "} in import\n    }\ne let y\n    z ( \"s\"",
        )];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("} in import\n    }\ne let y\n    z ( \"s\"")
        );
        // `}` twice and `import e` once in P, `import e` twice in the
        // answer: the dedented whitespace line emptied is emptied.
        let virtual_text = "    }\n    import e\n    )in)\n      \n    }\n";
        let map = dedent_lines(virtual_text, &[4, 4, 4, 4, 4]);
        let answer = [edit((0, 0), (5, 0), "import e\n}\nimport e\n\n  }\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("    import e\n    }\n    import e\n\n      }\n")
        );
        // Copies on an evenly dedented region: once the lines found once are
        // placed, copies found as many times on each side pair in order,
        // and those found more often on one side do not; each answer below
        // maps to the one text every reading of it writes.
        let cases: [(&str, &[usize], &str, &str); 3] = [
            // `x` twice, one rewritten and one written over the kept ` `.
            ("x\nx\n \n", &[0, 0, 0], "xx\nx\nx\n", "xx\nx\nx\n"),
            // `}` twice, reindented and copied, `x` beside the kept ` `.
            (
                "  }\n  }\n \n",
                &[2, 2, 0],
                " }\n}\n}\nx\n",
                "   }\n  }\n  }\n  x\n",
            ),
            // `\t}` twice, one rewritten, the kept ` ` removed.
            (" \n\t}\n\t}\n", &[0, 0, 0], "}x\n}\n", "}x\n}\n"),
        ];
        for (virtual_text, removed, answer, expected) in cases {
            let map = dedent_lines(virtual_text, removed);
            let answer = [edit((0, 0), (4, 0), answer)];
            assert_eq!(
                formatted_either_way(&map, virtual_text, &answer).as_deref(),
                Some(expected),
                "{virtual_text:?}"
            );
        }
    }

    #[test]
    fn text_in_a_kept_whitespace_lines_place_is_refused_but_more_lines_there_are_the_contents() {
        // `     ` kept, rewritten line for line as `      c`: typed on it,
        // or a line in its place, no answer tells.
        let virtual_text = "  a\n     \n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 2]);
        let answer = [edit((0, 0), (3, 0), "a\n      c\nb\n")];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
        // With two lines there for the one blank line, neither starting
        // with exactly its whitespace, the blank line went and both are
        // lines of the content.
        let answer = [edit((0, 0), (3, 0), "a\n      c\nd\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n        c\n  d\n  b\n")
        );
    }

    #[test]
    fn answers_on_a_peer_dedenting_unevenly_are_refused() {
        // A line of the content the peer left alone, or dedented otherwise
        // than the rest: which line of a whole answer is which old one,
        // and so which indent each regains, only a pairing reads, and a
        // reading can cross copies or move a line off its own indent. Each
        // shape below is one a pairing once mapped, some to the wrong text.
        let cases: [(&str, &[usize], u32, &str); 22] = [
            // A line left alone deleted, the next reindented.
            ("0b c0\n  1a1\n", &[0, 2], 2, " 1a1\n"),
            // A blank line written after the content, a line left alone.
            ("    0x\n\n2x\n", &[4, 0, 0], 3, "0x\n  w\n\n2x\n\n"),
            // A line break turned CRLF above a line left alone.
            ("x\n  q\n  b\n", &[0, 0, 2], 3, "x\r\nq\nb\n"),
            // A line left alone beside a kept whitespace line removed.
            (
                "    y\n  a=f('q')\n      \n",
                &[4, 0, 4],
                3,
                "y\n  }\n  a=f(\"q\")\n",
            ),
            // A line left alone swapped with one of the content, also
            // reindented; two lines left alone swapped.
            ("  }\nk\n  }\n", &[2, 0, 2], 3, "}\n}\nk\n"),
            ("  }\nk\n  }\n", &[2, 0, 2], 3, "}\n}\n k\n"),
            ("a\nb\n  c\n", &[0, 0, 2], 3, "b\na\nc\n"),
            // Copies reindented so one reads as the other did.
            (
                "  x\n    foo(\n    foo(\n",
                &[2, 0, 2],
                3,
                "x\n        foo(\n    foo(\n",
            ),
            // Copies, one given punctuation and the other an indent.
            (
                "  x\na = 1\n  y\n    a = 1\n",
                &[2, 0, 2, 2],
                4,
                "x\na = 1,\ny\n    a = 1\n",
            ),
            (
                "  x\n\ta = 1\n  y\n  \t\ta = 1\n",
                &[2, 0, 2, 2],
                4,
                "x\n  a = 1,\ny\n    a = 1\n",
            ),
            // Copies of a line left alone written more or fewer times.
            (
                "\t\ta\rlet s = \"é\";\r\t\tlet s = \"é\"; \r\t\t)\r\t\tpass  \r",
                &[2, 0, 2, 2, 2],
                5,
                "let s = \"é\"; \r\n)\r\npass  \r\n",
            ),
            ("    a\n }\n    b\n", &[4, 0, 4], 3, "a\n }\n }\nb\n"),
            (
                "\t\ta\nlet s;\n\t\tlet s;\n\t\tb\n",
                &[2, 0, 2, 2],
                4,
                "let s;\nb\n",
            ),
            ("  f(a)\ny\ny\n  f(a)\n", &[2, 0, 0, 2], 5, "y\nf(a)\ny\n"),
            ("}\n  x\n   \n", &[0, 2, 2], 4, "x\nx\n\n"),
            // Lines alike but for whitespace, each left as it was.
            (
                "    x='a'\n    }\n  }\n    y='b'\n",
                &[4, 4, 0, 4],
                4,
                "x = \"a\"\n}\n  }\ny = \"b\"\n",
            ),
            // Lines joined, deleted or rewritten beside a line left alone.
            (" \n    1a1\n2a2\n", &[0, 2, 0], 3, "  1a1 2a2\n"),
            ("  0a0\r\n\t1b c1\r\n", &[2, 0], 2, "1b c1\r\n"),
            ("a\n  ab\n", &[0, 2], 2, "ab\n"),
            ("a\n  \tb\n", &[0, 2], 2, "  b\n"),
            ("0a0\n  1a1\n", &[0, 2], 2, "0a0\r\n"),
            (
                "0if x:0\n    1if x:1\n    2if x:2\n",
                &[0, 4, 4],
                3,
                "1if x:1 2if x:2\n",
            ),
        ];
        for (virtual_text, removed, lines, answer) in cases {
            let map = dedent_lines(virtual_text, removed);
            let answer = [edit((0, 0), (lines, 0), answer)];
            assert_eq!(
                map.map_formatting_answer(&answer),
                Err(FormattingRefusal::Shape),
                "{virtual_text:?}"
            );
        }
        // Indents removed that differ (no uniform one), even with every
        // line kept in place.
        let virtual_text = "  a\n    b\n";
        let map = dedent_lines(virtual_text, &[2, 4]);
        assert_eq!(
            map.map_formatting_answer(&[edit((0, 0), (2, 0), "A\nb\n")]),
            Err(FormattingRefusal::Shape)
        );
        // An answer changing nothing is no edit; whitespace lines left alone,
        // and a line starting with a gap's text, do not count.
        assert_eq!(
            map.map_formatting_answer(&[edit((0, 0), (2, 0), "a\nb\n")]),
            Ok(Vec::new())
        );
        let virtual_text = "  a\n \n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 2]);
        assert_eq!(
            formatted_either_way(&map, virtual_text, &[edit((0, 0), (3, 0), "a\n\nB\n")])
                .as_deref(),
            Some("  a\n\n  B\n")
        );
        let virtual_text = "  a\n     c\n  b\n";
        let dedent = |line: u32| json!({"range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 2}}, "newText": ""});
        let prepared = prepare_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..8, "${x}".to_string()),
                (SegmentKind::Content, 8..15, String::new()),
            ],
            json!({"segments": [
                {"type": "content", "changes": [dedent(0)]},
                {"type": "gap", "content": "None"},
                {"type": "content", "changes": [dedent(1)]}
            ]}),
        );
        let map = std::sync::Arc::try_unwrap(prepared.map).unwrap();
        assert_eq!(map.prepared_lines.text, "a\nNone c\nb\n");
        assert_eq!(
            formatted_either_way(
                &map,
                virtual_text,
                &[edit((0, 0), (3, 0), "a\nNone c\nB\n")]
            )
            .as_deref(),
            Some("  a\n     c\n  B\n")
        );
    }

    #[test]
    fn the_allowance_per_byte_counts_both_texts() {
        // Every line rewritten, a whitespace line after each: the blank
        // line table is quadratic in them, so 1,100 such lines need more
        // than the base budget and fifteen steps a byte, and no more than
        // sixteen, counted over P and the edited P alike.
        let count = 1_100;
        let virtual_text: String = (0..count).map(|line| format!("  a{line}\n  \n")).collect();
        let formatted: String = (0..count).map(|line| format!("b{line}\n\n")).collect();
        let map = dedent_by_two(&virtual_text);
        let answer = [edit((0, 0), (2 * count as u32, 0), &formatted)];
        assert!(map.map_formatting_answer(&answer).is_ok());
        assert_eq!(
            map.map_formatting_within(
                &answer,
                FORMATTING_DIFF_BUDGET,
                15,
                Some(FORMATTING_DIFF_BUDGET_CAP)
            ),
            Err(FormattingRefusal::OutOfWork)
        );
    }

    #[test]
    fn pairing_lines_alike_in_letters_and_digits_costs_work() {
        // Every line's punctuation rewritten: only their letters and digits
        // pair them, and that pairing is charged, a step a line and one for
        // every 64 bytes compared. 474 steps take the answer, 473 do not.
        let virtual_text: String = (0..10).map(|line| format!("  f{line}('a')\n")).collect();
        let formatted: String = (0..10).map(|line| format!("f{line}(\"a\");\n")).collect();
        let map = dedent_by_two(&virtual_text);
        let answer = [edit((0, 0), (10, 0), &formatted)];
        assert!(map.map_formatting(&answer, 474).is_ok());
        assert_eq!(
            map.map_formatting(&answer, 473),
            Err(FormattingRefusal::OutOfWork)
        );
    }

    #[test]
    fn non_ascii_text_in_many_stretches_maps_back_exactly() {
        // `["あ","é",...]` respaced after each comma: a stretch after every
        // comma of one line, each mapped back to UTF-16 columns.
        let virtual_text = "  [\"\u{3042}\",\"\u{e9}\",\"\u{1F600}\"]\n";
        let map = dedent_by_two(virtual_text);
        let answer = [edit(
            (0, 0),
            (1, 0),
            "[\"\u{3042}\", \"\u{e9}\", \"\u{1F600}\"]\n",
        )];
        let mapped = map.map_formatting_answer(&answer).unwrap();
        assert_eq!(
            mapped,
            vec![edit((0, 7), (0, 7), " "), edit((0, 11), (0, 11), " "),]
        );
        assert_eq!(
            apply_to(virtual_text, &mapped),
            "  [\"\u{3042}\", \"\u{e9}\", \"\u{1F600}\"]\n"
        );
    }

    #[test]
    fn a_long_non_ascii_line_in_many_stretches_converts_in_linear_work() {
        // One line of 20,000 `"あ"` elements respaced after each comma: some
        // 20,000 stretches on one line. Their ends and the answer's convert
        // in one pass each, never one by one rescanning the line.
        let count = 20_000;
        let elements = vec!["\"\u{3042}\""; count];
        let virtual_text = format!("  [{}]\n", elements.join(","));
        let formatted = format!("[{}]\n", elements.join(", "));
        let map = dedent_by_two(&virtual_text);
        let answer = [edit((0, 0), (1, 0), &formatted)];
        use super::super::super::{CONVERSIONS, SCANNED};
        CONVERSIONS.with(|conversions| conversions.set(0));
        SCANNED.with(|scanned| scanned.set(0));
        let mapped = map.map_formatting_answer(&answer).unwrap();
        assert_eq!(mapped.len(), count - 1);
        assert!(CONVERSIONS.with(|conversions| conversions.get()) <= 16);
        let scanned = SCANNED.with(|scanned| scanned.get());
        assert!(scanned <= 2 * virtual_text.len(), "{scanned}");
        assert_eq!(apply_to(&virtual_text, &mapped), format!("  {formatted}"));
        // The same answer as an edit after each comma: the answer's own
        // ends convert in one pass over the line too, each position from
        // the one before.
        let mut column = 1;
        let answer: Vec<TextEdit> = (1..count)
            .map(|_| {
                column += 4;
                edit((0, column), (0, column), " ")
            })
            .collect();
        CONVERSIONS.with(|conversions| conversions.set(0));
        SCANNED.with(|scanned| scanned.set(0));
        let mapped = map.map_formatting_answer(&answer).unwrap();
        assert!(CONVERSIONS.with(|conversions| conversions.get()) <= 16);
        let scanned = SCANNED.with(|scanned| scanned.get());
        assert!(scanned <= 2 * virtual_text.len(), "{scanned}");
        assert_eq!(apply_to(&virtual_text, &mapped), format!("  {formatted}"));
    }

    #[test]
    fn a_line_inserted_above_an_emptied_whitespace_line_maps_with_its_breaks_switched() {
        // The whitespace line `  ` emptied by the peer, the answer switching
        // CRLF to LF (prettier's default) and inserting `y = 2` above it:
        // the switched break before it and the insertion both meet the
        // indent the emptied line had, and each leaves it to the next.
        let virtual_text = "  x = 1\r\n  \r\n  foo(a,b)\r\n";
        let map = dedent_lines(virtual_text, &[2, 2, 2]);
        assert_eq!(map.prepared_lines.text, "x = 1\r\n\r\nfoo(a,b)\r\n");
        let answer = [edit((0, 0), (4, 0), "x = 1\ny = 2\n\nfoo(a, b)\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  x = 1\n  y = 2\n\n  foo(a, b)\n")
        );
        // Without the insertion, as before.
        let answer = [edit((0, 0), (4, 0), "x = 1\n\nfoo(a, b)\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  x = 1\n\n  foo(a, b)\n")
        );
    }

    #[test]
    fn the_final_break_is_judged_on_the_whole_answer() {
        // `  a`, a blank line, then the closing indent the peer deleted:
        // P is `a\n\n`.
        let virtual_text = "  a\n\n  ";
        let map = prepare(
            virtual_text,
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]),
        );
        assert_eq!(map.prepared_lines.text, "a\n\n");
        // Both line breaks deleted by two edits: the last one alone, read
        // against P, seems to leave `a\n`, but nothing breaks the line.
        assert!(map.drops_the_break_before_the_closing(&[
            edit((0, 1), (1, 0), ""),
            edit((1, 0), (2, 0), ""),
        ]));
        // The line breaks deleted, an earlier edit writing one in their
        // place: the answer still ends with a line break.
        assert!(!map.drops_the_break_before_the_closing(&[
            edit((0, 0), (0, 1), "b\n"),
            edit((0, 1), (2, 0), ""),
        ]));
    }

    #[test]
    fn text_after_p_s_final_break_or_its_loss_is_told_apart_from_answers_that_keep_it() {
        // `\t  x` dedented by three, then the closing indent `\t` the peer
        // deleted, shallower than the content: P is `x\r\n`.
        let virtual_text = "\t  x\r\n\t";
        let map = dedent_lines(virtual_text, &[3, 1]);
        assert_eq!(map.prepared_lines.text, "x\r\n");
        // Text after P's final break, written in whole or inserted, or the
        // break dropped: mapped, it would edit the closing (`\tfoo(a,b)`,
        // not the content's `\t  foo(a,b)`), so an on-type answer, whose
        // edges are not restored, is refused.
        for answer in [
            [edit((0, 0), (1, 0), "x\r\nfoo(a,b)")],
            [edit((1, 0), (1, 0), "foo(a,b)")],
            [edit((0, 1), (1, 0), "\r\nfoo(a,b)")],
            [edit((0, 0), (1, 0), "y")],
            [edit((0, 1), (1, 0), "")],
        ] {
            assert!(
                map.drops_the_break_before_the_closing(&answer),
                "{answer:?}"
            );
        }
        // Answers ending with a line break, or not reaching P's end, keep it.
        for answer in [
            [edit((0, 0), (0, 1), "b")],
            [edit((0, 0), (1, 0), "x\r\nfoo(a,b)\r\n")],
            [edit((1, 0), (1, 0), "foo(a,b)\n")],
            [edit((0, 1), (1, 0), "\n")],
            [edit((0, 0), (0, 0), "w\r\n")],
        ] {
            assert!(
                !map.drops_the_break_before_the_closing(&answer),
                "{answer:?}"
            );
        }
        // With no closing deleted after P, nothing there to edit.
        let virtual_text = "\t  x\r\n";
        let map = dedent_lines(virtual_text, &[3]);
        assert!(!map.drops_the_break_before_the_closing(&[edit((1, 0), (1, 0), "foo(a,b)")]));
    }

    #[test]
    fn copies_and_rewrites_among_lines_left_alone_pair_by_what_they_hold() {
        // Copies of `y` and `a = 1` around lines rewritten or written anew:
        // each line keeps its own extra indentation.
        let virtual_text = "  y\n \n    y\n  f('a')\n  foo(\n";
        let map = dedent_lines(virtual_text, &[2, 0, 2, 2, 2]);
        let answer = [edit((0, 0), (5, 0), "a = 1\n}\n  y\na = 1\n    a = 1\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a = 1\n  }\n    y\n  a = 1\n      a = 1\n")
        );
        let virtual_text = "  a = 1\n  x\n  f(\"a\");\n  a = 1,\n \n";
        let map = dedent_lines(virtual_text, &[2, 2, 2, 2, 0]);
        let answer = [edit((0, 0), (5, 0), "}\n  a = 1,\na = 1\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  }\n    a = 1,\n  a = 1\n")
        );
        // A line written twice, its copy reindented: mapped, not refused.
        let virtual_text = "  x\n  }\n    f(\"a\");\n  f(\"a\");\n \n";
        let map = dedent_lines(virtual_text, &[2, 2, 2, 2, 0]);
        let answer = [edit(
            (0, 0),
            (5, 0),
            "f(\"a\");\n  a = 1\n    f(\"a\");\n  f(\"a\");\n",
        )];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  f(\"a\");\n    a = 1\n      f(\"a\");\n    f(\"a\");\n")
        );
    }

    #[test]
    fn long_lines_cost_their_length_in_every_pairing_stage() {
        // 200-character lines: alike to the byte, alike in letters and
        // digits (quotes rewritten), joined, copied and rewritten. Each
        // stage that hashes or compares them charges their length: 1,138
        // steps take the answer, 1,137 do not.
        let long = |c: char| c.to_string().repeat(200);
        let virtual_text: String = [
            format!("{}1", long('a')),
            format!("{}('x')", long('b')),
            long('c'),
            long('d'),
            format!("{}1", long('a')),
            format!("{}2", long('e')),
        ]
        .iter()
        .map(|line| format!("  {line}\n"))
        .collect();
        let formatted: String = [
            format!("{}1", long('a')),
            format!("{}(\"x\");", long('b')),
            format!("{} {}", long('c'), long('d')),
            format!("{}1", long('a')),
            format!("{}3", long('e')),
        ]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
        let map = dedent_by_two(&virtual_text);
        let answer = [edit((0, 0), (6, 0), &formatted)];
        assert!(map.map_formatting(&answer, 1_138).is_ok());
        assert_eq!(
            map.map_formatting(&answer, 1_137),
            Err(FormattingRefusal::OutOfWork)
        );
    }

    #[test]
    fn the_budget_stops_at_its_cap_however_large_the_texts() {
        // 1.2 MB of lines alike, then 4,100 lines rewritten with whitespace
        // lines between: the blank line table needs more than 2^24 steps,
        // which the allowance per byte of these texts would give, but the
        // cap does not.
        let count = 4_100;
        let head = |indent: &str| -> String {
            (0..12_000)
                .map(|line| format!("{indent}{}{line}\n", "h".repeat(90)))
                .collect()
        };
        let virtual_text = format!(
            "{}{}",
            head("  "),
            (0..count)
                .map(|line| format!("  a{line}\n  \n"))
                .collect::<String>()
        );
        let formatted = format!(
            "{}{}",
            head(""),
            (0..count)
                .map(|line| format!("b{line}\n\n"))
                .collect::<String>()
        );
        let map = dedent_by_two(&virtual_text);
        let answer = [edit((0, 0), ((12_000 + 2 * count) as u32, 0), &formatted)];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::OutOfWork)
        );
        let uncapped = FORMATTING_DIFF_BUDGET
            + FORMATTING_DIFF_WORK_PER_BYTE * (map.prepared_lines.text.len() + formatted.len());
        assert!(uncapped > FORMATTING_DIFF_BUDGET_CAP);
        assert!(map.map_formatting(&answer, uncapped).is_ok());
    }

    #[test]
    fn long_whitespace_lines_cost_their_length_in_the_blank_line_table() {
        // 50 whitespace lines of 10,000 spaces between two lines rewritten:
        // nothing alike at the ends is set aside, and the blank lines pair
        // among themselves. Comparing them costs their length, not a step
        // a cell: the answer takes at least a step per 64 bytes of them.
        let count = 50;
        let blank = " ".repeat(10_000);
        let virtual_text = format!("a\n{}z\n", format!("{blank}\n").repeat(count));
        let formatted = format!("b\n{}c\n", format!("{blank}\n").repeat(count));
        let map = dedent_lines(&virtual_text, &[]);
        let answer = [edit((0, 0), (count as u32 + 2, 0), &formatted)];
        let bytes = 2 * count * blank.len();
        assert_eq!(
            map.map_formatting(&answer, bytes / BYTES_PER_STEP),
            Err(FormattingRefusal::OutOfWork)
        );
        // Mapped with the work their bytes and the table's cells take.
        assert_eq!(
            map.map_formatting(
                &answer,
                2 * bytes / BYTES_PER_STEP + 4 * (count + 1) * (count + 1)
            )
            .map(|mapped| apply_to(&virtual_text, &mapped)),
            Ok(formatted.clone())
        );
    }

    #[test]
    fn lines_reordered_keep_their_places_and_indents() {
        // Patience keeps the longest run of unique lines in order: `b`
        // and `c` swapped cannot both anchor.
        let virtual_text = "  a\n  b\n  c\n  d\n  x y\n";
        let map = dedent_lines(virtual_text, &[2; 5]);
        let answer = [edit((0, 0), (5, 0), "a\nc\nb\nd\nx y\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n  c\n  b\n  d\n  x y\n")
        );
    }

    #[rstest::rstest]
    #[case("a\na\nb\n", "  a\n  a\n  b\n")]
    #[case("b\na\na\n", "  b\n  a\n  a\n")]
    #[case("x\na\na\n", "  x\n  a\n  a\n")]
    fn a_line_written_twice_is_not_unique(#[case] answer: &str, #[case] expected: &str) {
        // Found once in P but twice in the answer, `a` anchors nothing: an
        // anchor is a line found once on each side.
        let virtual_text = "  a\n  b\n";
        let map = dedent_lines(virtual_text, &[2, 2]);
        let answer = [edit((0, 0), (2, 0), answer)];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn a_long_line_respaced_is_diffed_between_its_unchanged_characters() {
        // 25 KB of JSON on one line, spaced after each `,` and `:`: the
        // characters other than whitespace pair along one diagonal, and
        // only the spaces between them are diffed. Anchoring on every
        // character, or on none, takes millions of steps.
        let (minified, _) = json_pair(25_000);
        let spaced = minified.replace(',', ", ").replace(':', ": ");
        let virtual_text = format!("  {minified}\n");
        let map = dedent_lines(&virtual_text, &[2]);
        let answer = [edit((0, 0), (1, 0), &format!("{spaced}\n"))];
        assert_eq!(
            map.map_formatting(&answer, 50_000)
                .map(|mapped| apply_to(&virtual_text, &mapped)),
            Ok(format!("  {spaced}\n"))
        );
    }

    #[test]
    fn a_removed_indent_of_tabs_is_left_to_the_next_stretch() {
        let virtual_text = "\r\n\t\t\t\t \n\t\t\t\t  q";
        let map = dedent_lines(virtual_text, &[0, 4, 4]);
        let answer = [edit((0, 0), (2, 3), " \n\n  q")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("\t\t\t\t \n\n\t\t\t\t  q")
        );
    }

    #[test]
    fn blank_lines_pair_as_many_as_can_before_any_alike() {
        // Pairing `\t` with ` ` and so on down the line pairs all five; any
        // pairing more alike pairs fewer.
        let keys = |lines: &[&str]| -> LineKeys {
            let lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
            let trimmed = lines.iter().map(|line| line.trim().to_string()).collect();
            [lines.clone(), trimmed, lines.clone(), lines]
        };
        let old = keys(&["\t", " ", "  ", "   ", "    "]);
        let new = keys(&[" ", "  ", "   ", "    ", "\t\t"]);
        assert_eq!(
            blank_lines(&old, &new, &(0..5), &(0..5), &mut Work::new(usize::MAX)),
            Some((0..5).map(|line| (line, line)).collect())
        );
    }

    #[test]
    fn a_start_past_the_last_line_names_p_s_end_only_as_an_empty_range() {
        // P "if x:\n  y\n" has lines 0 to 2; line 3, just past them, names
        // P's end, as whole-document answers do. A start there with an end
        // before it is reversed, and one further on names nothing.
        let (_, map) = dedented();
        let past = map.prepared_lines.text.lines().count() as u32 + 1;
        for answer in [
            [edit((past + 5, 0), (past, 0), "x")],
            [edit((past, 1), (past, 1), "x")],
            [edit((past + 1, 0), (past + 1, 0), "x")],
        ] {
            assert_eq!(
                map.map_formatting_answer(&answer),
                Err(FormattingRefusal::Shape),
                "{answer:?}"
            );
        }
        // An insertion at P's end is that.
        let answer = [edit((past, 0), (past, 0), "z\n")];
        assert_eq!(
            map.map_formatting_answer(&answer)
                .map(|mapped| apply_to("  if x:\n    y\n", &mapped)),
            Ok("  if x:\n    y\n  z\n".to_string())
        );
    }

    #[test]
    fn kept_whitespace_lines_none_is_typed_on_cost_work_linear_in_them() {
        // 8,000 kept `\t` lines replaced by 8,001 lines of text, none typed
        // after a tab: which line each kept line keeps is looked up, not
        // searched for through every line of the answer.
        let count = 8_000;
        let virtual_text = format!("  a\n{}  b\n", "\t\n".repeat(count));
        let mut removed = vec![2];
        removed.extend(std::iter::repeat_n(0, count));
        removed.push(2);
        let map = dedent_lines(&virtual_text, &removed);
        let formatted = format!("a\n{}b\n", "x\n".repeat(count + 1));
        let answer = [edit((0, 0), (count as u32 + 2, 0), &formatted)];
        let budget = 40 * (virtual_text.len() + formatted.len());
        assert!(map.map_formatting(&answer, budget).is_ok());
    }

    #[test]
    fn a_small_answer_in_a_large_region_costs_work_for_its_size() {
        // One line changed among 50,000: the lines alike at either end are
        // set aside before any line is keyed or paired, uncharged.
        let count = 50_000;
        let virtual_text: String = (0..count).map(|index| format!("  x{index}\n")).collect();
        let map = dedent_lines(&virtual_text, &vec![2; count]);
        let line = (count / 2) as u32;
        let answer = [edit((line, 0), (line, 0), "y")];
        let mapped = map
            .map_formatting(&answer, 1_000)
            .map(|mapped| apply_to(&virtual_text, &mapped));
        let expected = virtual_text.replacen(
            &format!("  x{}\n", count / 2),
            &format!("  yx{}\n", count / 2),
            1,
        );
        assert_eq!(mapped, Ok(expected));
        // One character inserted in a line of 1.1 MB: the line is the
        // stretch that differs, its keys hashed once at a step for every 64
        // bytes, and the text alike at its ends set aside uncharged.
        let long = "a".repeat(1_100_000);
        let virtual_text = format!("  {long}\n");
        let map = dedent_lines(&virtual_text, &[2]);
        let answer = [edit((0, 550_000), (0, 550_000), "b")];
        let mut expected = virtual_text.clone();
        expected.insert(2 + 550_000, 'b');
        // (Some fifteen passes over the line: its keys and their lookups.)
        let budget = 16 * 1_100_000 / 64;
        assert_eq!(
            map.map_formatting(&answer, budget)
                .map(|mapped| apply_to(&virtual_text, &mapped)),
            Ok(expected)
        );
    }

    /// `count` assignments written tight, and as a formatter spacing every
    /// one: the same text but for whitespace inside each line.
    fn spaced_assignments(count: usize) -> (String, String) {
        (
            (0..count).map(|index| format!("x{index}=1\n")).collect(),
            (0..count).map(|index| format!("x{index} = 1\n")).collect(),
        )
    }

    #[rstest::rstest]
    #[case(1_500)]
    #[case(20_000)]
    fn every_line_respaced_maps(#[case] count: usize) {
        // No line is alike to another but for its edges, and none starts
        // with an old one: lines alike but for whitespace anywhere pair
        // first, each diffed by character on its own.
        let (old, new) = spaced_assignments(count);
        let map = dedent_lines(&old, &[]);
        let answer = [edit((0, 0), (count as u32, 0), &new)];
        assert_eq!(
            map.map_formatting_answer(&answer)
                .map(|mapped| apply_to(&old, &mapped)),
            Ok(new)
        );
    }

    #[test]
    fn respacing_costs_work_linear_in_the_lines_and_decides_alike() {
        let (old, new) = spaced_assignments(5_000);
        let map = dedent_lines(&old, &[]);
        let answer = [edit((0, 0), (5_000, 0), &new)];
        let budget = 4 * (old.len() + new.len());
        let first = map.map_formatting(&answer, budget);
        assert!(first.is_ok());
        for _ in 0..3 {
            assert_eq!(map.map_formatting(&answer, budget), first);
        }
    }

    #[test]
    fn lines_rewritten_with_whitespace_lines_between_cost_no_pairwise_search() {
        // Every text line changed (no two alike, none joined), whitespace
        // lines between: looking for joins costs a lookup for each line
        // and length, not a comparison with every other line.
        let texts = |count: usize, letter: char| -> String {
            (0..count)
                .map(|index| format!("{letter}{index}\n \n"))
                .collect()
        };
        let count = 3_000;
        let (old, new) = (texts(count, 'a'), texts(count, 'b'));
        let mut work = Work::new(20 * count);
        let (old_lines, new_lines) = (lines_with_breaks(&old), lines_with_breaks(&new));
        let keys = |lines: &[(usize, &str)]| -> LineKeys {
            let content: Vec<String> = lines
                .iter()
                .map(|&(_, line)| line.trim_end_matches(['\r', '\n']).to_string())
                .collect();
            let trimmed = content.iter().map(|line| line.trim().to_string()).collect();
            let bare = content.iter().map(|line| line.replace(' ', "")).collect();
            let alphanumeric = content
                .iter()
                .map(|line| line.chars().filter(|c| c.is_alphanumeric()).collect())
                .collect();
            [content, trimmed, bare, alphanumeric]
        };
        let pairs = joined_lines(
            &keys(&old_lines),
            &keys(&new_lines),
            &(0..old_lines.len()),
            &(0..new_lines.len()),
            &mut work,
        );
        assert_eq!(pairs, Some(vec![]));
        // What bounds such an answer is the table pairing its whitespace
        // lines, quadratic in them: some 1,100 fit the default budget.
        let accepted = |count: usize| {
            let (old, new) = (texts(count, 'a'), texts(count, 'b'));
            let map = dedent_lines(&old, &[]);
            let answer = [edit((0, 0), (2 * count as u32, 0), &new)];
            map.map_formatting_answer(&answer)
                .map(|mapped| apply_to(&old, &mapped) == new)
        };
        assert_eq!(accepted(900), Ok(true));
        assert_eq!(accepted(1_500), Err(FormattingRefusal::OutOfWork));
    }

    #[test]
    fn a_refusal_says_whether_shape_or_size_decided_it() {
        // Lines dedented by two and by four: a new line has no one indent.
        let map = dedent_lines("  a\n    b\n", &[2, 4]);
        let answer = [edit((0, 0), (2, 0), "a\nb\nc\n")];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::Shape)
        );
        // Text rewritten past any likeness: the diff runs out of work.
        let mut seed = 42u64;
        let mut text = |length: usize| -> String {
            (0..length)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    char::from(b'a' + (seed >> 60) as u8 % 4)
                })
                .collect()
        };
        let (old, new) = (text(20_000), text(20_000));
        let virtual_text = format!("  {old}\n");
        let map = dedent_lines(&virtual_text, &[2]);
        let answer = [edit((0, 0), (1, 0), &format!("{new}\n"))];
        assert_eq!(
            map.map_formatting_answer(&answer),
            Err(FormattingRefusal::OutOfWork)
        );
    }

    #[test]
    fn pairing_joined_lines_stops_when_out_of_work() {
        // Indexing the old lines and each lookup are spent, and out of
        // work the search refuses, not runs on.
        let keys = |lines: &[&str]| -> LineKeys {
            let lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
            [lines.clone(), lines.clone(), lines.clone(), lines]
        };
        let (old, new) = (keys(&["a"]), keys(&["a b"]));
        let pairs = |budget| joined_lines(&old, &new, &(0..1), &(0..1), &mut Work::new(budget));
        assert_eq!(pairs(2), Some(vec![(0, 0)]));
        assert_eq!(pairs(1), None);
    }

    #[test]
    fn pairing_blank_lines_takes_a_table_only_as_large_as_its_work() {
        // Every blank line's whitespace is numbered, a step each here, and
        // every cell of the table is spent before it is taken: a run of
        // blank lines just within the budget pairs, one just past it
        // refuses at once, not held in memory.
        let keys = |count: usize| -> LineKeys {
            let lines = vec![String::new(); count];
            [lines.clone(), lines.clone(), lines.clone(), lines]
        };
        let small = (keys(3), keys(2));
        let pairs =
            |budget| blank_lines(&small.0, &small.1, &(0..3), &(0..2), &mut Work::new(budget));
        assert_eq!(pairs(5 + 12), Some(vec![(0, 0), (1, 1)]));
        assert_eq!(pairs(5 + 11), None);
        // The largest square run the default budget holds pairs whole.
        let side = (1 << 10) - 2;
        let large = keys(side);
        let mut work = Work::new(FORMATTING_DIFF_BUDGET);
        let paired = blank_lines(&large, &large, &(0..side), &(0..side), &mut work);
        assert_eq!(paired.map(|pairs| pairs.len()), Some(side));
        let side = (1 << 10) - 1;
        let large = keys(side);
        let mut work = Work::new(FORMATTING_DIFF_BUDGET);
        assert_eq!(
            blank_lines(&large, &large, &(0..side), &(0..side), &mut work),
            None
        );
        // And so the whole answer, its blank lines all rewritten (the
        // budget grows with the texts, a little).
        let side = 1_100;
        let virtual_text = format!("  a\n{}", "\n".repeat(side));
        let map = dedent_lines(&virtual_text, &[2]);
        let formatted = format!("b\n{}", " \n".repeat(side));
        let answer = [edit((0, 0), (side as u32 + 1, 0), &formatted)];
        assert_eq!(map.map_formatting_answer(&answer).ok(), None);
    }

    #[test]
    fn lines_broken_by_a_lone_cr_are_paired_as_lines() {
        // A kept ` ` joined onto `a` (dedented), lines broken by a lone CR:
        // the joined line is `a`'s, its removed indent back.
        let virtual_text = " \r  a\r\r";
        let map = dedent_lines(virtual_text, &[0, 2, 0]);
        let answer = [edit((0, 0), (3, 0), "  a\r\r")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("    a\r\r")
        );
    }

    #[test]
    fn a_lone_cr_turned_crlf_and_back_maps_whole() {
        let virtual_text = "  a\r  b\r";
        let map = dedent_lines(virtual_text, &[2, 2]);
        let answer = [edit((0, 0), (2, 0), "a\r\nb\r\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\r\n  b\r\n")
        );
        let virtual_text = "  a\r\n  b\r\n";
        let map = dedent_lines(virtual_text, &[2, 2]);
        let answer = [edit((0, 0), (2, 0), "a\rb\r")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\r  b\r")
        );
    }

    #[test]
    fn blank_lines_pair_as_many_as_can_then_alike_first() {
        // Two kept ` ` lines and an empty one, rewritten ` `, empty, ` `:
        // all three pair in order (as many as can), the empty line written
        // ` ` taking the content's indent as text written on an empty line.
        let virtual_text = "  a\n \n \n\n";
        let map = dedent_lines(virtual_text, &[2, 0, 0, 0]);
        let answer = [edit((0, 0), (4, 0), "a\n \n\n \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n \n\n   \n")
        );
        // Of an empty line and a kept ` `, one going: the ` ` the answer
        // keeps is the kept one, not the empty line written ` `.
        let virtual_text = "  a\n\n \n";
        let map = dedent_lines(virtual_text, &[2, 0, 0]);
        let answer = [edit((0, 0), (3, 0), "a\n \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n \n")
        );
    }

    #[test]
    fn text_written_beside_a_kept_whitespace_line_is_a_line_of_its_own() {
        // `  ` and ` ` kept; the answer writes `w` above ` ` and empties
        // the other: both blank lines pair, `w` is a created line,
        // indented as the content, not text written on the kept `  `.
        let virtual_text = "  a\n  \n \n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 0, 2]);
        let answer = [edit((0, 0), (4, 0), "a\nw\n \n\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n  w\n \n\n  b\n")
        );
    }

    #[test]
    fn text_written_beside_blank_lines_is_a_line_of_its_own() {
        // ` ` kept and `   ` dedented to ` `; the answer writes `w` above
        // them and widens the last: the blank lines pair with the blank
        // lines, and `w` is a created line, indented as the content.
        let virtual_text = "  a\n \n   \n";
        let map = dedent_lines(virtual_text, &[2, 0, 2]);
        let answer = [edit((0, 0), (3, 0), "a\nw\n \n  \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n  w\n \n    \n")
        );
    }

    #[test]
    fn a_blank_line_given_other_whitespace_is_the_line_it_was() {
        // The kept ` ` widened to two spaces, a line inserted above it:
        // the inserted line is the content's, the blank line its own.
        let virtual_text = "  a\n \n";
        let map = dedent_lines(virtual_text, &[2, 0]);
        let answer = [edit((0, 0), (2, 0), "a\nw\n  \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n  w\n  \n")
        );
    }

    #[test]
    fn a_line_written_beside_a_widened_blank_line_is_one_of_its_own() {
        // `  w` inserted, the kept ` ` widened to two spaces: the blank
        // lines pair, so `w` is a created line (content indent and its own
        // two), not text written on the kept blank line.
        let virtual_text = "  a\n \n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 2]);
        let answer = [edit((0, 0), (3, 0), "a\n  w\n  \nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n    w\n  \n  b\n")
        );
    }

    #[test]
    fn a_stretch_ending_at_a_line_start_leaves_its_indent_to_the_next() {
        // A stretch ending at a dedented line's start takes that line's
        // removed indent along and gives it back; one starting there (a
        // blank line inserted, a whitespace-only line cleared) claims the
        // same indent. They map to touching edits, not overlapping ones.
        for (virtual_text, removed, formatted, expected) in [
            ("  a\r\n  c\r\n", &[2, 2][..], "a\n\nc\n", "  a\n\n  c\n"),
            (
                "  a\r\n    \r\n  c\r\n",
                &[2, 2, 2],
                "a\n\nc\n",
                "  a\n\n  c\n",
            ),
            (
                "  a\n  c\n",
                &[2, 2],
                "a\r\n\r\nc\r\n",
                "  a\r\n\r\n  c\r\n",
            ),
            ("  a\r  b\r", &[2, 2], "a\n\nb\n", "  a\n\n  b\n"),
            (
                "  a\n  b\n   \n  c\n",
                &[2, 2, 2, 2],
                "a b\n\nc\n",
                "  a b\n\n  c\n",
            ),
            (
                "  x = [\n    1,\n  ]\n    \n  c\n",
                &[2, 2, 2, 2, 2],
                "x = [1]\n\nc\n",
                "  x = [1]\n\n  c\n",
            ),
        ] {
            let map = dedent_lines(virtual_text, removed);
            let lines = map.prepared_lines.text.lines().count() as u32 + 1;
            let answer = [edit((0, 0), (lines, 0), formatted)];
            assert_eq!(
                formatted_either_way(&map, virtual_text, &answer).as_deref(),
                Some(expected),
                "{virtual_text:?} -> {formatted:?}"
            );
        }
    }

    #[test]
    fn blank_lines_rewritten_in_order_map_without_both_claiming_an_indent() {
        // Blank lines rewritten in order: the empty line written ` ` (text
        // on an empty line, taking the content's indent), the dedented ` `
        // emptied. The stretches touch at the second line's start, each
        // taking its removed indent along: mapped, they touch, not overlap.
        let virtual_text = "\r\n     \n      q";
        let map = dedent_lines(virtual_text, &[0, 4, 4]);
        assert_eq!(map.prepared_lines.text, "\r\n \n  q");
        let answer = [edit((0, 0), (3, 0), " \n\n  q")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("     \n\n      q")
        );
    }

    #[test]
    fn a_line_break_changed_before_text_written_on_a_line_keeps_them_apart() {
        // CRLF turned to LF on `a`, text written where the empty line below
        // it was: that text is a line of the content, not one the changed
        // break writes anew.
        let virtual_text = "\ta\r\n\r\n";
        let map = dedent_lines(virtual_text, &[1, 0]);
        let answer = [edit((0, 0), (2, 0), "a\nw\r\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("\ta\n\tw\r\n")
        );
    }

    #[test]
    fn a_whole_document_answer_maps_back_reindented() {
        let (virtual_text, map) = dedented();
        assert_eq!(map.prepared_lines.text, "if x:\n  y\n");
        let answer = [edit((0, 0), (2, 0), "if x:\n    y\n    z\n")];
        assert_eq!(
            formatted_either_way(&map, &virtual_text, &answer).as_deref(),
            Some("  if x:\n      y\n      z\n")
        );
        // Only what changed is edited: the first line is left alone.
        let mapped = map.map_formatting_answer(&answer).ok().unwrap();
        assert!(mapped.iter().all(|edit| edit.range.start.line >= 1));
    }

    #[test]
    fn an_answer_changing_nothing_maps_to_no_edit() {
        let (_, map) = dedented();
        let answer = [edit((0, 0), (2, 0), "if x:\n  y\n")];
        assert_eq!(map.map_formatting_answer(&answer).ok(), Some(vec![]));
        assert_eq!(map.map_formatting_answer(&[]).ok(), Some(vec![]));
        // Nothing to diff: not refused for want of work.
        assert_eq!(map.map_formatting(&answer, 0).ok(), Some(vec![]));
    }

    #[test]
    fn a_whole_document_range_may_end_on_the_line_past_the_last() {
        let (virtual_text, map) = dedented();
        // P has three lines, the last empty: (3, 0) names its end.
        let answer = [edit((0, 0), (3, 0), "if  x:\n  y\n")];
        assert_eq!(
            formatted_either_way(&map, &virtual_text, &answer).as_deref(),
            Some("  if  x:\n    y\n")
        );
        // Only that exact place: not a column on it, nor a line further.
        for end in [(3, 1), (4, 0)] {
            assert_eq!(
                map.map_formatting_answer(&[edit((0, 0), end, "")]).ok(),
                None,
                "{end:?}"
            );
        }
        // An extent names no such place.
        assert_eq!(
            map.map_edits(&[edit((0, 0), (3, 0), "")], EditMode::Extent),
            None
        );
    }

    #[test]
    fn crlf_turned_to_lf_maps_without_splitting_a_line_break() {
        let virtual_text = "  a\r\n  b\r\n";
        let map = dedent_by_two(virtual_text);
        assert_eq!(map.prepared_lines.text, "a\r\nb\r\n");
        let answer = [edit((0, 0), (2, 0), "a\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n  b\n")
        );
    }

    #[test]
    fn lines_created_without_one_indent_to_regain_are_refused() {
        // Line 0 lost two spaces, line 1 four: no indent for a new line
        // (nor, a formatting answer, for any line: see
        // `answers_on_a_peer_dedenting_unevenly_are_refused`).
        let virtual_text = "  a\n    b\n";
        let map = prepare(
            virtual_text,
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
            ]),
        );
        let answer = [edit((0, 0), (2, 0), "a\nb\nc\n")];
        assert_eq!(map.map_formatting_answer(&answer).ok(), None);
    }

    #[test]
    fn deleted_edge_lines_stay_whatever_the_answer() {
        let (virtual_text, map) = leading_blank_dedented();
        assert_eq!(map.prepared_lines.text, "a\nb\n");
        let answer = [edit((0, 0), (3, 0), "a\n\nb\n")];
        assert_eq!(
            formatted_either_way(&map, &virtual_text, &answer).as_deref(),
            Some("\n  a\n\n  b\n")
        );
    }

    #[test]
    fn overlapping_or_reversed_answers_are_refused() {
        let (_, map) = dedented();
        let overlapping = [edit((0, 0), (1, 0), "x"), edit((0, 2), (0, 3), "y")];
        assert_eq!(map.map_formatting_answer(&overlapping).ok(), None);
        let reversed = [edit((1, 0), (0, 0), "x")];
        assert_eq!(map.map_formatting_answer(&reversed).ok(), None);
    }
}

/// Formatting answers checked against host text built without the map: the
/// document is generated with the indentation its peer removes (from some
/// lines, some with indentation of their own past it; others, and kept
/// whitespace-only lines, left alone), a formatter
/// rewrites P line by line knowing which line each output line came from,
/// and the expected host text puts back on each line the indent its rules
/// (the ADR's) give it:
///
/// - a line rewritten: the indent the peer removed from it, none for one it
///   left alone (a kept whitespace-only line too, written on or not);
/// - a line created, or text written on an empty line (nothing there for
///   the peer to remove): the content's, the indent the peer removed;
/// - the indentation-only last line: its own, text written on it too;
/// - a blank line joined to the next: that line, the blank one deleted;
///
/// and the deleted edge lines around it all. Positions and the application
/// of edits are this module's own. Provenance no answer shows (a line
/// written where one went; which of several blank lines went or changed)
/// is not generated, rather than guessed.
mod formatting_reference {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    use std::cell::Cell;

    const BODIES: [&str; 5] = ["a", "é", "𝄞x", "b c", "if x:"];

    /// Bodies written as they are, unnumbered: lines alike to the byte,
    /// copies a pairing must tell apart as formatters write them.
    const SHARED: [&str; 3] = ["}", "a = 1", "f('a')"];

    /// Byte offsets of every line start in `text` (LF, CRLF and a lone CR
    /// each end a line).
    fn line_starts(text: &str) -> Vec<usize> {
        let bytes = text.as_bytes();
        let mut starts = vec![0];
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'\r' if bytes.get(at + 1) == Some(&b'\n') => {
                    at += 2;
                    starts.push(at);
                }
                b'\r' | b'\n' => {
                    at += 1;
                    starts.push(at);
                }
                _ => at += 1,
            }
        }
        starts
    }

    /// The end of line `line`'s content (before its line break).
    fn content_end(text: &str, starts: &[usize], line: usize) -> usize {
        let end = starts.get(line + 1).copied().unwrap_or(text.len());
        let line_text = &text[starts[line]..end];
        let content = line_text
            .strip_suffix("\r\n")
            .or_else(|| line_text.strip_suffix('\n'))
            .or_else(|| line_text.strip_suffix('\r'))
            .unwrap_or(line_text);
        starts[line] + content.len()
    }

    /// The LSP position (UTF-16 columns) of byte `offset`.
    fn position_of(text: &str, offset: usize) -> Position {
        let starts = line_starts(text);
        let line = starts.partition_point(|&start| start <= offset) - 1;
        let column: usize = text[starts[line]..offset]
            .chars()
            .map(char::len_utf16)
            .sum();
        Position::new(line as u32, column as u32)
    }

    /// The byte offset of `position`: a column past the line's content
    /// means its end; a line past the last, the text's end.
    fn offset_of(text: &str, position: Position) -> usize {
        let starts = line_starts(text);
        let line = position.line as usize;
        if line >= starts.len() {
            return text.len();
        }
        let end = content_end(text, &starts, line);
        let mut column = 0;
        for (at, character) in text[starts[line]..end].char_indices() {
            if column >= position.character as usize {
                return starts[line] + at;
            }
            column += character.len_utf16();
        }
        end
    }

    /// `text` with `edits` applied, in order of their start then end.
    fn apply(text: &str, edits: &[TextEdit]) -> String {
        let mut spans: Vec<(usize, usize, &str)> = edits
            .iter()
            .map(|edit| {
                (
                    offset_of(text, edit.range.start),
                    offset_of(text, edit.range.end),
                    edit.new_text.as_str(),
                )
            })
            .collect();
        spans.sort_by_key(|&(start, end, _)| (start, end));
        let mut output = String::new();
        let mut cursor = 0;
        for (start, end, new_text) in spans {
            assert!(cursor <= start && start <= end, "edits apply in order");
            output.push_str(&text[cursor..start]);
            output.push_str(new_text);
            cursor = end;
        }
        output.push_str(&text[cursor..]);
        output
    }

    /// One line of a generated document.
    #[derive(Debug, Clone, Copy)]
    enum Line {
        /// Text (`BODIES`, numbered, or `SHARED`), indented and dedented by the peer, or left at
        /// column 0 and alone.
        Content {
            body: usize,
            dedented: bool,
            /// Indentation of its own past what the peer removes.
            deeper: usize,
        },
        Empty,
        /// A single space the peer left alone: shorter than any indent.
        Space,
        /// The indent and a space, the peer removing the indent: a
        /// whitespace line kept with whitespace past the indent.
        IndentedSpace,
    }

    fn line() -> impl Strategy<Value = Line> {
        prop_oneof![
            5 => (0..BODIES.len() + SHARED.len(), prop::bool::weighted(0.8))
                .prop_flat_map(|(body, dedented)| {
                    prop_oneof![4 => Just(0usize), 1 => Just(1usize), 1 => Just(2usize)].prop_map(
                        move |deeper| Line::Content {
                            body,
                            dedented,
                            deeper,
                        },
                    )
                }),
            1 => Just(Line::Empty),
            1 => Just(Line::Space),
            1 => Just(Line::IndentedSpace),
        ]
    }

    /// A generated document: V, the peer's changes, and the host text the
    /// map keeps around P.
    struct Document {
        virtual_text: String,
        changes: Vec<serde_json::Value>,
        leading: String,
        closing: String,
        /// The last line holds only the indent the peer removed, with no
        /// closing lines before it: a content line, its indent kept.
        indent_only_last_line: bool,
        /// The indent text written on each line of P regains, in order: the
        /// one the peer removed from it, or for an empty line (text there
        /// starts a line of the content) `None`, the content's.
        removed: Vec<Option<&'static str>>,
        /// The indent of the indentation-only last line.
        last_line_indent: &'static str,
        /// The indent lines created in the content regain: the one the peer
        /// removed, or none when it removed none.
        uniform: &'static str,
    }

    /// `lines` indented by `indent` (numbered text made unique per line, so
    /// a diff has one way to align it; shared text repeated as it is),
    /// broken by `line_break`, between
    /// `leading` and `trailing` blank lines, optionally ending with the
    /// indent alone.
    fn document(
        lines: &[Line],
        indent: &'static str,
        line_break: &str,
        (leading, trailing, closing_indent): (usize, usize, bool),
    ) -> Option<Document> {
        if !lines
            .iter()
            .any(|line| matches!(line, Line::Content { .. }))
        {
            return None;
        }
        let leading_text = line_break.repeat(leading);
        let mut text = leading_text.clone();
        let mut dedented_lines = Vec::new();
        let mut removed = Vec::new();
        for (number, line) in lines.iter().enumerate() {
            match *line {
                Line::Content {
                    body,
                    dedented,
                    deeper,
                } => {
                    if dedented {
                        dedented_lines.push(text.len());
                        text.push_str(indent);
                    }
                    text.push_str(["", "\t", "  "][deeper]);
                    match BODIES.get(body) {
                        Some(body) => text.push_str(&format!("{number}{body}{number}")),
                        None => text.push_str(SHARED[body - BODIES.len()]),
                    }
                    removed.push(Some(if dedented { indent } else { "" }));
                }
                Line::Empty => removed.push(None),
                Line::Space => {
                    text.push(' ');
                    removed.push(Some(""));
                }
                Line::IndentedSpace => {
                    dedented_lines.push(text.len());
                    text.push_str(indent);
                    text.push(' ');
                    removed.push(Some(indent));
                }
            }
            text.push_str(line_break);
        }
        let content_end = text.len();
        text.push_str(&line_break.repeat(trailing));
        if closing_indent {
            dedented_lines.push(text.len());
            text.push_str(indent);
        }
        let mut changes = Vec::new();
        let change = |start: usize, end: usize| {
            json!({"range": {
                "start": position_of(&text, start),
                "end": position_of(&text, end)
            }, "newText": ""})
        };
        for &at in &line_starts(&text) {
            let edge_line = at < leading_text.len()
                || (at >= content_end && at < content_end + trailing * line_break.len());
            if edge_line {
                changes.push(change(at, at + line_break.len()));
            } else if dedented_lines.contains(&at) {
                changes.push(change(at, at + indent.len()));
            }
        }
        let indent_only_last_line = closing_indent && trailing == 0;
        let any_dedented = lines
            .iter()
            .any(|line| matches!(line, Line::Content { dedented: true, .. }));
        Some(Document {
            closing: if indent_only_last_line {
                String::new()
            } else {
                format!(
                    "{}{}",
                    line_break.repeat(trailing),
                    if closing_indent { indent } else { "" }
                )
            },
            leading: leading_text,
            indent_only_last_line,
            virtual_text: text,
            changes,
            removed,
            last_line_indent: indent,
            uniform: if any_dedented { indent } else { "" },
        })
    }

    /// One way a formatter rewrites a line.
    #[derive(Debug, Clone, Copy)]
    enum Rewrite {
        Keep,
        /// Its leading whitespace becomes this many spaces.
        Reindent(usize),
        TrailingSpace,
        /// A line of text after it, indented by this many spaces.
        LineAfter(usize),
        BlankAfter,
        Delete,
        JoinNext,
        /// Its line break switched, LF to CRLF to a lone CR to LF.
        SwitchBreak,
        /// Text written on it, after this many spaces, if it is blank.
        Write(usize),
        /// Its text replaced by other letters, its leading whitespace kept,
        /// if it is not blank.
        Rename,
    }

    /// Added to the origin of a line no answer places for sure (see
    /// `Rewrite::Write`): written on that line, or in its place.
    const AMBIGUOUS: usize = 1 << 40;

    fn rewrite() -> impl Strategy<Value = Rewrite> {
        prop_oneof![
            4 => Just(Rewrite::Keep),
            2 => (0..5usize).prop_map(Rewrite::Reindent),
            1 => Just(Rewrite::TrailingSpace),
            2 => (0..3usize).prop_map(Rewrite::LineAfter),
            1 => Just(Rewrite::BlankAfter),
            1 => Just(Rewrite::Delete),
            1 => Just(Rewrite::JoinNext),
            1 => (0..3usize).prop_map(Rewrite::Write),
            1 => Just(Rewrite::SwitchBreak),
            1 => Just(Rewrite::Rename),
        ]
    }

    /// A line of a formatter's output: its text, its line break, and the
    /// line of P it rewrites (`None` for a line it created).
    type Output = (String, String, Option<usize>);

    /// `prepared` rewritten line by line (`rewrites` cycled), its final
    /// line break dropped or added when `flip_final_break`.
    fn format(prepared: &str, rewrites: &[Rewrite], flip_final_break: bool) -> Vec<Output> {
        let starts = line_starts(prepared);
        let mut lines: Vec<(String, String)> = (0..starts.len())
            .map(|line| {
                let end = content_end(prepared, &starts, line);
                let next = starts.get(line + 1).copied().unwrap_or(prepared.len());
                (
                    prepared[starts[line]..end].to_string(),
                    prepared[end..next].to_string(),
                )
            })
            .collect();
        // The empty line after a final break is no line a formatter sees.
        if lines.len() > 1 && lines.last().is_some_and(|(text, _)| text.is_empty()) {
            lines.pop();
        }
        let mut output: Vec<Output> = Vec::new();
        let mut rewrites = rewrites.iter().cycle();
        let mut index = 0;
        // Whether the line before was removed: a blank line written then
        // takes its place as much as it is written on.
        let mut removed_before = false;
        // Whether the line before was a blank line written on: a line
        // removed after it reads alike as that line rewritten.
        let mut written_blank_before = false;
        while index < lines.len() {
            let (mut text, line_break) = lines[index].clone();
            let origin = Some(index);
            let some_break = if line_break.is_empty() {
                "\n".to_string()
            } else {
                line_break.clone()
            };
            let mut rewrite = rewrites.next().copied().unwrap_or(Rewrite::Keep);
            // Provenance no answer shows is left out, not guessed: a line
            // written where a line was removed is that line rewritten as much
            // as a new one; and with other blank lines about, which blank
            // line went, or took other whitespace or text, is not to be told
            // (blank lines carry nothing of their own to pair them by).
            // (Blank lines kept between them hide nothing of which is which.)
            let created_before = output
                .iter()
                .rev()
                .find(|(text, _, origin)| !(text.trim().is_empty() && origin.is_some()))
                .is_some_and(|(_, _, origin)| origin.is_none());
            let is_blank = |at: usize| {
                lines
                    .get(at)
                    .is_some_and(|(text, _)| text.trim().is_empty())
            };
            let other_blank =
                |at: usize| (0..lines.len()).any(|other| other != at && is_blank(other));
            // The lines this rewrite removes: its own, or the next it joins.
            let removed = match rewrite {
                Rewrite::Delete => Some(index),
                Rewrite::JoinNext if is_blank(index) => Some(index),
                Rewrite::JoinNext => Some(index + 1),
                _ => None,
            };
            let rewrites_blank = is_blank(index)
                && matches!(
                    rewrite,
                    Rewrite::Reindent(_) | Rewrite::TrailingSpace | Rewrite::Write(_)
                );
            let creates_beside_blank =
                is_blank(index) && matches!(rewrite, Rewrite::LineAfter(_) | Rewrite::BlankAfter);
            if ((created_before || written_blank_before) && (removed.is_some() || rewrites_blank))
                || (removed_before && (rewrites_blank || creates_beside_blank))
                || removed.is_some_and(|at| is_blank(at) && other_blank(at))
                || (rewrites_blank && other_blank(index))
                // A blank line created among blank lines: which one is new
                // is not to be told either.
                || (matches!(rewrite, Rewrite::BlankAfter)
                    && (0..lines.len()).any(is_blank))
            {
                rewrite = Rewrite::Keep;
            }
            written_blank_before = is_blank(index) && matches!(rewrite, Rewrite::Write(_));
            removed_before = matches!(rewrite, Rewrite::JoinNext if index + 1 < lines.len())
                || matches!(rewrite, Rewrite::Delete if lines.len() > 1)
                || (removed_before && is_blank(index));
            match rewrite {
                Rewrite::Keep => output.push((text, line_break, origin)),
                // A formatter leaves no whitespace on a line it rewrites
                // blank (what blank line took which whitespace is not to be
                // told, see above).
                Rewrite::Reindent(_) | Rewrite::TrailingSpace if is_blank(index) => {
                    output.push((String::new(), line_break, origin))
                }
                Rewrite::Reindent(spaces) => {
                    let trimmed = text.trim_start_matches([' ', '\t']).to_string();
                    output.push((
                        format!("{}{trimmed}", " ".repeat(spaces)),
                        line_break,
                        origin,
                    ));
                }
                Rewrite::TrailingSpace => output.push((format!("{text} "), line_break, origin)),
                // Text written where an empty line was is a line of the
                // content. Text where a kept whitespace line was, typed
                // after its whitespace or not, is ambiguous in a whole-text
                // answer (written on it, or in its place), and refused.
                Rewrite::Write(spaces) if is_blank(index) => {
                    let written = format!("{}w{index}", " ".repeat(spaces));
                    let origin = if text.is_empty() {
                        origin
                    } else {
                        origin.map(|line| AMBIGUOUS + line)
                    };
                    output.push((written, line_break, origin))
                }
                Rewrite::Write(_) => output.push((text, line_break, origin)),
                Rewrite::Rename if !is_blank(index) => {
                    let lead = text.len() - text.trim_start_matches([' ', '\t']).len();
                    output.push((format!("{}r{index}", &text[..lead]), line_break, origin))
                }
                Rewrite::Rename => output.push((text, line_break, origin)),
                Rewrite::LineAfter(spaces) => {
                    output.push((text, some_break, origin));
                    output.push((format!("{}w", " ".repeat(spaces)), line_break, None));
                }
                Rewrite::BlankAfter => {
                    output.push((text, some_break, origin));
                    output.push((String::new(), line_break, None));
                }
                Rewrite::Delete if lines.len() > 1 => {}
                Rewrite::Delete => output.push((text, line_break, origin)),
                Rewrite::JoinNext if index + 1 < lines.len() => {
                    index += 1;
                    // A blank line joined to the next is that line, the
                    // blank one deleted.
                    let origin = if text.trim().is_empty() {
                        Some(index)
                    } else {
                        origin
                    };
                    text.push(' ');
                    text.push_str(lines[index].0.trim_start());
                    output.push((text, lines[index].1.clone(), origin));
                }
                Rewrite::JoinNext => output.push((text, line_break, origin)),
                Rewrite::SwitchBreak => {
                    let switched = match line_break.as_str() {
                        "\n" => "\r\n",
                        "\r\n" => "\r",
                        "\r" => "\n",
                        other => other,
                    };
                    output.push((text, switched.to_string(), origin));
                }
            }
            index += 1;
        }
        if flip_final_break && let Some((_, line_break, _)) = output.last_mut() {
            if line_break.is_empty() {
                *line_break = "\n".to_string();
            } else {
                line_break.clear();
            }
        }
        output
    }

    /// The host text a formatter's `output` stands for: every line of text
    /// it rewrote indented as that line was, every one it created as the
    /// content is, the deleted edge lines put back around it. `None` when
    /// the closing lines would not start a line.
    /// Every host text [`reference`] gives for `output`, each line no
    /// answer places for sure read both ways: none when it refuses all.
    /// A line written last with no line break, where an empty line was, as
    /// other text than its line held, or with `copies` of text about (which
    /// copy it is no answer tells), is read both ways too when the
    /// last line holds only an indent: written on that line (the last
    /// line's break and indent going), or on the last line (that line
    /// going), which P shows empty.
    fn readings(
        document: &Document,
        prepared: &str,
        output: &[Output],
        copies: bool,
    ) -> Vec<String> {
        let last = output.len().wrapping_sub(1);
        let prepared_lines = lines_of(prepared);
        let on_empty_line_last = output.last().is_some_and(|(text, line_break, origin)| {
            document.indent_only_last_line
                && line_break.is_empty()
                && origin.is_some_and(|line| {
                    line < AMBIGUOUS
                        && (copies
                            || document.removed[line].is_none()
                            || prepared_lines
                                .get(line)
                                .is_none_or(|old| old.trim() != text.trim()))
                })
        });
        [(true, false), (false, false), (true, true), (false, true)]
            .into_iter()
            .filter(|&(_, on_last_line)| !on_last_line || on_empty_line_last)
            .filter_map(|(written_on, on_last_line)| {
                let resolved: Vec<Output> = output
                    .iter()
                    .enumerate()
                    .map(|(at, (text, line_break, origin))| {
                        let origin = match origin {
                            _ if on_last_line && at == last => None,
                            Some(line) if *line >= AMBIGUOUS => {
                                written_on.then_some(line - AMBIGUOUS)
                            }
                            origin => *origin,
                        };
                        (text.clone(), line_break.clone(), origin)
                    })
                    .collect();
                reference(document, &resolved)
            })
            .collect()
    }

    fn reference(document: &Document, output: &[Output]) -> Option<String> {
        let mut indented = String::new();
        for (at, (text, line_break, origin)) in output.iter().enumerate() {
            // A line written last, with no break, is written on the
            // indentation-only last line, whose indent it keeps.
            let on_last_line = document.indent_only_last_line
                && origin.is_none()
                && line_break.is_empty()
                && at + 1 == output.len();
            if !text.is_empty() {
                indented.push_str(if on_last_line {
                    document.last_line_indent
                } else {
                    origin
                        .and_then(|line| document.removed[line])
                        .unwrap_or(document.uniform)
                });
            }
            indented.push_str(text);
            indented.push_str(line_break);
        }
        let at_line_start = indented.is_empty() || indented.ends_with(['\n', '\r']);
        if document.indent_only_last_line && at_line_start {
            indented.push_str(document.last_line_indent);
        }
        if !document.closing.is_empty() && !indented.is_empty() && !at_line_start {
            return None;
        }
        // A lone CR ending the text would fuse with the closing lines' LF,
        // and an LF starting it with the leading lines' CR.
        if indented.ends_with('\r') && document.closing.starts_with('\n')
            || document.leading.ends_with('\r') && indented.starts_with('\n')
        {
            return None;
        }
        Some(format!(
            "{}{indented}{}",
            document.leading, document.closing
        ))
    }

    /// `output` with two lines of text the formatter left as they were,
    /// the content's indent removed from both, swapped (as sorting imports
    /// does) when `shuffle` says, each keeping its own line break. (A line
    /// written twice is left out: beside lines removed, which copy is the
    /// old line no answer tells.)
    fn shuffled(
        document: &Document,
        prepared: &str,
        mut output: Vec<Output>,
        shuffle: u8,
    ) -> Vec<Output> {
        let lines: Vec<&str> = lines_of(prepared);
        let movable = |(text, line_break, origin): &Output| {
            !text.trim_matches([' ', '\t']).is_empty()
                && !line_break.is_empty()
                && origin.is_some_and(|line| {
                    line < AMBIGUOUS
                        && document.removed[line] == Some(document.uniform)
                        && lines.get(line) == Some(&text.as_str())
                })
        };
        let pick = |candidates: Vec<usize>| {
            (!candidates.is_empty())
                .then(|| candidates[usize::from(shuffle / 2) % candidates.len()])
        };
        // Beside a whitespace line the peer kept, a moved line indented
        // in P reads as well as text written on it, and beside a line
        // removed, as that line rewritten: neither is generated.
        let kept_whitespace = lines
            .iter()
            .any(|line| !line.is_empty() && line.trim_matches([' ', '\t']).is_empty());
        // (The empty line after P's last line break is no line to keep.)
        let all_kept = (0..lines.len())
            .filter(|&line| line + 1 < lines.len() || !lines[line].is_empty())
            .all(|line| output.iter().any(|(_, _, origin)| *origin == Some(line)));
        match shuffle % 2 {
            1 if !kept_whitespace && all_kept => {
                let candidates = (0..output.len().saturating_sub(1))
                    .filter(|&at| {
                        movable(&output[at])
                            && movable(&output[at + 1])
                            && output[at].0 != output[at + 1].0
                    })
                    .collect();
                if let Some(at) = pick(candidates) {
                    output.swap(at, at + 1);
                }
            }
            _ => {}
        }
        output
    }

    /// `text`'s lines without their breaks (LF, CRLF or a lone CR).
    fn lines_of(text: &str) -> Vec<&str> {
        let starts = line_starts(text);
        starts
            .iter()
            .enumerate()
            .map(|(index, &start)| {
                let end = starts.get(index + 1).copied().unwrap_or(text.len());
                text[start..end].trim_end_matches(['\r', '\n'])
            })
            .collect()
    }

    /// The answer a formatter gives for turning `prepared` into
    /// `formatted`, in one of a few shapes servers use.
    fn answer(prepared: &str, formatted: &str, shape: u8) -> Vec<TextEdit> {
        let whole_end = match shape % 3 {
            // To the start of the line just past the last.
            0 => Position::new(line_starts(prepared).len() as u32, 0),
            // To the document end.
            1 => position_of(prepared, prepared.len()),
            // Only what differs, trimmed to whole characters off a CRLF.
            _ => {
                let prefix: usize = prepared
                    .chars()
                    .zip(formatted.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a.len_utf8())
                    .sum();
                let mut prefix = prefix;
                if prepared[..prefix].ends_with('\r') || formatted[..prefix].ends_with('\r') {
                    prefix -= 1;
                }
                let suffix: usize = prepared[prefix..]
                    .chars()
                    .rev()
                    .zip(formatted[prefix..].chars().rev())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a.len_utf8())
                    .sum();
                let mut suffix = suffix;
                let (old_end, new_end) = (prepared.len() - suffix, formatted.len() - suffix);
                if prepared[old_end..].starts_with('\n') || formatted[new_end..].starts_with('\n') {
                    suffix -= 1;
                }
                let (old_end, new_end) = (prepared.len() - suffix, formatted.len() - suffix);
                return vec![TextEdit {
                    range: LspRange::new(
                        position_of(prepared, prefix),
                        position_of(prepared, old_end),
                    ),
                    new_text: formatted[prefix..new_end].to_string(),
                }];
            }
        };
        vec![TextEdit {
            range: LspRange::new(Position::new(0, 0), whole_end),
            new_text: formatted.to_string(),
        }]
    }

    /// Run `cases` generated answers mapped with `budget`, checking each
    /// accepted one against [`reference`]; how many were mapped, and how
    /// many of those changed P.
    fn check(budget: usize, cases: u32) -> (usize, usize) {
        let mut runner = property_runner(cases);
        let (mapped_sets, changing, over_refused) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let strategy = (
            (
                prop::collection::vec(line(), 1..6),
                prop::bool::weighted(0.75),
            ),
            prop::sample::select(vec!["  ", "\t", "    "]),
            prop::sample::select(vec!["\n", "\r\n", "\r"]),
            (0..3usize, 0..3usize, any::<bool>()),
            prop::collection::vec(rewrite(), 1..6),
            (any::<bool>(), any::<u8>(), any::<u8>()),
        );
        runner
            .run(
                &strategy,
                |((mut lines, even), indent, line_break, edges, rewrites, (flip, shape, shuffle))| {
                    // Mostly peers dedenting evenly: every line of text
                    // dedented as the first is.
                    let first = lines.iter().find_map(|line| match line {
                        Line::Content { dedented, .. } => Some(*dedented),
                        _ => None,
                    });
                    if let (true, Some(first)) = (even, first) {
                        for line in &mut lines {
                            if let Line::Content { dedented, .. } = line {
                                *dedented = first;
                            }
                        }
                    }
                    let Some(document) = document(&lines, indent, line_break, edges) else {
                        return Ok(());
                    };
                    // Some line of text dedented and another left alone.
                    let dedented = |want: bool| {
                        lines.iter().any(
                            |line| matches!(line, Line::Content { dedented, .. } if *dedented == want),
                        )
                    };
                    let uneven = dedented(true) && dedented(false);
                    let map = prepare(
                        &document.virtual_text,
                        serde_json::Value::from(document.changes.clone()),
                    );
                    let prepared = map.prepared_lines.text.clone();
                    let output = shuffled(
                        &document,
                        &prepared,
                        format(&prepared, &rewrites, flip),
                        shuffle,
                    );
                    // A lone CR before an empty line's LF would fuse into
                    // one line break: lines the formatter did not write.
                    if output.windows(2).any(|pair| {
                        pair[0].1 == "\r" && pair[1].0.is_empty() && pair[1].1.starts_with('\n')
                    }) {
                        return Ok(());
                    }
                    let formatted: String = output
                        .iter()
                        .map(|(text, line_break, _)| format!("{text}{line_break}"))
                        .collect();
                    let edits = answer(&prepared, &formatted, shape);
                    prop_assert_eq!(apply(&prepared, &edits), formatted.clone());
                    let Some(mapped) = map.map_formatting(&edits, budget).ok() else {
                        // Refused with work enough: an answer the host
                        // cannot take, or (rarely) one turning line breaks
                        // among several blank lines, whose pairing can meet
                        // a CR and an LF the check refuses.
                        let sure = output
                            .iter()
                            .all(|(_, _, origin)| origin.is_none_or(|line| line < AMBIGUOUS));
                        if budget > 0 && sure && !uneven && reference(&document, &output).is_some() {
                            over_refused.set(over_refused.get() + 1);
                        }
                        return Ok(());
                    };
                    mapped_sets.set(mapped_sets.get() + 1);
                    if formatted != prepared {
                        // A peer dedenting unevenly: refused whatever the
                        // answer changes.
                        prop_assert!(!uneven, "{:?} mapped on {:?}", edits, document.virtual_text);
                        changing.set(changing.get() + 1);
                    }
                    let got = apply(&document.virtual_text, &mapped);
                    let copies = lines.iter().enumerate().any(|(at, line)| {
                        matches!(line, Line::Content { body, .. } if *body >= BODIES.len()
                            && lines[at + 1..].iter().any(|other| {
                                matches!(other, Line::Content { body: other, .. } if other == body)
                            }))
                    });
                    let expected = readings(&document, &prepared, &output, copies);
                    // With shared text written twice, which copy was kept
                    // can move a whitespace line's own whitespace (kept as
                    // it was, or written anew with the content's indent): a
                    // blank line's whitespace no answer places.
                    // So can it move the indentation-only last line's indent
                    // onto a last line written without a break (written on
                    // it, or on a copy's line). `text` with its
                    // whitespace-only lines emptied, and the indent of a
                    // last line without a break dropped, every other
                    // character kept.
                    let blanks_emptied = |text: &str| -> String {
                        let starts = line_starts(text);
                        (0..starts.len())
                            .map(|line| {
                                let end = content_end(text, &starts, line);
                                let next = starts.get(line + 1).copied().unwrap_or(text.len());
                                let content = &text[starts[line]..end];
                                if content.trim_matches([' ', '\t']).is_empty() {
                                    text[end..next].to_string()
                                } else if next == end && document.indent_only_last_line {
                                    content.trim_start_matches([' ', '\t']).to_string()
                                } else {
                                    text[starts[line]..next].to_string()
                                }
                            })
                            .collect()
                    };
                    prop_assert!(
                        expected.contains(&got)
                            || copies
                                && expected
                                    .iter()
                                    .any(|text| blanks_emptied(text) == blanks_emptied(&got)),
                        "{:?} -> {:?} on {:?} (P {:?}): {:?} not in {:?}",
                        edits,
                        mapped,
                        document.virtual_text,
                        prepared,
                        got,
                        expected
                    );
                    Ok::<(), TestCaseError>(())
                },
            )
            .unwrap();
        // Cases the host could take, refused: availability only, and rare
        // (one in 10,000 at most, the fixed seed's 3,000 cases none; one
        // allowed more from another seed).
        let fixed_seed = std::env::var_os("KAKEHASHI_PROPTEST_RANDOM").is_none()
            && std::env::var_os("PROPTEST_RNG_SEED").is_none();
        assert!(
            over_refused.get()
                <= usize::from(!fixed_seed) + property_cases(cases) as usize / 10_000,
            "refused {} answers the host could take",
            over_refused.get()
        );
        eprintln!(
            "formatting reference over {} cases: {} mapped, {} changing P, {} refused the host could take",
            property_cases(cases),
            mapped_sets.get(),
            changing.get(),
            over_refused.get()
        );
        (mapped_sets.get(), changing.get())
    }

    #[test]
    fn formatting_answers_give_exactly_the_reference_host_text() {
        let (mapped, changing) = check(FORMATTING_DIFF_BUDGET, 3000);
        // Every refusal is checked above; this only keeps the run from
        // passing on nothing.
        assert!(changing > 1000, "mapped {mapped}, {changing} changing P");
    }

    #[test]
    fn answers_whose_diff_runs_out_of_work_are_refused() {
        // Only an answer changing nothing needs no diff.
        let (mapped, changing) = check(0, 1000);
        assert_eq!(changing, 0, "mapped {mapped}");
        assert!(mapped > 0);
    }
}

/// `virtual_text` once `edits` to its prepared text are replayed onto it;
/// `None` when the set is refused.
fn replayed(virtual_text: &str, map: &PreparedMap, edits: &[TextEdit]) -> Option<String> {
    map.map_edits(edits, EditMode::Replay)
        .map(|mapped| apply_to(virtual_text, &mapped))
}

#[test]
fn a_rename_replayed_at_several_sites_keeps_every_indent() {
    let virtual_text = "  a = 1\n  b = a\n    a(b)\n";
    let map = dedent_by_two(virtual_text);
    assert_eq!(map.prepared_text(), "a = 1\nb = a\n  a(b)\n");
    let edits = [
        edit((0, 0), (0, 1), "x"),
        edit((1, 4), (1, 5), "x"),
        edit((2, 2), (2, 3), "x"),
    ];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  x = 1\n  b = x\n    x(b)\n")
    );
}

#[test]
fn touching_line_deletions_replayed_take_each_lines_indent_once() {
    let virtual_text = "  a\n  b\n  c\n";
    let map = dedent_by_two(virtual_text);
    // Mapped one by one, both would claim `b`'s removed indent.
    let edits = [edit((0, 0), (1, 0), ""), edit((1, 0), (2, 0), "")];
    assert_eq!(map.map_edits(&edits, EditMode::Extent), None);
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  c\n")
    );
}

#[test]
fn a_block_wrapped_by_touching_insertions_regains_the_content_indent() {
    let virtual_text = "  a\n  b\n";
    let map = dedent_by_two(virtual_text);
    // A wrap code action: a header inserted at the first line's start, and
    // every wrapped line indented there.
    let edits = [
        edit((0, 0), (0, 0), "if x:\n"),
        edit((0, 0), (0, 0), "  "),
        edit((1, 0), (1, 0), "  "),
    ];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  if x:\n    a\n    b\n")
    );
    // Unwrapping it back.
    let virtual_text = "  if x:\n    a\n    b\n";
    let map = dedent_by_two(virtual_text);
    let edits = [edit((0, 0), (1, 2), ""), edit((2, 0), (2, 2), "")];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  a\n  b\n")
    );
}

#[test]
fn an_import_inserted_above_edits_elsewhere_regains_the_indent() {
    let virtual_text = "  def f():\n    return g()\n";
    let map = dedent_by_two(virtual_text);
    let edits = [
        edit((0, 0), (0, 0), "from m import g\n"),
        edit((1, 11), (1, 11), "1"),
    ];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  from m import g\n  def f():\n    return g(1)\n")
    );
}

#[test]
fn a_replayed_whole_document_keeps_the_lines_it_leaves_alike() {
    // The whitespace-only line keeps its whitespace in the host, which a
    // whole replacement keeping its extent would rewrite as an empty line.
    let virtual_text = "  a\n  \n  b\n";
    let map = dedent_by_two(virtual_text);
    assert_eq!(map.prepared_text(), "a\n\nb\n");
    let edits = [edit((0, 0), (3, 0), "A\n\nb\n")];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  A\n  \n  b\n")
    );
    assert_eq!(
        map.map_edits(&edits, EditMode::Extent)
            .map(|mapped| apply_to(virtual_text, &mapped))
            .as_deref(),
        Some("  A\n\n  b\n")
    );
}

#[test]
fn deleting_one_of_two_alike_lines_deletes_the_one_named() {
    // The second `a` is followed by a whitespace line the first is not:
    // which went shows in the host text.
    let virtual_text = "  a\n  b\n  a\n  \n";
    let map = dedent_by_two(virtual_text);
    let edits = [edit((2, 0), (3, 0), "")];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  a\n  b\n  \n")
    );
    let edits = [edit((0, 0), (1, 0), "")];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  b\n  a\n  \n")
    );
}

#[test]
fn replayed_lines_on_an_unevenly_dedented_region_are_refused_and_in_line_edits_map() {
    // `c` is left alone by the peer.
    let virtual_text = "  a\nc\n  b\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
        ]),
    );
    assert_eq!(map.prepared_text(), "a\nc\nb\n");
    assert_eq!(
        replayed(
            virtual_text,
            &map,
            &[edit((0, 0), (0, 1), "x"), edit((2, 0), (2, 1), "y")]
        )
        .as_deref(),
        Some("  x\nc\n  y\n")
    );
    assert_eq!(
        replayed(virtual_text, &map, &[edit((0, 1), (0, 1), "\nq")]),
        None
    );
}

#[test]
fn replayed_lines_in_a_crlf_document_past_its_first_line_map() {
    let virtual_text = "  a\r\n  b\r\n  c\r\n";
    let map = dedent_by_two(virtual_text);
    assert_eq!(map.prepared_text(), "a\r\nb\r\nc\r\n");
    // A line inserted after `b`, and `c` renamed in place beside it.
    let edits = [edit((1, 1), (1, 1), "\r\nq"), edit((2, 0), (2, 1), "C")];
    assert_eq!(
        replayed(virtual_text, &map, &edits).as_deref(),
        Some("  a\r\n  b\r\n  q\r\n  C\r\n")
    );
}

#[test]
fn edits_on_the_lines_a_replayed_group_reaches_are_replayed_with_it() {
    let cases: [(&str, Vec<TextEdit>, &str); 5] = [
        // A single-line edit after a multi-line one, on its last line.
        (
            "  a\n  bcd\n",
            vec![edit((0, 0), (1, 1), "X"), edit((1, 2), (1, 3), "DDD")],
            "  XcDDD\n",
        ),
        (
            "  x\n  foo(a, b)\n  z\n",
            vec![edit((0, 1), (1, 3), "\nbar"), edit((1, 7), (1, 8), "bbbb")],
            "  x\n  bar(a, bbbb)\n  z\n",
        ),
        (
            "  x\n  y = 1\n  zz\n",
            vec![edit((0, 1), (1, 1), "1\nY"), edit((1, 4), (1, 5), "22")],
            "  x1\n  Y = 22\n  zz\n",
        ),
        (
            "  bcaba\n  aaaa\n  cbcab\n  cc\n  a\n",
            vec![
                edit((0, 5), (1, 1), "yy"),
                edit((1, 4), (2, 0), "zx"),
                edit((2, 1), (2, 1), "x\nz"),
            ],
            "  bcabayyaaazxcx\n  zbcab\n  cc\n  a\n",
        ),
        // Single-line edits before one, on its first line.
        (
            "  ab = cd\n  e\n",
            vec![
                edit((0, 0), (0, 1), "AAA"),
                edit((0, 2), (0, 2), "BB"),
                edit((0, 5), (0, 5), "\n"),
            ],
            "  AAAbBB = \n  cd\n  e\n",
        ),
    ];
    for (virtual_text, edits, expected) in cases {
        let map = dedent_by_two(virtual_text);
        assert_eq!(
            replayed(virtual_text, &map, &edits).as_deref(),
            Some(expected),
            "{edits:?} on {virtual_text:?}"
        );
    }
}

/// Random edit sets replayed on a document dedented evenly, without
/// whitespace-only lines, against the edits applied to P and every line of
/// text indented: what the host takes there whatever the edits' shape.
#[test]
fn random_replayed_edit_sets_give_p_edited_and_indented() {
    use proptest::prelude::*;
    const TEXTS: [&str; 9] = ["", "x", "\n", "y\nz", "  ", "q\n", "\nr", "a\n", "\r\n"];
    let mut runner = property_runner(4000);
    let mapped = std::cell::Cell::new(0);
    let strategy = (
        prop::collection::vec("[a-h =]{1,6}", 2..7),
        prop::collection::vec((any::<u16>(), any::<u16>(), 0..TEXTS.len()), 1..5),
    );
    runner
        .run(&strategy, |(lines, specs)| {
            let prepared: String = lines
                .iter()
                .map(|line| line.trim_matches(' ').to_string())
                .filter(|line| !line.is_empty())
                .map(|line| line + "\n")
                .collect();
            if prepared.is_empty() {
                return Ok(());
            }
            let virtual_text: String = prepared.lines().map(|line| format!("  {line}\n")).collect();
            let map = dedent_by_two(&virtual_text);
            prop_assert_eq!(map.prepared_text(), prepared.as_str());
            // Sorted edits apart from one another (touching allowed).
            let mut cuts: Vec<usize> = specs
                .iter()
                .flat_map(|&(a, b, _)| {
                    [
                        a as usize % (prepared.len() + 1),
                        b as usize % (prepared.len() + 1),
                    ]
                })
                .collect();
            cuts.sort_unstable();
            let lines_map = LineMap::new(prepared.clone());
            let edits: Vec<TextEdit> = cuts
                .chunks(2)
                .zip(&specs)
                .map(|(pair, &(_, _, text))| TextEdit {
                    range: LspRange::new(lines_map.position(pair[0]), lines_map.position(pair[1])),
                    new_text: TEXTS[text].to_string(),
                })
                .collect();
            let edited = apply_to(&prepared, &edits);
            let indented: String = edited
                .split_inclusive(['\n'])
                .map(|line| {
                    if line.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                        line.trim_start_matches([' ', '\t']).to_string()
                    } else {
                        format!("  {line}")
                    }
                })
                .collect();
            if let Some(got) = replayed(&virtual_text, &map, &edits) {
                mapped.set(mapped.get() + 1);
                let blank_free = |text: &str| -> String {
                    text.split_inclusive('\n')
                        .map(|line| {
                            if line.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                                line.trim_start_matches([' ', '\t']).to_string()
                            } else {
                                line.to_string()
                            }
                        })
                        .collect()
                };
                prop_assert_eq!(blank_free(&got), indented, "{:?} on {:?}", edits, prepared);
            }
            Ok(())
        })
        .unwrap();
    assert!(mapped.get() > 2000, "mapped {}", mapped.get());
}

/// A rename's or a code action's edit set replayed (see [`EditMode::Replay`])
/// against a reference built without the map: documents are generated as
/// lines of text over one indentation, the edits as a server would send
/// them for each kind of change, and the expected host text by making the
/// same change to those lines and indenting the result. Extent mapping and
/// a whole-text diff of the same sets are judged alike, to compare them.
mod replay_reference {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    /// One line of a generated document.
    #[derive(Debug, Clone, PartialEq)]
    enum Line {
        /// Text over the content's indentation, or at column 0 where the
        /// peer left it alone (its region then dedented unevenly).
        Text { text: String, alone: bool },
        /// An empty line.
        Empty,
        /// A whitespace-only line: the content's indentation, which the peer
        /// empties, or when `kept` a single space the peer keeps.
        Blank { kept: bool },
    }

    /// Line texts, as P shows them: some nested deeper than the content,
    /// most naming `a`, which renames rename.
    const TEXTS: [&str; 7] = ["a = 1", "f(a)", "  g(a)", "}", "return a", "b", "  a"];

    fn line() -> impl Strategy<Value = Line> {
        prop_oneof![
            6 => (0..TEXTS.len()).prop_map(|text| Line::Text { text: TEXTS[text].into(), alone: false }),
            1 => (0..TEXTS.len()).prop_map(|text| Line::Text { text: TEXTS[text].into(), alone: true }),
            1 => Just(Line::Empty),
            1 => any::<bool>().prop_map(|kept| Line::Blank { kept }),
        ]
    }

    /// The change a server makes, on the lines of P.
    #[derive(Debug, Clone)]
    enum Change {
        /// Every `a` renamed.
        Rename,
        /// An import inserted above line `at`, and every `a` renamed.
        Import { at: usize },
        /// Lines `from..=to` wrapped in a block: a header inserted before
        /// them and each line of text indented (blank lines among them left
        /// alone), the header and the first indent sent as one edit or two.
        Wrap { from: usize, to: usize, one: bool },
        /// The block line `at` opens unwrapped: it goes, and the lines of
        /// text after it nested deeper (blank lines among them left alone)
        /// lose that nesting, the line's deletion and the first one's sent
        /// as one edit or two; with `rename`, every `a` after it renamed
        /// too, beside those edits on their lines.
        Unwrap { at: usize, one: bool, rename: bool },
        /// Line `at` deleted, blank or not.
        Delete { at: usize },
    }

    fn change() -> impl Strategy<Value = (Change, bool)> {
        (
            prop_oneof![
                Just(Change::Rename),
                (0..8usize).prop_map(|at| Change::Import { at }),
                (0..8usize, 0..8usize, any::<bool>()).prop_map(|(a, b, one)| Change::Wrap {
                    from: a.min(b),
                    to: a.max(b),
                    one
                }),
                (0..8usize, any::<bool>(), any::<bool>())
                    .prop_map(|(at, one, rename)| Change::Unwrap { at, one, rename }),
                (0..8usize).prop_map(|at| Change::Delete { at }),
            ],
            // Sent as one edit replacing the whole document (a fix-all).
            any::<bool>(),
        )
    }

    fn prepared_line(line: &Line) -> &str {
        match line {
            Line::Text { text, .. } => text,
            Line::Empty | Line::Blank { kept: false } => "",
            Line::Blank { kept: true } => " ",
        }
    }

    fn virtual_line(line: &Line, indent: &str) -> String {
        match line {
            Line::Text { text, alone: false } => format!("{indent}{text}"),
            Line::Text { text, alone: true } => text.clone(),
            Line::Empty => String::new(),
            Line::Blank { kept: false } => indent.to_string(),
            Line::Blank { kept: true } => " ".to_string(),
        }
    }

    fn render(lines: &[Line], line: impl Fn(&Line) -> String, eol: &str) -> String {
        lines.iter().map(|l| line(l) + eol).collect()
    }

    fn insert(at: (usize, usize), text: &str) -> TextEdit {
        edit((at.0 as u32, at.1 as u32), (at.0 as u32, at.1 as u32), text)
    }

    /// Where `a` stands as a word in `text`.
    fn names(text: &str) -> Vec<usize> {
        let bytes = text.as_bytes();
        (0..bytes.len())
            .filter(|&at| {
                bytes[at] == b'a'
                    && (at == 0 || !bytes[at - 1].is_ascii_alphanumeric())
                    && bytes
                        .get(at + 1)
                        .is_none_or(|next| !next.is_ascii_alphanumeric())
            })
            .collect()
    }

    /// Every `a` renamed, but on line `skip`.
    fn rename(lines: &mut [Line], edits: &mut Vec<TextEdit>, skip: Option<usize>) {
        for (index, line) in lines.iter_mut().enumerate() {
            if Some(index) == skip {
                continue;
            }
            if let Line::Text { text, .. } = line {
                for at in names(text) {
                    edits.push(edit(
                        (index as u32, at as u32),
                        (index as u32, at as u32 + 1),
                        "xy",
                    ));
                }
                *text = text
                    .split(' ')
                    .map(|word| {
                        let mut word = word.to_string();
                        for at in names(&word).into_iter().rev() {
                            word.replace_range(at..at + 1, "xy");
                        }
                        word
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
            }
        }
    }

    /// The edits making `change` to `lines`, and the lines it makes;
    /// `None` when it does not apply there.
    fn make(change: &Change, lines: &[Line], eol: &str) -> Option<(Vec<TextEdit>, Vec<Line>)> {
        let mut edits = Vec::new();
        let mut after = lines.to_vec();
        let text = |at: usize| match lines.get(at) {
            Some(Line::Text { text, .. }) => Some(text.as_str()),
            _ => None,
        };
        match *change {
            Change::Rename => rename(&mut after, &mut edits, None),
            Change::Import { at } => {
                let at = at.min(lines.len());
                rename(&mut after, &mut edits, None);
                edits.push(insert((at, 0), &format!("use m{eol}")));
                after.insert(
                    at,
                    Line::Text {
                        text: "use m".into(),
                        alone: false,
                    },
                );
            }
            Change::Wrap { from, to, one } => {
                if text(from).is_none() || text(to).is_none() {
                    return None;
                }
                if one {
                    edits.push(insert((from, 0), &format!("if x:{eol}  ")));
                } else {
                    edits.push(insert((from, 0), &format!("if x:{eol}")));
                    edits.push(insert((from, 0), "  "));
                }
                for at in from + 1..=to {
                    if text(at).is_some() {
                        edits.push(insert((at, 0), "  "));
                    }
                }
                for line in &mut after[from..=to] {
                    if let Line::Text { text, .. } = line {
                        text.insert_str(0, "  ");
                    }
                }
                after.insert(
                    from,
                    Line::Text {
                        text: "if x:".into(),
                        alone: false,
                    },
                );
            }
            Change::Unwrap {
                at,
                one,
                rename: renamed,
            } => {
                text(at)?;
                if !text(at + 1).is_some_and(|text| text.starts_with("  ")) {
                    return None;
                }
                // Through the last line of text nested deeper, blank lines
                // among them.
                let nested = (at + 1..lines.len())
                    .take_while(|&line| text(line).is_none_or(|text| text.starts_with("  ")))
                    .filter(|&line| text(line).is_some())
                    .last()
                    .map_or(0, |last| last - at);
                if renamed {
                    rename(&mut after, &mut edits, Some(at));
                }
                if one {
                    edits.push(edit((at as u32, 0), (at as u32 + 1, 2), ""));
                } else {
                    edits.push(edit((at as u32, 0), (at as u32 + 1, 0), ""));
                    edits.push(edit((at as u32 + 1, 0), (at as u32 + 1, 2), ""));
                }
                for line in at + 2..at + 1 + nested {
                    if text(line).is_some() {
                        edits.push(edit((line as u32, 0), (line as u32, 2), ""));
                    }
                }
                for line in &mut after[at + 1..at + 1 + nested] {
                    if let Line::Text { text, .. } = line {
                        text.replace_range(..2, "");
                    }
                }
                after.remove(at);
            }
            Change::Delete { at } => {
                // Blank lines too: those P shows alike may stand for other
                // whitespace in the host, which the line named tells.
                lines.get(at)?;
                edits.push(edit((at as u32, 0), (at as u32 + 1, 0), ""));
                after.remove(at);
            }
        }
        (!edits.is_empty()).then_some((edits, after))
    }

    /// How a variant fared on one case.
    #[derive(Debug, Default, Clone, Copy)]
    struct Tally {
        exact: usize,
        wrong: usize,
        refused: usize,
    }

    impl Tally {
        fn judge(&mut self, got: Option<String>, expected: &str) -> bool {
            match got {
                Some(got) if got == expected => self.exact += 1,
                Some(_) => {
                    self.wrong += 1;
                    return false;
                }
                None => self.refused += 1,
            }
            true
        }
    }

    #[test]
    fn replayed_edit_sets_give_exactly_the_reference_host_text() {
        let mut runner = property_runner(6000);
        // Per variant (replay, extent, whole-text diff), on evenly and
        // unevenly dedented documents, and per kind of change.
        let tallies = RefCell::new([[Tally::default(); 2]; 3]);
        let kinds = RefCell::new(std::collections::BTreeMap::<String, [[Tally; 2]; 3]>::new());
        let strategy = (
            prop::collection::vec(line(), 1..8),
            prop::sample::select(vec!["  ", "\t", "    "]),
            change(),
            prop::sample::select(vec!["\n", "\r\n"]),
        );
        runner
            .run(&strategy, |(lines, indent, (change, whole), eol)| {
                if !lines
                    .iter()
                    .any(|line| matches!(line, Line::Text { alone: false, .. }))
                {
                    return Ok(());
                }
                let Some((mut edits, after)) = make(&change, &lines, eol) else {
                    return Ok(());
                };
                let virtual_text = render(&lines, |line| virtual_line(line, indent), eol);
                let changes: Vec<serde_json::Value> = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| {
                        matches!(
                            line,
                            Line::Text { alone: false, .. } | Line::Blank { kept: false }
                        )
                    })
                    .map(|(at, _)| {
                        json!({"range": {
                            "start": {"line": at, "character": 0},
                            "end": {"line": at, "character": indent.len()}
                        }, "newText": ""})
                    })
                    .collect();
                let map = prepare(&virtual_text, json!(changes));
                prop_assert_eq!(
                    map.prepared_text(),
                    render(&lines, |l| prepared_line(l).to_string(), eol)
                );
                // Which of several blank lines P shows alike went, one edit
                // rewriting the whole document does not tell (nor does a
                // formatting answer): the host text of such a deletion is not
                // the edit's to state.
                if whole
                    && let Change::Delete { at } = change
                    && !matches!(lines[at], Line::Text { .. })
                {
                    return Ok(());
                }
                if whole {
                    edits = vec![edit(
                        (0, 0),
                        (lines.len() as u32, 0),
                        &render(&after, |l| prepared_line(l).to_string(), eol),
                    )];
                }
                let expected = render(&after, |line| virtual_line(line, indent), eol);
                let uneven = lines
                    .iter()
                    .any(|line| matches!(line, Line::Text { alone: true, .. }));
                let applied = |mapped: Option<Vec<TextEdit>>| {
                    mapped.map(|mapped| apply_to(&virtual_text, &mapped))
                };
                let got = [
                    applied(map.map_edits(&edits, EditMode::Replay)),
                    applied(map.map_edits(&edits, EditMode::Extent)),
                    applied(map.map_formatting_answer(&edits).ok()),
                ];
                let kind = format!(
                    "{}{}",
                    format!("{change:?}")
                        .split([' ', '{'])
                        .next()
                        .unwrap_or_default(),
                    if whole { " (whole)" } else { "" }
                );
                let mut kinds = kinds.borrow_mut();
                let by_kind = kinds.entry(kind).or_default();
                let mut tallies = tallies.borrow_mut();
                for (variant, got) in got.iter().enumerate() {
                    by_kind[variant][uneven as usize].judge(got.clone(), &expected);
                }
                let exact = tallies[0][uneven as usize].judge(got[0].clone(), &expected);
                tallies[1][uneven as usize].judge(got[1].clone(), &expected);
                tallies[2][uneven as usize].judge(got[2].clone(), &expected);
                prop_assert!(
                    exact,
                    "{:?} on {:?}: {:?} -> {:?}, expected {:?}",
                    change,
                    virtual_text,
                    edits,
                    applied(map.map_edits(&edits, EditMode::Replay)),
                    expected
                );
                Ok(())
            })
            .unwrap();
        let tallies = tallies.into_inner();
        for (kind, tallies) in kinds.borrow().iter() {
            for (name, tally) in ["replay", "extent", "whole diff"].iter().zip(tallies) {
                eprintln!("{kind} {name}: even {:?}, uneven {:?}", tally[0], tally[1]);
            }
        }
        for (name, tally) in ["replay", "extent", "whole diff"].iter().zip(&tallies) {
            eprintln!("{name}: even {:?}, uneven {:?}", tally[0], tally[1]);
        }
        let renames = kinds.borrow()["Rename"][0];
        let [replay, ..] = tallies;
        // On evenly dedented documents every such change maps; on unevenly
        // dedented ones, every rename, which changes no line's layout.
        assert_eq!(replay[0].refused, 0, "{:?}", replay[0]);
        assert_eq!(renames[1].refused, 0, "{:?}", renames[1]);
        assert!(renames[1].exact > 0, "{:?}", renames[1]);
        assert!(replay[0].exact > 1000, "{:?}", replay[0]);
    }
}

#[test]
fn a_long_line_broken_at_many_places_replays_as_one_group() {
    let words: Vec<String> = (0..200).map(|at| format!("w{at}")).collect();
    let virtual_text = format!("  {}\n  end\n", words.join(" "));
    let map = dedent_by_two(&virtual_text);
    // Every space of the long line broken.
    let mut column = 0;
    let mut edits = Vec::new();
    for word in &words[..words.len() - 1] {
        column += word.len() as u32;
        edits.push(edit((0, column), (0, column + 1), "\n"));
        column += 1;
    }
    let expected: String = words
        .iter()
        .map(|word| format!("  {word}\n"))
        .chain(["  end\n".to_string()])
        .collect();
    assert_eq!(
        replayed(&virtual_text, &map, &edits).as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn a_replayed_cr_meeting_an_lf_of_p_is_diffed_with_that_lines_break() {
    // `x\r` typed at an empty line's start: its CR and the line's LF are one
    // line break of the edited P, which the group's span must not split.
    let virtual_text = "  a\n\n  b\n";
    let map = dedent_by_two(virtual_text);
    assert_eq!(map.prepared_text(), "a\n\nb\n");
    assert_eq!(
        replayed(virtual_text, &map, &[edit((1, 0), (1, 0), "x\r")]).as_deref(),
        Some("  a\n  x\r\n  b\n")
    );
}

#[test]
fn a_line_written_as_one_of_several_copies_is_placed_among_what_pairs() {
    // `}` written as it was, P holding two: paired with the first, the blank
    // line between would be rewritten (and lose the host's whitespace); the
    // blank line and the second `}` pair as they stand.
    let virtual_text = "  }\n    a\n  \n  }\n  a = 1\n";
    let map = dedent_by_two(virtual_text);
    assert_eq!(map.prepared_text(), "}\n  a\n\n}\na = 1\n");
    let answer = [edit((0, 0), (5, 0), "xy\n\n}\nxy = 1\n")];
    let expected = "  xy\n  \n  }\n  xy = 1\n";
    assert_eq!(
        map.map_formatting_answer(&answer)
            .ok()
            .map(|mapped| apply_to(virtual_text, &mapped))
            .as_deref(),
        Some(expected)
    );
    assert_eq!(
        replayed(virtual_text, &map, &answer).as_deref(),
        Some(expected)
    );
}

#[test]
fn a_line_joined_onto_one_of_several_copies_is_placed_among_what_pairs() {
    // `} 3a3` joins onto a `}`, P holding two with a kept whitespace line
    // between: joined onto the first, that line would be read as rewritten.
    let virtual_text = "\n\n  }\n \n  }\n  \t3a3\n    4𝄞x4\n\n";
    let range = |start: (u32, u32), end: (u32, u32)| {
        json!({"range": {"start": {"line": start.0, "character": start.1},
            "end": {"line": end.0, "character": end.1}}, "newText": ""})
    };
    let map = prepare(
        virtual_text,
        json!([
            range((0, 0), (1, 0)),
            range((1, 0), (2, 0)),
            range((2, 0), (2, 2)),
            range((4, 0), (4, 2)),
            range((5, 0), (5, 2)),
            range((6, 0), (6, 2)),
            range((7, 0), (8, 0)),
        ]),
    );
    assert_eq!(map.prepared_text(), "}\n \n}\n\t3a3\n  4𝄞x4\n");
    let answer = [edit((0, 0), (5, 0), "   }\n \n} 3a3\n")];
    let expected = "\n\n     }\n \n  } 3a3\n\n";
    assert_eq!(
        map.map_formatting_answer(&answer)
            .ok()
            .map(|mapped| apply_to(virtual_text, &mapped))
            .as_deref(),
        Some(expected)
    );
    assert_eq!(
        replayed(virtual_text, &map, &answer).as_deref(),
        Some(expected)
    );
}
