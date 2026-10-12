//! Compiles parsed stages into an execution plan.
//!
//! Stages split into two phases:
//! * the **record phase** runs per line, in parallel: `where` filters (all ANDed together) and
//!   an optional `select`;
//! * then either an **aggregation** (`count`/`stats`/`top`) followed by row operations over its
//!   (small) result table, or record-level `sort`/`head`.

use crate::agg::{AggPlan, AggSpec};
use crate::eval::{required_needles, Expr, Lit};
use crate::json::{write_str, Seg, Trie};
use crate::query::{Ast, CmpOp, PSeg, Path, Stage};
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Raw lines / JSON objects for records, aligned table for aggregates
    Auto,
    Json,
    Table,
    Tsv,
    Csv,
}

#[derive(Debug)]
pub enum RowOp {
    Where(Expr),
    /// (column or sort-key index, descending)
    Sort(Vec<(usize, bool)>),
    Head(usize),
}

#[derive(Debug)]
pub struct Plan {
    pub trie: Trie,
    pub filter: Option<Expr>,
    /// Prefilter: every matching line contains at least one of these (empty = no prefilter).
    pub needles: Vec<Vec<u8>>,
    /// `select` fields: (output name, slot, pre-rendered `"name":` JSON prefix)
    pub project: Projection,
    pub agg: Option<AggPlan>,
    /// Applied to the aggregate table (agg mode) or buffered records (record mode).
    pub ops: Vec<RowOp>,
    /// Record mode: slots captured as sort keys for buffered records.
    pub sort_slots: Vec<usize>,
    pub format: Format,
}

impl Plan {
    /// Record mode with nothing but `head`: stream, stop after N.
    pub fn stream_limit(&self) -> Option<usize> {
        if self.agg.is_some() || self.buffers_records() {
            return None;
        }
        self.ops.iter().map(|op| if let RowOp::Head(n) = op { *n } else { usize::MAX }).min()
    }

    /// Record mode with a `sort`: records must be buffered.
    pub fn buffers_records(&self) -> bool {
        self.agg.is_none() && self.ops.iter().any(|op| matches!(op, RowOp::Sort(_)))
    }
}

fn segs(p: &Path) -> Vec<Seg> {
    p.0.iter()
        .map(|s| match s {
            PSeg::Key(k) => Seg::Key(k.as_bytes().into()),
            PSeg::Idx(i) => Seg::Idx(*i),
            PSeg::Col(_) => unreachable!("column refs are rejected before reaching the trie"),
        })
        .collect()
}

struct Compiler {
    trie: Trie,
    /// Output names of the `select`, once one has run: later stages can only see these.
    selected: Option<Vec<(String, usize)>>,
}

impl Compiler {
    fn slot(&mut self, p: &Path) -> Result<usize> {
        if let [PSeg::Col(c)] = p.0.as_slice() {
            bail!("'{c}' is an aggregate column; it can only be used after an aggregation (count/stats/top)");
        }
        if let Some(sel) = &self.selected {
            let name = p.to_string();
            return sel.iter().find(|(n, _)| *n == name).map(|(_, s)| *s).ok_or_else(|| {
                let names: Vec<_> = sel.iter().map(|(n, _)| n.as_str()).collect();
                anyhow::anyhow!("field '{name}' is not available after 'select' (selected: {})", names.join(", "))
            });
        }
        Ok(self.trie.insert(&segs(p)))
    }
}

pub fn compile_expr(ast: &Ast, resolve: &mut dyn FnMut(&Path) -> Result<usize>) -> Result<Expr> {
    let lit = |a: &Ast| -> Option<Lit> {
        Some(match a {
            Ast::Str(s) => Lit::Str(s.as_bytes().to_vec()),
            Ast::Num(n) => Lit::Num(*n),
            Ast::Bool(b) => Lit::Bool(*b),
            Ast::Null => Lit::Null,
            _ => return None,
        })
    };
    Ok(match ast {
        Ast::Path(p) => Expr::Field(resolve(p)?),
        Ast::Str(_) | Ast::Num(_) | Ast::Bool(_) | Ast::Null => Expr::Lit(lit(ast).unwrap()),
        Ast::Exists(p) => Expr::Exists(resolve(p)?),
        Ast::Not(x) => Expr::Not(Box::new(compile_expr(x, resolve)?)),
        Ast::And(a, b) => {
            let mut v = vec![];
            for x in [a, b] {
                match compile_expr(x, resolve)? {
                    Expr::And(inner) => v.extend(inner),
                    e => v.push(e),
                }
            }
            Expr::And(v)
        }
        Ast::Or(a, b) => {
            let mut v = vec![];
            for x in [a, b] {
                match compile_expr(x, resolve)? {
                    Expr::Or(inner) => v.extend(inner),
                    e => v.push(e),
                }
            }
            Expr::Or(v)
        }
        Ast::In(a, items) => {
            let lits = items.iter().map(|i| lit(i).ok_or_else(|| anyhow::anyhow!("'in' list must contain only literals"))).collect::<Result<_>>()?;
            Expr::In(Box::new(compile_expr(a, resolve)?), lits)
        }
        Ast::Cmp(op @ (CmpOp::Match | CmpOp::NotMatch), a, b) => {
            let Ast::Str(pat) = &**b else { bail!("the right side of =~ must be a string literal regex") };
            let re = regex::bytes::Regex::new(pat).map_err(|e| anyhow::anyhow!("bad regex {pat:?}: {e}"))?;
            Expr::Regex(Box::new(compile_expr(a, resolve)?), re, *op == CmpOp::NotMatch)
        }
        Ast::Cmp(op, a, b) => Expr::Cmp(*op, Box::new(compile_expr(a, resolve)?), Box::new(compile_expr(b, resolve)?)),
    })
}

type Projection = Option<Vec<(String, usize, Vec<u8>)>>;

fn select(c: &mut Compiler, project: &mut Projection, fields: Vec<(Path, Option<String>)>) -> Result<()> {
    if project.is_some() {
        bail!("only one 'select' stage is supported");
    }
    let mut out = vec![];
    for (path, alias) in fields {
        let slot = c.slot(&path)?;
        let name = alias.unwrap_or_else(|| path.to_string());
        let mut prefix = vec![if out.is_empty() { b'{' } else { b',' }];
        write_str(&mut prefix, name.as_bytes());
        prefix.push(b':');
        out.push((name, slot, prefix));
    }
    // Registered after the whole list, so `select a as b, b` reads field b.
    c.selected = Some(out.iter().map(|(n, s, _)| (n.clone(), *s)).collect());
    *project = Some(out);
    Ok(())
}

pub fn compile(stages: Vec<Stage>, format: Format) -> Result<Plan> {
    let mut c = Compiler { trie: Trie::default(), selected: None };
    let mut filters = vec![];
    let mut project = None;
    let mut it = stages.into_iter().peekable();

    while let Some(st) = it.next_if(|s| matches!(s, Stage::Where(_) | Stage::Select(_))) {
        match st {
            Stage::Where(ast) => filters.push(compile_expr(&ast, &mut |p| c.slot(p))?),
            Stage::Select(fields) => select(&mut c, &mut project, fields)?,
            _ => unreachable!(),
        }
    }

    let mut agg = None;
    let mut ops = vec![];
    let mut sort_slots = vec![];
    let mut cols: Vec<String> = vec![];

    let agg_stage = |by: Vec<Path>, aggs: Vec<crate::query::AggCall>, c: &mut Compiler| -> Result<AggPlan> {
        Ok(AggPlan {
            by: by.iter().map(|p| Ok((p.to_string(), c.slot(p)?))).collect::<Result<_>>()?,
            aggs: aggs
                .into_iter()
                .map(|a| Ok(AggSpec { func: a.func, slot: a.arg.as_ref().map(|p| c.slot(p)).transpose()?, name: a.name }))
                .collect::<Result<_>>()?,
        })
    };

    match it.peek() {
        Some(Stage::Agg { .. }) => {
            let Some(Stage::Agg { aggs, by }) = it.next() else { unreachable!() };
            agg = Some(agg_stage(by, aggs, &mut c)?);
        }
        Some(Stage::Top(..)) => {
            let Some(Stage::Top(n, by)) = it.next() else { unreachable!() };
            let count = crate::query::AggCall { func: crate::query::AggFunc::Count, arg: None, name: "count".into() };
            let nby = by.len();
            agg = Some(agg_stage(by, vec![count], &mut c)?);
            ops.push(RowOp::Sort(vec![(nby, true)]));
            ops.push(RowOp::Head(n));
        }
        _ => {}
    }

    if let Some(a) = &agg {
        cols = a.columns();
    }
    let col = |cols: &[String], p: &Path| -> Result<usize> {
        let name = p.to_string();
        cols.iter().position(|c| *c == name).ok_or_else(|| anyhow::anyhow!("unknown column '{name}' after aggregation (columns: {})", cols.join(", ")))
    };

    for st in it {
        match st {
            Stage::Head(n) => ops.push(RowOp::Head(n)),
            Stage::Sort(keys) if agg.is_some() => {
                ops.push(RowOp::Sort(keys.iter().map(|(p, d)| Ok((col(&cols, p)?, *d))).collect::<Result<_>>()?))
            }
            Stage::Sort(keys) => {
                let mut v = vec![];
                for (p, d) in keys {
                    v.push((sort_slots.len(), d));
                    sort_slots.push(c.slot(&p)?);
                }
                ops.push(RowOp::Sort(v));
            }
            Stage::Where(ast) if agg.is_some() => ops.push(RowOp::Where(compile_expr(&ast, &mut |p| col(&cols, p))?)),
            // Projection commutes with sort/head; sort keys were resolved before any alias.
            Stage::Select(fields) if agg.is_none() => select(&mut c, &mut project, fields)?,
            Stage::Where(_) => bail!("a record 'where' must come before 'sort'/'head'"),
            Stage::Select(_) => bail!("'select' must come before an aggregation"),
            Stage::Agg { .. } | Stage::Top(..) => bail!("only one aggregation stage is supported, and it must come before 'sort'/'head'"),
        }
    }

    let filter = match filters.len() {
        0 => None,
        1 => filters.pop(),
        _ => Some(Expr::And(filters)),
    };
    let needles = filter.as_ref().and_then(required_needles).unwrap_or_default();
    if agg.is_none() && project.is_none() && matches!(format, Format::Tsv | Format::Csv | Format::Table) {
        bail!("-o {format:?} needs a 'select' to choose the columns (or an aggregation); records without one are printed as JSON lines");
    }
    let format = match format {
        Format::Auto if agg.is_some() => Format::Table,
        Format::Auto => Format::Json,
        f => f,
    };
    Ok(Plan { trie: c.trie, filter, needles, project, agg, ops, sort_slots, format })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse;

    fn plan(q: &str) -> Result<Plan> {
        compile(parse(q)?, Format::Auto)
    }

    #[test]
    fn needles() {
        let n = |q: &str| -> Vec<String> { plan(q).unwrap().needles.into_iter().map(|n| String::from_utf8(n).unwrap()).collect() };
        assert_eq!(n(r#"level == "error""#), [r#""error""#]);
        assert_eq!(n(r#"status > 3 and msg contains "timeout" and level == "warn""#), ["timeout"]);
        assert_eq!(n(r#"path startswith "abc""#), [r#""abc"#]);
        assert_eq!(n(r#"level == "error" or level == "warn""#), [r#""error""#, r#""warn""#]);
        assert_eq!(n(r#"level in ["error", "warn"] and msg contains "x""#), ["x"], "single needle preferred");
        assert_eq!(n(r#"level in ["error", 5]"#), Vec::<String>::new(), "5 needs no quotes");
        assert_eq!(n(r#"level == "error" or status > 3"#), Vec::<String>::new());
        assert_eq!(n(r#"level != "error""#), Vec::<String>::new());
        assert_eq!(n(r#"url == "a/b""#), Vec::<String>::new(), "'/' may be escaped as \\/");
        assert_eq!(n(r#"msg == "say \"hi\"""#), Vec::<String>::new());
    }

    #[test]
    fn aliases_and_columns() {
        let p = plan("select latency_ms as l, service | where l > 5 | count by service | where count > 2 | sort count desc").unwrap();
        assert!(plan("select service | where level == 1").is_err());
        assert!(plan("avg(x) > 1").is_err());
        assert_eq!(p.trie.nslots(), 2);
        assert_eq!(p.ops.len(), 2);
        assert!(plan("count by a | sort nope").is_err());
        assert!(plan("head 1 | where a").is_err());
        assert!(plan("sort a | head 1 | select a").unwrap().project.is_some());
    }

    #[test]
    fn limits() {
        assert_eq!(plan("a | head 3 | head 2").unwrap().stream_limit(), Some(2));
        assert_eq!(plan("a | sort b | head 2").unwrap().stream_limit(), None);
        assert!(plan("a | sort b").unwrap().buffers_records());
    }
}
