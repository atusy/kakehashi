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
    use proptest::test_runner::{Config, TestCaseError, TestRunner};
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
        let mut runner = TestRunner::new(Config {
            cases: 4000,
            failure_persistence: None,
            ..Config::default()
        });
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
        let mut runner = TestRunner::new(Config {
            cases: 3000,
            failure_persistence: None,
            ..Config::default()
        });
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
    use proptest::test_runner::{Config, TestCaseError, TestRunner};
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
        let mut runner = TestRunner::new(Config {
            // Most generated documents bear gaps, skipped below.
            cases: 12000,
            failure_persistence: None,
            ..Config::default()
        });
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

mod formatting {
    use super::*;
    use std::time::Duration;

    /// V formatted through `map` by a formatter answering `edits` to P, the
    /// diff given `budget`; `None` when the map refuses the answer.
    fn formatted(
        map: &PreparedMap,
        virtual_text: &str,
        edits: &[TextEdit],
        budget: Duration,
    ) -> Option<String> {
        let mapped = map.map_edits_within(edits, EditMode::Formatting, budget)?;
        Some(apply_to(virtual_text, &mapped))
    }

    /// [`formatted`] with time enough; with none, an answer changing
    /// anything is refused.
    fn formatted_either_way(
        map: &PreparedMap,
        virtual_text: &str,
        edits: &[TextEdit],
    ) -> Option<String> {
        let precise = formatted(map, virtual_text, edits, Duration::from_secs(10));
        let coarse = formatted(map, virtual_text, edits, Duration::ZERO);
        if precise.as_deref() != Some(virtual_text) {
            assert_eq!(coarse, None, "a diff out of time is refused");
        }
        precise
    }

    #[test]
    fn lines_the_formatter_left_alone_keep_their_own_indent() {
        // `b` was left at column 0; the formatter changed the lines around
        // it. A diff out of time would cover `b` and indent it as content.
        let virtual_text = "  a\nb\n  c\n";
        let map = prepare(
            virtual_text,
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]),
        );
        let answer = [edit((0, 0), (3, 0), "A\nb\nC\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  A\nb\n  C\n")
        );
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
    fn a_line_keeps_its_indent_when_the_line_before_it_goes() {
        // `0b c0` (left alone) deleted, `1a1` reindented: the remaining
        // line is `1a1`, not `0b c0` rewritten.
        let virtual_text = "0b c0\n  1a1\n";
        let map = dedent_lines(virtual_text, &[0, 2]);
        let answer = [edit((0, 0), (2, 0), " 1a1\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("   1a1\n")
        );
    }

    #[test]
    fn a_line_of_text_is_not_given_up_for_a_blank_line() {
        // A blank line written after the content could pair with the blank
        // line above `2x`, leaving `2x` to be written anew, indented.
        let virtual_text = "    0x\n\n2x\n";
        let map = dedent_lines(virtual_text, &[4]);
        let answer = [edit((0, 0), (3, 0), "0x\n  w\n\n2x\n\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("    0x\n      w\n\n2x\n\n")
        );
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
    fn text_written_on_a_kept_whitespace_line_gains_no_indent() {
        // The peer removed nothing from ` `: a line left alone, which text
        // written on keeps so. An empty line had nothing to remove: text
        // there is the content's, indented as it is.
        let virtual_text = "  a\n \n\n  b\n";
        let map = dedent_lines(virtual_text, &[2, 0, 0, 2]);
        let answer = [edit((0, 0), (4, 0), "a\n x\ny\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n x\n  y\n  b\n")
        );
    }

    #[test]
    fn a_line_break_changed_above_a_line_left_alone_keeps_that_lines_indent() {
        // `q` was left alone (no indent removed), `b` dedented: the break
        // above `q` turned CRLF and `q` written at column 0 keep apart.
        let virtual_text = "x\n  q\n  b\n";
        let map = dedent_lines(virtual_text, &[0, 0, 2]);
        let answer = [edit((0, 0), (3, 0), "x\r\nq\nb\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("x\r\nq\n  b\n")
        );
    }

    #[test]
    fn pairing_many_unalike_lines_stops_at_the_deadline() {
        // Thousands of lines none of which pairs: the join search is
        // quadratic, and must give up with the diffs for the answer to be
        // refused in time, not run on.
        let virtual_text: String = (0..20000).map(|line| format!("  a{line}\n")).collect();
        let changes: Vec<_> = (0..20000)
            .map(|line| json!({"range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 2}}, "newText": ""}))
            .collect();
        let map = prepare(&virtual_text, json!(changes));
        let formatted: String = (0..20000).map(|line| format!("b{line}\n")).collect();
        let answer = [edit((0, 0), (20000, 0), &formatted)];
        let started = std::time::Instant::now();
        assert_eq!(
            map.map_edits_within(&answer, EditMode::Formatting, Duration::from_millis(50)),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_line_deleted_above_one_losing_its_own_indent_keeps_them_apart() {
        // `0a0` (dedented) deleted, `\t1b c1` (left alone, a tab of its own)
        // brought to column 0: the deletion and the tab's touch at the line
        // start, and the line keeps its removed indent (none).
        let virtual_text = "  0a0\r\n\t1b c1\r\n";
        let map = dedent_lines(virtual_text, &[2, 0]);
        let answer = [edit((0, 0), (2, 0), "1b c1\r\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("1b c1\r\n")
        );
    }

    #[test]
    fn a_rewritten_line_pairs_with_its_own_text_before_any_it_starts_with() {
        // `ab` (dedented) stays, `a` (left alone) goes: `ab` starts with
        // `a`, but is itself, not `a` with text joined onto it.
        let virtual_text = "a\n  ab\n";
        let map = dedent_lines(virtual_text, &[0, 2]);
        let answer = [edit((0, 0), (2, 0), "ab\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  ab\n")
        );
    }

    #[test]
    fn a_line_reindented_from_a_tab_is_the_line_it_was() {
        // `\tb` (dedented, a tab of its own past it) reindented with
        // spaces: the same line of text, keeping the removed indent.
        let virtual_text = "a\n  \tb\n";
        let map = dedent_lines(virtual_text, &[0, 2]);
        let answer = [edit((0, 0), (2, 0), "  b\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("    b\n")
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
    fn a_line_deleted_after_a_changed_line_break_goes_with_it() {
        // The break turned CRLF, the dedented line after it deleted: the
        // deletion takes the line's removed indent, so the two go together.
        let virtual_text = "0a0\n  1a1\n";
        let map = dedent_lines(virtual_text, &[0, 2]);
        let answer = [edit((0, 0), (2, 0), "0a0\r\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("0a0\r\n")
        );
    }

    #[test]
    fn a_kept_whitespace_line_pairs_with_itself_before_an_empty_line() {
        // Two kept ` ` lines and an empty one; an empty line written
        // between the two: each ` ` is the line it was, the empty lines
        // alike among themselves.
        let virtual_text = "  a\n \n \n\n";
        let map = dedent_lines(virtual_text, &[2, 0, 0, 0]);
        let answer = [edit((0, 0), (4, 0), "a\n \n\n \n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("  a\n \n\n \n")
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
    fn lines_joined_keep_the_first_ones_indent() {
        // `0x` deleted and `1x` joined with `2x`: the joined line is `1x`.
        let virtual_text = "0if x:0\n    1if x:1\n    2if x:2\n";
        let map = dedent_lines(virtual_text, &[0, 4, 4]);
        let answer = [edit((0, 0), (3, 0), "1if x:1 2if x:2\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("    1if x:1 2if x:2\n")
        );
    }

    #[test]
    fn a_line_break_changed_before_text_written_on_a_line_keeps_them_apart() {
        // CRLF turned to LF on `a`, text written on the kept ` ` below it:
        // that text is the line's, not one after the changed break.
        let virtual_text = "\ta\r\n \r\n";
        let map = dedent_lines(virtual_text, &[1, 0]);
        let answer = [edit((0, 0), (2, 0), "a\nw\r\n")];
        assert_eq!(
            formatted_either_way(&map, virtual_text, &answer).as_deref(),
            Some("\ta\nw\r\n")
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
        let mapped = map.map_edits(&answer, EditMode::Formatting).unwrap();
        assert!(mapped.iter().all(|edit| edit.range.start.line >= 1));
    }

    #[test]
    fn an_answer_changing_nothing_maps_to_no_edit() {
        let (_, map) = dedented();
        let answer = [edit((0, 0), (2, 0), "if x:\n  y\n")];
        assert_eq!(map.map_edits(&answer, EditMode::Formatting), Some(vec![]));
        assert_eq!(map.map_edits(&[], EditMode::Formatting), Some(vec![]));
        // Nothing to diff: not refused for want of time.
        assert_eq!(
            map.map_edits_within(&answer, EditMode::Formatting, Duration::ZERO),
            Some(vec![])
        );
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
                map.map_edits(&[edit((0, 0), end, "")], EditMode::Formatting),
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
        // Line 0 lost two spaces, line 1 four: no indent for a new line.
        let virtual_text = "  a\n    b\n";
        let map = prepare(
            virtual_text,
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
            ]),
        );
        let answer = [edit((0, 0), (2, 0), "a\nb\nc\n")];
        assert_eq!(map.map_edits(&answer, EditMode::Formatting), None);
        // Changing text within a line needs no indent.
        assert_eq!(
            formatted_either_way(&map, virtual_text, &[edit((0, 0), (2, 0), "A\nb\n")]).as_deref(),
            Some("  A\n    b\n")
        );
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
        assert_eq!(map.map_edits(&overlapping, EditMode::Formatting), None);
        let reversed = [edit((1, 0), (0, 0), "x")];
        assert_eq!(map.map_edits(&reversed, EditMode::Formatting), None);
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
    use proptest::test_runner::{Config, TestCaseError, TestRunner};
    use std::cell::Cell;
    use std::time::Duration;

    const BODIES: [&str; 5] = ["a", "é", "𝄞x", "b c", "if x:"];

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
        /// Text (`BODIES`), indented and dedented by the peer, or left at
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
    }

    fn line() -> impl Strategy<Value = Line> {
        prop_oneof![
            5 => (0..BODIES.len(), prop::bool::weighted(0.8))
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

    /// `lines` indented by `indent` (text made unique per line, so a diff
    /// has one way to align lines), broken by `line_break`, between
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
                    text.push_str(&format!("{number}{}{number}", BODIES[body]));
                    removed.push(Some(if dedented { indent } else { "" }));
                }
                Line::Empty => removed.push(None),
                Line::Space => {
                    text.push(' ');
                    removed.push(Some(""));
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
    }

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
                Rewrite::Write(spaces) if is_blank(index) => output.push((
                    format!("{}w{index}", " ".repeat(spaces)),
                    line_break,
                    origin,
                )),
                Rewrite::Write(_) => output.push((text, line_break, origin)),
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
    fn check(budget: Duration, cases: u32) -> (usize, usize) {
        let mut runner = TestRunner::new(Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        });
        let (mapped_sets, changing, over_refused) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let strategy = (
            prop::collection::vec(line(), 1..6),
            prop::sample::select(vec!["  ", "\t", "    "]),
            prop::sample::select(vec!["\n", "\r\n", "\r"]),
            (0..3usize, 0..3usize, any::<bool>()),
            prop::collection::vec(rewrite(), 1..6),
            (any::<bool>(), any::<u8>()),
        );
        runner
            .run(
                &strategy,
                |(lines, indent, line_break, edges, rewrites, (flip, shape))| {
                    let Some(document) = document(&lines, indent, line_break, edges) else {
                        return Ok(());
                    };
                    let map = prepare(
                        &document.virtual_text,
                        serde_json::Value::from(document.changes.clone()),
                    );
                    let prepared = map.prepared_lines.text.clone();
                    let output = format(&prepared, &rewrites, flip);
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
                    let Some(mapped) = map.map_edits_within(&edits, EditMode::Formatting, budget)
                    else {
                        // Refused with time enough: an answer the host
                        // cannot take, or (rarely) one turning line breaks
                        // among several blank lines, whose pairing can meet
                        // a CR and an LF the check refuses.
                        if budget > Duration::ZERO && reference(&document, &output).is_some() {
                            over_refused.set(over_refused.get() + 1);
                        }
                        return Ok(());
                    };
                    mapped_sets.set(mapped_sets.get() + 1);
                    if formatted != prepared {
                        changing.set(changing.get() + 1);
                    }
                    let got = apply(&document.virtual_text, &mapped);
                    let expected = reference(&document, &output);
                    prop_assert_eq!(
                        Some(&got),
                        expected.as_ref(),
                        "{:?} -> {:?} on {:?} (P {:?})",
                        edits,
                        mapped,
                        document.virtual_text,
                        prepared
                    );
                    Ok::<(), TestCaseError>(())
                },
            )
            .unwrap();
        // Cases the host could take, refused: availability only, and rare.
        assert!(
            over_refused.get() * 200 <= cases as usize,
            "refused {} answers the host could take",
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
    fn answers_whose_diff_runs_out_of_time_are_refused() {
        // Only an answer changing nothing needs no diff.
        let (mapped, changing) = check(Duration::ZERO, 1000);
        assert_eq!(changing, 0, "mapped {mapped}");
        assert!(mapped > 0);
    }
}
