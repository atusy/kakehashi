//! Edits a downstream server made to a prepared document (P), mapped onto
//! its virtual document (V).
//!
//! An edit keeps to the text P shows: it maps through the same runs
//! positions do, and the lines it creates regain the indentation the peer
//! removed from the segment they land in. The mapped set is then checked as
//! a whole against the host text V holds and P does not show, so edits that
//! are fine one by one cannot combine into a corruption of it.

use std::ops::Range;

use tower_lsp_server::ls_types::{Range as LspRange, TextEdit};

use super::{
    Bias, LineMap, PreparedMap, RunKind, Side, empty_run, is_line_start, map_offset,
    map_offset_stopping, splits_crlf,
};

/// How a set of edits to P is read when mapped to V
/// ([`PreparedMap::map_edits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditMode {
    /// Edits keeping their extents (a completion's replace range, an inlay
    /// hint's text edits), mapped one by one.
    Extent,
}

/// The indentation a mapped edit's first line regains.
#[derive(Clone, Copy)]
enum FirstIndent<'a> {
    /// None: the edit starts after an indent V still has.
    None,
    /// The segment's uniform indent: the edit starts a line V never indented
    /// (the document end, or a line the peer did not dedent).
    Uniform,
    /// Exactly this: the edit replaces a whole line including the indent the
    /// peer removed from it.
    Exact(&'a str),
}

impl PreparedMap {
    /// Translate a set of edits a downstream server made to P into edits to
    /// V, as `mode` reads them (see [`EditMode`]), one for each in order.
    ///
    /// Every edit is mapped on its own, keeping its extent and restoring the
    /// indentation the peer removed from lines it creates or replaces. The
    /// mapped set is then checked as a whole against the host text V holds
    /// and P does not show (see [`Self::mapped_edits_keep_host_text`]).
    /// `None` — refuse the set — when an edit is reversed, edits overlap or
    /// touch in P (or overlap once mapped), an edit touches a gap, a line
    /// created in content has no single indentation to regain, or the host
    /// text would not survive.
    pub(crate) fn map_edits(&self, edits: &[TextEdit], mode: EditMode) -> Option<Vec<TextEdit>> {
        let EditMode::Extent = mode;
        let prepared = &self.prepared_lines.text;
        let (ranges, order) = sorted_extents(&self.prepared_lines, edits)?;
        // Edits that touch — one ending where another starts, two inserting
        // at one place — are refused: how each regains a removed indent, and
        // on which side of the other it lands, depends on the other's text,
        // which mapped one by one neither sees.
        if order
            .windows(2)
            .any(|pair| ranges[pair[0]].end == ranges[pair[1]].start)
        {
            return None;
        }
        let mut mapped = vec![None; edits.len()];
        for &index in &order {
            let following = prepared[ranges[index].end..].chars().next();
            mapped[index] = Some(self.hunk_to_virtual(
                ranges[index].clone(),
                &edits[index].new_text,
                following,
            )?);
        }
        let mapped: Vec<(Range<usize>, String)> = mapped
            .into_iter()
            .map(|hunk| hunk.expect("every edit is placed"))
            .collect();
        self.mapped_edits_keep_host_text(&mapped).then(|| {
            mapped
                .into_iter()
                .map(|(range, new_text)| TextEdit {
                    range: LspRange::new(
                        self.virtual_lines.position(range.start),
                        self.virtual_lines.position(range.end),
                    ),
                    new_text,
                })
                .collect()
        })
    }

    /// Whether V edits, mapped one by one, keep apart and keep the host
    /// text V holds that P does not show as it stands:
    ///
    /// - host text that starts a line in V — a gap such as a closing fence
    ///   between combined blocks, or the document's deleted closing lines —
    ///   still starts one: a change ending right before it ends with a line
    ///   break or deletes whole lines, as at the region's own end;
    /// - a line break P hides — a deleted edge line's, or a gap's the peer
    ///   replaced — does not fuse with one beside it into a single CRLF,
    ///   which would drop a line P keeps apart from it. One P shows is the
    ///   edits' own doing.
    ///
    /// Checked on the edited V as a whole: a completion's main edit and its
    /// additional edits, say, each fine alone, can fuse a CR and an LF only
    /// together.
    fn mapped_edits_keep_host_text(&self, mapped: &[(Range<usize>, String)]) -> bool {
        let virtual_text = &self.virtual_lines.text;
        let mut sorted: Vec<&(Range<usize>, String)> = mapped.iter().collect();
        sorted.sort_by_key(|(range, _)| (range.start, range.end));
        if sorted
            .windows(2)
            .any(|pair| pair[0].0.end > pair[1].0.start)
        {
            return false;
        }
        // An edge between a CR and an LF names no LSP position: one a gap's
        // text opens with, after content ending with the CR, say.
        if sorted.iter().any(|(range, _)| {
            splits_crlf(virtual_text, range.start) || splits_crlf(virtual_text, range.end)
        }) {
            return false;
        }
        let mut edited = String::with_capacity(virtual_text.len());
        let mut cursor = 0;
        for (range, new_text) in &sorted {
            edited.push_str(&virtual_text[cursor..range.start]);
            edited.push_str(new_text);
            cursor = range.end;
        }
        edited.push_str(&virtual_text[cursor..]);
        // Where V offset `offset`, outside every change, lands in the edited
        // V: after the changes ending at or before it (an insertion there
        // included, which goes before what follows it).
        let shifted = |offset: usize| -> usize {
            sorted
                .iter()
                .take_while(|(range, _)| range.end <= offset)
                .fold(offset, |at, (range, new_text)| {
                    at + new_text.len() - range.len()
                })
        };
        // Changes never reach into a gap (see `hunk_touches_gap`) nor into
        // the deleted edge lines, which P has no position inside.
        let first_closing_line = self
            .runs
            .iter()
            .find(|run| run.kind == RunKind::TrailingLines);
        let line_starts = self
            .runs
            .iter()
            .filter(|run| {
                run.kind == RunKind::Gap && is_line_start(virtual_text, run.virtual_.start)
            })
            .chain(first_closing_line);
        for run in line_starts {
            if !is_line_start(&edited, shifted(run.virtual_.start)) {
                return false;
            }
        }
        let prepared = &self.prepared_lines.text;
        let host_owned = self.runs.iter().filter(|run| {
            matches!(
                run.kind,
                RunKind::Gap | RunKind::LeadingLines | RunKind::TrailingLines
            )
        });
        for run in host_owned {
            let text = &virtual_text[run.virtual_.clone()];
            let start = shifted(run.virtual_.start);
            let end = start + text.len();
            // A line break at the run's edge is hidden when P does not show
            // it there: one the peer's text shows too (a gap replaced by text
            // ending with the same CR) was the server's to join.
            let shown = &prepared[run.prepared.clone()];
            let cr_hidden = text.ends_with('\r') && !shown.ends_with('\r');
            let lf_hidden = text.starts_with('\n') && !shown.starts_with('\n');
            let fused_after = cr_hidden
                && edited[end..].starts_with('\n')
                && !virtual_text[run.virtual_.end..].starts_with('\n');
            let fused_before = lf_hidden
                && edited[..start].ends_with('\r')
                && !virtual_text[..run.virtual_.start].ends_with('\r');
            if fused_after || fused_before {
                return false;
            }
        }
        true
    }

    /// Map the replacement of P's `old` bytes by `new_text` onto V.
    /// `following` is the character after the replacement in P.
    fn hunk_to_virtual(
        &self,
        old: Range<usize>,
        new_text: &str,
        following: Option<char>,
    ) -> Option<(Range<usize>, String)> {
        if self.hunk_touches_gap(&old) {
            return None;
        }
        let prepared_after = following;
        // A change starting at a P line start that removes whole lines (its
        // old text spans a line break), clears a line, or opens with a line
        // break covers the whole line in V, its removed indent included:
        // deleting or clearing a dedented line must not leave that indent
        // behind as trailing whitespace. Its end then keeps the next line's
        // indent the same way. Anywhere else (a change within one line's
        // content, an insertion, a change joining onto the previous line) a
        // line start maps after its removed indent, as positions do — so a
        // completion replacing the line's first word keeps the indent.
        let prepared = &self.prepared_lines.text;
        let clears_line = new_text.is_empty() && is_line_end(prepared, old.end);
        let whole_lines = is_line_start(prepared, old.start)
            && (prepared[old.clone()].contains(['\n', '\r'])
                || clears_line && !old.is_empty()
                || new_text.starts_with(['\n', '\r']));
        let plain_start = match empty_run(&self.runs, old.start, |kind| kind == RunKind::Gap) {
            // An insertion where the peer emptied a gap goes before the gap's
            // host text, like a change ending there and an insertion at the
            // document end: the start bias alone would carry it past.
            Some(gap) if old.is_empty() => gap.virtual_.start,
            _ => map_offset(&self.runs, old.start, Bias::Start, Side::Prepared),
        };
        // The removed indent right before where the start plainly maps — not
        // one beyond a gap the peer emptied at the same P offset.
        let start_indent = whole_lines
            .then(|| empty_run(&self.runs, old.start, |kind| kind == RunKind::Deleted))
            .flatten()
            .filter(|run| run.virtual_.end == plain_start);
        let virtual_start = start_indent.map_or(plain_start, |run| run.virtual_.start);
        // The end maps after a removed indent at a line start, unlike a range
        // end: the change takes that indent along, and `reindent` restores
        // it once. It still stops before a gap the peer emptied there.
        let virtual_end = if old.is_empty() {
            plain_start
        } else {
            map_offset_stopping(&self.runs, old.end, Bias::End, Side::Prepared, |kind| {
                kind == RunKind::Gap
            })
        };
        // Backstop: host text a gap stands for is never inside a mapped change.
        if self.virtual_range_touches_gap(virtual_start, virtual_end) {
            return None;
        }
        // What follows the change in V: a gap the peer emptied right at its
        // end is not the text after it in P.
        let following = match empty_run(&self.runs, old.end, |kind| kind == RunKind::Gap) {
            Some(gap) if gap.virtual_.start == virtual_end => None,
            _ => following,
        };
        let virtual_text = &self.virtual_lines.text;
        // The existing line after a change ending at a P line start keeps its
        // own removed indent (none, for a line the peer did not dedent), not
        // the segment's uniform one.
        let tail = (following.is_some() && is_line_start(prepared, old.end)).then(|| {
            empty_run(&self.runs, old.end, |kind| kind == RunKind::Deleted)
                .map_or("", |run| &virtual_text[run.virtual_.clone()])
        });
        let first = match start_indent {
            // Whole lines replaced by text without a line break, before a
            // non-blank line: what remains is that existing line, which keeps
            // its own indent (deleting a line must not re-indent the next).
            Some(_)
                if !new_text.contains(['\n', '\r'])
                    && tail.is_some()
                    && following.is_some_and(|next| next != '\n' && next != '\r') =>
            {
                FirstIndent::Exact(tail.unwrap_or_default())
            }
            // The replaced line's own removed indent comes back.
            Some(run) => FirstIndent::Exact(&virtual_text[run.virtual_.clone()]),
            // Text landing at a V line start that kept no indent before it
            // starts a line of its own — at a blank line or the document end,
            // or when it breaks the line — and needs the indent too. Text that
            // just joins a non-blank line the peer did not dedent belongs to
            // that line, which has no indent to restore.
            None if is_line_start(virtual_text, virtual_start)
                && is_line_start(prepared, old.start)
                && (is_blank_from(virtual_text, virtual_start)
                    || new_text.contains(['\n', '\r'])) =>
            {
                FirstIndent::Uniform
            }
            None => FirstIndent::None,
        };
        // An empty insertion inserts nothing, not even an indent.
        let first = if old.is_empty() && new_text.is_empty() {
            FirstIndent::None
        } else {
            first
        };
        let inserted = self.reindent(virtual_start, first, new_text, following, tail)?;
        // A CR and an LF meeting at either edge of the change make one line
        // break in P, or two in V where host text the change stops beside (an
        // emptied gap) sits between them: the server's lines would no longer
        // be V's. Each edge must join or keep apart alike on both sides.
        let fuses =
            |before: Option<char>, after: Option<char>| before == Some('\r') && after == Some('\n');
        let prepared_before = prepared[..old.start].chars().next_back();
        let virtual_before = virtual_text[..virtual_start].chars().next_back();
        let virtual_after = virtual_text[virtual_end..].chars().next();
        let edges = [
            (
                fuses(prepared_before, new_text.chars().next().or(prepared_after)),
                fuses(virtual_before, inserted.chars().next().or(virtual_after)),
            ),
            (
                fuses(
                    new_text.chars().next_back().or(prepared_before),
                    prepared_after,
                ),
                fuses(
                    inserted.chars().next_back().or(virtual_before),
                    virtual_after,
                ),
            ),
        ];
        if edges
            .iter()
            .any(|(prepared, virtual_)| prepared != virtual_)
        {
            return None;
        }
        Some((virtual_start..virtual_end, inserted))
    }

    /// Whether replacing P's `old` bytes reaches into a gap's text in P (a
    /// gap emptied in P: whether it spans the gap's position).
    fn hunk_touches_gap(&self, old: &Range<usize>) -> bool {
        // Runs tile P in order: only those from the first ending at or after
        // the change's start can touch it.
        let first = self
            .runs
            .partition_point(|run| run.prepared.end < old.start);
        self.runs[first..]
            .iter()
            .take_while(|run| run.prepared.start <= old.end)
            .filter(|run| run.kind == RunKind::Gap)
            .any(|gap| {
                if gap.prepared.is_empty() {
                    // A gap replaced by nothing: only a change spanning its
                    // position removes the host text around it.
                    old.start < gap.prepared.start && gap.prepared.start < old.end
                } else {
                    old.start < gap.prepared.end && gap.prepared.start < old.end
                }
            })
    }

    /// Re-insert the segment's lost indentation at the start of every
    /// non-empty line the text begins: after each line break (the segment's
    /// uniform indent, or `tail` before the existing line that follows), and
    /// at the very start as `first` says. `None` when a line needs the
    /// segment's indent and the peer removed none uniformly there.
    fn reindent(
        &self,
        virtual_start: usize,
        first: FirstIndent<'_>,
        text: &str,
        following: Option<char>,
        tail: Option<&str>,
    ) -> Option<String> {
        let begins_line =
            |next: Option<char>| next.is_some_and(|next| next != '\n' && next != '\r');
        let breaks_line = text.contains(['\n', '\r']);
        let first = if begins_line(text.chars().next().or(following)) {
            first
        } else {
            FirstIndent::None
        };
        if matches!(first, FirstIndent::None) && !breaks_line {
            return Some(text.to_string());
        }
        let uniform = || {
            let candidate = self
                .indents
                .partition_point(|indent| indent.virtual_range.end < virtual_start);
            self.indents
                .get(candidate)
                .filter(|indent| indent.virtual_range.start <= virtual_start)
                .map_or(Some(""), |indent| indent.uniform.as_deref())
        };
        let first_indent = match first {
            FirstIndent::None => "",
            FirstIndent::Exact(indent) => indent,
            FirstIndent::Uniform => uniform()?,
        };
        let line_indent = if breaks_line { uniform()? } else { "" };
        let mut output = String::with_capacity(text.len() + first_indent.len() + line_indent.len());
        output.push_str(first_indent);
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            output.push(character);
            let line_break =
                character == '\n' || (character == '\r' && chars.peek() != Some(&'\n'));
            if line_break && begins_line(chars.peek().copied().or(following)) {
                match (chars.peek(), tail) {
                    // The break before existing text: that line's own indent.
                    (None, Some(tail)) => output.push_str(tail),
                    _ => output.push_str(line_indent),
                }
            }
        }
        Some(output)
    }
}

/// The byte ranges `edits` replace in `lines`' text (a column past its
/// line's end meaning the line's end, as LSP prescribes), and their indexes
/// in the order they apply: by start, then end, so an insertion goes before
/// a replacement starting where it is, in whichever order they came, and
/// same-position insertions in the order they were sent. `None` when an
/// edit is reversed or edits overlap.
fn sorted_extents(lines: &LineMap, edits: &[TextEdit]) -> Option<(Vec<Range<usize>>, Vec<usize>)> {
    let ranges = edits
        .iter()
        .map(|edit| {
            // A reversed range is refused as sent: clamped past the end, both
            // its ends would read as one insertion there.
            if edit.range.start > edit.range.end {
                return None;
            }
            let start = lines.offset_clamped(edit.range.start);
            let end = lines.offset_clamped(edit.range.end);
            (start <= end).then_some(start..end)
        })
        .collect::<Option<Vec<_>>>()?;
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&index| (ranges[index].start, ranges[index].end));
    if order
        .windows(2)
        .any(|pair| ranges[pair[0]].end > ranges[pair[1]].start)
    {
        return None;
    }
    Some((ranges, order))
}

fn is_line_end(text: &str, offset: usize) -> bool {
    offset == text.len() || matches!(text.as_bytes().get(offset), Some(b'\n' | b'\r'))
}

/// Whether `text` holds only whitespace from `offset` to its line's end.
fn is_blank_from(text: &str, offset: usize) -> bool {
    text[offset..]
        .split(['\n', '\r'])
        .next()
        .is_none_or(|rest| rest.trim().is_empty())
}

#[cfg(test)]
mod tests;
