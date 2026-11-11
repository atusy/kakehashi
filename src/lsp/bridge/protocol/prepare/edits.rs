//! Edits a downstream server made to a prepared document (P), mapped onto
//! its virtual document (V).
//!
//! An edit keeps to the text P shows: it maps through the same runs
//! positions do, and the lines it creates regain the indentation the peer
//! removed from the segment they land in. Edits that rewrite lines (a
//! formatter's answer, a workspace edit creating or removing lines) are
//! read as the text they leave instead: the lines V takes for the edited P
//! are written first, and the edits are what V differs from them in. The
//! mapped set is then checked as a whole against the host text V holds and
//! P does not show, so edits that are fine one by one cannot combine into a
//! corruption of it.

use std::collections::HashMap;
use std::ops::Range;

use tower_lsp_server::ls_types::{Position, Range as LspRange, TextEdit};

use super::{
    Bias, LineMap, PreparedMap, Run, RunKind, Side, empty_run, is_line_start, map_offset,
    map_offset_stopping, rest_of_line_blank, splits_crlf,
};

mod diff;

use diff::{Work, char_hunks, myers_pairs};

/// How a set of edits to P is read when mapped to V
/// ([`PreparedMap::map_edits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditMode {
    /// Edits keeping their extents (a completion's replace range, an inlay
    /// hint's text edits), mapped one by one.
    Extent,
    /// One document's edits a client applies together (a rename's, a code
    /// action's workspace edit): each edit, or run of edits touching one
    /// another, within one line maps keeping its extent; one creating,
    /// removing or rewriting lines is replayed on P and its own lines
    /// rewritten in V, as a formatting answer's are (see
    /// [`PreparedMap::map_edits`]).
    Replay,
    /// Edits that must map one for each (annotated edits, keeping their
    /// identity): taken only when each stays within one line, apart from
    /// the others, where replaying them maps each as it keeps its extent;
    /// any other set refused.
    InLine,
}

/// Why a formatting answer was refused (see
/// [`PreparedMap::map_formatting_answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormattingRefusal {
    /// Its shape: the host cannot take what it changes — a line of text
    /// where the region lost no one indentation to regain, any change to a
    /// region the peer dedented unevenly or to lines holding a gap, or text
    /// meeting the host's own (the lines deleted at the document's edges)
    /// other than as the server wrote it.
    Shape,
}

impl std::fmt::Display for FormattingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Shape => "its result cannot be mapped back through the prepared virtual document",
        })
    }
}

/// How much work telling what a formatting answer changes may do (see
/// [`PreparedMap::map_formatting_answer`]), counted in steps of its
/// searches (see [`Work`]): pairing the lines the answer left alike, then
/// narrowing each change to the characters that differ. The work decides
/// only how small the edits are, never whether the answer maps: out of
/// work, the lines not yet paired are replaced whole, on any machine and
/// under any load alike (the count, not a clock, decides). What runs around
/// the searches (applying the answer to P, reading both texts' lines,
/// writing the lines V takes) is linear in the texts.
const FORMATTING_DIFF_BUDGET: usize = 1 << 20;

/// The most a formatting answer's diff may spend, the per-byte allowance
/// included. What the searches keep of the answer is spent before it is
/// taken, some four bytes a step (`LINE_STEPS` for each line kept to pair,
/// `FORMATTING_DIFF_WORK_PER_BYTE` for each byte compared by character,
/// one for each cell of a table), so this bounds it at some 64 MiB however
/// large the answer. Beside it the mapping keeps what is linear in the
/// texts: P's lines and their index, the edited P, and the text it writes.
const FORMATTING_DIFF_BUDGET_CAP: usize = 1 << 24;

/// How many bytes a step of work covers where the searches hash a line's
/// text whole: such a step costs its length, not one.
const BYTES_PER_STEP: usize = 64;

/// What a line indexed or kept to pair costs, before its length: it is
/// held in several lists until the pairing is read.
const LINE_STEPS: usize = 8;

/// Work the diff of a formatting answer may do on top of
/// `FORMATTING_DIFF_BUDGET` for each byte of P and of the edited P, and
/// what narrowing a change by character costs for each byte of it: an
/// answer rewriting every line is narrowed line by line, in work linear in
/// the texts, which no fixed budget covers for a large region.
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
            EditMode::Replay => self.map_replayed(edits),
            EditMode::InLine => self.map_in_line(edits),
        }
    }

    /// [`Self::map_edits`] for a formatter's answer, saying why when it is
    /// refused: what it changes counts, not how it says so. The edits are
    /// applied to P, the text V takes for the result is written (see
    /// [`Self::map_lines`]), and the edits returned are what V differs
    /// from it in, in V's order (none when the answer changes nothing): a
    /// whole-document replacement maps as precisely as small edits, and the
    /// set mapped is that diff's, not the edits sent. A range may also end
    /// at the line just past P's last (`(lineCount, 0)`), naming P's end,
    /// as whole-document answers do. The formatting transforms clamp such
    /// an end to P's last line before mapping, to the same place; it is
    /// read here too so that an answer is taken as servers send it, whoever
    /// calls this.
    pub(crate) fn map_formatting_answer(
        &self,
        edits: &[TextEdit],
    ) -> Result<Vec<TextEdit>, FormattingRefusal> {
        self.map_formatting_within(
            edits,
            FORMATTING_DIFF_BUDGET,
            FORMATTING_DIFF_WORK_PER_BYTE,
            Some(FORMATTING_DIFF_BUDGET_CAP),
        )
    }

    /// Whether a formatting answer of `edits` drops P's final line break
    /// where the host's deleted closing (lines after the content, a closing
    /// indent) follows it: mapped, it would edit that closing, taking its
    /// indent off or writing a line on it. Answers whose edge layout is not
    /// restored before they are mapped (on-type ones) are refused so.
    pub(crate) fn drops_the_break_before_the_closing(&self, edits: &[TextEdit]) -> bool {
        let prepared = &self.prepared_lines.text;
        if !prepared.ends_with(['\n', '\r']) || self.removed_indent_at(prepared.len()).is_none() {
            return false;
        }
        let Some((ranges, order)) = sorted_extents(&self.prepared_lines, edits, true) else {
            return false;
        };
        // What ends the edited P, judged on the whole set: walking back from
        // P's end, each edit reaching where the text so far ends either ends
        // it with its own text, or (writing nothing) leaves it to what comes
        // before its start, another edit touching it included.
        let mut end = prepared.len();
        for &index in order.iter().rev() {
            if ranges[index].end != end {
                break;
            }
            if let Some(last) = edits[index].new_text.chars().next_back() {
                return !matches!(last, '\n' | '\r');
            }
            end = ranges[index].start;
        }
        !prepared[..end].ends_with(['\n', '\r'])
    }

    /// [`Self::map_formatting_answer`], the diff doing at most `budget`
    /// steps of work.
    #[cfg(test)]
    fn map_formatting(
        &self,
        edits: &[TextEdit],
        budget: usize,
    ) -> Result<Vec<TextEdit>, FormattingRefusal> {
        self.map_formatting_within(edits, budget, 0, None)
    }

    /// Whether a line of the content (not whitespace alone, nor starting in
    /// a gap's text) lost other indentation than its segment's uniform one:
    /// the peer left it alone, or removed another indent. Every line a
    /// formatting answer writes regains the one uniform indent (see
    /// [`Self::map_lines`]), which such a region does not have: a line
    /// left alike would keep its own, and one rewritten lose it. Edits
    /// keeping their extents state where they go, and are not so refused.
    fn dedents_unevenly(&self) -> bool {
        lines_with_breaks(&self.prepared_lines.text).any(|(start, line)| {
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
            let own = self
                .removed_indent_at(start)
                .map_or("", |run| &self.virtual_lines.text[run.virtual_.clone()]);
            let at = map_offset(&self.runs, start, Bias::Start, Side::Prepared);
            self.uniform_indent_at(at) != Some(own)
        })
    }

    /// [`Self::map_formatting_answer`], the diff doing at most `budget`
    /// steps of work and `per_byte` more for each byte of P and the edited
    /// P, all of it no more than `cap`, if any.
    fn map_formatting_within(
        &self,
        edits: &[TextEdit],
        budget: usize,
        per_byte: usize,
        cap: Option<usize>,
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
            .min(cap.unwrap_or(usize::MAX));
        let mut work = Work::new(budget);
        let mapped = self
            .map_lines(0..prepared.len(), &edited, 0..edited.len(), &mut work)
            .ok_or(FormattingRefusal::Shape)?;
        self.to_text_edits(mapped).ok_or(FormattingRefusal::Shape)
    }

    /// P's whole lines `old` as `edited[new]` rewrote them (other spans may
    /// have changed elsewhere), as changes to V in V's order, apart from one
    /// another; not yet checked as a set. The text V takes is written
    /// first, and the changes are what V differs from it in:
    ///
    /// - A line the two texts hold alike (whole lines alike at either end,
    ///   then a longest run of lines alike between them) stays as V has it,
    ///   whatever the peer removed from it: a whitespace-only line keeps
    ///   the host's whitespace.
    /// - Any other line is written as the edited P has it, after the
    ///   indentation the region lost: every line the answer wrote regains
    ///   it, but an empty one, which carries none.
    /// - A last line written with no line break where P ends with one takes
    ///   the place of the indentation-only last line the peer removed.
    ///
    /// So which lines pair decides which whitespace a whitespace-only line
    /// keeps and how small the changes are, never where text goes: with
    /// `work` run out, the lines not yet paired are replaced whole.
    ///
    /// `None` when lines it rewrites hold a gap (see
    /// [`Self::lines_hold_a_gap`]), or a line is written where the region
    /// lost no one indentation. The caller refuses
    /// regions dedented unevenly (see [`Self::dedents_unevenly`]): there a
    /// line alike in P need not regain the one indentation.
    ///
    /// The spans are whole lines, each starting at a line start and ending
    /// at one (never between a CR and its LF) or at its text's end, both
    /// at their text's end or neither.
    fn map_lines(
        &self,
        old: Range<usize>,
        edited: &str,
        new: Range<usize>,
        work: &mut Work,
    ) -> Option<Vec<(Range<usize>, String)>> {
        let prepared = &self.prepared_lines.text;
        debug_assert!(
            is_line_start(prepared, old.start)
                && (old.end == prepared.len() || is_line_start(prepared, old.end))
                && is_line_start(edited, new.start)
                && (new.end == edited.len() || is_line_start(edited, new.end))
                && (old.end == prepared.len()) == (new.end == edited.len()),
            "a span mapped is whole lines"
        );
        let (old_text, new_text) = (&prepared[old.clone()], &edited[new.clone()]);
        // Whole lines alike at either end are set aside, uncharged: a small
        // answer in a large region costs work for its own size.
        let (head, tail) = alike_line_ends(old_text, new_text);
        let old_middle = &old_text[head..old_text.len() - tail];
        let new_middle = &new_text[head..new_text.len() - tail];
        let old_lines: Vec<(usize, &str)> = lines_with_breaks(old_middle).collect();
        // Where the middle's old line `line` starts in P (past the last,
        // where the middle ends), and where that is in V, the indentation
        // the peer removed from the line included.
        let prepared_start = |line: usize| {
            let start = old_lines
                .get(line)
                .map_or(old_middle.len(), |&(start, _)| start);
            old.start + head + start
        };
        let line_start = |line: usize| self.virtual_line_start(prepared_start(line));
        // Where the middle ends in V. At P's end, P ending with a line
        // break (or empty) shows an empty last line there: the edited P
        // ending so too leaves it alone, the indentation-only last line the
        // peer removed with it; a last line written with no line break
        // takes its place.
        let ends_line = |text: &str| text.is_empty() || text.ends_with(['\n', '\r']);
        let middle_end = match self.closing_indent() {
            Some(closing)
                if tail == 0
                    && old.end == prepared.len()
                    && ends_line(prepared)
                    && !ends_line(edited) =>
            {
                closing.virtual_.end
            }
            _ => line_start(old_lines.len()),
        };

        let mut changes = Vec::new();
        let (mut old_line, mut new_at) = (0, 0);
        let pairs = alike_lines(&old_lines, new_middle, work);
        let ends = pairs
            .into_iter()
            .map(|(line, start)| (line, start, start + old_lines[line].1.len()))
            .chain([(old_lines.len(), new_middle.len(), new_middle.len())]);
        for (line, start, next) in ends {
            if old_line < line || new_at < start {
                // Lines left alike may hold a gap; lines rewritten may
                // not, V holding other text there than P shows.
                if self.lines_hold_a_gap(&(prepared_start(old_line)..prepared_start(line))) {
                    return None;
                }
                let from = line_start(old_line);
                let to = if line < old_lines.len() {
                    line_start(line)
                } else {
                    middle_end
                };
                let written = self.reindented(&new_middle[new_at..start], from)?;
                self.push_changes(from..to, &written, work, &mut changes);
            }
            (old_line, new_at) = (line + 1, next);
        }
        Some(changes)
    }

    /// Whether P's whole lines `old` (or, empty, the lines written there)
    /// hold a gap's text, or start right after one: a gap emptied in P
    /// counts where its place is in them, but at their end, where the next
    /// line starts with its host text. A gap opening the line after them
    /// leaves them whole lines of V too; one ending the line before them
    /// may show a line break there that V lacks.
    fn lines_hold_a_gap(&self, old: &Range<usize>) -> bool {
        let first = self
            .runs
            .partition_point(|run| run.prepared.end < old.start);
        self.runs[first..]
            .iter()
            .take_while(|run| run.prepared.start <= old.end)
            .filter(|run| run.kind == RunKind::Gap)
            .any(|gap| {
                if gap.prepared.is_empty() {
                    old.start <= gap.prepared.start && gap.prepared.start < old.end
                } else {
                    gap.prepared.start < old.end && old.start <= gap.prepared.end
                }
            })
    }

    /// Where the line P starts at `offset` starts in V, the indentation the
    /// peer removed from it included: after the lines deleted from the
    /// document's start, before those deleted from its end.
    fn virtual_line_start(&self, offset: usize) -> usize {
        let first = self.runs.partition_point(|run| run.prepared.end < offset);
        for run in &self.runs[first..] {
            if run.prepared.start < offset {
                // The text before the line start, or text holding it.
                if offset < run.prepared.end {
                    return run.virtual_.start + (offset - run.prepared.start);
                }
            } else if run.kind != RunKind::LeadingLines {
                return run.virtual_.start;
            }
        }
        self.virtual_lines.text.len()
    }

    /// The indentation-only last line the peer removed (the indent before a
    /// closing `''`), with no closing lines deleted before it: the empty
    /// last line P shows after its final line break.
    fn closing_indent(&self) -> Option<&Run> {
        let end = self.prepared_lines.text.len();
        empty_run(&self.runs, end, |kind| kind == RunKind::TrailingLines)
            .is_none()
            .then(|| self.removed_indent_at(end))
            .flatten()
    }

    /// Whole lines the edited P holds, as V takes them at `virtual_start`:
    /// each after the indentation its segment lost, but an empty one.
    /// `None` when a line is written where the segment lost no one
    /// indentation.
    fn reindented(&self, lines: &str, virtual_start: usize) -> Option<String> {
        let mut uniform = None;
        let mut output = String::with_capacity(lines.len());
        for (_, line) in lines_with_breaks(lines) {
            if !line.trim_end_matches(['\r', '\n']).is_empty() {
                let indent = match uniform {
                    Some(indent) => indent,
                    None => *uniform.insert(self.uniform_indent_at(virtual_start)?),
                };
                output.push_str(indent);
            }
            output.push_str(line);
        }
        Some(output)
    }

    /// The change of V's `range` (whole lines) into `written`, narrowed to
    /// what differs and pushed onto `changes`, after the ones before it.
    /// The lines are paired where they read alike but for whitespace and
    /// punctuation (a line a formatter respaced or requoted is the line it
    /// was), and each pair, and each run of lines between pairs, is
    /// narrowed on its own. With `work` out, they are taken line for line
    /// where both hold as many lines, as one text otherwise. Whatever the
    /// pairing, the change writes `written`: it decides only how small
    /// the stretches are.
    fn push_changes(
        &self,
        range: Range<usize>,
        written: &str,
        work: &mut Work,
        changes: &mut Vec<(Range<usize>, String)>,
    ) {
        let held = &self.virtual_lines.text[range.clone()];
        let counts = (
            lines_with_breaks(held).count(),
            lines_with_breaks(written).count(),
        );
        let Some(pairs) = rewritten_lines(held, written, counts, work) else {
            if counts.0 == counts.1 {
                for ((start, held), (_, written)) in
                    lines_with_breaks(held).zip(lines_with_breaks(written))
                {
                    self.push_narrowed(range.start + start, held, written, work, changes);
                }
            } else {
                self.push_narrowed(range.start, held, written, work, changes);
            }
            return;
        };
        let (mut held_at, mut written_at) = (0, 0);
        for (old, new) in pairs
            .into_iter()
            .chain([(held.len()..held.len(), written.len()..written.len())])
        {
            // The lines between pairs, then the pair.
            for (old, new) in [(held_at..old.start, written_at..new.start), (old, new)] {
                (held_at, written_at) = (old.end, new.end);
                self.push_narrowed(
                    range.start + old.start,
                    &held[old],
                    &written[new],
                    work,
                    changes,
                );
            }
        }
    }

    /// The change of V's text `held`, at `start`, into `written`, pushed
    /// onto `changes` as the stretches that differ: what the two hold alike
    /// at either end is left out, and what is left is diffed by character
    /// while `work` lasts (each byte of it costing
    /// `FORMATTING_DIFF_WORK_PER_BYTE` before the search), replaced whole
    /// once it is out. A stretch never starts or ends between a CR and its
    /// LF in V (no LSP position names that place): it widens over the
    /// character beside it, and one so meeting the stretch before it joins
    /// it.
    fn push_narrowed(
        &self,
        start: usize,
        held: &str,
        written: &str,
        work: &mut Work,
        changes: &mut Vec<(Range<usize>, String)>,
    ) {
        if held == written {
            return;
        }
        let head = alike_bytes(held.chars(), written.chars());
        let (held_rest, written_rest) = (&held[head..], &written[head..]);
        let tail = alike_bytes(held_rest.chars().rev(), written_rest.chars().rev());
        let held_rest = &held_rest[..held_rest.len() - tail];
        let written_rest = &written_rest[..written_rest.len() - tail];
        // The stretches of the two that differ, as bytes of each.
        let whole = || vec![(0..held_rest.len(), 0..written_rest.len())];
        let stretches = if held_rest.is_empty() || written_rest.is_empty() {
            whole()
        } else {
            work.spend(
                FORMATTING_DIFF_WORK_PER_BYTE.saturating_mul(held_rest.len() + written_rest.len()),
            )
            .and_then(|()| {
                let old_chars: Vec<char> = held_rest.chars().collect();
                let new_chars: Vec<char> = written_rest.chars().collect();
                let hunks = char_hunks(&old_chars, &new_chars, work)?;
                // Hunks come in order: each text's characters are walked
                // once for the bytes they start at.
                let bytes = |chars: &[char], from: usize, to: usize| -> usize {
                    chars[from..to].iter().map(|c| c.len_utf8()).sum()
                };
                let (mut old_at, mut old_byte, mut new_at, mut new_byte) = (0, 0, 0, 0);
                Some(
                    hunks
                        .into_iter()
                        .map(|(old, new)| {
                            old_byte += bytes(&old_chars, old_at, old.start);
                            let old_end = old_byte + bytes(&old_chars, old.start, old.end);
                            new_byte += bytes(&new_chars, new_at, new.start);
                            let new_end = new_byte + bytes(&new_chars, new.start, new.end);
                            let stretch = (old_byte..old_end, new_byte..new_end);
                            (old_at, old_byte, new_at, new_byte) =
                                (old.end, old_end, new.end, new_end);
                            stretch
                        })
                        .collect(),
                )
            })
            .unwrap_or_else(whole)
        };
        let virtual_text = &self.virtual_lines.text;
        for (old, new) in stretches {
            let mut range = start + head + old.start..start + head + old.end;
            let mut text = written_rest[new].to_string();
            // The character widened over is V's own, written back.
            if splits_crlf(virtual_text, range.start) {
                range.start -= 1;
                text.insert(0, '\r');
            }
            if splits_crlf(virtual_text, range.end) {
                range.end += 1;
                text.push('\n');
            }
            match changes.last_mut() {
                Some((last, last_text)) if last.end >= range.start => {
                    last.end = range.end;
                    last_text.push_str(&text);
                }
                _ => changes.push((range, text)),
            }
        }
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

    /// [`EditMode::InLine`]: each edit within one line (neither what it
    /// replaces nor its text holds a line break) and touching no other, as
    /// replaying them would take each, mapped keeping its extent.
    fn map_in_line(&self, edits: &[TextEdit]) -> Option<Vec<TextEdit>> {
        let prepared = &self.prepared_lines.text;
        let (ranges, order) = sorted_extents(&self.prepared_lines, edits, false)?;
        let breaks = |text: &str| text.contains(['\n', '\r']);
        if order
            .windows(2)
            .any(|pair| ranges[pair[0]].end == ranges[pair[1]].start)
            || edits
                .iter()
                .zip(&ranges)
                .any(|(edit, range)| breaks(&prepared[range.clone()]) || breaks(&edit.new_text))
        {
            return None;
        }
        self.map_extents(edits)
    }

    /// [`EditMode::Replay`]. The edits are taken in P's order and grouped
    /// where they touch (one ending where the next starts, two inserting at
    /// one place): mapped one by one, touching edits could each claim the
    /// removed indent of the line they meet at. A group within one line
    /// (neither what it replaces nor its text holds a line break) creates no
    /// line and regains no indent: it maps as the one edit it makes, keeping
    /// its extent, as completion edits do. Any other group is replayed on
    /// the whole lines it reaches, and those lines are rewritten in V as a
    /// formatting answer's are (see [`Self::map_lines`]): what it leaves
    /// alike stays as the host has it (a blank line's whitespace), and the
    /// lines it writes regain the content's indent. Such a group is
    /// refused, with the whole set, where the peer dedented the region
    /// unevenly (see [`Self::dedents_unevenly`]). Groups are rewritten
    /// apart, never as one text: across the whole text, a line the edits
    /// left alone could be read as another reading the same in P, against
    /// what the edits state, and keep that one's whitespace. Groups whose
    /// lines meet are rewritten together. The mapped set is checked as a
    /// whole.
    fn map_replayed(&self, edits: &[TextEdit]) -> Option<Vec<TextEdit>> {
        let prepared = &self.prepared_lines.text;
        // An end at the start of the line just past P's last names P's end,
        // as whole-document replacements (a fix-all's) send it, read as a
        // formatting answer's is.
        let (ranges, order) = sorted_extents(&self.prepared_lines, edits, true)?;
        // The edited P, and where each edit's text lands in it.
        let mut edited = String::with_capacity(prepared.len());
        let mut landed = vec![0..0; edits.len()];
        let mut cursor = 0;
        for &index in &order {
            edited.push_str(&prepared[cursor..ranges[index].start]);
            let start = edited.len();
            edited.push_str(&edits[index].new_text);
            landed[index] = start..edited.len();
            cursor = ranges[index].end;
        }
        edited.push_str(&prepared[cursor..]);

        // How far each edit, in order, and the ones before it moved the text
        // after them: where a P offset outside every edit lands in the
        // edited P is read from it, never added up from distances in P,
        // which other edits on the same lines would make stale.
        let mut shifts = Vec::with_capacity(order.len() + 1);
        shifts.push(0isize);
        for &index in &order {
            let moved = landed[index].len() as isize - ranges[index].len() as isize;
            shifts.push(shifts.last().expect("starts with one") + moved);
        }

        // Groups of touching edits, in order: P range, the edits it holds
        // (as positions in `order`), and whether it reaches across lines.
        struct Unit {
            old: Range<usize>,
            edits: Range<usize>,
            lines: bool,
        }
        // The edited P a unit's P range became: its edits are every edit
        // within it.
        let new_of = |unit: &Unit| {
            let at =
                |offset: usize, shift: isize| offset.checked_add_signed(shift).expect("in text");
            at(unit.old.start, shifts[unit.edits.start])..at(unit.old.end, shifts[unit.edits.end])
        };
        let mut groups: Vec<Unit> = Vec::new();
        for (at, &index) in order.iter().enumerate() {
            let old = ranges[index].clone();
            match groups.last_mut() {
                Some(group) if group.old.end == old.start => {
                    group.old.end = old.end;
                    group.edits.end = at + 1;
                }
                _ => groups.push(Unit {
                    old,
                    edits: at..at + 1,
                    lines: false,
                }),
            }
        }
        let breaks = |text: &str| text.contains(['\n', '\r']);
        for group in &mut groups {
            group.lines = breaks(&prepared[group.old.clone()]) || breaks(&edited[new_of(group)]);
        }
        // A group reaching across lines takes the whole lines it reaches, in
        // P and alike in the edited P; groups whose lines meet or overlap are
        // one.
        // The start of the line `offset` is on, looked for back to `floor`
        // only (the end of the unit before, which units visited in P's
        // order never reach back past): `floor` itself when the line starts
        // before it, the group then joining that unit, whose start is
        // looked for in turn. Each stretch of P is so searched once, not
        // once for each group on a long line.
        let line_start = |offset: usize, floor: usize| {
            // (A group starting inside the unit before joins it anyway.)
            let floor = floor.min(offset);
            prepared[floor..offset]
                .rfind(['\n', '\r'])
                .map_or(floor, |at| floor + at + 1)
        };
        // Past the line break ending the line `offset` is on (P's end on the
        // last line).
        let line_end = |offset: usize| {
            let rest = &prepared[offset..];
            rest.find(['\n', '\r']).map_or(prepared.len(), |at| {
                offset + at + if rest[at..].starts_with("\r\n") { 2 } else { 1 }
            })
        };
        // An end both texts have a line boundary at: a line start (not
        // inside a CRLF the group's text completes) or the text's end.
        let boundary = |text: &str, at: usize| {
            at == text.len() || is_line_start(text, at) && !splits_crlf(text, at)
        };
        // A unit's end, past the line it ends on unless both texts have a
        // line boundary there; read once the unit holds every edit up to it.
        let line_end_of = |unit: &Unit| {
            if boundary(prepared, unit.old.end) && boundary(&edited, new_of(unit).end) {
                unit.old.end
            } else {
                line_end(unit.old.end)
            }
        };
        let mut units: Vec<Unit> = Vec::with_capacity(groups.len());
        for mut group in groups {
            let floor = |units: &[Unit]| units.last().map_or(0, |unit| unit.old.end);
            if group.lines {
                group.old.start = line_start(group.old.start, floor(&units));
            }
            // Every unit the group's lines reach, before it on its first
            // line included, joins it first: only then does it hold every
            // edit within it.
            while let Some(last) = units.last()
                && (group.lines || last.lines)
                && group.old.start <= last.old.end
            {
                let last = units.pop().expect("a unit is there");
                group = Unit {
                    old: line_start(last.old.start.min(group.old.start), floor(&units))
                        ..last.old.end.max(group.old.end),
                    edits: last.edits.start..group.edits.end,
                    lines: true,
                };
            }
            if group.lines {
                group.old.end = line_end_of(&group);
            }
            units.push(group);
        }

        let diffed_bytes: usize = units
            .iter()
            .filter(|unit| unit.lines)
            .map(|unit| unit.old.len() + new_of(unit).len())
            .sum();
        let mut work = Work::new(
            FORMATTING_DIFF_BUDGET
                .saturating_add(FORMATTING_DIFF_WORK_PER_BYTE.saturating_mul(diffed_bytes))
                .min(FORMATTING_DIFF_BUDGET_CAP),
        );
        let mut unevenly = None;
        let mut mapped = Vec::with_capacity(units.len());
        for unit in units {
            let new = new_of(&unit);
            if !unit.lines {
                let following = prepared[unit.old.end..].chars().next();
                mapped.push(self.hunk_to_virtual(unit.old, &edited[new], following)?);
                continue;
            }
            if prepared[unit.old.clone()] == edited[new.clone()] {
                continue;
            }
            if *unevenly.get_or_insert_with(|| self.dedents_unevenly()) {
                return None;
            }
            mapped.extend(self.map_lines(unit.old, &edited, new, &mut work)?);
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
        // their order, see `map_edits`), and the changes rewritten lines
        // map to are apart as they are made (see `map_lines`). Checked all
        // the same: every later read of the sorted set relies on it.
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

    /// Map the replacement of P's `old` bytes by `new_text`, an edit
    /// keeping its extent, onto V. `following` is the character after the
    /// replacement in P.
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

/// A longest run of whole lines `old_lines` and the text `new` hold alike,
/// in order: for each, the old line's index and where the new one starts in
/// `new`. Only the old lines are indexed, and of the new ones only those
/// alike to an old one are kept, each spent from `work` as it is taken
/// (`LINE_STEPS`, and its length): a large answer over a small region
/// keeps only its lines that a line of the region can pair with, and of
/// those no more than the work covers. None at all once `work` runs out:
/// what the lines hold alike then decides nothing but how small the
/// changes are.
fn alike_lines(old_lines: &[(usize, &str)], new: &str, work: &mut Work) -> Vec<(usize, usize)> {
    if old_lines.is_empty() || new.is_empty() {
        return Vec::new();
    }
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let mut old_ids = Vec::with_capacity(old_lines.len());
    for &(_, line) in old_lines {
        if work
            .spend(LINE_STEPS + line.len() / BYTES_PER_STEP)
            .is_none()
        {
            return Vec::new();
        }
        let next = ids.len() as u32;
        old_ids.push(*ids.entry(line).or_insert(next));
    }
    let mut starts = Vec::new();
    let mut new_ids = Vec::new();
    for (start, line) in lines_with_breaks(new) {
        if let Some(&id) = ids.get(line) {
            if work
                .spend(LINE_STEPS + line.len() / BYTES_PER_STEP)
                .is_none()
            {
                return Vec::new();
            }
            starts.push(start);
            new_ids.push(id);
        }
    }
    myers_pairs(&old_ids, &new_ids, work)
        .unwrap_or_default()
        .into_iter()
        .map(|(old, new)| (old, starts[new]))
        .collect()
}

/// The lines of `held` and `written` (holding `counts` lines) that read
/// alike in their letters and digits, a longest run of them in order: for
/// each, the bytes of the line in either text. `None` when `work` runs
/// out, each line costing `LINE_STEPS` before it is kept. Lines are
/// compared by a hash of those characters: two that only hash alike pair
/// as well, which costs a stretch narrowed less, nothing else.
fn rewritten_lines(
    held: &str,
    written: &str,
    counts: (usize, usize),
    work: &mut Work,
) -> Option<Vec<(Range<usize>, Range<usize>)>> {
    use std::hash::{Hash, Hasher};
    work.spend(LINE_STEPS.saturating_mul(counts.0 + counts.1))?;
    work.spend((held.len() + written.len()) / BYTES_PER_STEP)?;
    let lines = |text: &str| -> (Vec<Range<usize>>, Vec<u64>) {
        lines_with_breaks(text)
            .map(|(start, line)| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                for character in line.chars().filter(|c| c.is_alphanumeric()) {
                    character.hash(&mut hasher);
                }
                (start..start + line.len(), hasher.finish())
            })
            .unzip()
    };
    let ((old_lines, old_keys), (new_lines, new_keys)) = (lines(held), lines(written));
    Some(
        myers_pairs(&old_keys, &new_keys, work)?
            .into_iter()
            .map(|(old, new)| (old_lines[old].clone(), new_lines[new].clone()))
            .collect(),
    )
}

/// How many bytes of characters `a` and `b` give alike before one differs.
fn alike_bytes(a: impl Iterator<Item = char>, b: impl Iterator<Item = char>) -> usize {
    a.zip(b)
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum()
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
fn lines_with_breaks(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let bytes = text.as_bytes();
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == bytes.len() {
            return None;
        }
        let mut at = start;
        let end = loop {
            match bytes.get(at) {
                None => break at,
                Some(b'\r') if bytes.get(at + 1) == Some(&b'\n') => break at + 2,
                Some(b'\r' | b'\n') => break at + 1,
                Some(_) => at += 1,
            }
        };
        let line = (start, &text[start..end]);
        start = end;
        Some(line)
    })
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
#[cfg(test)]
pub(crate) use tests::exact_reference;
