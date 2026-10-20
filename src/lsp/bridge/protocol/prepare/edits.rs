//! Edits a downstream server made to a prepared document (P), mapped onto
//! its virtual document (V).
//!
//! An edit keeps to the text P shows: it maps through the same runs
//! positions do, and the lines it creates regain the indentation the peer
//! removed from the segment they land in. The mapped set is then checked as
//! a whole against the host text V holds and P does not show, so edits that
//! are fine one by one cannot combine into a corruption of it.

use std::collections::HashMap;
use std::ops::Range;

use tower_lsp_server::ls_types::{Position, Range as LspRange, TextEdit};

use super::{
    Bias, LineMap, PreparedMap, Run, RunKind, Side, empty_run, is_line_start, map_offset,
    map_offset_stopping, rest_of_line_blank, splits_crlf,
};

mod diff;

use diff::{Work, char_hunks, patience_pairs};

/// How a set of edits to P is read when mapped to V
/// ([`PreparedMap::map_edits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditMode {
    /// Edits keeping their extents (a completion's replace range, an inlay
    /// hint's text edits), mapped one by one.
    Extent,
}

/// Why a formatting answer was refused (see
/// [`PreparedMap::map_formatting_answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormattingRefusal {
    /// Its shape: the host cannot take what it changes, as mapped — or no
    /// reading of a whole answer places it for sure: text where a
    /// whitespace line the peer kept was, or any change to a region the
    /// peer dedented unevenly.
    Shape,
    /// Its size: the diff ran out of work (see `FORMATTING_DIFF_BUDGET`)
    /// before telling what it changes.
    OutOfWork,
}

impl std::fmt::Display for FormattingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Shape => "its result cannot be mapped back through the prepared virtual document",
            Self::OutOfWork => {
                "its diff against the prepared virtual document ran out of work (the region is too heavily rewritten to compare)"
            }
        })
    }
}

/// How much work the diff of a formatting answer may do, pairing lines and
/// then the fewest changes within them (see
/// [`PreparedMap::map_formatting_answer`]),
/// counted in steps of its searches (see [`Work`]); the work around the
/// search (applying the answer to P, the per-character tables, mapping
/// each stretch) runs in time linear in the texts on top. An answer whose
/// diff runs out of work is refused, on any machine and under any load
/// alike: the count, not a clock, decides. Everything the searches keep
/// is spent before it is taken, a step for each cell of a table and for
/// each element they index, so their memory is linear in the budget
/// (`FORMATTING_DIFF_BUDGET_CAP` bounds it whatever the texts) besides what
/// is linear in the texts. Hashing
/// or comparing a line's text whole costs a step for every
/// `BYTES_PER_STEP` bytes of it, so long lines spend the budget as they
/// cost time; with lines of usual length, spending this fixed part whole
/// takes some 10 ms in a release build, and the part that grows with the
/// texts (`FORMATTING_DIFF_WORK_PER_BYTE`) time linear in what the answer
/// changes (the lines alike at either end are set aside first): a 20,000
/// line region of half a megabyte rewritten whole maps in some 30 to 100
/// ms, and a one-line answer in any region in well under a millisecond
/// past the scan for what is alike. It runs where the answer arrives, on
/// the async runtime, rather than on a blocking thread.
const FORMATTING_DIFF_BUDGET: usize = 1 << 20;

/// The most a formatting answer's diff may spend, the per-byte allowance
/// included: the tables quadratic in what its searches compare (the line
/// diff's trace, the blank-line table) take a cell per step (4 bytes),
/// some 64 MiB at most however large the texts; what else the diff keeps
/// (the line diff's frontier, the lines' keys and indexes) is linear in
/// the texts.
const FORMATTING_DIFF_BUDGET_CAP: usize = 1 << 24;

/// How many bytes a step of work covers where the searches hash or compare
/// a line's text whole: such a step costs its length, not one.
const BYTES_PER_STEP: usize = 64;

/// Work the diff of a formatting answer may do on top of
/// `FORMATTING_DIFF_BUDGET` for each byte of P and of the edited P: a diff
/// pairing every line and diffing each on its own does work linear in the
/// texts, a few dozen steps for each line, which no fixed budget covers
/// for a large region. What is quadratic in a region outgrows this as the
/// region grows, and is refused still.
const FORMATTING_DIFF_WORK_PER_BYTE: usize = 16;

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
    /// V, as `mode` reads them (see [`EditMode`]), one for each in order (a
    /// formatting answer is read by [`Self::map_formatting_answer`]).
    ///
    /// Every edit is mapped on its own, keeping its extent and
    /// restoring the indentation the peer removed from lines it creates or
    /// replaces. The mapped set is then checked as a whole against the host
    /// text V holds and P does not show (see
    /// [`Self::mapped_edits_keep_host_text`]). `None` — refuse the set — when
    /// an edit is reversed, reaches a line past P's last, edits overlap in P
    /// or once mapped (or touch where either's text bears on the other), an
    /// edit reaches into a gap, a line of text created in content has no
    /// single indentation to regain, or the host text would not survive.
    ///
    /// The host's own text, gaps and edge lines, is always kept; but on a
    /// map with gaps the indentation lines regain is not yet exact in some
    /// shapes (a placeholder's own line break, a gap ending the document,
    /// segments that say nothing of their indent). Edit carriers open on
    /// contiguous regions only, which have none; one opening on a
    /// noncontiguous region needs those fixed first.
    pub(crate) fn map_edits(&self, edits: &[TextEdit], mode: EditMode) -> Option<Vec<TextEdit>> {
        match mode {
            EditMode::Extent => self.map_extents(edits),
        }
    }

    /// [`Self::map_edits`] for a formatter's answer, saying why when it is
    /// refused: what it changes counts, not how it says so. The edits are
    /// applied to P and the result diffed against P; each changed stretch
    /// maps as one edit would, in P's order (none when it changes nothing),
    /// so a whole-document replacement maps as precisely as small edits.
    /// The set mapped is the diff's, not the edits sent. A range may also
    /// end at the line just past P's last (`(lineCount, 0)`), naming P's
    /// end, as whole-document answers do. The formatting transforms clamp
    /// such an end to P's last line before mapping, to the same place; it
    /// is read here too so that an answer is taken as servers send it,
    /// whoever calls this.
    pub(crate) fn map_formatting_answer(
        &self,
        edits: &[TextEdit],
    ) -> Result<Vec<TextEdit>, FormattingRefusal> {
        self.map_formatting_within(edits, FORMATTING_DIFF_BUDGET, FORMATTING_DIFF_WORK_PER_BYTE)
    }

    /// Whether a formatting answer of `edits` drops P's final line break
    /// where the host's deleted closing (lines after the content, a closing
    /// indent) follows it: mapped, it would edit that closing, taking its
    /// indent off or writing a line on it. Answers whose edge layout is not
    /// restored before they are mapped (on-type ones) are refused so.
    pub(crate) fn drops_the_break_before_the_closing(&self, edits: &[TextEdit]) -> bool {
        let prepared = &self.prepared_lines.text;
        if !prepared.ends_with(['\n', '\r'])
            || empty_run(&self.runs, prepared.len(), |kind| kind == RunKind::Deleted).is_none()
        {
            return false;
        }
        let Some((ranges, order)) = sorted_extents(&self.prepared_lines, edits, true) else {
            return false;
        };
        // What ends the edited P: the last edit's text if it reaches P's end,
        // else P's own end.
        match order.last() {
            Some(&last) if ranges[last].end == prepared.len() => {
                let text = &edits[last].new_text;
                if text.is_empty() {
                    !prepared[..ranges[last].start].ends_with(['\n', '\r'])
                } else {
                    !text.ends_with(['\n', '\r'])
                }
            }
            _ => false,
        }
    }

    /// [`Self::map_formatting_answer`], the diff doing at most `budget`
    /// steps of work.
    #[cfg(test)]
    fn map_formatting(
        &self,
        edits: &[TextEdit],
        budget: usize,
    ) -> Result<Vec<TextEdit>, FormattingRefusal> {
        self.map_formatting_within(edits, budget, 0)
    }

    /// Whether a line of the content (not whitespace alone, nor starting in
    /// a gap's text) lost other indentation than its segment's uniform one:
    /// the peer left it alone, or removed another indent. A formatting
    /// answer rewrites lines wholesale, and which of its lines is which old
    /// one, and so which indent each regains, only a pairing reads; with
    /// every line regaining the one uniform indent, no reading decides any
    /// indent. Edits keeping their extents state where they go, and are not
    /// so refused.
    fn dedents_unevenly(&self) -> bool {
        lines_with_breaks(&self.prepared_lines.text)
            .into_iter()
            .any(|(start, line)| {
                if line.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                    return false;
                }
                // Defensive: formatting is open only on prepared documents
                // without gaps, so no line starts in a gap's text there; a
                // map with gaps keeps such a line out of the count.
                let run = self.runs.partition_point(|run| run.prepared.end <= start);
                if self
                    .runs
                    .get(run)
                    .is_some_and(|run| run.kind == RunKind::Gap && run.prepared.start <= start)
                {
                    return false;
                }
                let own = empty_run(&self.runs, start, |kind| kind == RunKind::Deleted)
                    .map_or("", |run| &self.virtual_lines.text[run.virtual_.clone()]);
                let at = map_offset(&self.runs, start, Bias::Start, Side::Prepared);
                self.uniform_indent_at(at) != Some(own)
            })
    }

    /// [`Self::map_formatting_answer`], the diff doing at most `budget`
    /// steps of work and `per_byte` more for each byte of P and the edited
    /// P.
    fn map_formatting_within(
        &self,
        edits: &[TextEdit],
        budget: usize,
        per_byte: usize,
    ) -> Result<Vec<TextEdit>, FormattingRefusal> {
        let prepared = &self.prepared_lines.text;
        let (ranges, order) =
            sorted_extents(&self.prepared_lines, edits, true).ok_or(FormattingRefusal::Shape)?;
        let mut edited = String::with_capacity(prepared.len());
        let mut cursor = 0;
        for &index in &order {
            edited.push_str(&prepared[cursor..ranges[index].start]);
            edited.push_str(&edits[index].new_text);
            cursor = ranges[index].end;
        }
        edited.push_str(&prepared[cursor..]);
        if edited == *prepared {
            return Ok(Vec::new());
        }
        if self.dedents_unevenly() {
            return Err(FormattingRefusal::Shape);
        }

        let budget = budget
            .saturating_add(per_byte.saturating_mul(prepared.len() + edited.len()))
            .min(if per_byte > 0 {
                FORMATTING_DIFF_BUDGET_CAP
            } else {
                usize::MAX
            });
        let mut work = Work::new(budget);
        // Told apart from the refusals below, which the answer's shape
        // decides: this one only its size does.
        let hunks = diff_hunks(prepared, &edited, &mut work)?;
        // Each stretch maps on its own as an edit does: stretches touch only
        // at a line start where the later replaces some of that line's start
        // with text, or where the earlier ends with a line break of P and the
        // later replaces no line break (see `diff_hunks`), each then on its
        // own line.
        let mut mapped = hunks
            .into_iter()
            .map(|hunk| {
                let following = prepared[hunk.old.end..].chars().next();
                self.hunk_to_virtual(hunk.old, &edited[hunk.new], following, true)
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(FormattingRefusal::Shape)?;
        // A stretch ending at a line start takes the removed indent there
        // along and gives it back at its end; one starting there claims the
        // same indent (a blank line inserted, a whitespace-only line
        // cleared). The first need not: ending before the indent, and not
        // giving it back, it writes the same text, and the two touch rather
        // than overlap. Anything else overlapping is left for the set's
        // check to refuse.
        let virtual_text = &self.virtual_lines.text;
        // An insertion at that line start lands after the indent, and gives
        // it back at its end, while the next stretch claims the indent from
        // the line start: the insertion goes before the indent instead,
        // writing it for its own first line rather than giving it back (the
        // same text), and the two touch.
        let indent = |text: &str| text.chars().all(|c| c == ' ' || c == '\t');
        // A stretch taking along the indent at `next_start`, which the next
        // stretch claims, ends before it instead.
        let leave_indent = |(range, new_text): &mut (Range<usize>, String), next_start: usize| {
            if range.start <= next_start && next_start < range.end {
                let taken = &virtual_text[next_start..range.end];
                // Only a removed indent is ever taken along and given back:
                // anything else overlapping is the set check's to refuse.
                if new_text.ends_with(taken) && indent(taken) {
                    new_text.truncate(new_text.len() - taken.len());
                    range.end = next_start;
                }
            }
        };
        for at in 1..mapped.len() {
            let (next_start, next_end) = (mapped[at].0.start, mapped[at].0.end);
            let (earlier, _) = mapped.split_at_mut(at);
            let (range, new_text) = &mut earlier[at - 1];
            if range.start <= next_start && next_start < range.end {
                leave_indent(&mut earlier[at - 1], next_start);
            } else if range.start == range.end
                && next_start < range.start
                && range.start <= next_end
            {
                let taken = &virtual_text[next_start..range.start];
                if new_text.ends_with(taken) && indent(taken) {
                    new_text.truncate(new_text.len() - taken.len());
                    new_text.insert_str(0, taken);
                    *range = next_start..next_start;
                    // Moved before the indent, the insertion now meets the
                    // stretch before it there, which may have taken the same
                    // indent along (a line break switched above a cleared
                    // whitespace line): that one leaves it too.
                    if at >= 2 {
                        leave_indent(&mut earlier[at - 2], next_start);
                    }
                }
            }
        }
        self.to_text_edits(mapped).ok_or(FormattingRefusal::Shape)
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
                false,
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
            let ends: Vec<usize> = mapped
                .iter()
                .flat_map(|(range, _)| [range.start, range.end])
                .collect();
            let positions = self.virtual_lines.positions(&ends);
            mapped
                .into_iter()
                .zip(positions.as_chunks::<2>().0)
                .map(|((_, new_text), &[start, end])| TextEdit {
                    range: LspRange::new(start, end),
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
        // their order, see `map_edits`), but stretches of a formatting
        // answer that touch in P can overlap once mapped, each regaining an
        // indent the other claims (see `map_formatting_within`): this check
        // refuses those, and every later read of the sorted set relies on
        // it.
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
    /// `following` is the character after the replacement in P;
    /// `whole_answer` tells a stretch of a formatting answer from an edit
    /// keeping its extent.
    fn hunk_to_virtual(
        &self,
        old: Range<usize>,
        new_text: &str,
        following: Option<char>,
        whole_answer: bool,
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
        // Text a formatting answer writes over a whole blank line, other
        // than after the whitespace the peer kept on it, is a line of the
        // content in its place: that line's whitespace was never an indent
        // (a line the peer emptied, an empty line), or the text is not
        // written after it. (Edits that keep their extents state where they
        // go, and are placed as before.)
        let blank_line_text_in_place = whole_answer && whole_lines && {
            let line_end = prepared[old.start..]
                .find(['\n', '\r'])
                .map_or(prepared.len(), |end| old.start + end);
            let kept = &prepared[old.start..line_end];
            let first = new_text.split(['\n', '\r']).next().unwrap_or_default();
            kept.trim_matches([' ', '\t']).is_empty()
                && !first.trim_matches([' ', '\t']).is_empty()
                && (kept.is_empty() || !first.starts_with(kept))
        };
        let first = first_indent(&EdgeShape {
            whole_lines,
            lines_before_own_indent,
            blank_line_text_in_place,
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
    /// Text written over a whole blank line, other than after the whitespace
    /// the peer kept on it: a line of the content in its place.
    blank_line_text_in_place: bool,
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
        _ if shape.blank_line_text_in_place => FirstIndent::Uniform,
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
    // Every end on a line of the text, converted in one pass.
    let on_a_line = |position: Position| lines.index.0.line(position.line).is_some();
    let ends: Vec<Position> = edits
        .iter()
        .flat_map(|edit| [edit.range.start, edit.range.end])
        .collect();
    let offsets = lines.offsets_clamped(&ends);
    let ranges = edits
        .iter()
        .zip(offsets.as_chunks::<2>().0)
        .map(|(edit, &[start, end_offset])| {
            // A reversed range is refused as sent: clamped past the end, both
            // its ends would read as one insertion there.
            if edit.range.start > edit.range.end {
                return None;
            }
            // A line past the text's last names no place in it: LSP reads a
            // column past its line as the line's end, but not a line past the
            // document as its end (an import "at line 7" of a 3-line P would
            // land after its content).
            let end = if on_a_line(edit.range.end) {
                end_offset
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

/// The stretches `new` differs from `old` in; `None` when `work` runs out
/// (see `FORMATTING_DIFF_BUDGET`). Lines are
/// paired first (see [`align_lines`]): a line of text with the line it was
/// rewritten from, blank lines among themselves, lines joined onto another
/// with that one. Each pair that differs, and each run of lines left
/// between pairs, is then diffed by character. So a stretch never covers
/// a line paired as alike, and a character diff aligns text of one
/// line with text of another only within a run, among lines no pairing
/// could tell apart; outside a run, either would map that text as written
/// on the wrong line, with that line's indent. A stretch never starts or ends between a CR
/// and its LF in either text (no LSP position names that place): it widens
/// over the unchanged character beside it. Stretches left touching merge,
/// but at the start of a line one writes text over or the other's line
/// break ends before.
fn diff_hunks(old: &str, new: &str, work: &mut Work) -> Result<Vec<Hunk>, FormattingRefusal> {
    // Whole lines alike at either end are set aside, uncharged: a small
    // answer in a large region costs work for its own size.
    let (head, tail) = alike_line_ends(old, new);
    let (old_end, new_end) = (old.len() - tail, new.len() - tail);
    fn lines_from(text: &str, range: Range<usize>) -> Vec<(usize, &str)> {
        lines_with_breaks(&text[range.clone()])
            .into_iter()
            .map(|(start, line)| (start + range.start, line))
            .collect()
    }
    let old_lines = lines_from(old, head..old_end);
    let new_lines = lines_from(new, head..new_end);
    let line_start = |lines: &[(usize, &str)], end: usize, index: usize| {
        lines.get(index).map_or(end, |&(start, _)| start)
    };
    let line_keys = |lines: &[(usize, &str)]| -> LineKeys {
        let content = |line: &str| line.trim_end_matches(['\r', '\n']).to_string();
        [
            lines.iter().map(|&(_, line)| content(line)).collect(),
            lines
                .iter()
                .map(|&(_, line)| content(line).trim_matches([' ', '\t']).to_string())
                .collect(),
            lines
                .iter()
                .map(|&(_, line)| content(line).replace([' ', '\t'], ""))
                .collect(),
            lines
                .iter()
                .map(|&(_, line)| {
                    content(line)
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .collect()
                })
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
        work,
        &mut aligned,
    )
    .ok_or(FormattingRefusal::OutOfWork)?;
    let mut runs = Vec::new();
    for (old_range, new_range) in aligned {
        runs.extend(runs_to_diff(
            &old_lines, &new_lines, &old_keys, old_range, new_range, work,
        )?);
    }
    let block = |lines: &[(usize, &str)], end: usize, range: Range<usize>| {
        line_start(lines, end, range.start)..line_start(lines, end, range.end)
    };
    let blocks = runs
        .into_iter()
        .filter_map(|(old_range, new_range, blanks_out)| {
            // A pair alike to the byte, or nothing on either side, is no change.
            let same = old_range.len() == 1
                && new_range.len() == 1
                && old_lines[old_range.start].1 == new_lines[new_range.start].1;
            // The old blank lines a run removes whole (see `push_run`).
            let removed: Vec<Range<usize>> = if blanks_out {
                old_range
                    .clone()
                    .filter(|&line| old_keys[1][line].is_empty())
                    .map(|line| {
                        let (start, text) = old_lines[line];
                        start..start + text.len()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (!same && !(old_range.is_empty() && new_range.is_empty())).then(|| {
                (
                    block(&old_lines, old_end, old_range),
                    block(&new_lines, new_end, new_range),
                    removed,
                )
            })
        });
    let mut hunks: Vec<Hunk> = Vec::new();
    for (old_block, new_block, removed) in blocks {
        let old_chars: Vec<(usize, char)> = old[old_block.clone()]
            .char_indices()
            .map(|(at, c)| (old_block.start + at, c))
            .filter(|(at, _)| {
                let next = removed.partition_point(|line| line.end <= *at);
                !removed.get(next).is_some_and(|line| line.contains(at))
            })
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
        // The character diff, with each removed blank line no stretch spans
        // deleted whole where it is: just before the next stretch, or in
        // the stretch the texts hold alike around it.
        let mut stretches: Vec<(Range<usize>, Range<usize>)> = Vec::new();
        let mut removed = removed
            .into_iter()
            .map(|line| (old_chars.partition_point(|&(at, _)| at < line.start), line))
            .peekable();
        let mut shift = 0isize;
        let new_byte = |index: isize| byte_at(&new_chars, new_block.end, index as usize);
        for (old_range, new_range) in
            char_hunks(&old_values, &new_values, work).ok_or(FormattingRefusal::OutOfWork)?
        {
            while let Some((kept, line)) = removed.next_if(|&(kept, _)| kept <= old_range.start) {
                let at = new_byte(kept as isize + shift);
                stretches.push((line, at..at));
            }
            while removed
                .next_if(|&(kept, _)| kept <= old_range.end)
                .is_some()
            {}
            shift = new_range.end as isize - old_range.end as isize;
            stretches.push((
                byte_at(&old_chars, old_block.end, old_range.start)
                    ..byte_at(&old_chars, old_block.end, old_range.end),
                byte_at(&new_chars, new_block.end, new_range.start)
                    ..byte_at(&new_chars, new_block.end, new_range.end),
            ));
        }
        for (kept, line) in removed {
            let at = new_byte(kept as isize + shift);
            stretches.push((line, at..at));
        }
        for (mut old_bytes, mut new_bytes) in stretches {
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
            // replaces some of that line's start with text, or, after a line
            // break of P the earlier ends with, changes nothing past that
            // line's break (no line break of P in what it replaces): merged, the later line would map as one the
            // stretch writes anew, with the content's indent rather than its
            // own. Apart, neither's text bears on the other there. Whole
            // lines the later removes go with the earlier, the removed
            // indents they take along with them.
            // A stretch removing the rest of P, which ends with no line
            // break, is no line of its own to keep apart: the line before
            // it takes its place, whatever the earlier wrote there.
            let removes_the_end = new_bytes.is_empty()
                && old_bytes.end == old.len()
                && !old[old_bytes.clone()].contains(['\n', '\r']);
            let apart = |last: &Hunk| {
                is_line_start(old, old_bytes.start)
                    && !removes_the_end
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
    Ok(hunks)
}

/// Each line's text without its break (0), that trimmed of whitespace at
/// its edges (1), with no whitespace at all (2), and its letters and digits
/// alone (3).
type LineKeys = [Vec<String>; 4];

/// One way [`align_lines`] pairs lines.
#[derive(Clone, Copy)]
enum Alignment {
    /// Lines whose keys (see [`LineKeys`]) a patience diff finds alike,
    /// among the lines whose key passes; stretches with no key found once
    /// on each side are diffed further only when `true` (see
    /// [`patience_pairs`]).
    Diff(usize, fn(&str) -> bool, bool),
    /// A line written as an old line's text with more after it: that line,
    /// lines joined onto it (see [`joined_lines`]).
    Joined,
    /// Lines of text at the run's two ends, in order, alike in their
    /// letters and digits (see [`alike_ends`]).
    AlikeEnds,
    /// Blank lines with blank lines: as many as can pair, and among those
    /// alignments, the one with the most pairs alike to the whitespace (see
    /// [`blank_lines`]).
    Blanks,
}

/// How [`align_lines`] pairs lines, from the surest to the loosest:
///
/// - Lines of text alike to the byte, a line found once in each text
///   anchoring the rest (patience): of lines alike but for whitespace, the
///   one written as it was is that line. Not where as many on each side
///   are alike but for whitespace: reindented, one copy may read as another
///   did, and they pair in order below.
/// - Lines of text alike but for the whitespace around it, a line found
///   once in each text anchoring the rest (patience): a reindented line is
///   the line it was, and a blank line cannot pair with it instead.
/// - Lines of text alike but for whitespace anywhere in them, the same
///   way: a line a formatter respaced (`x=1` to `x = 1`) is the line it
///   was, diffed by character on its own.
/// - Then, between those, both again, lines found as many times more than
///   once on each side paired too (by the fewest changes): only once every
///   line telling where it belongs is placed may a line alike to several
///   (`}`) take one of them, and only where each has one to take.
/// - Lines of text other lines were joined onto.
/// - Blank lines among themselves: as many as can pair, so that text the
///   formatter writes beside blank lines is never read as written on one
///   while a blank line of the answer could be that line; of the
///   alignments pairing as many, the one with the most pairs alike to the
///   whitespace (a kept whitespace line kept as it was).
/// - Lines of text in order from either end of what is left, while they
///   are alike in their letters and digits: a line whose quotes,
///   separators or punctuation a formatter rewrote (`f('a')` to
///   `f("a");`) is the line it was, however many lines it rewrote.
///
/// What is left between pairs (lines rewritten past telling, lines written
/// or removed) is diffed by character as one run. Which of several blank
/// lines alike a line was written on, or which went, no answer tells; text
/// in the place of a whitespace line the peer kept is refused (see
/// [`runs_to_diff`]).
const ALIGNMENTS: [Alignment; 8] = [
    Alignment::Diff(0, |key| !key.trim_matches([' ', '\t']).is_empty(), false),
    Alignment::Diff(1, |key| !key.is_empty(), false),
    Alignment::Diff(2, |key| !key.is_empty(), false),
    Alignment::Diff(1, |key| !key.is_empty(), true),
    Alignment::Diff(2, |key| !key.is_empty(), true),
    Alignment::Joined,
    Alignment::Blanks,
    Alignment::AlikeEnds,
];

/// Pairs of the blank lines of `old_range` with those of `new_range`, in
/// order: as many as an alignment of them can pair, and among those, as
/// many pairs alike to the whitespace as can be. Lines of text take no
/// part. Its table is quadratic in the blank lines, every cell spent from
/// `work` before it is taken: `None`, to refuse the answer, when there is
/// not as much.
fn blank_lines<'a>(
    old_keys: &'a LineKeys,
    new_keys: &'a LineKeys,
    old_range: &Range<usize>,
    new_range: &Range<usize>,
    work: &mut Work,
) -> Option<Vec<(usize, usize)>> {
    let blanks = |keys: &LineKeys, range: &Range<usize>| -> Vec<usize> {
        range
            .clone()
            .filter(|&index| keys[1][index].is_empty())
            .collect()
    };
    let (old, new) = (blanks(old_keys, old_range), blanks(new_keys, new_range));
    // Each blank line's whitespace numbered once, its bytes charged: the
    // table's cells then compare numbers, a step each whatever a line's
    // length. Empty lines are numbered `None`: all alike, they say nothing
    // of which is which.
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let mut numbered = |keys: &'a LineKeys, lines: &[usize]| -> Option<Vec<Option<u32>>> {
        let mut numbers = Vec::with_capacity(lines.len());
        for &line in lines {
            let key = keys[0][line].as_str();
            work.spend(1 + key.len() / BYTES_PER_STEP)?;
            let next = ids.len() as u32;
            numbers.push((!key.is_empty()).then(|| *ids.entry(key).or_insert(next)));
        }
        Some(numbers)
    };
    let old_ids = numbered(old_keys, &old)?;
    let new_ids = numbered(new_keys, &new)?;
    work.spend((old.len() + 1).saturating_mul(new.len() + 1))?;
    // Alike to the whitespace, holding some.
    let alike = |i: usize, j: usize| old_ids[i].is_some() && old_ids[i] == new_ids[j];
    // score[i][j]: the best of old[i..] against new[j..], by the count of
    // pairs, then of pairs alike: a pair counts `pair`, more than every
    // pair alike together, and a pair alike 1 more. A score is at most
    // `pair` squared less one; a side too long for that to fit (far past
    // the default budget) refuses rather than overflow.
    let pair = u32::try_from(old.len().min(new.len()) + 1)
        .ok()
        .filter(|pair| pair.checked_mul(*pair).is_some())?;
    let mut score = vec![vec![0u32; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            let alike = alike(i, j);
            let paired = score[i + 1][j + 1] + pair + u32::from(alike);
            score[i][j] = paired.max(score[i + 1][j]).max(score[i][j + 1]);
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        let alike = alike(i, j);
        if score[i][j] == score[i + 1][j + 1] + pair + u32::from(alike) {
            pairs.push((old[i], new[j]));
            (i, j) = (i + 1, j + 1);
        } else if score[i][j] == score[i + 1][j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    Some(pairs)
}

/// Pairs of an old line and the new line written as its text with more
/// after it, in order: the first such old line for each new line, past the
/// one paired before.
///
/// The old lines' texts are indexed: each new line looks up its own
/// prefixes of the lengths old texts have, each lookup spent from `work`,
/// rather than comparing itself with every old line. `None`, to refuse the
/// answer, when the work runs out.
fn joined_lines(
    old_keys: &LineKeys,
    new_keys: &LineKeys,
    old_range: &Range<usize>,
    new_range: &Range<usize>,
    work: &mut Work,
) -> Option<Vec<(usize, usize)>> {
    work.spend(old_range.len())?;
    // Each text an old line holds, and where, in order.
    let mut index: HashMap<&str, Vec<usize>> = HashMap::new();
    for old_line in old_range.clone() {
        let text = old_keys[1][old_line].as_str();
        if !text.is_empty() {
            index.entry(text).or_default().push(old_line);
        }
    }
    let mut lengths: Vec<usize> = index.keys().map(|text| text.len()).collect();
    lengths.sort_unstable();
    lengths.dedup();
    let mut pairs = Vec::new();
    let mut old_from = old_range.start;
    for new_line in new_range.clone() {
        let written = new_keys[1][new_line].as_str();
        let mut first: Option<usize> = None;
        for &length in lengths
            .iter()
            .take_while(|&&length| length <= written.len())
        {
            if !written.is_char_boundary(length) {
                continue;
            }
            // A lookup hashes the prefix: charged by its length too.
            work.spend(1 + length / BYTES_PER_STEP)?;
            if let Some(lines) = index.get(&written[..length]) {
                let at = lines.partition_point(|&line| line < old_from);
                if let Some(&line) = lines.get(at) {
                    first = Some(first.map_or(line, |first| first.min(line)));
                }
            }
        }
        if let Some(old_line) = first {
            pairs.push((old_line, new_line));
            old_from = old_line + 1;
        }
    }
    Some(pairs)
}

/// Pairs of the lines of text (blank lines skipped) of `old_range` and
/// `new_range` taken in order from the start while their letters and
/// digits are alike, then from the end the same way. Only lines at the
/// same place from an end pair: a line no later than where the two texts
/// part ways is the line it was there. A whitespace line the peer kept
/// left between them, the answer's text in its place, is read line for
/// line, and that text refused (see [`runs_to_diff`]).
fn alike_ends(
    old_keys: &LineKeys,
    new_keys: &LineKeys,
    old_range: &Range<usize>,
    new_range: &Range<usize>,
    work: &mut Work,
) -> Option<Vec<(usize, usize)>> {
    work.spend(old_range.len() + new_range.len())?;
    let text = |keys: &LineKeys, range: &Range<usize>| -> Vec<usize> {
        range
            .clone()
            .filter(|&index| !keys[1][index].is_empty())
            .collect()
    };
    let (old, new) = (text(old_keys, old_range), text(new_keys, new_range));
    // Each line compared at most twice, a comparison costing its length.
    let bytes: usize = old
        .iter()
        .map(|&line| old_keys[3][line].len())
        .sum::<usize>()
        + new
            .iter()
            .map(|&line| new_keys[3][line].len())
            .sum::<usize>();
    work.spend(2 * bytes / BYTES_PER_STEP)?;
    let alike = |old_line: usize, new_line: usize| old_keys[3][old_line] == new_keys[3][new_line];
    let mut head = 0;
    while head < old.len().min(new.len()) && alike(old[head], new[head]) {
        head += 1;
    }
    let mut tail = 0;
    while tail < old.len().min(new.len()) - head
        && alike(old[old.len() - 1 - tail], new[new.len() - 1 - tail])
    {
        tail += 1;
    }
    let mut pairs: Vec<(usize, usize)> = old[..head]
        .iter()
        .copied()
        .zip(new[..head].iter().copied())
        .collect();
    pairs.extend(
        old[old.len() - tail..]
            .iter()
            .copied()
            .zip(new[new.len() - tail..].iter().copied()),
    );
    Some(pairs)
}

/// Old lines, new lines, and whether the old side's blank lines are left
/// out of the character diff (see [`runs_to_diff`]).
type DiffRun = (Range<usize>, Range<usize>, bool);

/// How to diff what [`align_lines`] gave, `old_range` against
/// `new_range`: as it is (`false`), or with the old side's blank lines left
/// out of the character diff and removed whole (`true`); a pair is left as
/// it is. Where the old side holds lines of text, its blank lines (a run
/// holds blank lines on one side at most, the blank-line stage having
/// paired as many as can) go whole: the new text is the old text
/// rewritten, and diffed with the blank lines, it could be read as written
/// on one the answer removed, regaining that line's indent, not the
/// content's. Where the old side holds blank lines only, as many as the new
/// lines (the new not ending with a line break the old lacks), the new
/// lines take their places line for line: a blank line is that line; text
/// is a line of the content in place of an empty line, and refused in
/// place of a kept whitespace line (written on it, or in its place, a
/// whole-text answer does not tell). With more or fewer, the blank lines
/// went and the other lines are the content's, before them; text starting
/// with exactly a kept line's whitespace, which may have been typed on it,
/// is refused.
fn runs_to_diff(
    old_lines: &[(usize, &str)],
    new_lines: &[(usize, &str)],
    old_keys: &LineKeys,
    old_range: Range<usize>,
    new_range: Range<usize>,
    work: &mut Work,
) -> Result<Vec<DiffRun>, FormattingRefusal> {
    let blank = |line: usize| old_keys[1][line].is_empty();
    let breaks = |lines: &[(usize, &str)], range: &Range<usize>| {
        range.end == 0 || lines[range.end - 1].1.ends_with(['\n', '\r'])
    };
    Ok(if new_range.is_empty() || !old_range.clone().any(blank) {
        vec![(old_range, new_range, false)]
    } else if !old_range.clone().all(blank) {
        vec![(old_range, new_range, true)]
    } else if old_range.len() == new_range.len()
        && (breaks(old_lines, &old_range) || !breaks(new_lines, &new_range))
    {
        // Line for line: a blank line for a blank line; text in place of
        // an empty line is a line of the content. Text where a kept
        // whitespace line was is refused, even typed right after its
        // whitespace: written on it (keeping no indent) or in its place
        // (taking the content's), a whole-text answer does not tell.
        let mut runs = Vec::new();
        for (old_line, new_line) in old_range.zip(new_range) {
            let kept = &old_keys[0][old_line];
            let written = new_lines[new_line].1;
            if written.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                runs.push((old_line..old_line + 1, new_line..new_line + 1, false));
            } else if kept.is_empty() {
                runs.push((old_line..old_line, new_line..new_line + 1, false));
                runs.push((old_line..old_line + 1, new_line + 1..new_line + 1, false));
            } else {
                return Err(FormattingRefusal::Shape);
            }
        }
        runs
    } else {
        // More or fewer lines than the blank lines: the blank lines went,
        // and the other lines are the content's, before them. Text that
        // may have been typed right after a kept line's whitespace (it
        // starts with exactly that whitespace) is refused, as line for
        // line: the new lines' leading whitespace is gathered once, and
        // each kept line looks its own up.
        // Each hash costs the whitespace it reads: a step per 64 bytes.
        let mut leading = std::collections::HashSet::new();
        for new_line in new_range.clone() {
            let line = new_lines[new_line].1;
            let whitespace = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
            work.spend(1 + whitespace.len() / BYTES_PER_STEP)
                .ok_or(FormattingRefusal::OutOfWork)?;
            if !whitespace.is_empty() {
                leading.insert(whitespace);
            }
        }
        for old_line in old_range.clone() {
            let kept = old_keys[0][old_line].as_str();
            work.spend(1 + kept.len() / BYTES_PER_STEP)
                .ok_or(FormattingRefusal::OutOfWork)?;
            if leading.contains(kept) {
                return Err(FormattingRefusal::Shape);
            }
        }
        let (start, end) = (old_range.start, new_range.end);
        vec![
            (start..start, new_range, false),
            (old_range, end..end, false),
        ]
    })
}

/// Pair the lines of `old_range` with those of `new_range` at
/// `ALIGNMENTS[level]`, and each looser one between the pairs it finds:
/// `aligned` is given in order every pair (one line each) and every run
/// left between pairs (to diff by character); `None` when `work` runs
/// out. `old_keys` and `new_keys` are each line's keys (see
/// [`LineKeys`]). A line joined onto
/// another is left to a run, deleted whole: the text it gave is written on
/// the line it joined.
fn align_lines(
    old_keys: &LineKeys,
    new_keys: &LineKeys,
    level: usize,
    old_range: Range<usize>,
    new_range: Range<usize>,
    work: &mut Work,
    aligned: &mut Vec<(Range<usize>, Range<usize>)>,
) -> Option<()> {
    if old_range.is_empty() || new_range.is_empty() {
        aligned.push((old_range, new_range));
        return Some(());
    }
    let Some(&alignment) = ALIGNMENTS.get(level) else {
        aligned.push((old_range, new_range));
        return Some(());
    };
    let pairs: Vec<(usize, usize)> = match alignment {
        Alignment::Joined => joined_lines(old_keys, new_keys, &old_range, &new_range, work)?,
        Alignment::AlikeEnds => alike_ends(old_keys, new_keys, &old_range, &new_range, work)?,
        Alignment::Blanks => blank_lines(old_keys, new_keys, &old_range, &new_range, work)?,
        Alignment::Diff(keys, takes_part, fall_back) => {
            // To the byte, a line alike but for whitespace to several, as
            // many on each side, is no surer than by its order: reindented,
            // one copy may be written as another was, and pairing to the
            // byte would cross them. Those are left to pair in order (see
            // `ALIGNMENTS`).
            let mut copies: HashMap<&str, (usize, usize)> = HashMap::new();
            if keys == 0 && old_range.len() > 1 && new_range.len() > 1 {
                for line in old_range.clone() {
                    work.spend(1 + old_keys[1][line].len() / BYTES_PER_STEP)?;
                    copies.entry(old_keys[1][line].as_str()).or_default().0 += 1;
                }
                for line in new_range.clone() {
                    work.spend(1 + new_keys[1][line].len() / BYTES_PER_STEP)?;
                    copies.entry(new_keys[1][line].as_str()).or_default().1 += 1;
                }
                copies.retain(|_, &mut (old, new)| old == new && old > 1);
            }
            let old_candidates: Vec<usize> = old_range
                .clone()
                .filter(|&index| {
                    takes_part(&old_keys[keys][index])
                        && !copies.contains_key(old_keys[1][index].as_str())
                })
                .collect();
            let new_candidates: Vec<usize> = new_range
                .clone()
                .filter(|&index| {
                    takes_part(&new_keys[keys][index])
                        && !copies.contains_key(new_keys[1][index].as_str())
                })
                .collect();
            // Keys interned once, their bytes charged: the searches then
            // compare and hash numbers, a step each whatever a line's length.
            let mut ids: HashMap<&str, u32> = HashMap::new();
            let mut old_ids = Vec::with_capacity(old_candidates.len());
            for &index in &old_candidates {
                let key = old_keys[keys][index].as_str();
                work.spend(1 + key.len() / BYTES_PER_STEP)?;
                let next = ids.len() as u32;
                old_ids.push(*ids.entry(key).or_insert(next));
            }
            let mut new_ids = Vec::with_capacity(new_candidates.len());
            for &index in &new_candidates {
                let key = new_keys[keys][index].as_str();
                work.spend(1 + key.len() / BYTES_PER_STEP)?;
                let next = ids.len() as u32;
                new_ids.push(*ids.entry(key).or_insert(next));
            }
            patience_pairs(&old_ids, &new_ids, fall_back, work)?
                .into_iter()
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
            work,
            aligned,
        )?;
        aligned.push((old_line..old_line + 1, new_line..new_line + 1));
        (old_at, new_at) = (old_line + 1, new_line + 1);
    }
    align_lines(
        old_keys,
        new_keys,
        level + 1,
        old_at..old_range.end,
        new_at..new_range.end,
        work,
        aligned,
    )
}

/// How many bytes of whole lines `old` and `new` hold alike at their start,
/// and at their end (not overlapping): each ends, or starts, where a line
/// does in both, never between a CR and its LF.
fn alike_line_ends(old: &str, new: &str) -> (usize, usize) {
    let (old, new) = (old.as_bytes(), new.as_bytes());
    let starts_line = |text: &[u8], at: usize| {
        at == 0 || text[at - 1] == b'\n' || (text[at - 1] == b'\r' && text.get(at) != Some(&b'\n'))
    };
    let mut head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    while head > 0 && !(starts_line(old, head) && starts_line(new, head)) {
        head -= 1;
    }
    let most = old.len().min(new.len()) - head;
    let mut tail = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(most)
        .take_while(|(a, b)| a == b)
        .count();
    while tail > 0 && !(starts_line(old, old.len() - tail) && starts_line(new, new.len() - tail)) {
        tail -= 1;
    }
    (head, tail)
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
