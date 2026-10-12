//! Compiled filter expressions and their evaluation.
//!
//! Field references are resolved at compile time to slot indices, so evaluation is a walk over
//! a small tree with no name lookups. The same expressions evaluate against raw JSON slots
//! (record filters) and against aggregate rows (`count by x | where count > 10`) through
//! [`Env`].

use crate::json::{parse_num, str_content};
use crate::query::CmpOp;
use memchr::memmem;
use std::borrow::Cow;
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u8>),
}

#[derive(Debug)]
pub enum Expr {
    Field(usize),
    Lit(Lit),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Regex(Box<Expr>, regex::bytes::Regex, bool),
    In(Box<Expr>, Vec<Lit>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Exists(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Val<'a> {
    Missing,
    Null,
    Bool(bool),
    Num(f64),
    Str(Cow<'a, [u8]>),
    /// Object or array, compared by raw text.
    Json(&'a [u8]),
}

impl<'a> Val<'a> {
    #[inline]
    pub fn from_raw(raw: Option<&'a [u8]>) -> Val<'a> {
        let Some(r) = raw else { return Val::Missing };
        match r[0] {
            b'"' => Val::Str(str_content(r)),
            b'n' => Val::Null,
            b't' => Val::Bool(true),
            b'f' => Val::Bool(false),
            b'{' | b'[' => Val::Json(r),
            _ => parse_num(r).map(Val::Num).unwrap_or(Val::Json(r)),
        }
    }

    fn lit(l: &'a Lit) -> Val<'a> {
        match l {
            Lit::Null => Val::Null,
            Lit::Bool(b) => Val::Bool(*b),
            Lit::Num(n) => Val::Num(*n),
            Lit::Str(s) => Val::Str(Cow::Borrowed(s)),
        }
    }

    fn truthy(&self) -> bool {
        !matches!(self, Val::Missing | Val::Null | Val::Bool(false))
    }
}

pub trait Env<'a> {
    fn get(&self, i: usize) -> Val<'a>;
    fn present(&self, i: usize) -> bool;
}

/// Raw JSON value spans for one record.
pub struct Slots<'s, 'a>(pub &'s [Option<&'a [u8]>]);

impl<'a> Env<'a> for Slots<'_, 'a> {
    #[inline]
    fn get(&self, i: usize) -> Val<'a> {
        Val::from_raw(self.0[i])
    }
    #[inline]
    fn present(&self, i: usize) -> bool {
        self.0[i].is_some()
    }
}

#[inline]
fn value<'a, E: Env<'a>>(e: &'a Expr, env: &E) -> Val<'a> {
    match e {
        Expr::Field(i) => env.get(*i),
        Expr::Lit(l) => Val::lit(l),
        other => Val::Bool(truth(other, env)),
    }
}

pub fn truth<'a, E: Env<'a>>(e: &'a Expr, env: &E) -> bool {
    match e {
        Expr::And(v) => v.iter().all(|x| truth(x, env)),
        Expr::Or(v) => v.iter().any(|x| truth(x, env)),
        Expr::Not(x) => !truth(x, env),
        Expr::Exists(i) => env.present(*i),
        Expr::Cmp(op, a, b) => compare(*op, &value(a, env), &value(b, env)),
        Expr::In(a, lits) => {
            let v = value(a, env);
            lits.iter().any(|l| eq(&v, &Val::lit(l)))
        }
        Expr::Regex(a, re, negate) => {
            let m = match value(a, env) {
                Val::Str(s) => re.is_match(&s),
                _ => false,
            };
            m != *negate
        }
        Expr::Field(_) | Expr::Lit(_) => value(e, env).truthy(),
    }
}

pub fn eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Missing | Val::Null, Val::Missing | Val::Null) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Num(x), Val::Num(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Json(x), Val::Json(y)) => x == y,
        _ => false,
    }
}

fn ord(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Num(x), Val::Num(y)) => x.partial_cmp(y),
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn compare(op: CmpOp, a: &Val, b: &Val) -> bool {
    use CmpOp::*;
    match op {
        Eq => eq(a, b),
        Ne => !eq(a, b),
        Lt => ord(a, b) == Some(Ordering::Less),
        Le => matches!(ord(a, b), Some(Ordering::Less | Ordering::Equal)),
        Gt => ord(a, b) == Some(Ordering::Greater),
        Ge => matches!(ord(a, b), Some(Ordering::Greater | Ordering::Equal)),
        Contains | StartsWith | EndsWith => {
            let (Val::Str(h), Val::Str(n)) = (a, b) else { return false };
            match op {
                Contains => memmem::find(h, n).is_some(),
                StartsWith => h.starts_with(n),
                _ => h.ends_with(n),
            }
        }
        Match | NotMatch => unreachable!("regex ops compile to Expr::Regex"),
    }
}

/// Literal byte strings, at least one of which must appear verbatim in any line the filter can
/// accept. Used to skip lines (or whole stretches of input) with a SIMD substring search before
/// any parsing.
///
/// Soundness: a JSON string's encoded form is its content verbatim except for escapes. Without
/// `\u` escapes the only escaped characters are `"` `\` `/` and controls, so a literal free of
/// those must appear as-is. Callers must still consider lines containing `\u` as candidates.
pub fn required_needles(e: &Expr) -> Option<Vec<Vec<u8>>> {
    fn safe(s: &[u8]) -> bool {
        !s.is_empty() && s.iter().all(|&c| c >= 0x20 && c != b'"' && c != b'\\' && c != b'/')
    }
    fn quoted(pre: bool, s: &[u8], post: bool) -> Vec<u8> {
        let mut v = Vec::with_capacity(s.len() + 2);
        if pre {
            v.push(b'"');
        }
        v.extend_from_slice(s);
        if post {
            v.push(b'"');
        }
        v
    }
    /// Lower is more selective: fewer alternatives first, then longer shortest needle.
    fn cost(set: &[Vec<u8>]) -> (usize, std::cmp::Reverse<usize>) {
        (set.len(), std::cmp::Reverse(set.iter().map(Vec::len).min().unwrap_or(0)))
    }
    match e {
        // Any one conjunct's requirement holds for the whole conjunction.
        Expr::And(v) => v.iter().filter_map(required_needles).min_by_key(|set| cost(set)),
        // Every branch must contribute, and then any of their needles may appear.
        Expr::Or(v) => {
            let mut all = vec![];
            for x in v {
                all.extend(required_needles(x)?);
            }
            all.sort();
            all.dedup();
            Some(all)
        }
        Expr::In(a, lits) if matches!(**a, Expr::Field(_)) && !lits.is_empty() => lits
            .iter()
            .map(|l| match l {
                Lit::Str(s) if safe(s) => Some(quoted(true, s, true)),
                _ => None,
            })
            .collect(),
        Expr::Cmp(op, a, b) => {
            let s = match (&**a, &**b) {
                (Expr::Field(_), Expr::Lit(Lit::Str(s))) => s,
                (Expr::Lit(Lit::Str(s)), Expr::Field(_)) if *op == CmpOp::Eq => s,
                _ => return None,
            };
            if !safe(s) {
                return None;
            }
            let n = match op {
                CmpOp::Eq => quoted(true, s, true),
                CmpOp::Contains => quoted(false, s, false),
                CmpOp::StartsWith => quoted(true, s, false),
                CmpOp::EndsWith => quoted(false, s, true),
                _ => return None,
            };
            Some(vec![n])
        }
        _ => None,
    }
}
