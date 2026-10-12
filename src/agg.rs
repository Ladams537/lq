//! Group-by aggregation. Each worker thread owns an [`AggState`]; states are merged at the end
//! (or kept running in follow mode).

use crate::eval::{Env, Val};
use crate::json::{parse_num, str_content};
use crate::query::AggFunc;
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cmp::Ordering;

#[derive(Debug)]
pub struct AggSpec {
    pub func: AggFunc,
    pub slot: Option<usize>,
    pub name: String,
}

#[derive(Debug)]
pub struct AggPlan {
    /// (column name, slot)
    pub by: Vec<(String, usize)>,
    pub aggs: Vec<AggSpec>,
}

impl AggPlan {
    pub fn columns(&self) -> Vec<String> {
        self.by.iter().map(|(n, _)| n.clone()).chain(self.aggs.iter().map(|a| a.name.clone())).collect()
    }
}

/// An output value: a group key or an aggregate result.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Json(String),
}

impl Cell {
    pub fn from_raw(raw: Option<&[u8]>) -> Cell {
        match Val::from_raw(raw) {
            Val::Missing | Val::Null => Cell::Null,
            Val::Bool(b) => Cell::Bool(b),
            Val::Num(n) => Cell::Num(n),
            Val::Str(s) => Cell::Str(String::from_utf8_lossy(&s).into_owned()),
            Val::Json(j) => Cell::Json(String::from_utf8_lossy(j).into_owned()),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Cell::Null => 0,
            Cell::Bool(_) => 1,
            Cell::Num(_) => 2,
            Cell::Str(_) => 3,
            Cell::Json(_) => 4,
        }
    }

    /// Total order, jq-style: null < bools < numbers < strings < objects/arrays.
    pub fn total_cmp(&self, o: &Cell) -> Ordering {
        match (self, o) {
            (Cell::Bool(a), Cell::Bool(b)) => a.cmp(b),
            (Cell::Num(a), Cell::Num(b)) => a.total_cmp(b),
            (Cell::Str(a), Cell::Str(b)) | (Cell::Json(a), Cell::Json(b)) => a.cmp(b),
            _ => self.rank().cmp(&o.rank()),
        }
    }
}

pub struct Row<'a>(pub &'a [Cell]);

impl<'a> Env<'a> for Row<'a> {
    fn get(&self, i: usize) -> Val<'a> {
        match &self.0[i] {
            Cell::Null => Val::Null,
            Cell::Bool(b) => Val::Bool(*b),
            Cell::Num(n) => Val::Num(*n),
            Cell::Str(s) => Val::Str(Cow::Borrowed(s.as_bytes())),
            Cell::Json(s) => Val::Json(s.as_bytes()),
        }
    }
    fn present(&self, i: usize) -> bool {
        self.0[i] != Cell::Null
    }
}

#[derive(Debug, Clone)]
enum Acc {
    Count(u64),
    Sum(f64),
    Avg(f64, u64),
    Min(Option<f64>),
    Max(Option<f64>),
    Pct(Vec<f64>),
    Distinct(FxHashSet<Box<[u8]>>),
}

impl Acc {
    fn new(f: AggFunc) -> Acc {
        match f {
            AggFunc::Count => Acc::Count(0),
            AggFunc::Sum => Acc::Sum(0.0),
            AggFunc::Avg => Acc::Avg(0.0, 0),
            AggFunc::Min => Acc::Min(None),
            AggFunc::Max => Acc::Max(None),
            AggFunc::Pct(_) => Acc::Pct(vec![]),
            AggFunc::Distinct => Acc::Distinct(FxHashSet::default()),
        }
    }

    #[inline]
    fn update(&mut self, has_arg: bool, raw: Option<&[u8]>, scratch: &mut Vec<u8>) {
        let num = || raw.and_then(parse_num);
        match self {
            Acc::Count(c) => {
                if !has_arg || raw.is_some_and(|r| r[0] != b'n') {
                    *c += 1
                }
            }
            Acc::Sum(s) => {
                if let Some(v) = num() {
                    *s += v
                }
            }
            Acc::Avg(s, n) => {
                if let Some(v) = num() {
                    *s += v;
                    *n += 1;
                }
            }
            Acc::Min(m) => {
                if let Some(v) = num() {
                    *m = Some(m.map_or(v, |m| m.min(v)))
                }
            }
            Acc::Max(m) => {
                if let Some(v) = num() {
                    *m = Some(m.map_or(v, |m| m.max(v)))
                }
            }
            Acc::Pct(vals) => {
                if let Some(v) = num() {
                    vals.push(v)
                }
            }
            Acc::Distinct(set) => {
                if raw.is_some() {
                    scratch.clear();
                    push_key(scratch, raw);
                    if !set.contains(&scratch[..]) {
                        set.insert(scratch[..].into());
                    }
                }
            }
        }
    }

    fn merge(&mut self, o: Acc) {
        match (self, o) {
            (Acc::Count(a), Acc::Count(b)) => *a += b,
            (Acc::Sum(a), Acc::Sum(b)) => *a += b,
            (Acc::Avg(a, n), Acc::Avg(b, m)) => {
                *a += b;
                *n += m;
            }
            (Acc::Min(a), Acc::Min(b)) => *a = opt2(*a, b, f64::min),
            (Acc::Max(a), Acc::Max(b)) => *a = opt2(*a, b, f64::max),
            (Acc::Pct(a), Acc::Pct(b)) => a.extend(b),
            (Acc::Distinct(a), Acc::Distinct(b)) => a.extend(b),
            _ => unreachable!("mismatched accumulators"),
        }
    }

    /// Takes `&mut self` so percentiles can select in place: re-rendering a live table must not
    /// copy every collected value.
    fn finish(&mut self, f: AggFunc) -> Cell {
        let num = |v: Option<f64>| v.map_or(Cell::Null, Cell::Num);
        match self {
            Acc::Count(c) => Cell::Num(*c as f64),
            Acc::Sum(s) => Cell::Num(*s),
            Acc::Avg(s, n) => num((*n > 0).then(|| *s / *n as f64)),
            Acc::Min(m) | Acc::Max(m) => num(*m),
            Acc::Distinct(s) => Cell::Num(s.len() as f64),
            Acc::Pct(vals) => {
                let AggFunc::Pct(q) = f else { unreachable!() };
                if vals.is_empty() {
                    return Cell::Null;
                }
                // Nearest-rank percentile.
                let rank = ((q / 100.0) * vals.len() as f64).ceil() as usize;
                let idx = rank.clamp(1, vals.len()) - 1;
                let (_, x, _) = vals.select_nth_unstable_by(idx, f64::total_cmp);
                Cell::Num(*x)
            }
        }
    }
}

fn opt2(a: Option<f64>, b: Option<f64>, f: fn(f64, f64) -> f64) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(f(x, y)),
        (x, y) => x.or(y),
    }
}

/// Appends a self-delimiting encoding of a value: strings are compared by decoded content,
/// everything else by raw text.
#[inline]
fn push_key(buf: &mut Vec<u8>, raw: Option<&[u8]>) {
    match raw {
        None => buf.push(0),
        Some(r) if r[0] == b'n' => buf.push(0),
        Some(r) => {
            let (tag, body) = if r[0] == b'"' { (1, str_content(r)) } else { (2, Cow::Borrowed(r)) };
            buf.push(tag);
            buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
            buf.extend_from_slice(&body);
        }
    }
}

fn decode_key(mut k: &[u8], out: &mut Vec<Cell>) {
    while let Some((&tag, rest)) = k.split_first() {
        if tag == 0 {
            out.push(Cell::Null);
            k = rest;
            continue;
        }
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        let body = &rest[4..4 + len];
        k = &rest[4 + len..];
        out.push(if tag == 1 {
            Cell::Str(String::from_utf8_lossy(body).into_owned())
        } else {
            Cell::from_raw(Some(body))
        });
    }
}

pub struct AggState {
    groups: FxHashMap<Box<[u8]>, Vec<Acc>>,
    key: Vec<u8>,
    scratch: Vec<u8>,
}

impl AggState {
    pub fn new(plan: &AggPlan) -> AggState {
        let mut groups = FxHashMap::default();
        if plan.by.is_empty() {
            // A global aggregate always yields exactly one row, even over no input.
            groups.insert(Box::default(), plan.aggs.iter().map(|a| Acc::new(a.func)).collect());
        }
        AggState { groups, key: vec![], scratch: vec![] }
    }

    #[inline]
    pub fn update(&mut self, plan: &AggPlan, slots: &[Option<&[u8]>]) {
        self.key.clear();
        for &(_, s) in &plan.by {
            push_key(&mut self.key, slots[s]);
        }
        let accs = match self.groups.get_mut(&self.key[..]) {
            Some(a) => a,
            None => self
                .groups
                .entry(self.key[..].into())
                .or_insert_with(|| plan.aggs.iter().map(|a| Acc::new(a.func)).collect()),
        };
        for (acc, spec) in accs.iter_mut().zip(&plan.aggs) {
            acc.update(spec.slot.is_some(), spec.slot.and_then(|s| slots[s]), &mut self.scratch);
        }
    }

    pub fn merge(&mut self, other: AggState) {
        for (k, accs) in other.groups {
            match self.groups.get_mut(&k) {
                Some(mine) => mine.iter_mut().zip(accs).for_each(|(a, b)| a.merge(b)),
                None => {
                    self.groups.insert(k, accs);
                }
            }
        }
    }

    /// Rows sorted by group key.
    pub fn rows(&mut self, plan: &AggPlan) -> Vec<Vec<Cell>> {
        let mut rows: Vec<Vec<Cell>> = self
            .groups
            .iter_mut()
            .map(|(k, accs)| {
                let mut row = Vec::with_capacity(plan.by.len() + accs.len());
                decode_key(k, &mut row);
                row.extend(accs.iter_mut().zip(&plan.aggs).map(|(a, s)| a.finish(s.func)));
                row
            })
            .collect();
        let nk = plan.by.len();
        rows.sort_by(|a, b| {
            a[..nk].iter().zip(&b[..nk]).map(|(x, y)| x.total_cmp(y)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
        });
        rows
    }
}
