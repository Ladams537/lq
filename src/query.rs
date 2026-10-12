//! Query language: a `|`-separated pipeline of stages.
//!
//! ```text
//! level == "error" and status >= 500 | count by service | sort count desc | head 5
//! ```
//!
//! See `HELP` for the full grammar.

use anyhow::{anyhow, bail, Result};
use std::fmt;

pub const HELP: &str = r#"QUERY LANGUAGE
  A query is a pipeline of stages separated by '|'. A stage that is not a keyword is a filter.

  Filters       level == "error" and (status >= 500 or msg contains "timeout")
                http.method in ["POST", "PUT"]      not exists(user_id)
                msg =~ "conn(ection)? refused"      path startswith "/api/"
                ops: == != < <= > >= =~ !~ contains startswith endswith in and or not
                missing fields compare as null; < > only compare number/number or string/string
  Fields        level   .level   http.status   tags[0]   ["@timestamp"]   a.b["key with space"]

  Stages
    where EXPR                      filter (the keyword is optional)
    select a, b.c as x              project fields (JSON by default; -o tsv|csv|table)
    count [by a, b]                 count records (per group)
    stats AGG, ... [by a, b]        AGG: count() count(x) sum(x) avg(x) min(x) max(x)
                                         median(x) p50..p99.9(x) distinct(x)   [as name]
    top N a[, b]                    = count by a | sort count desc | head N
    sort a [desc], b ...            sort records or aggregate rows
    head N                          first N records / rows (alias: limit)
    after an aggregation, 'where' filters rows by column name, e.g.
      count by service | where count > 1000 | sort count desc

EXAMPLES
  lq 'level == "error"' app.log
  lq 'status >= 500 | select ts, service, msg' app.log
  lq 'stats count(), avg(latency_ms), p99(latency_ms) by service' app.log
  lq -f 'level == "error" | top 10 service' app.log      # live, re-rendered every second
  kubectl logs -f deploy/api | lq 'latency_ms > 1000'
"#;

#[derive(Debug, Clone, PartialEq)]
pub enum PSeg {
    Key(String),
    Idx(usize),
    /// An aggregate column referenced by function, e.g. `avg(latency_ms)` (canonical name).
    Col(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Path(pub Vec<PSeg>);

fn is_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && is_ident_start(b[0]) && b.iter().all(|&c| is_ident_char(c))
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, s) in self.0.iter().enumerate() {
            match s {
                PSeg::Key(k) if is_ident(k) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(k)?;
                }
                PSeg::Key(k) => write!(f, "[{k:?}]")?,
                PSeg::Idx(n) => write!(f, "[{n}]")?,
                PSeg::Col(c) => f.write_str(c)?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Match,
    NotMatch,
    Contains,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ast {
    Path(Path),
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
    Cmp(CmpOp, Box<Ast>, Box<Ast>),
    And(Box<Ast>, Box<Ast>),
    Or(Box<Ast>, Box<Ast>),
    Not(Box<Ast>),
    In(Box<Ast>, Vec<Ast>),
    Exists(Path),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Pct(f64),
    Distinct,
}

pub fn agg_func(name: &str) -> Option<AggFunc> {
    Some(match name {
        "count" => AggFunc::Count,
        "sum" => AggFunc::Sum,
        "avg" | "mean" => AggFunc::Avg,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        "median" => AggFunc::Pct(50.0),
        "distinct" | "count_distinct" | "dcount" => AggFunc::Distinct,
        n => {
            let q: f64 = n.strip_prefix('p')?.replace('_', ".").parse().ok()?;
            if !(q > 0.0 && q <= 100.0) {
                return None;
            }
            AggFunc::Pct(q)
        }
    })
}

/// Canonical column name of an aggregate, so `median(x)` and `p50(x)` name the same column.
pub fn agg_name(func: AggFunc, arg: Option<&Path>) -> String {
    let f = match func {
        AggFunc::Count => "count".to_string(),
        AggFunc::Sum => "sum".into(),
        AggFunc::Avg => "avg".into(),
        AggFunc::Min => "min".into(),
        AggFunc::Max => "max".into(),
        AggFunc::Pct(q) => format!("p{q}"),
        AggFunc::Distinct => "distinct".into(),
    };
    match arg {
        Some(a) => format!("{f}({a})"),
        None => f,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggCall {
    pub func: AggFunc,
    pub arg: Option<Path>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Where(Ast),
    Select(Vec<(Path, Option<String>)>),
    Agg { aggs: Vec<AggCall>, by: Vec<Path> },
    Top(usize, Vec<Path>),
    Sort(Vec<(Path, bool)>),
    Head(usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    Op(&'static str),
    LParen,
    RParen,
    LBrack,
    RBrack,
    Comma,
    Pipe,
    Dot,
    Eof,
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c == b'$' || c >= 0x80
}
fn is_ident_char(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'-'
}

fn lex(src: &str) -> Result<Vec<(Tok, usize)>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = vec![];
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let next = b.get(i + 1).copied();
        let (tok, len) = match c {
            b'(' => (Tok::LParen, 1),
            b')' => (Tok::RParen, 1),
            b'[' => (Tok::LBrack, 1),
            b']' => (Tok::RBrack, 1),
            b',' => (Tok::Comma, 1),
            b'.' => (Tok::Dot, 1),
            b'|' if next == Some(b'|') => (Tok::Op("||"), 2),
            b'|' => (Tok::Pipe, 1),
            b'&' if next == Some(b'&') => (Tok::Op("&&"), 2),
            b'=' if next == Some(b'=') => (Tok::Op("=="), 2),
            b'=' if next == Some(b'~') => (Tok::Op("=~"), 2),
            b'=' => (Tok::Op("=="), 1),
            b'!' if next == Some(b'=') => (Tok::Op("!="), 2),
            b'!' if next == Some(b'~') => (Tok::Op("!~"), 2),
            b'!' => (Tok::Op("!"), 1),
            b'<' if next == Some(b'=') => (Tok::Op("<="), 2),
            b'<' => (Tok::Op("<"), 1),
            b'>' if next == Some(b'=') => (Tok::Op(">="), 2),
            b'>' => (Tok::Op(">"), 1),
            b'"' | b'\'' => {
                let (s, end) = lex_string(src, i)?;
                out.push((Tok::Str(s), start));
                i = end;
                continue;
            }
            b'-' | b'0'..=b'9' => {
                let mut j = i + (c == b'-') as usize;
                let digits = |j: &mut usize| {
                    let s = *j;
                    while b.get(*j).is_some_and(u8::is_ascii_digit) {
                        *j += 1;
                    }
                    *j > s
                };
                if !digits(&mut j) {
                    bail!("expected a number at position {i}");
                }
                if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                    digits(&mut j);
                }
                if matches!(b.get(j), Some(b'e' | b'E')) {
                    let mut k = j + 1;
                    if matches!(b.get(k), Some(b'+' | b'-')) {
                        k += 1;
                    }
                    if digits(&mut k) {
                        j = k;
                    }
                }
                (Tok::Num(src[i..j].parse()?), j - i)
            }
            c if is_ident_start(c) => {
                let mut j = i;
                while b.get(j).is_some_and(|&c| is_ident_char(c)) {
                    j += 1;
                }
                // Percentile names keep their fraction: `p99.9`, `p99.05`.
                let pct = b[i] == b'p' && j > i + 1 && b[i + 1..j].iter().all(u8::is_ascii_digit);
                if pct && b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                    while b.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                }
                (Tok::Ident(src[i..j].to_string()), j - i)
            }
            _ => bail!("unexpected character {:?} at position {i}", &src[i..].chars().next().unwrap()),
        };
        out.push((tok, start));
        i += len;
    }
    out.push((Tok::Eof, b.len()));
    Ok(out)
}

fn lex_string(src: &str, start: usize) -> Result<(String, usize)> {
    let quote = src.as_bytes()[start] as char;
    let mut out = String::new();
    let mut it = src[start + 1..].char_indices();
    while let Some((i, c)) = it.next() {
        match c {
            c if c == quote => return Ok((out, start + 1 + i + 1)),
            '\\' => match it.next().map(|(_, c)| c) {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    let hex: String = (0..4).filter_map(|_| it.next().map(|(_, c)| c)).collect();
                    let cp = u32::from_str_radix(&hex, 16).map_err(|_| anyhow!("bad \\u escape in string at {start}"))?;
                    out.push(char::from_u32(cp).ok_or_else(|| anyhow!("bad \\u escape in string at {start}"))?);
                }
                Some(c) => out.push(c),
                None => break,
            },
            c => out.push(c),
        }
    }
    bail!("unterminated string starting at position {start}")
}

const STAGE_KWS: &[&str] = &["where", "select", "count", "stats", "top", "sort", "head", "limit"];
const INFIX_KWS: &[&str] = &["and", "or", "in", "contains", "startswith", "endswith", "matches"];

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

pub fn parse(src: &str) -> Result<Vec<Stage>> {
    let mut p = Parser { toks: lex(src)?, pos: 0 };
    let mut stages = vec![];
    if *p.peek() == Tok::Eof {
        return Ok(stages);
    }
    loop {
        stages.push(p.stage()?);
        match p.bump() {
            Tok::Pipe => {}
            Tok::Eof => return Ok(stages),
            t => return Err(p.err_at(p.pos - 1, &format!("expected '|' or end of query, found {t:?}"))),
        }
    }
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].0
    }
    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.pos + k).min(self.toks.len() - 1)].0
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].0.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn err_at(&self, pos: usize, msg: &str) -> anyhow::Error {
        anyhow!("query error at position {}: {msg}", self.toks[pos].1)
    }
    fn err(&self, msg: &str) -> anyhow::Error {
        self.err_at(self.pos, msg)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == kw)
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        let y = self.is_kw(kw);
        if y {
            self.bump();
        }
        y
    }
    fn eat(&mut self, t: &Tok) -> bool {
        let y = self.peek() == t;
        if y {
            self.bump();
        }
        y
    }
    fn expect(&mut self, t: Tok) -> Result<()> {
        if self.eat(&t) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {t:?}, found {:?}", self.peek())))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.bump() {
            Tok::Ident(s) => Ok(s),
            t => Err(self.err_at(self.pos - 1, &format!("expected a name, found {t:?}"))),
        }
    }
    fn usize(&mut self) -> Result<usize> {
        match self.bump() {
            Tok::Num(n) if n >= 0.0 && n.fract() == 0.0 => Ok(n as usize),
            t => Err(self.err_at(self.pos - 1, &format!("expected a non-negative integer, found {t:?}"))),
        }
    }

    /// A stage keyword followed by an operator is a field reference, e.g. `count > 3`.
    fn at_stage_kw(&self) -> Option<String> {
        let Tok::Ident(k) = self.peek() else { return None };
        if !STAGE_KWS.contains(&k.as_str()) {
            return None;
        }
        if k == "where" {
            return Some(k.clone());
        }
        match self.peek_at(1) {
            Tok::Op(_) | Tok::Dot | Tok::LBrack | Tok::LParen => None,
            Tok::Ident(x) if INFIX_KWS.contains(&x.as_str()) => None,
            _ => Some(k.clone()),
        }
    }

    fn stage(&mut self) -> Result<Stage> {
        let Some(kw) = self.at_stage_kw() else {
            return Ok(Stage::Where(self.expr()?));
        };
        self.bump();
        Ok(match kw.as_str() {
            "where" => Stage::Where(self.expr()?),
            "select" => Stage::Select(self.list(|p| {
                let path = p.path()?;
                let alias = if p.eat_kw("as") { Some(p.ident()?) } else { None };
                Ok((path, alias))
            })?),
            "count" => {
                let by = if self.eat_kw("by") { self.list(Parser::path)? } else { vec![] };
                Stage::Agg { aggs: vec![AggCall { func: AggFunc::Count, arg: None, name: "count".into() }], by }
            }
            "stats" => {
                let aggs = self.list(Parser::agg)?;
                let by = if self.eat_kw("by") { self.list(Parser::path)? } else { vec![] };
                Stage::Agg { aggs, by }
            }
            "top" => {
                let n = self.usize()?;
                Stage::Top(n, self.list(Parser::path)?)
            }
            "sort" => Stage::Sort(self.list(|p| {
                let path = p.path()?;
                let desc = if p.eat_kw("desc") {
                    true
                } else {
                    p.eat_kw("asc");
                    false
                };
                Ok((path, desc))
            })?),
            "head" | "limit" => Stage::Head(self.usize()?),
            _ => unreachable!(),
        })
    }

    fn list<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let mut v = vec![f(self)?];
        while self.eat(&Tok::Comma) {
            v.push(f(self)?);
        }
        Ok(v)
    }

    /// `func(arg)` / `count` / `count()`, as written in `stats` or as a column reference.
    fn agg_call(&mut self) -> Result<(AggFunc, Option<Path>)> {
        let start = self.pos;
        let name = self.ident()?;
        let func = agg_func(&name).ok_or_else(|| self.err_at(start, &format!("unknown aggregate '{name}'")))?;
        let mut arg = None;
        if self.eat(&Tok::LParen) && !self.eat(&Tok::RParen) {
            arg = Some(self.path()?);
            self.expect(Tok::RParen)?;
        }
        if arg.is_none() && func != AggFunc::Count {
            return Err(self.err_at(start, &format!("'{name}' needs a field, e.g. {name}(latency_ms)")));
        }
        Ok((func, arg))
    }

    fn agg(&mut self) -> Result<AggCall> {
        let (func, arg) = self.agg_call()?;
        let name = if self.eat_kw("as") { self.ident()? } else { agg_name(func, arg.as_ref()) };
        Ok(AggCall { func, arg, name })
    }

    fn path(&mut self) -> Result<Path> {
        if let (Tok::Ident(name), Tok::LParen) = (self.peek(), self.peek_at(1)) {
            if agg_func(name).is_some() {
                let (func, arg) = self.agg_call()?;
                return Ok(Path(vec![PSeg::Col(agg_name(func, arg.as_ref()))]));
            }
        }
        let mut segs = vec![];
        match self.peek().clone() {
            Tok::Dot => {
                self.bump();
                if let Tok::Ident(s) = self.peek().clone() {
                    self.bump();
                    segs.push(PSeg::Key(s));
                }
            }
            Tok::Ident(s) => {
                self.bump();
                segs.push(PSeg::Key(s));
            }
            Tok::Str(s) => {
                self.bump();
                segs.push(PSeg::Key(s));
            }
            Tok::LBrack => {}
            t => return Err(self.err(&format!("expected a field, found {t:?}"))),
        }
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.bump();
                    match self.bump() {
                        Tok::Ident(s) | Tok::Str(s) => segs.push(PSeg::Key(s)),
                        t => return Err(self.err_at(self.pos - 1, &format!("expected a field name after '.', found {t:?}"))),
                    }
                }
                Tok::LBrack => {
                    self.bump();
                    match self.bump() {
                        Tok::Str(s) => segs.push(PSeg::Key(s)),
                        Tok::Num(n) if n >= 0.0 && n.fract() == 0.0 => segs.push(PSeg::Idx(n as usize)),
                        t => return Err(self.err_at(self.pos - 1, &format!("expected \"key\" or index in [...], found {t:?}"))),
                    }
                    self.expect(Tok::RBrack)?;
                }
                _ => break,
            }
        }
        if segs.is_empty() {
            return Err(self.err("expected a field name"));
        }
        Ok(Path(segs))
    }

    fn expr(&mut self) -> Result<Ast> {
        let mut l = self.and()?;
        while self.eat_kw("or") || self.eat(&Tok::Op("||")) {
            l = Ast::Or(Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Ast> {
        let mut l = self.unary()?;
        while self.eat_kw("and") || self.eat(&Tok::Op("&&")) {
            l = Ast::And(Box::new(l), Box::new(self.unary()?));
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Ast> {
        if self.eat_kw("not") || self.eat(&Tok::Op("!")) {
            return Ok(Ast::Not(Box::new(self.unary()?)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Ast> {
        let l = self.primary()?;
        let op = match self.peek() {
            Tok::Op("==") => CmpOp::Eq,
            Tok::Op("!=") => CmpOp::Ne,
            Tok::Op("<") => CmpOp::Lt,
            Tok::Op("<=") => CmpOp::Le,
            Tok::Op(">") => CmpOp::Gt,
            Tok::Op(">=") => CmpOp::Ge,
            Tok::Op("=~") => CmpOp::Match,
            Tok::Op("!~") => CmpOp::NotMatch,
            Tok::Ident(k) if k == "contains" => CmpOp::Contains,
            Tok::Ident(k) if k == "startswith" => CmpOp::StartsWith,
            Tok::Ident(k) if k == "endswith" => CmpOp::EndsWith,
            Tok::Ident(k) if k == "matches" => CmpOp::Match,
            Tok::Ident(k) if k == "in" => {
                self.bump();
                let close = if self.eat(&Tok::LBrack) {
                    Tok::RBrack
                } else {
                    self.expect(Tok::LParen)?;
                    Tok::RParen
                };
                let items = if self.peek() == &close { vec![] } else { self.list(Parser::primary)? };
                self.expect(close)?;
                return Ok(Ast::In(Box::new(l), items));
            }
            _ => return Ok(l),
        };
        self.bump();
        let r = self.primary()?;
        Ok(Ast::Cmp(op, Box::new(l), Box::new(r)))
    }

    fn primary(&mut self) -> Result<Ast> {
        match self.peek().clone() {
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Ast::Str(s))
            }
            Tok::Num(n) => {
                self.bump();
                Ok(Ast::Num(n))
            }
            Tok::Ident(k) if !matches!(self.peek_at(1), Tok::Dot | Tok::LBrack) && matches!(k.as_str(), "true" | "false" | "null") => {
                self.bump();
                Ok(match k.as_str() {
                    "true" => Ast::Bool(true),
                    "false" => Ast::Bool(false),
                    _ => Ast::Null,
                })
            }
            Tok::Ident(k) if self.peek_at(1) == &Tok::LParen => {
                let start = self.pos;
                self.bump();
                self.bump();
                let r = match k.as_str() {
                    "exists" | "has" => Ast::Exists(self.path()?),
                    "contains" | "startswith" | "endswith" | "matches" | "test" => {
                        let a = self.primary()?;
                        self.expect(Tok::Comma)?;
                        let b = self.primary()?;
                        let op = match k.as_str() {
                            "contains" => CmpOp::Contains,
                            "startswith" => CmpOp::StartsWith,
                            "endswith" => CmpOp::EndsWith,
                            _ => CmpOp::Match,
                        };
                        Ast::Cmp(op, Box::new(a), Box::new(b))
                    }
                    _ if agg_func(&k).is_some() => {
                        self.pos = start;
                        return Ok(Ast::Path(self.path()?));
                    }
                    _ => return Err(self.err_at(start, &format!("unknown function '{k}'"))),
                };
                self.expect(Tok::RParen)?;
                Ok(r)
            }
            Tok::Ident(_) | Tok::Dot | Tok::LBrack => Ok(Ast::Path(self.path()?)),
            t => Err(self.err(&format!("expected a value or field, found {t:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Path {
        Path(s.split('.').map(|k| PSeg::Key(k.into())).collect())
    }

    #[test]
    fn pipeline() {
        let s = parse(r#"level == "error" and status >= 500 | count by service, http.status | sort count desc | head 5"#).unwrap();
        assert_eq!(s.len(), 4);
        assert!(matches!(&s[0], Stage::Where(Ast::And(..))));
        assert_eq!(s[1], Stage::Agg { aggs: vec![AggCall { func: AggFunc::Count, arg: None, name: "count".into() }], by: vec![p("service"), p("http.status")] });
        assert_eq!(s[2], Stage::Sort(vec![(p("count"), true)]));
        assert_eq!(s[3], Stage::Head(5));
    }

    #[test]
    fn keywords_as_fields() {
        let s = parse("count > 3 | select count, sort").unwrap();
        assert!(matches!(&s[0], Stage::Where(Ast::Cmp(CmpOp::Gt, ..))));
        assert_eq!(s[1], Stage::Select(vec![(p("count"), None), (p("sort"), None)]));
    }

    #[test]
    fn paths() {
        let s = parse(r#".a["b c"][2].d == 1"#).unwrap();
        let Stage::Where(Ast::Cmp(_, l, _)) = &s[0] else { panic!() };
        let Ast::Path(path) = &**l else { panic!() };
        assert_eq!(path.to_string(), r#"a["b c"][2].d"#);
    }

    #[test]
    fn stats_names() {
        let s = parse("stats count(), avg(lat) as a, p99.9(lat), p50(x), distinct(u) by svc").unwrap();
        let Stage::Agg { aggs, .. } = &s[0] else { panic!() };
        let names: Vec<_> = aggs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["count", "a", "p99.9(lat)", "p50(x)", "distinct(u)"]);
        assert_eq!(aggs[2].func, AggFunc::Pct(99.9));
    }

    #[test]
    fn in_and_funcs() {
        let s = parse(r#"level in ["a", "b"] and not exists(x) and contains(msg, "t") and m =~ 'r.x'"#).unwrap();
        assert!(matches!(&s[0], Stage::Where(Ast::And(..))));
    }

    #[test]
    fn errors() {
        assert!(parse("a ==").is_err());
        assert!(parse("stats avg").is_err());
        assert!(parse("head x").is_err());
        assert!(parse(r#"a == "x"#).is_err());
        assert!(parse("a == 1 b").is_err());
    }
}
