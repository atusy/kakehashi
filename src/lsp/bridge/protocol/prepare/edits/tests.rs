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
    let (ranges, order) = sorted_extents(&lines, edits).expect("edits apply");
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
fn touching_edits_are_refused() {
    // How each regains a removed indent depends on the other's text: two
    // insertions at one place, or an edit ending where another starts.
    let (_, map) = dedented();
    for edits in [
        [edit((0, 1), (0, 1), "\n"), edit((0, 1), (0, 1), "b")],
        [edit((0, 0), (0, 0), "a"), edit((0, 0), (0, 2), "if")],
        [edit((0, 0), (1, 0), "z\n"), edit((1, 0), (2, 0), "")],
        [edit((1, 0), (1, 1), "w"), edit((0, 0), (1, 0), "")],
    ] {
        assert_eq!(map.map_edits(&edits, EditMode::Extent), None, "{edits:?}");
    }
    // Apart, they map.
    let edits = [edit((0, 0), (0, 2), "if"), edit((1, 0), (1, 1), "w")];
    assert!(map.map_edits(&edits, EditMode::Extent).is_some());
}

#[test]
fn edits_touching_mid_line_between_text_map() {
    // Neither decides an indent nor a line break where they meet (a
    // completion rewriting the receiver right before its own range, say).
    let (virtual_text, map) = dedented();
    let edits = [edit((0, 3), (0, 3), "z"), edit((0, 2), (0, 3), "_")];
    let mapped = map.map_edits(&edits, EditMode::Extent).unwrap();
    assert_eq!(apply_to(&virtual_text, &mapped), "  if_zx:\n    y\n");
    // With no text, or a line break, on the side where they meet, they do.
    for edits in [
        [edit((0, 2), (0, 3), ""), edit((0, 3), (0, 3), "z")],
        [edit((0, 2), (0, 3), "\n"), edit((0, 3), (0, 3), "z")],
        [edit((0, 2), (0, 3), "_"), edit((0, 3), (0, 3), "\rz")],
    ] {
        assert_eq!(map.map_edits(&edits, EditMode::Extent), None, "{edits:?}");
    }
}

#[test]
fn a_reversed_edit_past_the_end_is_refused() {
    let map = prepare("a", json!([]));
    assert_eq!(map.map_one_with_extent(&edit((2, 0), (1, 0), "X")), None);
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
        assert!(mapped_sets.get() > 500, "mapped {}", mapped_sets.get());
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
    let virtual_text = "  a\n\n";
    let map = prepare(
        virtual_text,
        json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]),
    );
    for at in [(1, 0), (2, 0)] {
        let mapped = map.map_one_with_extent(&edit(at, at, "")).unwrap();
        assert_eq!(mapped.new_text, "", "{at:?}");
    }
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
