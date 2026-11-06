//! Wasm-specific correspondence: control labels/arms first, instructions second.
//! Hash tables accelerate full typed-key equality; no hash is identity evidence.
//! This is a syntactic correspondence, never a proof of program equivalence.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::ir::{CatchIr, Immediate, Opcode, ParsedOperator};

const WORK_LIMIT: usize = 4 * 1024 * 1024;
const GAP_LIMIT: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlignmentReport {
    pub algorithm: &'static str,
    pub complete: bool,
    pub fallback_reason: Option<&'static str>,
    pub identity_unproven: bool,
    pub regions: Vec<RegionDelta>,
    pub basic_blocks: Vec<BasicBlockDelta>,
    pub branch_targets: Vec<BranchDelta>,
}

/// Direct instruction spans within a paired control arm, split at branch,
/// return, trap and tail-call boundaries. Ranges are half-open; multiple spans
/// avoid attributing intervening nested control regions to this basic block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BasicBlockDelta {
    pub arm: &'static str,
    pub old_control_start: Option<usize>,
    pub new_control_start: Option<usize>,
    pub old_ranges: Vec<[usize; 2]>,
    pub new_ranges: Vec<[usize; 2]>,
    pub old_terminator: Option<usize>,
    pub new_terminator: Option<usize>,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegionSite {
    pub kind: &'static str,
    /// Half-open operator range, including control header and closing end.
    pub start_operator_index: usize,
    pub end_operator_index: usize,
    pub first_offset: u64,
    pub last_offset: u64,
    pub parent_start_operator_index: Option<usize>,
    pub parent_arm: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegionDelta {
    pub old: Option<RegionSite>,
    pub new: Option<RegionSite>,
    pub relation: &'static str,
    pub matching: &'static str,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchDelta {
    pub old_operator_index: usize,
    pub new_operator_index: usize,
    /// br_table targets in order, followed by its default; try_table catches
    /// in declaration order. A simple branch has edge zero.
    pub edge: usize,
    pub edge_role: &'static str,
    pub old_depth: u32,
    pub new_depth: u32,
    pub old_target: Option<RegionSite>,
    pub new_target: Option<RegionSite>,
    pub relation: &'static str,
}

pub(crate) struct Alignment {
    pub old_to_new: Vec<Option<usize>>,
    pub new_used: Vec<bool>,
    pub ambiguous_old: Vec<bool>,
    pub report: AlignmentReport,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Part {
    Op(usize),
    Child(usize),
}

struct Region {
    kind: &'static str,
    start: usize,
    end: usize,
    parent: Option<(usize, usize)>,
    arms: Vec<Vec<Part>>,
    separator: Option<usize>,
}

#[derive(Clone, Copy)]
struct LabelTarget {
    role: &'static str,
    edge: usize,
    depth: u32,
    target: Option<usize>,
}

struct Tree {
    regions: Vec<Region>,
    targets: Vec<Vec<LabelTarget>>,
}

impl Tree {
    fn build(ops: &[ParsedOperator]) -> Option<Self> {
        if ops.is_empty() {
            return None;
        }
        let mut tree = Self {
            regions: vec![Region {
                kind: "function",
                start: 0,
                end: ops.len(),
                parent: None,
                arms: vec![Vec::new()],
                separator: None,
            }],
            targets: vec![Vec::new(); ops.len()],
        };
        let mut stack = vec![(0, 0)];
        for (i, op) in ops.iter().enumerate() {
            let &(parent, arm) = stack.last()?;
            let depths = branch_depths(&op.immediate);
            // try_table catch labels are relative to the enclosing labels,
            // before the new try_table label is introduced.
            tree.targets[i] = depths
                .into_iter()
                .map(|(role, edge, depth)| {
                    let target = stack
                        .len()
                        .checked_sub(depth as usize + 1)
                        .and_then(|p| stack.get(p))
                        .map(|p| p.0);
                    LabelTarget {
                        role,
                        edge,
                        depth,
                        target,
                    }
                })
                .collect();
            let kind = match op.opcode {
                Opcode::Block => Some("block"),
                Opcode::Loop => Some("loop"),
                Opcode::If => Some("if"),
                Opcode::TryTable => Some("try_table"),
                // The parser preserves legacy Try as an unknown opcode.
                // Do not invent label structure for it or future controls.
                Opcode::Unrecognized(_)
                | Opcode::Catch
                | Opcode::CatchAll
                | Opcode::Delegate
                | Opcode::Rethrow => return None,
                _ => None,
            };
            if let Some(kind) = kind {
                let id = tree.regions.len();
                tree.regions[parent].arms[arm].push(Part::Child(id));
                tree.regions.push(Region {
                    kind,
                    start: i,
                    end: ops.len(),
                    parent: Some((parent, arm)),
                    arms: vec![Vec::new()],
                    separator: None,
                });
                stack.push((id, 0));
            } else if op.opcode == Opcode::Else {
                let region = &mut tree.regions[parent];
                if region.kind != "if" || arm != 0 || region.separator.is_some() {
                    return None;
                }
                region.separator = Some(i);
                region.arms.push(Vec::new());
                stack.last_mut()?.1 = 1;
            } else if op.opcode == Opcode::End {
                tree.regions[parent].end = i + 1;
                stack.pop();
                if stack.is_empty() && i + 1 != ops.len() {
                    return None;
                }
            } else {
                tree.regions[parent].arms[arm].push(Part::Op(i));
            }
        }
        stack.is_empty().then_some(tree)
    }

    fn site(&self, id: usize, ops: &[ParsedOperator]) -> RegionSite {
        let r = &self.regions[id];
        RegionSite {
            kind: r.kind,
            start_operator_index: r.start,
            end_operator_index: r.end,
            first_offset: ops[r.start].offset,
            last_offset: ops[r.end - 1].offset,
            parent_start_operator_index: r.parent.map(|(p, _)| self.regions[p].start),
            parent_arm: match r.parent {
                Some((p, 0)) if self.regions[p].kind == "if" => "then",
                Some((p, 1)) if self.regions[p].kind == "if" => "else",
                Some(_) => "body",
                None => "function",
            },
        }
    }
}

fn branch_depths(immediate: &Immediate) -> Vec<(&'static str, usize, u32)> {
    match immediate {
        Immediate::Branch(depth) => vec![("branch", 0, *depth)],
        Immediate::BrOnCast { label, .. } => vec![("branch", 0, *label)],
        Immediate::BrTable {
            targets,
            default_target,
        } => targets
            .iter()
            .enumerate()
            .map(|(i, d)| ("table_target", i, *d))
            .chain(std::iter::once(("table_default", 0, *default_target)))
            .collect(),
        Immediate::TryTableFull { catches, .. } => catches
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    "catch",
                    i,
                    match c {
                        CatchIr::One { label, .. }
                        | CatchIr::OneRef { label, .. }
                        | CatchIr::All { label }
                        | CatchIr::AllRef { label } => *label,
                    },
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

// Calls and label depths can change while their instruction site persists.
// Raw typed operands are compared AFTER alignment, and never suppressed.
#[derive(PartialEq, Eq, Hash)]
struct Token<'a> {
    opcode: &'a Opcode,
    immediate: Option<&'a Immediate>,
}

fn tokens<'a>(old: &'a [ParsedOperator], new: &'a [ParsedOperator]) -> (Vec<usize>, Vec<usize>) {
    let mut intern = HashMap::new();
    let mut encode = |ops: &'a [ParsedOperator]| {
        ops.iter()
            .map(|op| {
                let immediate = match op.immediate {
                    Immediate::Call(_)
                    | Immediate::RefFunc(_)
                    | Immediate::Branch(_)
                    | Immediate::BrTable { .. }
                    | Immediate::BrOnCast { .. } => None,
                    _ => Some(&op.immediate),
                };
                let next = intern.len();
                *intern
                    .entry(Token {
                        opcode: &op.opcode,
                        immediate,
                    })
                    .or_insert(next)
            })
            .collect::<Vec<_>>()
    };
    (encode(old), encode(new))
}

#[derive(PartialEq, Eq, Hash)]
enum SignaturePart {
    Header(usize),
    Op(usize),
    Child(usize),
    Arm,
}

fn signatures(
    tree: &Tree,
    token: &[usize],
    intern: &mut HashMap<Vec<SignaturePart>, usize>,
) -> Vec<usize> {
    let mut ids = vec![0; tree.regions.len()];
    for (id, r) in tree.regions.iter().enumerate().rev() {
        let mut key = vec![SignaturePart::Header(token[r.start])];
        for arm in &r.arms {
            key.push(SignaturePart::Arm);
            for part in arm {
                key.push(match *part {
                    Part::Op(i) => SignaturePart::Op(token[i]),
                    Part::Child(i) => SignaturePart::Child(ids[i]),
                });
            }
        }
        // Function has no header; it is never a child candidate. Header
        // tokens include opcode; shape keys use opcode-only tokens below.
        let next = intern.len();
        ids[id] = *intern.entry(key).or_insert(next);
    }
    ids
}

struct Budget {
    remaining: usize,
    complete: bool,
}

/// Long equal runs and unique anchors are linear. Only bounded gaps use
/// Myers; adversarial repetitive gaps fall back deterministically with an
/// explicit incomplete flag, instead of a wall-clock-dependent deadline.
fn align_sequence(old: &[usize], new: &[usize], budget: &mut Budget) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut pending = vec![(0, old.len(), 0, new.len())];
    while let Some((mut a, mut ae, mut b, mut be)) = pending.pop() {
        while a < ae && b < be && old[a] == new[b] {
            pairs.push((a, b));
            a += 1;
            b += 1;
        }
        while a < ae && b < be && old[ae - 1] == new[be - 1] {
            ae -= 1;
            be -= 1;
            pairs.push((ae, be));
        }
        if a == ae || b == be {
            continue;
        }
        let scan_work = (ae - a).saturating_add(be - b);
        if scan_work > budget.remaining {
            budget.complete = false;
            pairs.extend((a..ae).zip(b..be));
            continue;
        }
        budget.remaining -= scan_work;
        let mut occurrences = HashMap::<usize, (usize, usize, usize, usize)>::new();
        for (i, key) in old.iter().enumerate().take(ae).skip(a) {
            let e = occurrences.entry(*key).or_default();
            e.0 += 1;
            e.1 = i;
        }
        for (i, key) in new.iter().enumerate().take(be).skip(b) {
            let e = occurrences.entry(*key).or_default();
            e.2 += 1;
            e.3 = i;
        }
        let mut anchors: Vec<_> = occurrences
            .values()
            .filter(|e| e.0 == 1 && e.2 == 1)
            .map(|e| (e.1, e.3))
            .collect();
        anchors.sort_unstable();
        let anchors = increasing_pairs(&anchors);
        if !anchors.is_empty() {
            for (x, y) in anchors {
                pending.push((a, x, b, y));
                pairs.push((x, y));
                a = x + 1;
                b = y + 1;
            }
            pending.push((a, ae, b, be));
            continue;
        }
        let work = (ae - a).saturating_add(be - b).saturating_pow(2);
        if work <= GAP_LIMIT && work <= budget.remaining {
            budget.remaining -= work;
            for op in
                similar::capture_diff_slices(similar::Algorithm::Myers, &old[a..ae], &new[b..be])
            {
                match op {
                    similar::DiffOp::Equal {
                        old_index,
                        new_index,
                        len,
                    } => pairs.extend((0..len).map(|i| (a + old_index + i, b + new_index + i))),
                    similar::DiffOp::Replace {
                        old_index,
                        new_index,
                        old_len,
                        new_len,
                    } => pairs.extend(
                        (0..old_len.min(new_len)).map(|i| (a + old_index + i, b + new_index + i)),
                    ),
                    _ => {}
                }
            }
        } else {
            budget.complete = false;
            pairs.extend((a..ae).zip(b..be));
        }
    }
    pairs.sort_unstable();
    pairs
}

fn increasing_pairs(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut tails = Vec::<usize>::new();
    let mut prev = vec![None; pairs.len()];
    for (i, &(_, y)) in pairs.iter().enumerate() {
        let p = tails.partition_point(|&j| pairs[j].1 < y);
        if p > 0 {
            prev[i] = Some(tails[p - 1]);
        }
        if p == tails.len() {
            tails.push(i);
        } else {
            tails[p] = i;
        }
    }
    let mut result = Vec::with_capacity(tails.len());
    let mut next = tails.last().copied();
    while let Some(i) = next {
        result.push(pairs[i]);
        next = prev[i];
    }
    result.reverse();
    result
}

#[derive(Clone, Copy)]
struct RegionPair {
    old: usize,
    new: usize,
    matching: &'static str,
    ambiguous: bool,
    moved: bool,
}

fn is_transfer(op: &Opcode) -> bool {
    matches!(
        op,
        Opcode::Br
            | Opcode::BrIf
            | Opcode::BrTable
            | Opcode::Return
            | Opcode::Unreachable
            | Opcode::ReturnCall
            | Opcode::ReturnCallIndirect
            | Opcode::ReturnCallRef
            | Opcode::Throw
            | Opcode::ThrowRef
            | Opcode::BrOnNull
            | Opcode::BrOnNonNull
            | Opcode::BrOnCast
            | Opcode::BrOnCastFail
    )
}

fn operator_ranges(indices: &[usize]) -> Vec<[usize; 2]> {
    let mut ranges = Vec::<[usize; 2]>::new();
    for &i in indices {
        if let Some(last) = ranges.last_mut().filter(|last| last[1] == i) {
            last[1] += 1;
        } else {
            ranges.push([i, i + 1]);
        }
    }
    ranges
}

struct LinearContext<'a> {
    old: &'a [ParsedOperator],
    new: &'a [ParsedOperator],
    arm: &'static str,
    old_control_start: Option<usize>,
    new_control_start: Option<usize>,
}

impl Alignment {
    fn pair(&mut self, a: usize, b: usize) {
        debug_assert!(self.old_to_new[a].is_none() && !self.new_used[b]);
        self.old_to_new[a] = Some(b);
        self.new_used[b] = true;
    }

    fn linear(
        &mut self,
        a: &[usize],
        b: &[usize],
        x: &[usize],
        y: &[usize],
        context: LinearContext<'_>,
        budget: &mut Budget,
    ) {
        let ac: Vec<_> = a
            .iter()
            .enumerate()
            .filter(|(_, i)| is_transfer(&context.old[**i].opcode))
            .map(|(i, _)| i)
            .collect();
        let bc: Vec<_> = b
            .iter()
            .enumerate()
            .filter(|(_, i)| is_transfer(&context.new[**i].opcode))
            .map(|(i, _)| i)
            .collect();
        let ak: Vec<_> = ac.iter().map(|i| x[a[*i]]).collect();
        let bk: Vec<_> = bc.iter().map(|i| y[b[*i]]).collect();
        let mut ai = 0;
        let mut bi = 0;
        for (ae, be) in align_sequence(&ak, &bk, budget)
            .into_iter()
            .map(|(i, j)| (ac[i], bc[j]))
            .chain(std::iter::once((a.len(), b.len())))
        {
            let aend = (ae + 1).min(a.len());
            let bend = (be + 1).min(b.len());
            let left = &a[ai..aend];
            let right = &b[bi..bend];
            self.linear_inner(left, right, x, y, budget);
            let changed = left.iter().any(|i| {
                self.old_to_new[*i].is_none_or(|j| {
                    context.old[*i].opcode != context.new[j].opcode
                        || context.old[*i].immediate != context.new[j].immediate
                })
            }) || right.iter().any(|j| !self.new_used[*j]);
            if changed {
                self.report.basic_blocks.push(BasicBlockDelta {
                    arm: context.arm,
                    old_control_start: context.old_control_start,
                    new_control_start: context.new_control_start,
                    old_ranges: operator_ranges(left),
                    new_ranges: operator_ranges(right),
                    old_terminator: (ae < a.len()).then(|| a[ae]),
                    new_terminator: (be < b.len()).then(|| b[be]),
                    ambiguous: left.iter().any(|i| self.ambiguous_old[*i]),
                });
            }
            ai = aend;
            bi = bend;
        }
    }

    fn linear_inner(
        &mut self,
        a: &[usize],
        b: &[usize],
        x: &[usize],
        y: &[usize],
        budget: &mut Budget,
    ) {
        let left: Vec<_> = a.iter().map(|i| x[*i]).collect();
        let right: Vec<_> = b.iter().map(|i| y[*i]).collect();
        let mut ambiguous = vec![false; left.len()];
        if left != right {
            let mut counts = HashMap::<usize, (usize, usize, usize, usize)>::new();
            for (i, k) in left.iter().enumerate() {
                let e = counts.entry(*k).or_default();
                e.0 += 1;
                e.1 = i;
            }
            for (i, k) in right.iter().enumerate() {
                let e = counts.entry(*k).or_default();
                e.2 += 1;
                e.3 = i;
            }
            let mut anchors: Vec<_> = counts
                .values()
                .filter(|e| e.0 == 1 && e.2 == 1)
                .map(|e| (e.1, e.3))
                .collect();
            anchors.sort_unstable();
            let mut ai = 0;
            let mut bi = 0;
            for (ae, be) in increasing_pairs(&anchors)
                .into_iter()
                .chain(std::iter::once((left.len(), right.len())))
            {
                let mut local = HashMap::<usize, (usize, usize)>::new();
                for k in &left[ai..ae] {
                    local.entry(*k).or_default().0 += 1;
                }
                for k in &right[bi..be] {
                    local.entry(*k).or_default().1 += 1;
                }
                for i in ai..ae {
                    ambiguous[i] = local.get(&left[i]).is_some_and(|c| c.0 > 1 || c.1 > 1);
                }
                ai = ae + 1;
                bi = be + 1;
            }
        }
        for (i, j) in align_sequence(&left, &right, budget) {
            self.pair(a[i], b[j]);
            self.ambiguous_old[a[i]] = ambiguous[i];
        }
    }
}

// A newly inserted/removed non-loop block can wrap an existing straight-line
// run. Keep its header/end as edits, while comparing the enclosed instructions.
// Never flatten if arms, loops, exception regions, or nested control trees.
fn leaf_block_ops(region: &Region, out: &mut Vec<usize>) {
    if region.kind == "block" && region.arms[0].iter().all(|p| matches!(p, Part::Op(_))) {
        out.extend(region.arms[0].iter().filter_map(|p| {
            if let Part::Op(i) = p {
                Some(*i)
            } else {
                None
            }
        }));
    }
}

pub(crate) fn align<'a>(old: &'a [ParsedOperator], new: &'a [ParsedOperator]) -> Alignment {
    let mut result = Alignment {
        old_to_new: vec![None; old.len()],
        new_used: vec![false; new.len()],
        ambiguous_old: vec![false; old.len()],
        report: AlignmentReport {
            algorithm: "wasm_structured_regions_v1",
            complete: true,
            fallback_reason: None,
            identity_unproven: true,
            regions: Vec::new(),
            basic_blocks: Vec::new(),
            branch_targets: Vec::new(),
        },
    };
    if old.len() == new.len()
        && old
            .iter()
            .zip(new)
            .all(|(a, b)| a.opcode == b.opcode && a.immediate == b.immediate)
    {
        for i in 0..old.len() {
            result.pair(i, i);
        }
        return result;
    }
    let mut budget = Budget {
        remaining: WORK_LIMIT,
        complete: true,
    };
    let (x, y) = tokens(old, new);
    let (Some(a), Some(b)) = (Tree::build(old), Tree::build(new)) else {
        result.linear(
            &(0..old.len()).collect::<Vec<_>>(),
            &(0..new.len()).collect::<Vec<_>>(),
            &x,
            &y,
            LinearContext {
                old,
                new,
                arm: "unstructured",
                old_control_start: None,
                new_control_start: None,
            },
            &mut budget,
        );
        result.ambiguous_old.fill(true);
        for block in &mut result.report.basic_blocks {
            block.ambiguous = true;
        }
        result.report.complete = false;
        result.report.fallback_reason = Some("unsupported_or_malformed_control_structure");
        return result;
    };
    let mut exact_intern = HashMap::new();
    let ax = signatures(&a, &x, &mut exact_intern);
    let bx = signatures(&b, &y, &mut exact_intern);
    let mut opcode_intern = HashMap::new();
    let mut shapes = |ops: &'a [ParsedOperator]| {
        ops.iter()
            .map(|op| {
                let next = opcode_intern.len();
                *opcode_intern.entry(&op.opcode).or_insert(next)
            })
            .collect::<Vec<_>>()
    };
    let (sx, sy) = (shapes(old), shapes(new));
    let mut shape_intern = HashMap::new();
    let ashape = signatures(&a, &sx, &mut shape_intern);
    let bshape = signatures(&b, &sy, &mut shape_intern);
    let mut region_pairs = vec![RegionPair {
        old: 0,
        new: 0,
        matching: "function_role",
        ambiguous: false,
        moved: false,
    }];
    let mut region_map = vec![None; a.regions.len()];
    let mut reverse_map = vec![None; b.regions.len()];
    region_map[0] = Some(0);
    reverse_map[0] = Some(0);
    let mut cursor = 0;
    while cursor < region_pairs.len() {
        let pair = region_pairs[cursor];
        cursor += 1;
        let (ar, br) = (&a.regions[pair.old], &b.regions[pair.new]);
        if pair.old != 0 {
            result.pair(ar.start, br.start);
        }
        result.pair(ar.end - 1, br.end - 1);
        if let (Some(i), Some(j)) = (ar.separator, br.separator) {
            result.pair(i, j);
        }
        for arm in 0..ar.arms.len().min(br.arms.len()) {
            let ac: Vec<_> = ar.arms[arm]
                .iter()
                .filter_map(|p| {
                    if let Part::Child(i) = p {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            let bc: Vec<_> = br.arms[arm]
                .iter()
                .filter_map(|p| {
                    if let Part::Child(i) = p {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            let mut children = Vec::new();
            for (keys_a, keys_b, reason) in [
                (&ax, &bx, "unique_reference_normalized_region"),
                (&ashape, &bshape, "unique_control_shape"),
            ] {
                let mut groups = BTreeMap::<usize, (Vec<usize>, Vec<usize>)>::new();
                for &i in &ac {
                    if region_map[i].is_none() {
                        groups.entry(keys_a[i]).or_default().0.push(i);
                    }
                }
                for &i in &bc {
                    if reverse_map[i].is_none() {
                        groups.entry(keys_b[i]).or_default().1.push(i);
                    }
                }
                for (left, right) in groups.values() {
                    if let ([i], [j]) = (&left[..], &right[..]) {
                        region_map[*i] = Some(*j);
                        reverse_map[*j] = Some(*i);
                        children.push(RegionPair {
                            old: *i,
                            new: *j,
                            matching: reason,
                            ambiguous: pair.ambiguous,
                            moved: false,
                        });
                    }
                }
            }
            let au: Vec<_> = ac
                .iter()
                .copied()
                .filter(|i| region_map[*i].is_none())
                .collect();
            let bu: Vec<_> = bc
                .iter()
                .copied()
                .filter(|i| reverse_map[*i].is_none())
                .collect();
            let ak: Vec<_> = au.iter().map(|i| ashape[*i]).collect();
            let bk: Vec<_> = bu.iter().map(|i| bshape[*i]).collect();
            for (i, j) in align_sequence(&ak, &bk, &mut budget) {
                let (i, j) = (au[i], bu[j]);
                if a.regions[i].kind != b.regions[j].kind {
                    continue;
                }
                region_map[i] = Some(j);
                reverse_map[j] = Some(i);
                children.push(RegionPair {
                    old: i,
                    new: j,
                    matching: "ordered_control_neighborhood",
                    ambiguous: true,
                    moved: false,
                });
            }
            children.sort_by_key(|p| p.old);
            let ordered: Vec<_> = children.iter().map(|p| (p.old, p.new)).collect();
            let stable = increasing_pairs(&ordered);
            for child in &mut children {
                child.moved = stable.binary_search(&(child.old, child.new)).is_err();
            }
            // Stable labels are hard boundaries. Moved children are handled
            // through their own correspondence, never duplicated as op edits.
            let mut ai = 0;
            let mut bi = 0;
            for (stop_a, stop_b) in stable
                .into_iter()
                .map(|(i, j)| (Some(i), Some(j)))
                .chain(std::iter::once((None, None)))
            {
                let mut ao = Vec::new();
                let mut bo = Vec::new();
                while ai < ar.arms[arm].len() {
                    let p = ar.arms[arm][ai];
                    ai += 1;
                    match p {
                        Part::Child(i) if Some(i) == stop_a => break,
                        Part::Op(i) => ao.push(i),
                        Part::Child(i) if region_map[i].is_none() => {
                            leaf_block_ops(&a.regions[i], &mut ao)
                        }
                        _ => {}
                    }
                }
                while bi < br.arms[arm].len() {
                    let p = br.arms[arm][bi];
                    bi += 1;
                    match p {
                        Part::Child(i) if Some(i) == stop_b => break,
                        Part::Op(i) => bo.push(i),
                        Part::Child(i) if reverse_map[i].is_none() => {
                            leaf_block_ops(&b.regions[i], &mut bo)
                        }
                        _ => {}
                    }
                }
                result.linear(
                    &ao,
                    &bo,
                    &x,
                    &y,
                    LinearContext {
                        old,
                        new,
                        arm: if ar.kind == "if" {
                            if arm == 0 {
                                "then"
                            } else {
                                "else"
                            }
                        } else {
                            "body"
                        },
                        old_control_start: Some(ar.start),
                        new_control_start: Some(br.start),
                    },
                    &mut budget,
                );
            }
            region_pairs.extend(children);
        }
        for arm in ar.arms.len().min(br.arms.len())..ar.arms.len().max(br.arms.len()) {
            let direct = |r: &Region| {
                r.arms
                    .get(arm)
                    .into_iter()
                    .flatten()
                    .filter_map(|p| if let Part::Op(i) = p { Some(*i) } else { None })
                    .collect::<Vec<_>>()
            };
            result.linear(
                &direct(ar),
                &direct(br),
                &x,
                &y,
                LinearContext {
                    old,
                    new,
                    arm: if ar.kind == "if" && arm == 1 {
                        "else"
                    } else {
                        "body"
                    },
                    old_control_start: (arm < ar.arms.len()).then_some(ar.start),
                    new_control_start: (arm < br.arms.len()).then_some(br.start),
                },
                &mut budget,
            );
        }
    }
    let mut ambiguous_labels = vec![false; a.regions.len()];
    for pair in &region_pairs {
        ambiguous_labels[pair.old] = pair.ambiguous;
    }
    let mut ambiguous_regions = vec![0i32; old.len() + 1];
    for pair in region_pairs.iter().filter(|p| p.ambiguous) {
        let r = &a.regions[pair.old];
        ambiguous_regions[r.start] += 1;
        ambiguous_regions[r.end] -= 1;
    }
    let mut active = 0;
    for (i, delta) in ambiguous_regions.iter().enumerate().take(old.len()) {
        active += delta;
        result.ambiguous_old[i] |= active > 0;
    }
    let old_changed: Vec<_> = old
        .iter()
        .enumerate()
        .map(|(i, op)| {
            result.old_to_new[i]
                .is_none_or(|j| op.opcode != new[j].opcode || op.immediate != new[j].immediate)
        })
        .collect();
    let mut new_changed = vec![false; new.len()];
    for (i, mapped) in result.old_to_new.iter().enumerate() {
        if let Some(j) = mapped {
            new_changed[*j] = old_changed[i];
        }
    }
    for (i, used) in result.new_used.iter().enumerate() {
        if !used {
            new_changed[i] = true;
        }
    }
    let prefix = |changed: &[bool]| {
        let mut p = Vec::with_capacity(changed.len() + 1);
        p.push(0usize);
        for c in changed {
            p.push(p.last().unwrap() + usize::from(*c));
        }
        p
    };
    let (ap, bp) = (prefix(&old_changed), prefix(&new_changed));
    let moved = region_pairs.iter().any(|p| p.moved);
    for pair in &region_pairs {
        let (ar, br) = (&a.regions[pair.old], &b.regions[pair.new]);
        let direct_changed = |r: &Region, changed: &[bool], root: bool| {
            (!root && changed[r.start])
                || changed[r.end - 1]
                || r.separator.is_some_and(|i| changed[i])
                || r.arms
                    .iter()
                    .flatten()
                    .any(|p| matches!(p, Part::Op(i) if changed[*i]))
        };
        if pair.moved
            || direct_changed(ar, &old_changed, pair.old == 0)
            || direct_changed(br, &new_changed, pair.new == 0)
            || (pair.old == 0 && (moved || ap[ar.end] != 0 || bp[br.end] != 0))
        {
            result.report.regions.push(RegionDelta {
                old: Some(a.site(pair.old, old)),
                new: Some(b.site(pair.new, new)),
                relation: if pair.moved { "moved" } else { "modified" },
                matching: pair.matching,
                ambiguous: pair.ambiguous,
            });
        }
    }
    for (tree, mapping, ops, is_old) in
        [(&a, &region_map, old, true), (&b, &reverse_map, new, false)]
    {
        for (id, r) in tree.regions.iter().enumerate().skip(1) {
            // Report only maximal unmatched subtrees; operator edits still
            // retain every instruction in nested unmatched regions.
            if mapping[id].is_some() || r.parent.is_some_and(|(p, _)| mapping[p].is_none()) {
                continue;
            }
            let site = tree.site(id, ops);
            result.report.regions.push(RegionDelta {
                old: is_old.then_some(site.clone()),
                new: (!is_old).then_some(site),
                relation: if is_old { "deleted" } else { "inserted" },
                matching: "unmatched_control_region",
                ambiguous: false,
            });
        }
    }
    if !budget.complete {
        result.ambiguous_old.fill(true);
        for block in &mut result.report.basic_blocks {
            block.ambiguous = true;
        }
    }
    for (i, mapped) in result.old_to_new.iter().enumerate() {
        let Some(j) = *mapped else {
            continue;
        };
        if old[i].opcode != new[j].opcode {
            continue;
        }
        for old_edge in &a.targets[i] {
            let candidate = if old_edge.role == "table_default" {
                b.targets[j].last()
            } else {
                b.targets[j].get(old_edge.edge)
            };
            let Some(new_edge) =
                candidate.filter(|e| e.role == old_edge.role && e.edge == old_edge.edge)
            else {
                continue;
            };
            let (at, bt, old_depth, new_depth) = (
                old_edge.target,
                new_edge.target,
                old_edge.depth,
                new_edge.depth,
            );
            let relation = match (at, bt) {
                _ if result.ambiguous_old[i] || at.is_some_and(|id| ambiguous_labels[id]) => {
                    "unresolved_label"
                }
                (Some(at), Some(bt)) => match region_map[at] {
                    Some(mapped) if mapped == bt && a.regions[at].kind == b.regions[bt].kind => {
                        "same_label_depth_relocated"
                    }
                    Some(_) => "different_label",
                    None => "unresolved_label",
                },
                _ => "unresolved_label",
            };
            if old_depth == new_depth && relation == "same_label_depth_relocated" {
                continue;
            }
            result.report.branch_targets.push(BranchDelta {
                old_operator_index: i,
                new_operator_index: j,
                edge: old_edge.edge,
                edge_role: old_edge.role,
                old_depth,
                new_depth,
                old_target: at.map(|id| a.site(id, old)),
                new_target: bt.map(|id| b.site(id, new)),
                relation,
            });
        }
    }
    result.report.complete = budget.complete;
    if !budget.complete {
        result.report.fallback_reason = Some("instruction_alignment_work_limit");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operators(body: &str) -> Vec<ParsedOperator> {
        let bytes = wat::parse_str(format!("(module (func (param i32) {body}))")).unwrap();
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .unwrap();
        crate::parse::parse_module(&bytes)
            .unwrap()
            .module
            .functions
            .remove(0)
            .operators
    }

    fn edits(a: &[ParsedOperator], b: &[ParsedOperator], r: &Alignment) -> (usize, usize, usize) {
        let mut replace = 0;
        let mut delete = 0;
        for (i, j) in r.old_to_new.iter().enumerate() {
            if let Some(j) = j {
                replace +=
                    usize::from(a[i].opcode != b[*j].opcode || a[i].immediate != b[*j].immediate);
            } else {
                delete += 1;
            }
        }
        (
            replace,
            r.new_used.iter().filter(|used| !**used).count(),
            delete,
        )
    }

    #[test]
    fn equal_instructions_cannot_cross_then_else_boundaries() {
        let a = operators("local.get 0 if i32.const 7 drop i32.const 8 drop else end");
        let b = operators("local.get 0 if else i32.const 7 drop i32.const 8 drop end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (0, 4, 4));
        assert_eq!(r.old_to_new[6], Some(2)); // The else marker, not its old body.
        assert!(r.report.complete);
    }

    #[test]
    fn unique_sibling_regions_move_without_becoming_four_constant_edits() {
        let a = operators("block i32.const 10 drop i32.const 11 drop end block i32.const 20 drop i32.const 21 drop end");
        let b = operators("block i32.const 20 drop i32.const 21 drop end block i32.const 10 drop i32.const 11 drop end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (0, 0, 0));
        assert_eq!(
            r.report
                .regions
                .iter()
                .filter(|r| r.relation == "moved")
                .count(),
            1
        );
        assert_eq!(r.old_to_new[1], Some(7));
    }

    #[test]
    fn inserted_wrapper_keeps_one_depth_edit_and_reports_same_resolved_label() {
        let a = operators("i32.const 1 drop br 0");
        let b = operators("block i32.const 1 drop br 1 end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (1, 2, 0));
        assert_eq!(r.report.branch_targets.len(), 1);
        assert_eq!(
            r.report.branch_targets[0].relation,
            "same_label_depth_relocated"
        );
        assert_eq!(
            r.report.branch_targets[0].old_target.as_ref().unwrap().kind,
            "function"
        );
    }

    #[test]
    fn unchanged_depth_can_change_the_target_when_a_wrapper_is_inserted() {
        let a = operators("i32.const 1 drop br 0");
        let b = operators("block i32.const 1 drop br 0 end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (0, 2, 0));
        assert_eq!(r.report.branch_targets[0].relation, "different_label");
        assert_eq!(
            r.report.branch_targets[0].new_target.as_ref().unwrap().kind,
            "block"
        );
    }

    #[test]
    fn an_ambiguous_nested_wrapper_cannot_prove_a_branch_retarget() {
        let a = operators("block i32.const 1 drop br 0 end");
        let b = operators("block block i32.const 1 drop br 0 end end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (0, 2, 0));
        assert_eq!(r.report.branch_targets.len(), 1);
        assert_eq!(r.report.branch_targets[0].relation, "unresolved_label");
    }

    #[test]
    fn br_table_reports_each_changed_label_and_keeps_the_default_separate() {
        let a = operators("block block local.get 0 br_table 0 1 2 end end");
        let b = operators("block block local.get 0 br_table 1 0 2 end end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (1, 0, 0));
        assert_eq!(r.report.branch_targets.len(), 2);
        assert_eq!(r.report.branch_targets[0].edge, 0);
        assert_eq!(r.report.branch_targets[1].edge, 1);
        assert!(r
            .report
            .branch_targets
            .iter()
            .all(|b| b.relation == "different_label"));
    }

    #[test]
    fn br_table_default_is_paired_by_role_when_target_count_changes() {
        let a = operators("block block local.get 0 br_table 0 1 end end");
        let b = operators("block block local.get 0 br_table 0 0 2 end end");
        let r = align(&a, &b);
        assert_eq!(r.report.branch_targets.len(), 1);
        assert_eq!(r.report.branch_targets[0].edge_role, "table_default");
        assert_eq!(r.report.branch_targets[0].old_depth, 1);
        assert_eq!(r.report.branch_targets[0].new_depth, 2);
    }

    #[test]
    fn try_table_catches_resolve_before_the_try_label_is_pushed() {
        let a = operators("block try_table (catch_all 0) nop end end");
        let b = operators("block try_table (catch_all 1) nop end end");
        let r = align(&a, &b);
        assert!(r.report.complete);
        assert_eq!(edits(&a, &b, &r), (1, 0, 0));
        let edge = &r.report.branch_targets[0];
        assert_eq!(edge.edge_role, "catch");
        assert_eq!(edge.old_target.as_ref().unwrap().kind, "block");
        assert_eq!(edge.new_target.as_ref().unwrap().kind, "function");
    }

    #[test]
    fn instructions_do_not_cross_an_existing_basic_block_branch_boundary() {
        let a = operators("i32.const 7 drop local.get 0 br_if 0");
        let b = operators("local.get 0 br_if 0 i32.const 7 drop");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (0, 2, 2));
        assert_eq!(r.report.basic_blocks.len(), 2);
        assert_eq!(r.old_to_new[3], Some(1));
    }

    #[test]
    fn duplicate_regions_remain_ambiguous_after_ordered_assignment() {
        let a = operators("block i32.const 1 drop end block i32.const 1 drop end");
        let b = operators("block i32.const 2 drop end block i32.const 2 drop end");
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (2, 0, 0));
        assert!(r
            .report
            .regions
            .iter()
            .filter(|r| r.old.as_ref().unwrap().kind == "block")
            .all(|r| r.ambiguous));
    }

    #[test]
    fn changed_block_does_not_consume_an_inserted_unique_sibling() {
        let a = operators("block i32.const 10 drop end block i64.const 20 drop end");
        let b = operators(
            "block f32.const 30 drop end block i32.const 11 drop end block i64.const 20 drop end",
        );
        let r = align(&a, &b);
        assert_eq!(edits(&a, &b, &r), (1, 4, 0));
        assert_eq!(r.old_to_new[1], Some(5));
    }

    #[test]
    fn deep_control_trees_use_no_recursion_and_report_only_the_smallest_changed_region() {
        let depth = 8192;
        let mut a: Vec<_> = (0..depth)
            .map(|i| ParsedOperator {
                offset: i as u64,
                opcode: Opcode::Block,
                immediate: Immediate::BlockType("Empty".into()),
            })
            .collect();
        a.push(ParsedOperator {
            offset: depth as u64,
            opcode: Opcode::Nop,
            immediate: Immediate::None,
        });
        a.extend((depth + 1..2 * depth + 2).map(|i| ParsedOperator {
            offset: i as u64,
            opcode: Opcode::End,
            immediate: Immediate::None,
        }));
        let mut b = a.clone();
        b[depth].opcode = Opcode::Unreachable;
        let r = align(&a, &b);
        assert!(r.report.complete);
        assert_eq!(edits(&a, &b, &r), (1, 0, 0));
        assert_eq!(r.report.regions.len(), 2);
        assert_eq!(
            r.report.regions[1]
                .old
                .as_ref()
                .unwrap()
                .start_operator_index,
            depth - 1
        );
    }

    #[test]
    fn repetitive_unanchored_work_is_bounded_and_explicitly_incomplete() {
        let a = operators(&"i32.const 1 drop ".repeat(4096));
        let b = operators(&"i64.const 2 drop ".repeat(4096));
        let r = align(&a, &b);
        assert!(!r.report.complete);
        assert_eq!(
            r.report.fallback_reason,
            Some("instruction_alignment_work_limit")
        );
        assert_eq!(edits(&a, &b, &r), (4096, 0, 0));
    }

    #[test]
    fn unsupported_control_grammar_preserves_every_typed_operand_edit() {
        let a = vec![ParsedOperator {
            offset: 0,
            opcode: Opcode::Unrecognized("future_control".into()),
            immediate: Immediate::Unrecognized("a".into()),
        }];
        let mut b = a.clone();
        b[0].immediate = Immediate::Unrecognized("b".into());
        let r = align(&a, &b);
        assert!(!r.report.complete);
        assert_eq!(edits(&a, &b, &r), (1, 0, 0));
        assert!(r.ambiguous_old.iter().all(|v| *v));
        assert!(r.report.basic_blocks.iter().all(|b| b.ambiguous));
    }
}
