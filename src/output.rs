//! Rendering of records and result tables.

use crate::agg::Cell;
use crate::json::{str_content, write_str};
use crate::plan::{Format, Plan};

pub fn fmt_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

fn short_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    let s = format!("{f:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "0" || s == "-0" {
        format!("{f:.3e}")
    } else {
        s.to_string()
    }
}

/// Renders one matched record into `out` (newline-terminated).
#[inline]
pub fn record(plan: &Plan, line: &[u8], slots: &[Option<&[u8]>], out: &mut Vec<u8>) {
    let Some(fields) = &plan.project else {
        out.extend_from_slice(line);
        out.push(b'\n');
        return;
    };
    match plan.format {
        Format::Json | Format::Auto => {
            for (_, slot, prefix) in fields {
                out.extend_from_slice(prefix);
                out.extend_from_slice(slots[*slot].unwrap_or(b"null"));
            }
            if fields.is_empty() {
                out.push(b'{');
            }
            out.extend_from_slice(b"}\n");
        }
        Format::Tsv | Format::Table => {
            for (i, (_, slot, _)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(b'\t');
                }
                tsv_field(out, slots[*slot]);
            }
            out.push(b'\n');
        }
        Format::Csv => {
            for (i, (_, slot, _)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                csv_field(out, &raw_text(slots[*slot]));
            }
            out.push(b'\n');
        }
    }
}

/// Text of a value for delimited output: strings decoded, null/missing empty, others raw.
fn raw_text(raw: Option<&[u8]>) -> std::borrow::Cow<'_, [u8]> {
    match raw {
        None => b"".as_slice().into(),
        Some(r) if r[0] == b'"' => str_content(r),
        Some(b"null") => b"".as_slice().into(),
        Some(r) => r.into(),
    }
}

fn tsv_field(out: &mut Vec<u8>, raw: Option<&[u8]>) {
    tsv_escape(out, &raw_text(raw));
}

/// TSV has no quoting, so tab, newline, CR and backslash are written as backslash escapes.
fn tsv_escape(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        match c {
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            c => out.push(c),
        }
    }
}

fn csv_field(out: &mut Vec<u8>, s: &[u8]) {
    if s.iter().any(|&c| matches!(c, b',' | b'"' | b'\n' | b'\r')) {
        out.push(b'"');
        for &c in s {
            if c == b'"' {
                out.push(b'"');
            }
            out.push(c);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(s);
    }
}

/// Header line for delimited record output, if any.
pub fn record_header(plan: &Plan) -> Option<Vec<u8>> {
    let fields = plan.project.as_ref()?;
    let sep = match plan.format {
        Format::Tsv | Format::Table => b'\t',
        Format::Csv => b',',
        _ => return None,
    };
    let mut out = vec![];
    for (i, (name, _, _)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        out.extend_from_slice(name.as_bytes());
    }
    out.push(b'\n');
    Some(out)
}

fn cell_json(out: &mut Vec<u8>, c: &Cell) {
    match c {
        Cell::Null => out.extend_from_slice(b"null"),
        Cell::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Cell::Num(n) if n.is_finite() => out.extend_from_slice(fmt_num(*n).as_bytes()),
        Cell::Num(_) => out.extend_from_slice(b"null"),
        Cell::Str(s) => write_str(out, s.as_bytes()),
        Cell::Json(s) => out.extend_from_slice(s.as_bytes()),
    }
}

fn cell_text(c: &Cell, pretty: bool) -> String {
    match c {
        Cell::Null => if pretty { "-" } else { "" }.to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Num(n) if pretty => short_num(*n),
        Cell::Num(n) => fmt_num(*n),
        Cell::Str(s) | Cell::Json(s) => s.clone(),
    }
}

/// Renders a result table in the requested format.
pub fn table(cols: &[String], rows: &[Vec<Cell>], format: Format) -> Vec<u8> {
    let mut out = vec![];
    match format {
        Format::Json | Format::Auto => {
            for row in rows {
                for (i, (c, v)) in cols.iter().zip(row).enumerate() {
                    out.push(if i == 0 { b'{' } else { b',' });
                    write_str(&mut out, c.as_bytes());
                    out.push(b':');
                    cell_json(&mut out, v);
                }
                out.extend_from_slice(b"}\n");
            }
        }
        Format::Tsv | Format::Csv => {
            let sep = if format == Format::Tsv { b'\t' } else { b',' };
            let line = |out: &mut Vec<u8>, cells: &mut dyn Iterator<Item = String>| {
                for (i, s) in cells.enumerate() {
                    if i > 0 {
                        out.push(sep);
                    }
                    if sep == b',' {
                        csv_field(out, s.as_bytes());
                    } else {
                        tsv_escape(out, s.as_bytes());
                    }
                }
                out.push(b'\n');
            };
            line(&mut out, &mut cols.iter().cloned());
            for row in rows {
                line(&mut out, &mut row.iter().map(|c| cell_text(c, false)));
            }
        }
        Format::Table => {
            let text: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| cell_text(c, true).replace('\n', "\\n")).collect()).collect();
            let numeric: Vec<bool> = (0..cols.len()).map(|i| rows.iter().all(|r| matches!(r[i], Cell::Num(_) | Cell::Null))).collect();
            aligned(&mut out, cols, &text, &numeric);
        }
    }
    out
}

/// Column-aligned text; numeric columns are right-aligned.
fn aligned(out: &mut Vec<u8>, cols: &[String], text: &[Vec<String>], numeric: &[bool]) {
    let width: Vec<usize> = (0..cols.len())
        .map(|i| text.iter().map(|r| r[i].chars().count()).chain([cols[i].chars().count()]).max().unwrap_or(0))
        .collect();
    let mut line = |cells: &[String]| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            let pad = width[i] - c.chars().count();
            let last = i + 1 == cells.len();
            if numeric[i] {
                s.extend(std::iter::repeat_n(' ', pad));
                s.push_str(c);
            } else {
                s.push_str(c);
                if !last {
                    s.extend(std::iter::repeat_n(' ', pad));
                }
            }
            if !last {
                s.push_str("  ");
            }
        }
        out.extend_from_slice(s.trim_end().as_bytes());
        out.push(b'\n');
    };
    line(cols);
    for r in text {
        line(r);
    }
}

/// Aligns buffered TSV record output (`-o table` without aggregation). Columns whose values
/// all parse as numbers (or are empty) are right-aligned; values keep their original text.
pub fn align_tsv(tsv: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(tsv);
    let mut lines = text.lines().map(|l| l.split('\t').map(str::to_string).collect::<Vec<_>>());
    let Some(cols) = lines.next() else { return vec![] };
    let rows: Vec<Vec<String>> = lines.collect();
    let numeric: Vec<bool> = (0..cols.len())
        .map(|i| rows.iter().all(|r| r.get(i).is_none_or(|c| c.is_empty() || c.parse::<f64>().is_ok())))
        .collect();
    let mut out = vec![];
    aligned(&mut out, &cols, &rows, &numeric);
    out
}
