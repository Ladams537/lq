//! Lazy, allocation-free JSON field extraction.
//!
//! A query only ever needs a handful of fields from each record, so nothing is parsed into a
//! DOM. The needed paths are compiled into a [`Trie`]; one left-to-right pass over the line
//! records the raw byte span of each wanted value, skips everything else structurally, and
//! stops as soon as every wanted value has been found. A value is only decoded (unescaped,
//! number-parsed) when a predicate or aggregate actually looks at it.
//!
//! The scanner checks structure only as far as it walks. A line that is broken *after* the last
//! needed field is not reported as malformed. A duplicated key resolves to its first occurrence
//! as a whole (jq uses the last): later copies of the same key in the same object are skipped,
//! so leaves from two copies are never mixed.

use memchr::{memchr, memchr2};
use std::borrow::Cow;

#[derive(Clone, PartialEq, Eq)]
pub enum Seg {
    Key(Box<[u8]>),
    Idx(usize),
}

impl std::fmt::Debug for Seg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Seg::Key(k) => write!(f, "Key({:?})", String::from_utf8_lossy(k)),
            Seg::Idx(i) => write!(f, "Idx({i})"),
        }
    }
}

#[derive(Debug, Default)]
struct Node {
    slot: Option<usize>,
    children: Vec<(Seg, usize)>,
}

/// Set of field paths to extract; each distinct path gets a slot index.
#[derive(Debug)]
pub struct Trie {
    nodes: Vec<Node>,
    nslots: usize,
}

impl Default for Trie {
    fn default() -> Self {
        Trie { nodes: vec![Node::default()], nslots: 0 }
    }
}

impl Trie {
    pub fn nslots(&self) -> usize {
        self.nslots
    }

    /// Registers a path (deduplicated) and returns its slot.
    pub fn insert(&mut self, path: &[Seg]) -> usize {
        let mut n = 0;
        for seg in path {
            n = match self.nodes[n].children.iter().find(|(s, _)| s == seg) {
                Some(&(_, c)) => c,
                None => {
                    let c = self.nodes.len();
                    self.nodes.push(Node::default());
                    self.nodes[n].children.push((seg.clone(), c));
                    c
                }
            };
        }
        if let Some(s) = self.nodes[n].slot {
            return s;
        }
        let s = self.nslots;
        self.nslots += 1;
        self.nodes[n].slot = Some(s);
        s
    }

    /// Fills `out[slot]` with the raw value span of every registered path present in the
    /// object starting at `line[start]` (which must be `{`). `out` must be all `None` on entry.
    /// Returns `false` if the line is malformed in the region that had to be scanned.
    #[inline]
    pub fn extract<'a>(&self, line: &'a [u8], start: usize, out: &mut [Option<&'a [u8]>]) -> bool {
        let mut remaining = self.nslots;
        if remaining == 0 {
            return true;
        }
        self.scan_obj(line, start, 0, out, &mut remaining).is_some()
    }

    fn scan_obj<'a>(
        &self,
        b: &'a [u8],
        i: usize,
        node: usize,
        out: &mut [Option<&'a [u8]>],
        rem: &mut usize,
    ) -> Option<usize> {
        let mut i = skip_ws(b, i + 1);
        if b.get(i) == Some(&b'}') {
            return Some(i + 1);
        }
        // Children (by position) already visited in this object: a repeated key is skipped
        // whole. Nodes with more than 64 children fall back to per-slot first-wins.
        let mut seen = 0u64;
        loop {
            if b.get(i) != Some(&b'"') {
                return None;
            }
            let (kend, escaped) = scan_string(b, i)?;
            let key = &b[i + 1..kend - 1];
            i = skip_ws(b, kend);
            if b.get(i) != Some(&b':') {
                return None;
            }
            i = skip_ws(b, i + 1);
            let end = match self.child_key(node, key, escaped) {
                Some((pos, _)) if pos < 64 && seen & (1 << pos) != 0 => skip_value(b, i)?,
                Some((pos, c)) => {
                    if pos < 64 {
                        seen |= 1 << pos;
                    }
                    let e = self.visit(b, i, c, out, rem)?;
                    if *rem == 0 {
                        return Some(e);
                    }
                    e
                }
                None => skip_value(b, i)?,
            };
            i = skip_ws(b, end);
            match b.get(i) {
                Some(b',') => i = skip_ws(b, i + 1),
                Some(b'}') => return Some(i + 1),
                _ => return None,
            }
        }
    }

    fn scan_arr<'a>(
        &self,
        b: &'a [u8],
        i: usize,
        node: usize,
        out: &mut [Option<&'a [u8]>],
        rem: &mut usize,
    ) -> Option<usize> {
        let mut i = skip_ws(b, i + 1);
        if b.get(i) == Some(&b']') {
            return Some(i + 1);
        }
        let mut idx = 0;
        loop {
            let child = self.nodes[node]
                .children
                .iter()
                .find(|(s, _)| *s == Seg::Idx(idx))
                .map(|&(_, c)| c);
            let end = match child {
                Some(c) => {
                    let e = self.visit(b, i, c, out, rem)?;
                    if *rem == 0 {
                        return Some(e);
                    }
                    e
                }
                None => skip_value(b, i)?,
            };
            i = skip_ws(b, end);
            match b.get(i) {
                Some(b',') => {
                    i = skip_ws(b, i + 1);
                    idx += 1;
                }
                Some(b']') => return Some(i + 1),
                _ => return None,
            }
        }
    }

    /// Value at `b[i]` matched trie node `c`: capture it and/or descend into it.
    #[inline]
    fn visit<'a>(
        &self,
        b: &'a [u8],
        i: usize,
        c: usize,
        out: &mut [Option<&'a [u8]>],
        rem: &mut usize,
    ) -> Option<usize> {
        let n = &self.nodes[c];
        if let Some(s) = n.slot {
            let e = skip_value(b, i)?;
            if out[s].is_none() {
                out[s] = Some(&b[i..e]);
                *rem -= 1;
            }
            if !n.children.is_empty() && *rem > 0 {
                // Both `a` and `a.b` are wanted: descend within the already-delimited value.
                let sub = &b[..e];
                match b[i] {
                    b'{' => self.scan_obj(sub, i, c, out, rem)?,
                    b'[' => self.scan_arr(sub, i, c, out, rem)?,
                    _ => 0,
                };
            }
            return Some(e);
        }
        match b.get(i)? {
            b'{' => self.scan_obj(b, i, c, out, rem),
            b'[' => self.scan_arr(b, i, c, out, rem),
            _ => skip_value(b, i),
        }
    }

    /// (position among the node's children, child node)
    #[inline]
    fn child_key(&self, node: usize, key: &[u8], escaped: bool) -> Option<(usize, usize)> {
        let ch = &self.nodes[node].children;
        if !escaped {
            // Hot path: compare raw key bytes, no Cow.
            for (p, (s, c)) in ch.iter().enumerate() {
                if let Seg::Key(k) = s {
                    if **k == *key {
                        return Some((p, *c));
                    }
                }
            }
            return None;
        }
        let key = unescape(key);
        ch.iter().position(|(s, _)| matches!(s, Seg::Key(k) if **k == *key)).map(|p| (p, ch[p].1))
    }
}

#[inline(always)]
pub fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while let Some(&c) = b.get(i) {
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// `b[i]` is an opening quote. Returns (index past the closing quote, saw a backslash).
#[inline(always)]
fn scan_string(b: &[u8], i: usize) -> Option<(usize, bool)> {
    let mut j = i + 1;
    let mut escaped = false;
    loop {
        let k = j + memchr2(b'"', b'\\', b.get(j..)?)?;
        if b[k] == b'"' {
            return Some((k + 1, escaped));
        }
        escaped = true;
        j = k + 2;
    }
}

#[inline]
pub fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => scan_string(b, i).map(|(e, _)| e),
        b'{' | b'[' => skip_container(b, i),
        _ => {
            let mut j = i;
            while let Some(&c) = b.get(j) {
                match c {
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
                    _ => j += 1,
                }
            }
            (j > i).then_some(j)
        }
    }
}

fn skip_container(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while let Some(&c) = b.get(j) {
        match c {
            b'"' => {
                j = scan_string(b, j)?.0;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Decoded content of a raw JSON string value (`raw` includes the quotes).
#[inline]
pub fn str_content(raw: &[u8]) -> Cow<'_, [u8]> {
    unescape(&raw[1..raw.len() - 1])
}

pub fn unescape(s: &[u8]) -> Cow<'_, [u8]> {
    if memchr(b'\\', s).is_none() {
        return Cow::Borrowed(s);
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(&e) = s.get(i + 1) else {
            out.push(c);
            break;
        };
        i += 2;
        match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'u' => {
                let Some(cp) = hex4(s, i) else {
                    out.extend_from_slice(b"\\u");
                    continue;
                };
                i += 4;
                let mut ch = char::from_u32(cp);
                if (0xD800..0xDC00).contains(&cp) && s.get(i) == Some(&b'\\') && s.get(i + 1) == Some(&b'u') {
                    if let Some(lo) = hex4(s, i + 2).filter(|lo| (0xDC00..0xE000).contains(lo)) {
                        i += 6;
                        ch = char::from_u32(0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00));
                    }
                }
                let mut buf = [0; 4];
                out.extend_from_slice(ch.unwrap_or('\u{FFFD}').encode_utf8(&mut buf).as_bytes());
            }
            other => out.push(other), // \" \\ \/ and anything unknown
        }
    }
    Cow::Owned(out)
}

fn hex4(s: &[u8], i: usize) -> Option<u32> {
    let h = s.get(i..i + 4)?;
    let mut v = 0u32;
    for &c in h {
        v = v * 16 + (c as char).to_digit(16)?;
    }
    Some(v)
}

/// Parses a raw JSON number. Returns `None` for non-numbers.
#[inline]
pub fn parse_num(raw: &[u8]) -> Option<f64> {
    let (neg, digits) = match *raw.first()? {
        b'-' => (true, &raw[1..]),
        c if c.is_ascii_digit() => (false, raw),
        _ => return None,
    };
    if !digits.is_empty() && digits.len() <= 18 && digits.iter().all(u8::is_ascii_digit) {
        let mut v: u64 = 0;
        for &d in digits {
            v = v * 10 + (d - b'0') as u64;
        }
        let f = v as f64;
        return Some(if neg { -f } else { f });
    }
    std::str::from_utf8(raw).ok()?.parse().ok()
}

/// Writes `s` as a JSON string literal.
pub fn write_str(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    let mut last = 0;
    for (i, &c) in s.iter().enumerate() {
        let esc: &[u8] = match c {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0..=0x1f => {
                out.extend_from_slice(&s[last..i]);
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
                last = i + 1;
                continue;
            }
            _ => continue,
        };
        out.extend_from_slice(&s[last..i]);
        out.extend_from_slice(esc);
        last = i + 1;
    }
    out.extend_from_slice(&s[last..]);
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Seg {
        Seg::Key(s.as_bytes().into())
    }

    fn get<'a>(line: &'a str, paths: &[&[Seg]]) -> Vec<Option<&'a str>> {
        let mut t = Trie::default();
        for p in paths {
            t.insert(p);
        }
        let mut out = vec![None; t.nslots()];
        assert!(t.extract(line.as_bytes(), skip_ws(line.as_bytes(), 0), &mut out));
        out.into_iter().map(|o| o.map(|b| std::str::from_utf8(b).unwrap())).collect()
    }

    #[test]
    fn extracts_top_level_and_nested() {
        let l = r#"{"a":1,"s":"x\"y","o":{"b":[1,{"c":true}],"d":null},"z":[ "q" , 2 ]}"#;
        let r = get(l, &[&[key("a")], &[key("s")], &[key("o"), key("b"), Seg::Idx(1), key("c")], &[key("z"), Seg::Idx(1)], &[key("nope")]]);
        assert_eq!(r, vec![Some("1"), Some(r#""x\"y""#), Some("true"), Some("2"), None]);
    }

    #[test]
    fn parent_and_child_both_wanted() {
        let l = r#"{"o":{"b":2,"c":3},"x":1}"#;
        let r = get(l, &[&[key("o")], &[key("o"), key("c")], &[key("x")]]);
        assert_eq!(r, vec![Some(r#"{"b":2,"c":3}"#), Some("3"), Some("1")]);
    }

    #[test]
    fn escaped_keys_and_whitespace() {
        let l = "  { \"a\\u0062\" : \"v\" , \"c\" :\t[ ] }";
        let r = get(l, &[&[key("ab")], &[key("c")]]);
        assert_eq!(r, vec![Some("\"v\""), Some("[ ]")]);
    }

    #[test]
    fn first_duplicate_wins() {
        assert_eq!(get(r#"{"a":1,"a":2}"#, &[&[key("a")]]), vec![Some("1")]);
        let r = get(r#"{"o":{"a":1},"o":{"b":2}}"#, &[&[key("o"), key("a")], &[key("o"), key("b")]]);
        assert_eq!(r, vec![Some("1"), None]);
    }

    #[test]
    fn malformed() {
        let mut t = Trie::default();
        t.insert(&[key("b")]);
        for l in [r#"{"a":1"#, r#"{"a" 1}"#, r#"{"a":"unterminated}"#, r#"{a:1}"#] {
            let mut out = vec![None; 1];
            assert!(!t.extract(l.as_bytes(), 0, &mut out), "{l}");
        }
    }

    #[test]
    fn unescape_variants() {
        assert_eq!(&*unescape(br#"a\nb\"c\\d\/e"#), b"a\nb\"c\\d/e");
        assert_eq!(&*unescape(b"\\u00e9\\ud83d\\ude00"), "é😀".as_bytes());
        assert_eq!(&*unescape(br#"\ud800x"#), "\u{FFFD}x".as_bytes());
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_num(b"-42"), Some(-42.0));
        assert_eq!(parse_num(b"1.5e3"), Some(1500.0));
        assert_eq!(parse_num(b"123456789012345678901"), Some(123456789012345678901.0));
        assert_eq!(parse_num(b"true"), None);
    }

    #[test]
    fn write_str_roundtrip() {
        let mut o = vec![];
        write_str(&mut o, "a\"b\\c\n\u{1}é".as_bytes());
        let v: String = serde_json::from_slice(&o).unwrap();
        assert_eq!(v, "a\"b\\c\n\u{1}é");
    }

    /// Every prefix and many single-byte mutations of realistic lines: extraction may reject a
    /// line but must never panic or return a span that the value helpers choke on.
    #[test]
    fn mutation_fuzz_never_panics() {
        let seeds = [
            r#"{"ts":"2026","level":"error","msg":"a \"q\" \u00e9 \ud83d\ude00","http":{"status":503,"h":[1,{"x":"y"}]},"tags":["a","b"],"n":-1.5e3,"t":true,"z":null}"#,
            r#"  { "a" : [ [ ] , { } ] , "a\u0062" : "\\" }  "#,
        ];
        let paths: Vec<Vec<Seg>> = vec![
            vec![key("level")],
            vec![key("http"), key("status")],
            vec![key("http"), key("h"), Seg::Idx(1), key("x")],
            vec![key("http")],
            vec![key("tags"), Seg::Idx(1)],
            vec![key("z")],
            vec![key("ab")],
            vec![key("a"), Seg::Idx(0)],
        ];
        let mut t = Trie::default();
        for p in &paths {
            t.insert(p);
        }
        fn check(t: &Trie, line: &[u8]) {
            let mut out = vec![None; t.nslots()];
            let start = skip_ws(line, 0);
            if line.get(start) == Some(&b'{') && t.extract(line, start, &mut out) {
                for v in out.iter().flatten() {
                    let _ = crate::eval::Val::from_raw(Some(v));
                }
            }
        }
        let mut rng = 0x2545F4914F6CDD1Du64;
        let structural = b"{}[]\",: \\u0x1e-";
        for seed in seeds {
            let b = seed.as_bytes();
            for n in 0..=b.len() {
                check(&t, &b[..n]);
            }
            for _ in 0..20_000 {
                let mut m = b.to_vec();
                for _ in 0..1 + (rng % 3) {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let i = (rng as usize) % m.len();
                    match rng % 3 {
                        0 => m[i] = structural[(rng >> 8) as usize % structural.len()],
                        1 => {
                            m.remove(i);
                        }
                        _ => m.insert(i, structural[(rng >> 8) as usize % structural.len()]),
                    }
                }
                check(&t, &m);
            }
        }
    }

    /// Differential test against serde_json over generated documents.
    #[test]
    fn differential_against_serde() {
        use serde_json::{json, Value};
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        fn gen_value(r: &mut dyn FnMut(u64) -> u64, depth: u32) -> Value {
            match r(if depth > 2 { 5 } else { 7 }) {
                0 => Value::Null,
                1 => json!(r(2) == 0),
                2 => json!(r(100000) as i64 - 50000),
                3 => json!((r(100000) as f64) / 7.0),
                4 => json!(["plain", "q\"uote", "back\\slash", "nl\n", "üñí", "\u{1}"][r(6) as usize]),
                5 => Value::Array((0..r(4)).map(|_| gen_value(r, depth + 1)).collect()),
                _ => {
                    let mut m = serde_json::Map::new();
                    for _ in 0..r(5) {
                        m.insert(["a", "b", "c", "k\"q", "é"][r(5) as usize].to_string(), gen_value(r, depth + 1));
                    }
                    Value::Object(m)
                }
            }
        }
        for _ in 0..3000 {
            let mut m = serde_json::Map::new();
            for _ in 0..r_count(&mut rnd) {
                m.insert(["a", "b", "c", "k\"q", "é"][rnd(5) as usize].to_string(), gen_value(&mut rnd, 0));
            }
            let doc = Value::Object(m);
            let line = if rnd(2) == 0 { serde_json::to_string(&doc).unwrap() } else { serde_json::to_string_pretty(&doc).unwrap() };
            let paths: Vec<Vec<Seg>> = vec![
                vec![key("a")],
                vec![key("b"), key("c")],
                vec![key("k\"q")],
                vec![key("c"), Seg::Idx(1)],
                vec![key("é"), key("a"), key("b")],
                vec![key("b")],
            ];
            let mut t = Trie::default();
            for p in &paths {
                t.insert(p);
            }
            let mut out = vec![None; t.nslots()];
            let lb = line.as_bytes();
            assert!(t.extract(lb, skip_ws(lb, 0), &mut out), "{line}");
            for (p, got) in paths.iter().zip(&out) {
                let mut v = Some(&doc);
                for s in p {
                    v = v.and_then(|v| match s {
                        Seg::Key(k) => v.get(std::str::from_utf8(k).unwrap()),
                        Seg::Idx(i) => v.as_array().and_then(|a| a.get(*i)),
                    });
                }
                let got: Option<Value> = got.map(|g| serde_json::from_slice(g).unwrap());
                assert_eq!(got.as_ref(), v, "path {p:?} in {line}");
            }
        }
        fn r_count(r: &mut dyn FnMut(u64) -> u64) -> u64 {
            r(6)
        }
    }
}
