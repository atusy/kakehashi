//! Sequence diffs for mapping a formatting answer, bounded by the work they
//! may do rather than by time: the same answer is accepted or refused alike
//! on any machine, under any load.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Range;

/// What is left of the work a diff may do. Each step of a search (a
/// comparison, a cell of a table, a diagonal followed) spends one; a search
/// out of work gives up, and the answer it was for is refused.
pub(super) struct Work {
    left: usize,
}

impl Work {
    pub(super) fn new(budget: usize) -> Self {
        Self { left: budget }
    }

    /// Spend `steps`; `None` (with nothing left) when there are not as many.
    pub(super) fn spend(&mut self, steps: usize) -> Option<()> {
        match self.left.checked_sub(steps) {
            Some(left) => {
                self.left = left;
                Some(())
            }
            None => {
                self.left = 0;
                None
            }
        }
    }
}

/// The pairs of a longest common subsequence of `a` and `b` (Myers'
/// greedy search), in order; `None` when `work` runs out. Elements only one
/// side holds can pair with nothing and are left out of the search first,
/// so a side mostly new costs no more than its length. The table kept to
/// read the pairs back grows with the square of the differences left,
/// every cell of it spent from `work`, so the work bounds its memory too;
/// the frontier holds no more diagonals than the work left could reach.
pub(super) fn myers_pairs<T: Eq + Hash>(
    a: &[T],
    b: &[T],
    work: &mut Work,
) -> Option<Vec<(usize, usize)>> {
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix
        && suffix < b.len() - prefix
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    // The scan for what is alike at the ends is linear in the input and
    // not charged: a long text changed in one place costs that place.
    work.spend(1)?;
    let (core_a, core_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    work.spend(core_a.len() + core_b.len())?;
    let shared = |from: &[T], other: &[T]| -> Vec<usize> {
        let other: HashSet<&T> = other.iter().collect();
        (0..from.len())
            .filter(|&at| other.contains(&from[at]))
            .collect()
    };
    let (kept_a, kept_b) = (shared(core_a, core_b), shared(core_b, core_a));
    let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|at| (at, at)).collect();
    let (search_a, search_b): (Vec<&T>, Vec<&T>) = (
        kept_a.iter().map(|&at| &core_a[at]).collect(),
        kept_b.iter().map(|&at| &core_b[at]).collect(),
    );
    for (old, new) in myers_core(&search_a, &search_b, work)? {
        pairs.push((kept_a[old] + prefix, kept_b[new] + prefix));
    }
    let (tail_a, tail_b) = (a.len() - suffix, b.len() - suffix);
    pairs.extend((0..suffix).map(|at| (tail_a + at, tail_b + at)));
    Some(pairs)
}

#[cfg(test)]
thread_local! {
    /// The cells of the frontier [`myers_core`] last allocated.
    static FRONTIER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [`myers_pairs`] for texts sharing no prefix or suffix.
fn myers_core<T: PartialEq>(a: &[T], b: &[T], work: &mut Work) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 || m == 0 {
        return Some(Vec::new());
    }
    // The furthest x, kept as 4 bytes.
    u32::try_from(n).ok()?;
    // Step d spends 2d + 1, so the steps 0 to d spend (d + 1)^2: no
    // further step than the square root of the work left can be reached,
    // and the frontier holds no more diagonals than that.
    let max = ((n + m) as usize).min(work.left.isqrt());
    let offset = max as isize + 1;
    // v[k + offset]: the furthest x on diagonal k. `trace` holds, from
    // d * d on, the band of `v` before step d, diagonals -d to d: a cell
    // for each step spent on it, 4 bytes each.
    let mut v = vec![0isize; 2 * max + 3];
    #[cfg(test)]
    FRONTIER.with(|cells| cells.set(v.len()));
    let mut trace: Vec<u32> = Vec::new();
    let mut reached = None;
    'search: for d in 0..=max as isize {
        work.spend(2 * d as usize + 1)?;
        trace.extend(
            v[(offset - d) as usize..=(offset + d) as usize]
                .iter()
                .map(|&x| x as u32),
        );
        let mut k = -d;
        while k <= d {
            let index = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) {
                v[index + 1]
            } else {
                v[index - 1] + 1
            };
            let mut y = x - k;
            let start = x;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            work.spend((x - start) as usize)?;
            v[index] = x;
            if x >= n && y >= m {
                reached = Some(d);
                break 'search;
            }
            k += 2;
        }
    }
    // Out of steps before the end: the next would have spent past the work.
    let Some(d_end) = reached else {
        work.left = 0;
        return None;
    };
    // Read the snakes back from the end.
    let mut pairs = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=d_end).rev() {
        let band = &trace[(d * d) as usize..];
        let at = |k: isize| band[(k + d) as usize] as isize;
        let k = x - y;
        let previous_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = at(previous_k);
        let previous_y = previous_x - previous_k;
        // The snake after the step from (previous_x, previous_y).
        let (step_x, step_y) = if previous_k == k + 1 {
            (previous_x, previous_y + 1)
        } else {
            (previous_x + 1, previous_y)
        };
        while x > step_x && y > step_y {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        (x, y) = (previous_x, previous_y);
    }
    // The first snake, from (0, 0).
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        pairs.push((x as usize, y as usize));
    }
    pairs.reverse();
    Some(pairs)
}

/// Pairs of `a` and `b` alike, in order, by patience: elements found once in
/// each anchor the rest (a longest increasing run of them), the stretches
/// between anchors paired the same way in turn, and a stretch with no such
/// element paired by [`myers_pairs`] when `fall_back` (among the elements
/// found as many times on each side), left unpaired otherwise; `None` when
/// `work` runs out.
pub(super) fn patience_pairs<T: Eq + Hash>(
    a: &[T],
    b: &[T],
    fall_back: bool,
    work: &mut Work,
) -> Option<Vec<(usize, usize)>> {
    let mut pairs = Vec::new();
    let mut stack = vec![(0..a.len(), 0..b.len())];
    while let Some((old, new)) = stack.pop() {
        if old.is_empty() || new.is_empty() {
            continue;
        }
        work.spend(old.len() + new.len())?;
        let mut counts: HashMap<&T, (usize, usize, usize)> = HashMap::new();
        for at in old.clone() {
            let entry = counts.entry(&a[at]).or_insert((0, 0, at));
            entry.0 += 1;
            entry.2 = at;
        }
        for at in new.clone() {
            if let Some(entry) = counts.get_mut(&b[at]) {
                entry.1 += 1;
            }
        }
        // Unique in both: (position in a, position in b), by position in a.
        let mut unique: Vec<(usize, usize)> = new
            .clone()
            .filter_map(|at| {
                counts
                    .get(&b[at])
                    .filter(|&&(in_a, in_b, _)| in_a == 1 && in_b == 1)
                    .map(|&(_, _, old_at)| (old_at, at))
            })
            .collect();
        if unique.is_empty() {
            if !fall_back {
                continue;
            }
            // Only elements found as many times on each side: of one found
            // more often on one side, which is which no order tells.
            let balanced = |at: &usize, side: &[T]| {
                counts
                    .get(&side[*at])
                    .is_some_and(|&(in_a, in_b, _)| in_a == in_b)
            };
            let kept_a: Vec<usize> = old.clone().filter(|at| balanced(at, a)).collect();
            let kept_b: Vec<usize> = new.clone().filter(|at| balanced(at, b)).collect();
            let (search_a, search_b): (Vec<&T>, Vec<&T>) = (
                kept_a.iter().map(|&at| &a[at]).collect(),
                kept_b.iter().map(|&at| &b[at]).collect(),
            );
            for (old_at, new_at) in myers_pairs(&search_a, &search_b, work)? {
                pairs.push((kept_a[old_at], kept_b[new_at]));
            }
            continue;
        }
        unique.sort_unstable();
        let anchors = longest_increasing(&unique, work)?;
        // Recurse into the stretches between anchors.
        let (mut old_at, mut new_at) = (old.start, new.start);
        for &(anchor_old, anchor_new) in &anchors {
            stack.push((old_at..anchor_old, new_at..anchor_new));
            pairs.push((anchor_old, anchor_new));
            (old_at, new_at) = (anchor_old + 1, anchor_new + 1);
        }
        stack.push((old_at..old.end, new_at..new.end));
    }
    pairs.sort_unstable();
    Some(pairs)
}

/// The longest run of `pairs` (sorted by their first) increasing in their
/// second.
fn longest_increasing(pairs: &[(usize, usize)], work: &mut Work) -> Option<Vec<(usize, usize)>> {
    // tails[l]: index into `pairs` of the smallest end of a run of l + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![usize::MAX; pairs.len()];
    for (index, &(_, second)) in pairs.iter().enumerate() {
        let length = tails.partition_point(|&tail| pairs[tail].1 < second);
        work.spend(tails.len().max(1).ilog2() as usize + 1)?;
        if length > 0 {
            previous[index] = tails[length - 1];
        }
        if length == tails.len() {
            tails.push(index);
        } else {
            tails[length] = index;
        }
    }
    let mut run = Vec::with_capacity(tails.len());
    let mut at = tails.last().copied();
    while let Some(index) = at {
        run.push(pairs[index]);
        at = (previous[index] != usize::MAX).then(|| previous[index]);
    }
    run.reverse();
    Some(run)
}

/// The stretches the characters `new` differ from those of `old` in, as
/// index ranges into each: characters other than whitespace are paired
/// first (a formatter mostly moves whitespace, so they are mostly alike,
/// and the search follows one diagonal), then each stretch between pairs
/// is diffed by character. `None` when `work` runs out.
pub(super) fn char_hunks(
    old: &[char],
    new: &[char],
    work: &mut Work,
) -> Option<Vec<(Range<usize>, Range<usize>)>> {
    let text = |chars: &[char]| -> Vec<usize> {
        (0..chars.len())
            .filter(|&at| !chars[at].is_whitespace())
            .collect()
    };
    let (old_text, new_text) = (text(old), text(new));
    let anchors = myers_pairs(
        &old_text.iter().map(|&at| old[at]).collect::<Vec<_>>(),
        &new_text.iter().map(|&at| new[at]).collect::<Vec<_>>(),
        work,
    )?;
    let mut hunks = Vec::new();
    let (mut old_at, mut new_at) = (0, 0);
    let ends = anchors
        .iter()
        .map(|&(old_index, new_index)| (old_text[old_index], new_text[new_index]))
        .chain([(old.len(), new.len())]);
    for (anchor_old, anchor_new) in ends {
        let (old_piece, new_piece) = (&old[old_at..anchor_old], &new[new_at..anchor_new]);
        if old_piece != new_piece {
            let mut gap = (0, 0);
            let matched = myers_pairs(old_piece, new_piece, work)?;
            for (old_index, new_index) in matched
                .into_iter()
                .chain([(old_piece.len(), new_piece.len())])
            {
                if (old_index, new_index) != gap {
                    hunks.push((
                        old_at + gap.0..old_at + old_index,
                        new_at + gap.1..new_at + new_index,
                    ));
                }
                gap = (old_index + 1, new_index + 1);
            }
        }
        (old_at, new_at) = (anchor_old + 1, anchor_new + 1);
    }
    Some(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search as it sized its frontier before: by the texts, not the
    /// work. Kept to check the bounded one gives the same results.
    fn myers_core_by_length<T: PartialEq>(
        a: &[T],
        b: &[T],
        work: &mut Work,
    ) -> Option<Vec<(usize, usize)>> {
        let (n, m) = (a.len() as isize, b.len() as isize);
        if n == 0 || m == 0 {
            return Some(Vec::new());
        }
        // The furthest x, kept as 4 bytes.
        u32::try_from(n).ok()?;
        let max = (n + m) as usize;
        let offset = max as isize;
        // v[k + offset]: the furthest x on diagonal k. `trace` holds, from
        // d * d on, the band of `v` before step d, diagonals -d to d: a cell
        // for each step spent on it, 4 bytes each.
        let mut v = vec![0isize; 2 * max + 2];
        let mut trace: Vec<u32> = Vec::new();
        let mut reached = None;
        'search: for d in 0..=max as isize {
            work.spend(2 * d as usize + 1)?;
            trace.extend(
                v[(offset - d) as usize..=(offset + d) as usize]
                    .iter()
                    .map(|&x| x as u32),
            );
            let mut k = -d;
            while k <= d {
                let index = (k + offset) as usize;
                let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) {
                    v[index + 1]
                } else {
                    v[index - 1] + 1
                };
                let mut y = x - k;
                let start = x;
                while x < n && y < m && a[x as usize] == b[y as usize] {
                    x += 1;
                    y += 1;
                }
                work.spend((x - start) as usize)?;
                v[index] = x;
                if x >= n && y >= m {
                    reached = Some(d);
                    break 'search;
                }
                k += 2;
            }
        }
        let d_end = reached?;
        // Read the snakes back from the end.
        let mut pairs = Vec::new();
        let (mut x, mut y) = (n, m);
        for d in (1..=d_end).rev() {
            let band = &trace[(d * d) as usize..];
            let at = |k: isize| band[(k + d) as usize] as isize;
            let k = x - y;
            let previous_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
                k + 1
            } else {
                k - 1
            };
            let previous_x = at(previous_k);
            let previous_y = previous_x - previous_k;
            // The snake after the step from (previous_x, previous_y).
            let (step_x, step_y) = if previous_k == k + 1 {
                (previous_x, previous_y + 1)
            } else {
                (previous_x + 1, previous_y)
            };
            while x > step_x && y > step_y {
                x -= 1;
                y -= 1;
                pairs.push((x as usize, y as usize));
            }
            (x, y) = (previous_x, previous_y);
        }
        // The first snake, from (0, 0).
        while x > 0 && y > 0 {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        pairs.reverse();
        Some(pairs)
    }

    #[test]
    fn the_frontier_is_sized_by_the_work_not_the_texts() {
        // 100,000 elements against 100,000 others, nothing alike to the
        // byte at the ends: the search can reach only as many diagonals as
        // the work allows, and its frontier holds no more.
        let a: Vec<u32> = (0..100_000).map(|at| at % 7).collect();
        let b: Vec<u32> = (0..100_000).map(|at| at % 11).collect();
        FRONTIER.with(|cells| cells.set(0));
        assert_eq!(myers_core(&a, &b, &mut Work::new(10_000)), None);
        let cells = FRONTIER.with(|cells| cells.get());
        assert!(cells <= 2 * 100 + 3, "{cells}");
    }

    #[test]
    fn the_bounded_search_finds_what_the_unbounded_one_did() {
        let mut seed: u64 = 3;
        let mut next = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) % n) as usize
        };
        for _ in 0..3_000 {
            let a: Vec<u8> = (0..next(40)).map(|_| next(4) as u8).collect();
            let b: Vec<u8> = (0..next(40)).map(|_| next(4) as u8).collect();
            let budget = next(400);
            let (mut old, mut new) = (Work::new(budget), Work::new(budget));
            assert_eq!(
                myers_core(&a, &b, &mut new),
                myers_core_by_length(&a, &b, &mut old),
                "{a:?} {b:?} {budget}"
            );
            assert_eq!(new.left, old.left, "{a:?} {b:?} {budget}");
        }
    }

    fn pairs(a: &str, b: &str) -> Vec<(usize, usize)> {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        myers_pairs(&a, &b, &mut Work::new(usize::MAX)).unwrap()
    }

    #[test]
    fn myers_pairs_as_many_as_a_longest_common_subsequence_holds() {
        assert_eq!(pairs("aabb", "baa").len(), 2);
        assert_eq!(pairs("xaby", "zabw"), vec![(1, 1), (2, 2)]);
    }

    #[test]
    fn each_search_takes_exactly_the_work_it_spends() {
        // The least budget a search completes with: one step less gives
        // up. Every charge counts toward it (a step left uncharged lowers
        // it), and what the tables keep is charged as it is taken.
        let (a, b): (Vec<char>, Vec<char>) = (
            "the quick brown fox".chars().collect(),
            "a quick brown dog jumps".chars().collect(),
        );
        assert!(myers_pairs(&a, &b, &mut Work::new(66)).is_some());
        assert_eq!(myers_pairs(&a, &b, &mut Work::new(65)), None);
        let (a, b) = (
            ["a", "x", "b", "c", "x", "d"],
            ["a", "b", "x", "c", "d", "x"],
        );
        assert!(patience_pairs(&a, &b, true, &mut Work::new(18)).is_some());
        assert_eq!(patience_pairs(&a, &b, true, &mut Work::new(17)), None);
    }

    #[test]
    fn myers_finds_a_longest_common_subsequence() {
        assert_eq!(pairs("abcabba", "cbabac").len(), 4);
        assert_eq!(pairs("", "abc"), vec![]);
        assert_eq!(pairs("abc", "abc"), vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(pairs("axc", "ayc"), vec![(0, 0), (2, 2)]);
        // Every pair alike, in order.
        let (a, b) = ("kitten sitting", "sitting kitten");
        let found = pairs(a, b);
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        assert!(found.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        assert!(found.iter().all(|&(x, y)| a[x] == b[y]));
    }

    #[test]
    fn a_search_out_of_work_gives_up_and_the_same_work_decides_alike() {
        let (a, b): (Vec<char>, Vec<char>) =
            ("abcdefgh".chars().collect(), "hgfedcba".chars().collect());
        assert_eq!(myers_pairs(&a, &b, &mut Work::new(10)), None);
        let enough = myers_pairs(&a, &b, &mut Work::new(1000));
        assert!(enough.is_some());
        assert_eq!(myers_pairs(&a, &b, &mut Work::new(1000)), enough);
    }

    #[test]
    fn elements_one_side_lacks_cost_only_their_count() {
        // Nothing alike to pair: no search over the differences at all,
        // only a pass over both sides.
        let (a, b) = (vec!['a'], vec!['b'; 5000]);
        assert_eq!(myers_pairs(&a, &b, &mut Work::new(5002)), Some(vec![]));
        let b: Vec<char> = "ab".repeat(2500).chars().collect();
        assert_eq!(
            myers_pairs(&a, &b, &mut Work::new(20_000)).map(|p| p.len()),
            Some(1)
        );
    }

    #[test]
    fn whitespace_moved_costs_one_diagonal() {
        // Minified JSON pretty-printed: the text other than whitespace is
        // alike, found along one diagonal; only whitespace stretches differ.
        let old: Vec<char> = "{\"a\":1,\"b\":[1,2]}".chars().collect();
        let new: Vec<char> = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}".chars().collect();
        let mut work = Work::new(usize::MAX);
        let hunks = char_hunks(&old, &new, &mut work).unwrap();
        assert!(hunks.iter().all(|(old_range, new_range)| {
            old[old_range.clone()].iter().all(|c| c.is_whitespace())
                && new[new_range.clone()].iter().all(|c| c.is_whitespace())
        }));
        let mut rebuilt = String::new();
        let mut at = 0;
        for (old_range, new_range) in &hunks {
            rebuilt.extend(&old[at..old_range.start]);
            rebuilt.extend(&new[new_range.clone()]);
            at = old_range.end;
        }
        rebuilt.extend(&old[at..]);
        assert_eq!(rebuilt, new.iter().collect::<String>());
    }

    #[test]
    fn patience_anchors_unique_elements_then_fills_between() {
        let a = ["x", "}", "y", "}", "z"];
        let b = ["x", "}", "w", "}", "z"];
        let found = patience_pairs(&a, &b, true, &mut Work::new(usize::MAX)).unwrap();
        assert_eq!(found, vec![(0, 0), (1, 1), (3, 3), (4, 4)]);
    }
}
