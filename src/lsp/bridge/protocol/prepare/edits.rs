//! Edits a downstream server made to a prepared document (P), mapped onto
//! its virtual document (V).
//!
//! An edit keeps to the text P shows: it maps through the same runs
//! positions do, and the lines it creates regain the indentation the peer
//! removed from the segment they land in. The mapped set is then checked as
//! a whole against the host text V holds and P does not show, so edits that
//! are fine one by one cannot combine into a corruption of it.

use std::ops::Range;
use std::time::{Duration, Instant};

use tower_lsp_server::ls_types::{Range as LspRange, TextEdit};

use super::{
    Bias, LineMap, PreparedMap, Run, RunKind, Side, empty_run, is_line_start, map_offset,
    map_offset_stopping, rest_of_line_blank, splits_crlf,
};

/// How a set of edits to P is read when mapped to V
/// ([`PreparedMap::map_edits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditMode {
    /// Edits keeping their extents (a completion's replace range, an inlay
    /// hint's text edits), mapped one by one.
    Extent,
    /// A formatter's answer: what it changes counts, not how it says so. The
    /// edits are applied to P and the result diffed against P; each changed
    /// stretch maps as one edit would, so a whole-document replacement maps
    /// as precisely as small edits. The set mapped is the diff's, not the
    /// edits sent. A range may also end at the line just past P's last
    /// (`(lineCount, 0)`), naming P's end, as whole-document answers do.
    Formatting,
}

/// How long the diff of a formatting answer may search, pairing lines and
/// then the fewest changes within them (see [`EditMode::Formatting`]); the
/// work around the search (applying the answer to P, the per-character
/// tables, mapping each stretch) runs in time linear in the texts on top of
/// it. An answer whose diff runs out of time is refused: the diff then falls back to coarser stretches, which can
/// span lines the formatter left alone, and a stretch maps every line it
/// spans as one it wrote, with the segment's indent rather than the line's
/// own. So the budget bears only on which answers are refused.
const FORMATTING_DIFF_BUDGET: Duration = Duration::from_millis(200);

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
    /// V, as `mode` reads them (see [`EditMode`]): with extents, one for each
    /// in order; for a formatting answer, one for each stretch the edited P
    /// differs from P in, in P's order (none when it changes nothing).
    ///
    /// Every edit (with [`EditMode::Formatting`], every stretch the edited P
    /// differs from P in) is mapped on its own, keeping its extent and
    /// restoring the indentation the peer removed from lines it creates or
    /// replaces. The mapped set is then checked as a whole against the host
    /// text V holds and P does not show (see
    /// [`Self::mapped_edits_keep_host_text`]). `None` — refuse the set — when
    /// an edit is reversed, reaches a line past P's last (but for a
    /// formatting answer's end at P's end), edits overlap in P or once mapped
    /// (or, keeping their extents, touch where either's text bears on the
    /// other), an edit reaches into a gap, a line of text created in content
    /// has no single indentation to regain, or the host text would not
    /// survive.
    ///
    /// The host's own text, gaps and edge lines, is always kept; but on a
    /// map with gaps the indentation lines regain is not yet exact in some
    /// shapes (a placeholder's own line break, a gap ending the document,
    /// segments that say nothing of their indent). Edit carriers open on
    /// contiguous regions only, which have none; one opening on a
    /// noncontiguous region needs those fixed first.
    pub(crate) fn map_edits(&self, edits: &[TextEdit], mode: EditMode) -> Option<Vec<TextEdit>> {
        self.map_edits_within(edits, mode, FORMATTING_DIFF_BUDGET)
    }

    /// [`Self::map_edits`], the diff of a formatting answer searching for
    /// at most `budget`.
    pub(super) fn map_edits_within(
        &self,
        edits: &[TextEdit],
        mode: EditMode,
        budget: Duration,
    ) -> Option<Vec<TextEdit>> {
        match mode {
            EditMode::Extent => self.map_extents(edits),
            EditMode::Formatting => self.map_formatting(edits, budget),
        }
    }

    /// [`EditMode::Formatting`]: the edits applied to P, and each stretch
    /// the result differs from P in mapped as one edit.
    fn map_formatting(&self, edits: &[TextEdit], budget: Duration) -> Option<Vec<TextEdit>> {
        let prepared = &self.prepared_lines.text;
        let (ranges, order) = sorted_extents(&self.prepared_lines, edits, true)?;
        let mut edited = String::with_capacity(prepared.len());
        let mut cursor = 0;
        for &index in &order {
            edited.push_str(&prepared[cursor..ranges[index].start]);
            edited.push_str(&edits[index].new_text);
            cursor = ranges[index].end;
        }
        edited.push_str(&prepared[cursor..]);
        if edited == *prepared {
            return Some(Vec::new());
        }
        let deadline = Instant::now() + budget;
        let hunks = diff_hunks(prepared, &edited, deadline);
        // The diff gives up only once it has seen the deadline pass: a diff
        // finishing past it may have, and is not trusted (see
        // `FORMATTING_DIFF_BUDGET`).
        if Instant::now() >= deadline {
            return None;
        }
        // Each stretch maps on its own as an edit does: stretches touch only
        // where one writes over the start of a line another ends at (see
        // `diff_hunks`), each then on its own line; one taking a removed
        // indent the other also claims is refused as an overlap once mapped.
        let mapped = hunks
            .into_iter()
            .map(|hunk| {
                let following = prepared[hunk.old.end..].chars().next();
                self.hunk_to_virtual(hunk.old, &edited[hunk.new], following)
            })
            .collect::<Option<Vec<_>>>()?;
        self.to_text_edits(mapped)
    }

    /// [`EditMode::Extent`]: each edit mapped on its own.
    fn map_extents(&self, edits: &[TextEdit]) -> Option<Vec<TextEdit>> {
        let prepared = &self.prepared_lines.text;
        let (ranges, order) = sorted_extents(&self.prepared_lines, edits, false)?;
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
        // text, keeps to that line — provided both map to the same V start,
        // where the client applies them in order (a later edit taking the
        // line's removed indent along would start before the insertion;
        // checked once mapped).
        // Only at a line start the host has too: one a placeholder's own line
        // break shows in P mid-line in V starts no existing host line, whose
        // indent the inserted lines' last break could give back.
        let host_line_start = |offset: usize| {
            self.removed_indent_at(offset).is_some()
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
        self.to_text_edits(mapped)
    }

    /// The mapped V edits as LSP edits, once checked as a set (see
    /// [`Self::mapped_edits_keep_host_text`]).
    fn to_text_edits(&self, mapped: Vec<(Range<usize>, String)>) -> Option<Vec<TextEdit>> {
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
        let starts_line =
            is_line_start(prepared, old.start) || self.removed_indent_at(old.start).is_some();
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
        let removed_at_start = self
            .removed_indent_at(old.start)
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
        // A blank line inside the content the peer emptied of its whitespace
        // (not the indentation-only last line, which keeps its own): text
        // placed on it
        // is content (or joins the line the change ends in), so the change
        // covers that whitespace, which says nothing of the indent the text
        // regains.
        // (Only where that whitespace is not the content's indent: there,
        // text placed after it, as positions place it, reads the same.)
        let blank_at_start = removed_at_start.filter(|run| {
            let removed = &self.virtual_lines.text[run.virtual_.clone()];
            self.virtual_lines.text[run.virtual_.end..].starts_with(['\n', '\r'])
                && self.uniform_indent_at(plain_start) != Some(removed)
        });
        let start_indent = (whole_lines
            || lines_before_own_indent
            || blank_at_start.is_some() && !(old.is_empty() && new_text.is_empty()))
        .then_some(removed_at_start)
        .flatten();
        // Text without a line break replacing whole lines up to partway into
        // a later one that keeps text after it is the start of that later
        // line: the text left there is its own, and takes its removed indent
        // (none, for a line the peer did not dedent), not the one of the line
        // the change starts in. With nothing but whitespace left after it
        // (or nothing), the later line is gone, and the text is the start
        // line's.
        let ends_in_later_line = prepared[old.clone()].contains(['\n', '\r'])
            && !is_line_start(prepared, old.end)
            && !new_text.contains(['\n', '\r'])
            && !rest_of_line_blank(prepared, old.end);
        let end_line_indent = ends_in_later_line.then(|| {
            let line_start = prepared[..old.end]
                .rfind(['\n', '\r'])
                .map_or(0, |at| at + 1);
            self.removed_indent_at(line_start)
                .map_or("", |run| &self.virtual_lines.text[run.virtual_.clone()])
        });
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
        let removed_at_end = self
            .removed_indent_at(old.end)
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
        let first = first_indent(&EdgeShape {
            whole_lines,
            lines_before_own_indent,
            start_indent: start_indent.map(|run| &virtual_text[run.virtual_.clone()]),
            blank_at_start: blank_at_start.is_some(),
            end_line_indent,
            starts_virtual_line: is_line_start(virtual_text, virtual_start),
            starts_prepared_line_or_after_gap: is_line_start(prepared, old.start) || after_gap,
            ends_virtual_line: is_line_end(virtual_text, virtual_start),
            // Read only where `first_indent` does: a whitespace-only rest
            // of a long line is not scanned again for every edit on it.
            virtual_rest_blank: whole_lines
                && is_line_start(virtual_text, virtual_start)
                && rest_of_line_blank(virtual_text, virtual_start),
            text_breaks_line: new_text.contains(['\n', '\r']),
            tail,
            text_joins_line: new_text.is_empty() || begins_line(following),
            inserts_nothing: old.is_empty() && new_text.is_empty(),
        });
        // Text joining that line takes its indent only from a line of text
        // or one holding the indent the peer removed: joining a blank line,
        // it starts a new line of the content — a blank line the peer
        // emptied of its whitespace included, which says nothing of it.
        let emptied_at_end = removed_at_end.is_some_and(|run| {
            let rest = &virtual_text[run.virtual_.end..];
            rest.starts_with(['\n', '\r'])
        });
        let joined =
            tail.filter(|_| removed_at_end.is_some() && !emptied_at_end || begins_line(following));
        let inserted = self.reindent(virtual_start, first, new_text, following, tail, joined)?;
        let prepared_before = prepared[..old.start].chars().next_back();
        let virtual_before = virtual_text[..virtual_start].chars().next_back();
        let virtual_after = virtual_text[virtual_end..].chars().next();
        if !line_breaks_agree(
            (prepared_before, new_text, prepared_after),
            (virtual_before, &inserted, virtual_after),
        ) {
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
        // Only a new line starting with text needs the segment's indent: a
        // blank one, or one joining the existing line, does not.
        let mut line_indent = None;
        let mut output = String::with_capacity(text.len() + first_indent.len());
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
                (next, _, _) if begins_line(next.or(following)) => {
                    let indent = match line_indent {
                        Some(indent) => indent,
                        None => *line_indent.insert(uniform()?),
                    };
                    output.push_str(indent)
                }
                _ => {}
            }
        }
        Some(output)
    }
}

/// What decides the indentation an edit's first line regains: where its
/// edges fall in P and V, and what its text does there (see
/// [`first_indent`]).
struct EdgeShape<'a> {
    /// The change covers whole lines from a line start (see
    /// `PreparedMap::hunk_to_virtual`).
    whole_lines: bool,
    /// Lines inserted at a line start whose removed indent is not the
    /// content's: lines of the content, going before that indent.
    lines_before_own_indent: bool,
    /// The removed indent the change starts before, covering it.
    start_indent: Option<&'a str>,
    /// That indent is a blank line's the peer emptied, not the content's.
    blank_at_start: bool,
    /// The removed indent of the later line text without a line break is
    /// left the start of (see `PreparedMap::hunk_to_virtual`).
    end_line_indent: Option<&'a str>,
    /// The mapped start is a V line start.
    starts_virtual_line: bool,
    /// The change starts at a P line start, or right after a gap's own line
    /// break.
    starts_prepared_line_or_after_gap: bool,
    /// The mapped start ends its V line.
    ends_virtual_line: bool,
    /// The rest of the V line from the mapped start is only spaces and tabs.
    virtual_rest_blank: bool,
    /// The edit's text holds a line break.
    text_breaks_line: bool,
    /// The removed indent of the existing line the change ends before.
    tail: Option<&'a str>,
    /// The edit's text is empty, or the character after the change (P's, or
    /// the first of a gap's V text when one starts right there) is neither a
    /// line break nor the end.
    text_joins_line: bool,
    /// An empty insertion.
    inserts_nothing: bool,
}

/// The indentation the edit `shape` describes regains on its first line.
fn first_indent<'a>(shape: &EdgeShape<'a>) -> FirstIndent<'a> {
    // An empty insertion inserts nothing, not even an indent.
    if shape.inserts_nothing {
        return FirstIndent::None;
    }
    match shape.start_indent {
        _ if shape.lines_before_own_indent => FirstIndent::Uniform,
        // Whole lines deleted before an existing line, or replaced by text
        // without a line break before a non-blank one: what remains is that
        // line, which keeps its own indent (deleting a line must not
        // re-indent the next). Text joining a blank line is that line's
        // content instead, indented as the replaced line was.
        _ if shape.whole_lines
            && shape.starts_virtual_line
            && !shape.text_breaks_line
            && shape.tail.is_some()
            && shape.text_joins_line =>
        {
            FirstIndent::Existing(shape.tail.unwrap_or_default())
        }
        _ if shape.whole_lines
            && let Some(indent) = shape.end_line_indent =>
        {
            FirstIndent::Exact(indent)
        }
        // Text on a blank line is a line of the content.
        Some(_) if shape.blank_at_start => FirstIndent::Uniform,
        // Likewise replacing whole lines from a whitespace-only line the peer
        // kept as is: that whitespace says nothing of the indent.
        None if shape.whole_lines && shape.starts_virtual_line && shape.virtual_rest_blank => {
            FirstIndent::Uniform
        }
        // The replaced line's own removed indent comes back.
        Some(indent) => FirstIndent::Exact(indent),
        // Text landing at a V line start that kept no indent before it starts
        // a line of its own — at a blank line or the document end, or when it
        // breaks the line — and needs the indent too. Text that just joins a
        // non-blank line the peer did not dedent belongs to that line, which
        // has no indent to restore.
        None if shape.starts_virtual_line
            && shape.starts_prepared_line_or_after_gap
            && (shape.ends_virtual_line || shape.text_breaks_line) =>
        {
            FirstIndent::Uniform
        }
        None => FirstIndent::None,
    }
}

/// Whether a CR and an LF meet alike at either edge of a change in both
/// documents: each side is the character before the change, its text, and
/// the character after it. Meeting, they make one line break in P, or two
/// in V where host text the change stops beside (an emptied gap) sits
/// between them: the server's lines would no longer be V's.
fn line_breaks_agree(
    prepared: (Option<char>, &str, Option<char>),
    virtual_: (Option<char>, &str, Option<char>),
) -> bool {
    let fuses =
        |before: Option<char>, after: Option<char>| before == Some('\r') && after == Some('\n');
    let at_start = |(before, text, after): (Option<char>, &str, Option<char>)| {
        fuses(before, text.chars().next().or(after))
    };
    let at_end = |(before, text, after): (Option<char>, &str, Option<char>)| {
        fuses(text.chars().next_back().or(before), after)
    };
    at_start(prepared) == at_start(virtual_) && at_end(prepared) == at_end(virtual_)
}

/// The byte ranges `edits` replace in `lines`' text (a column past its
/// line's end meaning the line's end, as LSP prescribes), and their indexes
/// in the order they apply: by start, then end, so an insertion goes before
/// a replacement starting where it is, in whichever order they came, and
/// same-position insertions in the order they were sent. `None` when an
/// edit is reversed, names a line past the text's last, or edits overlap —
/// but for an end at the start of the line just past the last, naming the
/// text's end, when `end_past_last_line`.
fn sorted_extents(
    lines: &LineMap,
    edits: &[TextEdit],
    end_past_last_line: bool,
) -> Option<(Vec<Range<usize>>, Vec<usize>)> {
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
            let end = if lines.index.0.line(edit.range.end.line).is_some() {
                lines.offset_clamped(edit.range.end)
            } else if end_past_last_line
                && edit.range.end.character == 0
                && edit
                    .range
                    .end
                    .line
                    .checked_sub(1)
                    .is_some_and(|last| lines.index.0.line(last).is_some())
            {
                lines.text.len()
            } else {
                return None;
            };
            let start = lines.offset_clamped(edit.range.start);
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

/// A stretch two texts differ in: `old` bytes of the one replaced by `new`
/// bytes of the other.
struct Hunk {
    old: Range<usize>,
    new: Range<usize>,
}

/// The stretches `new` differs from `old` in, until `deadline`. Lines are
/// paired first (see [`align_lines`]): a line of text with the line it was
/// rewritten from, blank lines among themselves, lines joined onto another
/// with that one. Each pair, and each run of lines left between pairs, is
/// then diffed by character. So a stretch never covers a line both texts
/// hold alike, nor does a character diff align text of one line with text
/// of another; either would map that text as written on the wrong line,
/// with that line's indent. Past the deadline the diffs give coarser
/// stretches, which still turn `old` into `new` (see
/// `FORMATTING_DIFF_BUDGET`). A stretch never starts or ends between a CR
/// and its LF in either text (no LSP position names that place): it widens
/// over the unchanged character beside it. Stretches left touching merge,
/// but at the start of a line one writes text over or the other's line
/// break ends before.
fn diff_hunks(old: &str, new: &str, deadline: Instant) -> Vec<Hunk> {
    let old_lines = lines_with_breaks(old);
    let new_lines = lines_with_breaks(new);
    let line_start = |lines: &[(usize, &str)], text: &str, index: usize| {
        lines.get(index).map_or(text.len(), |&(start, _)| start)
    };
    let line_keys = |lines: &[(usize, &str)]| -> [Vec<String>; 2] {
        let content = |line: &str| line.trim_end_matches(['\r', '\n']).to_string();
        [
            lines.iter().map(|&(_, line)| content(line)).collect(),
            lines
                .iter()
                .map(|&(_, line)| content(line).trim_matches([' ', '\t']).to_string())
                .collect(),
        ]
    };
    let (old_keys, new_keys) = (line_keys(&old_lines), line_keys(&new_lines));
    let mut aligned = Vec::new();
    align_lines(
        &old_keys,
        &new_keys,
        0,
        0..old_lines.len(),
        0..new_lines.len(),
        deadline,
        &mut aligned,
    );
    let block = |lines: &[(usize, &str)], text: &str, range: Range<usize>| {
        line_start(lines, text, range.start)..line_start(lines, text, range.end)
    };
    let blocks = aligned.into_iter().filter_map(|(old_range, new_range)| {
        // A pair alike to the byte, or nothing on either side, is no change.
        let same = old_range.len() == 1
            && new_range.len() == 1
            && old_lines[old_range.start].1 == new_lines[new_range.start].1;
        (!same && !(old_range.is_empty() && new_range.is_empty())).then(|| {
            (
                block(&old_lines, old, old_range),
                block(&new_lines, new, new_range),
            )
        })
    });
    let mut hunks: Vec<Hunk> = Vec::new();
    for (old_block, new_block) in blocks {
        let old_chars: Vec<(usize, char)> = old[old_block.clone()]
            .char_indices()
            .map(|(at, c)| (old_block.start + at, c))
            .collect();
        let new_chars: Vec<(usize, char)> = new[new_block.clone()]
            .char_indices()
            .map(|(at, c)| (new_block.start + at, c))
            .collect();
        let byte_at = |chars: &[(usize, char)], end: usize, index: usize| {
            chars.get(index).map_or(end, |&(byte, _)| byte)
        };
        let old_values: Vec<char> = old_chars.iter().map(|&(_, c)| c).collect();
        let new_values: Vec<char> = new_chars.iter().map(|&(_, c)| c).collect();
        for op in similar::capture_diff_slices_deadline(
            similar::Algorithm::Myers,
            &old_values,
            &new_values,
            Some(deadline),
        ) {
            if matches!(op, similar::DiffOp::Equal { .. }) {
                continue;
            }
            let (old_range, new_range) = (op.old_range(), op.new_range());
            let mut old_bytes = byte_at(&old_chars, old_block.end, old_range.start)
                ..byte_at(&old_chars, old_block.end, old_range.end);
            let mut new_bytes = byte_at(&new_chars, new_block.end, new_range.start)
                ..byte_at(&new_chars, new_block.end, new_range.end);
            // The character widened over is unchanged, the same in both
            // texts: a CR before the start, an LF after the end.
            if splits_crlf(old, old_bytes.start) || splits_crlf(new, new_bytes.start) {
                old_bytes.start -= 1;
                new_bytes.start -= 1;
            }
            if splits_crlf(old, old_bytes.end) || splits_crlf(new, new_bytes.end) {
                old_bytes.end += 1;
                new_bytes.end += 1;
            }
            // Touching stretches merge (mapped apart, each would read the
            // other's place as P's), but not at a line start where the later
            // writes text over that line's start, or, after a line break of
            // P the earlier ends with, changes only that line's leading
            // whitespace: merged, the later line would map as one the
            // stretch writes anew, with the content's indent rather than its
            // own. Apart, neither's text bears on the other there. Whole
            // lines the later removes go with the earlier, the removed
            // indents they take along with them.
            let apart = |last: &Hunk| {
                is_line_start(old, old_bytes.start)
                    && ((!new_bytes.is_empty() && !old_bytes.is_empty())
                        || (old[..last.old.end].ends_with(['\n', '\r'])
                            && !old[old_bytes.clone()].contains(['\n', '\r'])))
            };
            match hunks.last_mut() {
                Some(last)
                    if last.old.end > old_bytes.start
                        || last.new.end > new_bytes.start
                        || ((last.old.end == old_bytes.start
                            || last.new.end == new_bytes.start)
                            && !apart(last)) =>
                {
                    last.old.end = old_bytes.end;
                    last.new.end = new_bytes.end;
                }
                _ => hunks.push(Hunk {
                    old: old_bytes,
                    new: new_bytes,
                }),
            }
        }
    }
    hunks
}

/// One way [`align_lines`] pairs lines.
#[derive(Clone, Copy)]
enum Alignment {
    /// Lines whose keys (0: the text without its break; 1: that trimmed of
    /// whitespace) a diff finds alike, among the lines whose key passes.
    Diff(usize, similar::Algorithm, fn(&str) -> bool),
    /// A line written as an old line's text with more after it: that line,
    /// lines joined onto it (see [`joined_lines`]).
    Joined,
}

/// How [`align_lines`] pairs lines, from the surest to the loosest:
///
/// - Lines of text alike but for the whitespace around it, a line found
///   once in each text anchoring the rest (patience): a reindented line is
///   the line it was, and a blank line cannot pair with it instead.
/// - Lines of text other lines were joined onto.
/// - Lines holding whitespace alike but for their line break (patience
///   again: such a line unlike the others is that line), before empty
///   lines, which are all alike, can pair with it instead.
/// - Blank lines alike but for whitespace, before text written in a run
///   can take a blank line's place: a blank line given other whitespace is
///   the line it was, and a line written beside it one of its own.
///
/// What is left between pairs (lines rewritten past telling, lines written
/// or removed) is diffed by character as one run. Which of several blank
/// lines alike a line was written on, or which went, no answer tells: the
/// run's diff decides, and text it writes on a whitespace line the peer
/// kept gains no indent.
const ALIGNMENTS: [Alignment; 4] = [
    Alignment::Diff(1, similar::Algorithm::Patience, |key| !key.is_empty()),
    Alignment::Joined,
    Alignment::Diff(0, similar::Algorithm::Patience, |key| !key.is_empty()),
    Alignment::Diff(1, similar::Algorithm::Myers, |_| true),
];

/// Pairs of an old line and the new line written as its text with more
/// after it, in order: the first such old line for each new line, past the
/// one paired before.
///
/// The search is quadratic in the lines left unpaired: it stops at
/// `deadline`, as the diffs do, for the answer to be refused.
fn joined_lines(
    old_keys: &[Vec<String>; 2],
    new_keys: &[Vec<String>; 2],
    old_range: &Range<usize>,
    new_range: &Range<usize>,
    deadline: Instant,
) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut old_from = old_range.start;
    for new_line in new_range.clone() {
        if Instant::now() >= deadline {
            break;
        }
        let written = &new_keys[1][new_line];
        if let Some(old_line) = (old_from..old_range.end).find(|&old_line| {
            let text = &old_keys[1][old_line];
            !text.is_empty() && written.starts_with(text.as_str())
        }) {
            pairs.push((old_line, new_line));
            old_from = old_line + 1;
        }
    }
    pairs
}

/// Pair the lines of `old_range` with those of `new_range` at
/// `ALIGNMENTS[level]`, and each looser one between the pairs it finds:
/// `aligned` is given in order every pair (one line each) and every run
/// left between pairs (to diff by character). `keys` are each line's text
/// without its break, then trimmed of whitespace. A line joined onto
/// another is left to a run, deleted whole: the text it gave is written on
/// the line it joined.
fn align_lines(
    old_keys: &[Vec<String>; 2],
    new_keys: &[Vec<String>; 2],
    level: usize,
    old_range: Range<usize>,
    new_range: Range<usize>,
    deadline: Instant,
    aligned: &mut Vec<(Range<usize>, Range<usize>)>,
) {
    if old_range.is_empty() || new_range.is_empty() {
        aligned.push((old_range, new_range));
        return;
    }
    let Some(&alignment) = ALIGNMENTS.get(level) else {
        aligned.push((old_range, new_range));
        return;
    };
    let pairs: Vec<(usize, usize)> = match alignment {
        Alignment::Joined => joined_lines(old_keys, new_keys, &old_range, &new_range, deadline),
        Alignment::Diff(keys, algorithm, takes_part) => {
            let candidates = |all: &[String], range: &Range<usize>| -> Vec<usize> {
                range
                    .clone()
                    .filter(|&index| takes_part(&all[index]))
                    .collect()
            };
            let old_candidates = candidates(&old_keys[keys], &old_range);
            let new_candidates = candidates(&new_keys[keys], &new_range);
            similar::capture_diff_slices_deadline(
                algorithm,
                &old_candidates
                    .iter()
                    .map(|&index| &old_keys[keys][index])
                    .collect::<Vec<_>>(),
                &new_candidates
                    .iter()
                    .map(|&index| &new_keys[keys][index])
                    .collect::<Vec<_>>(),
                Some(deadline),
            )
            .into_iter()
            .filter(|op| matches!(op, similar::DiffOp::Equal { .. }))
            .flat_map(|op| op.old_range().zip(op.new_range()))
            .map(|(old_at, new_at)| (old_candidates[old_at], new_candidates[new_at]))
            .collect()
        }
    };
    let (mut old_at, mut new_at) = (old_range.start, new_range.start);
    for (old_line, new_line) in pairs {
        align_lines(
            old_keys,
            new_keys,
            level + 1,
            old_at..old_line,
            new_at..new_line,
            deadline,
            aligned,
        );
        aligned.push((old_line..old_line + 1, new_line..new_line + 1));
        (old_at, new_at) = (old_line + 1, new_line + 1);
    }
    align_lines(
        old_keys,
        new_keys,
        level + 1,
        old_at..old_range.end,
        new_at..new_range.end,
        deadline,
        aligned,
    );
}

/// `text`'s lines, each with its line break (LF, CRLF or a lone CR), and
/// the byte offset each starts at.
fn lines_with_breaks(text: &str) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        let end = match bytes[at] {
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => at + 2,
            b'\r' | b'\n' => at + 1,
            _ => {
                at += 1;
                continue;
            }
        };
        lines.push((start, &text[start..end]));
        start = end;
        at = end;
    }
    if start < text.len() {
        lines.push((start, &text[start..]));
    }
    lines
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
