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
    Bias, LineMap, PreparedMap, Run, RunKind, Side, empty_run, is_line_start, map_offset,
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
    /// The existing line the edit ends before, which it joins: that line's
    /// own removed indent, whatever the line holds.
    Existing(&'a str),
}

impl PreparedMap {
    /// Translate a set of edits a downstream server made to P into edits to
    /// V, as `mode` reads them (see [`EditMode`]), one for each in order.
    ///
    /// Every edit is mapped on its own, keeping its extent and restoring the
    /// indentation the peer removed from lines it creates or replaces. The
    /// mapped set is then checked as a whole against the host text V holds
    /// and P does not show (see [`Self::mapped_edits_keep_host_text`]).
    /// `None` — refuse the set — when an edit is reversed, edits overlap in
    /// P or once mapped (or touch where either's text bears on the other),
    /// an edit reaches into a gap, a line created in content has no single
    /// indentation to regain, or the host text would not survive.
    ///
    /// The host's own text, gaps and edge lines, is always kept; but on a
    /// map with gaps the indentation lines regain is not yet exact in some
    /// shapes (a placeholder's own line break, a gap ending the document,
    /// segments that say nothing of their indent). Edit carriers open on
    /// contiguous regions only, which have none; one opening on a
    /// noncontiguous region needs those fixed first.
    pub(crate) fn map_edits(&self, edits: &[TextEdit], mode: EditMode) -> Option<Vec<TextEdit>> {
        let EditMode::Extent = mode;
        let prepared = &self.prepared_lines.text;
        let (ranges, order) = sorted_extents(&self.prepared_lines, edits)?;
        // Edits that touch — one ending where another starts, two inserting
        // at one place — are refused where how each regains a removed indent,
        // or how a CR and an LF meet, depends on the other's text, which
        // mapped one by one neither sees: at a line start, or with a line
        // break or no text at all on the side where they meet. Mid-line
        // between text, neither decides anything there.
        //
        // Lines inserted at a line start before another edit there (an
        // import above the line a completion rewrites) are the exception:
        // the insertion ends with a line break, after which the existing
        // line regains its own indent, and the later edit, starting with
        // text, keeps to that line — provided it maps after the insertion
        // (one taking the line's removed indent along would not; checked
        // once mapped).
        // Only at a line start the host has too: one a placeholder's own line
        // break shows in P mid-line in V starts no existing host line, whose
        // indent the inserted lines' last break could give back.
        let host_line_start = |offset: usize| {
            empty_run(&self.runs, offset, |kind| kind == RunKind::Deleted).is_some()
                || is_line_start(
                    &self.virtual_lines.text,
                    map_offset(&self.runs, offset, Bias::Start, Side::Prepared),
                )
        };
        let lines_before = |earlier: usize, later: usize| {
            ranges[earlier].is_empty()
                && edits[earlier].new_text.ends_with(['\n', '\r'])
                && begins_line(edits[later].new_text.chars().next())
                && host_line_start(ranges[later].start)
        };
        let mut lines_inserted_before = Vec::new();
        for pair in order.windows(2) {
            let (earlier, later) = (pair[0], pair[1]);
            let meeting = ranges[later].start;
            if ranges[earlier].end != meeting {
                continue;
            }
            if is_line_start(prepared, meeting) {
                if !lines_before(earlier, later) {
                    return None;
                }
                lines_inserted_before.push((earlier, later));
            } else if !begins_line(edits[earlier].new_text.chars().next_back())
                || !begins_line(edits[later].new_text.chars().next())
            {
                return None;
            }
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
        if lines_inserted_before
            .iter()
            .any(|&(earlier, later)| mapped[earlier].0.start != mapped[later].0.start)
        {
            return None;
        }
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
        // Edits apart in P map apart (touching ones are refused or keep
        // their order, see `map_edits`); the overlap check stays as the
        // guard every later read of the sorted set relies on.
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
        // The edited V is read only beside host runs, outside every change:
        // the character before or after V offset `offset` there, looked up
        // in the changes meeting it rather than in a copy of the whole text.
        // Before it come the changes ending there (an insertion there
        // included, which goes before what follows it), after it those
        // starting there; one inserting nothing passes the lookup on.
        let before = |mut offset: usize| {
            let met = sorted.partition_point(|(range, _)| range.end <= offset);
            for (range, new_text) in sorted[..met].iter().rev() {
                if range.end != offset {
                    break;
                }
                if let Some(last) = new_text.chars().next_back() {
                    return Some(last);
                }
                offset = range.start;
            }
            virtual_text[..offset].chars().next_back()
        };
        let after = |mut offset: usize| {
            let met = sorted.partition_point(|(range, _)| range.start < offset);
            for (range, new_text) in &sorted[met..] {
                if range.start != offset {
                    break;
                }
                if let Some(first) = new_text.chars().next() {
                    return Some(first);
                }
                offset = range.end;
            }
            virtual_text[offset..].chars().next()
        };
        // Changes never reach into a gap (see `hunk_reaches_into_gap`) nor into
        // the deleted edge lines, which P has no position inside. Only a run
        // a change meets at one of its edges can read differently there than
        // in V, where each held: those are the runs looked at. A CR and an LF
        // already meeting across such an edge in V cannot be one a change
        // meets: its edge would fall between them, refused above.
        // The document's deleted closing lines are among the runs empty in
        // P at its end: searched from there, not through the whole map.
        let first_closing_line = self
            .runs
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, run)| run.prepared.is_empty())
            .filter(|(_, run)| run.kind == RunKind::TrailingLines)
            .last()
            .map(|(index, _)| index);
        let prepared = &self.prepared_lines.text;
        let host_owned = |run: &Run| {
            matches!(
                run.kind,
                RunKind::Gap | RunKind::LeadingLines | RunKind::TrailingLines
            )
        };
        for (range, _) in &sorted {
            // Runs starting where the change ends.
            let first = self
                .runs
                .partition_point(|run| run.virtual_.start < range.end);
            for (index, run) in self.runs[first..]
                .iter()
                .enumerate()
                .take_while(|(_, run)| run.virtual_.start == range.end)
            {
                let starts_line = (run.kind == RunKind::Gap
                    && is_line_start(virtual_text, run.virtual_.start))
                    || first_closing_line == Some(first + index);
                let before = before(run.virtual_.start);
                if starts_line && begins_line(before) {
                    return false;
                }
                // A line break at the run's edge is hidden when P does not
                // show it there: one the peer's text shows too (a gap
                // replaced by text opening with the same LF) was the
                // server's to join.
                let shown = &prepared[run.prepared.clone()];
                if host_owned(run)
                    && virtual_text[run.virtual_.clone()].starts_with('\n')
                    && !shown.starts_with('\n')
                    && before == Some('\r')
                {
                    return false;
                }
            }
            // Runs ending where the change starts.
            let first = self
                .runs
                .partition_point(|run| run.virtual_.end < range.start);
            for run in self.runs[first..]
                .iter()
                .take_while(|run| run.virtual_.end == range.start)
            {
                let shown = &prepared[run.prepared.clone()];
                if host_owned(run)
                    && virtual_text[run.virtual_.clone()].ends_with('\r')
                    && !shown.ends_with('\r')
                    && after(run.virtual_.end) == Some('\n')
                {
                    return false;
                }
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
        if self.hunk_reaches_into_gap(&old) {
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
        // (A line start in V that P shows mid-line, after a gap whose
        // placeholder holds no line break, counts: its removed indent sits
        // there.)
        let starts_line = is_line_start(prepared, old.start)
            || empty_run(&self.runs, old.start, |kind| kind == RunKind::Deleted).is_some();
        let whole_lines = starts_line
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
        let removed_at_start = empty_run(&self.runs, old.start, |kind| kind == RunKind::Deleted)
            .filter(|run| run.virtual_.end == plain_start);
        // Lines inserted at a line start whose removed indent is not the
        // content's (the indentation-only last line's, shallower than the
        // content in the nixfmt layout) are lines of the content: they go
        // before that indent, which the existing line gets back after them —
        // joined by the text after their last line break, if any.
        let lines_before_own_indent = old.is_empty()
            && new_text.contains(['\n', '\r'])
            && removed_at_start.is_some_and(|run| {
                Some(&self.virtual_lines.text[run.virtual_.clone()])
                    != self.uniform_indent_at(plain_start)
            });
        let start_indent = (whole_lines || lines_before_own_indent)
            .then_some(removed_at_start)
            .flatten();
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
        if self.virtual_range_reaches_into_gap(virtual_start, virtual_end) {
            return None;
        }
        // What follows the change in V: a gap the peer emptied right at its
        // end, not the text after it in P — host text a line can start with
        // (an interpolation), or a line break (a fence's).
        let following = match empty_run(&self.runs, old.end, |kind| kind == RunKind::Gap) {
            Some(gap) if gap.virtual_.start == virtual_end => {
                self.virtual_lines.text[gap.virtual_.clone()].chars().next()
            }
            _ => following,
        };
        let virtual_text = &self.virtual_lines.text;
        // The existing line after a change ending at a P line start keeps its
        // own removed indent (none, for a line the peer did not dedent), not
        // the segment's uniform one — blank or whitespace-only as it may be,
        // the indentation-only last line too. The change took that indent
        // along (or, inserting, left it before its own text): it gives it
        // back. With nothing after P's end, there is no such line.
        let removed_at_end = empty_run(&self.runs, old.end, |kind| kind == RunKind::Deleted)
            .filter(|run| run.virtual_.end == virtual_end);
        // (A P line start a gap's placeholder opened mid-line in V starts
        // no existing line: what follows is the rest of that V line.)
        let tail = (is_line_start(prepared, old.end)
            && (removed_at_end.is_some()
                || following.is_some() && is_line_start(virtual_text, virtual_end)))
        .then(|| removed_at_end.map_or("", |run| &virtual_text[run.virtual_.clone()]));
        // A V line start right after a gap's own line break (a fence the
        // peer replaced by text without one) is a line start P shows
        // mid-line.
        let after_gap = {
            let first = self
                .runs
                .partition_point(|run| run.virtual_.end < virtual_start);
            self.runs[first..]
                .iter()
                .take_while(|run| run.virtual_.end == virtual_start)
                .any(|run| run.kind == RunKind::Gap && !run.prepared.is_empty())
        };
        let first = match start_indent {
            _ if lines_before_own_indent => FirstIndent::Uniform,
            // Whole lines deleted before an existing line, or replaced by
            // text without a line break before a non-blank one: what remains
            // is that line, which keeps its own indent (deleting a line must
            // not re-indent the next). Text joining a blank line is that
            // line's content instead, indented as the replaced line was.
            _ if whole_lines
                && is_line_start(virtual_text, virtual_start)
                && !new_text.contains(['\n', '\r'])
                && tail.is_some()
                && (new_text.is_empty() || begins_line(following)) =>
            {
                FirstIndent::Existing(tail.unwrap_or_default())
            }
            // The replaced line's own removed indent comes back.
            Some(run) => FirstIndent::Exact(&virtual_text[run.virtual_.clone()]),
            // Text landing at a V line start that kept no indent before it
            // starts a line of its own — at a blank line or the document end,
            // or when it breaks the line — and needs the indent too. Text that
            // just joins a non-blank line the peer did not dedent belongs to
            // that line, which has no indent to restore.
            None if is_line_start(virtual_text, virtual_start)
                && (is_line_start(prepared, old.start) || after_gap)
                && (is_line_end(virtual_text, virtual_start)
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
        // Text joining that line takes its indent only from a line of text
        // or one holding the indent the peer removed: joining a blank line,
        // it starts a new line of the content.
        let joined = tail.filter(|_| removed_at_end.is_some() || begins_line(following));
        let inserted = self.reindent(virtual_start, first, new_text, following, tail, joined)?;
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

    /// The indent lines created at V `virtual_start` regain: their segment's
    /// uniform one (`None` when its removed indents differ; none at all
    /// outside every content segment).
    fn uniform_indent_at(&self, virtual_start: usize) -> Option<&str> {
        let candidate = self
            .indents
            .partition_point(|indent| indent.virtual_range.end < virtual_start);
        self.indents
            .get(candidate)
            .filter(|indent| indent.virtual_range.start <= virtual_start)
            .map_or(Some(""), |indent| indent.uniform.as_deref())
    }

    /// Whether replacing P's `old` bytes reaches into a gap's text in P (a
    /// gap emptied in P: whether it spans the gap's position).
    fn hunk_reaches_into_gap(&self, old: &Range<usize>) -> bool {
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
                    // position removes the host text around it. (The mapped
                    // range's backstop in `hunk_to_virtual` refuses this
                    // too; kept so this check alone states the rule.)
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
        joined: Option<&str>,
    ) -> Option<String> {
        let breaks_line = text.contains(['\n', '\r']);
        // New text starting a line takes an indent unless it opens with a
        // line break (a blank line carries none); an existing line's own
        // indent comes back whatever it holds.
        let first = match first {
            FirstIndent::Existing(_) => first,
            _ if begins_line(text.chars().next().or(following)) => first,
            _ => FirstIndent::None,
        };
        if matches!(first, FirstIndent::None) && !breaks_line {
            return Some(text.to_string());
        }
        let uniform = || self.uniform_indent_at(virtual_start);
        let first_indent = match first {
            FirstIndent::None => "",
            FirstIndent::Exact(indent) | FirstIndent::Existing(indent) => indent,
            FirstIndent::Uniform => uniform()?,
        };
        let line_indent = if breaks_line { uniform()? } else { "" };
        let mut output = String::with_capacity(text.len() + first_indent.len() + line_indent.len());
        output.push_str(first_indent);
        // The text after the last line break joins the existing line the
        // edit ends before (when one follows): that line's own indent, not
        // the segment's, starts it — a line the peer left alone stays
        // unindented, the indentation-only last line keeps its own.
        let last_break = text.rfind(['\n', '\r']);
        let mut chars = text.char_indices().peekable();
        while let Some((at, character)) = chars.next() {
            output.push(character);
            let next = chars.peek().map(|&(_, next)| next);
            let line_break = character == '\n' || (character == '\r' && next != Some('\n'));
            if !line_break {
                continue;
            }
            match (next, tail, joined) {
                // The break before the existing line: that line's own indent.
                (None, Some(tail), _) => output.push_str(tail),
                // The last break, its text joining the existing line.
                (Some(next), _, Some(joined))
                    if Some(at) == last_break && begins_line(Some(next)) =>
                {
                    output.push_str(joined)
                }
                // A new line starting with text (not another break).
                (next, _, _) if begins_line(next.or(following)) => output.push_str(line_indent),
                _ => {}
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
/// edit is reversed, names a line past the text's last, or edits overlap.
fn sorted_extents(lines: &LineMap, edits: &[TextEdit]) -> Option<(Vec<Range<usize>>, Vec<usize>)> {
    let ranges = edits
        .iter()
        .map(|edit| {
            // A reversed range is refused as sent: clamped past the end, both
            // its ends would read as one insertion there.
            if edit.range.start > edit.range.end {
                return None;
            }
            // A line past the text's last names no place in it: LSP reads a
            // column past its line as the line's end, but not a line past the
            // document as its end (an import "at line 7" of a 3-line P would
            // land after its content).
            lines.index.0.line(edit.range.end.line)?;
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

/// Whether `next` is a character that is no line break: one that begins
/// (or continues) a line rather than ending it.
fn begins_line(next: Option<char>) -> bool {
    next.is_some_and(|next| next != '\n' && next != '\r')
}

fn is_line_end(text: &str, offset: usize) -> bool {
    offset == text.len() || matches!(text.as_bytes().get(offset), Some(b'\n' | b'\r'))
}

#[cfg(test)]
mod tests;
