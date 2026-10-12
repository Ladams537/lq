//! Execution: per-line evaluation, parallel chunked scanning with ordered output, and the
//! live-tail loop.

use crate::agg::{AggState, Cell, Row};
use crate::eval::{truth, Slots};
use crate::json::skip_ws;
use crate::output;
use crate::plan::{Format, Plan, RowOp};
use anyhow::Result;
use crossbeam_channel::{bounded, unbounded};
use memchr::{memchr, memchr_iter, memmem, memrchr};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A buffered record (record-mode `sort`): sort keys plus its rendered output.
pub struct Item {
    keys: Vec<Cell>,
    payload: Vec<u8>,
}

#[derive(Default)]
pub struct ChunkOut {
    bytes: Vec<u8>,
    /// End offset in `bytes` of each emitted record. A record is not always one line (a CSV
    /// field may contain a newline), so `head` cuts by these, never by counting newlines.
    ends: Vec<usize>,
    items: Vec<Item>,
}

/// Substring prefilter over one or several literals.
enum Prefilter<'p> {
    One(memmem::Finder<'p>),
    Many(aho_corasick::AhoCorasick),
}

impl Prefilter<'_> {
    #[inline]
    fn find(&self, hay: &[u8]) -> Option<usize> {
        match self {
            Prefilter::One(f) => f.find(hay),
            Prefilter::Many(ac) => ac.find(hay).map(|m| m.start()),
        }
    }
}

#[derive(Default)]
struct Counters {
    matched: u64,
    malformed: u64,
}

pub struct Worker<'p> {
    plan: &'p Plan,
    agg: Option<AggState>,
    n: Counters,
    finder: Option<Prefilter<'p>>,
    unicode_esc: memmem::Finder<'static>,
}

impl<'p> Worker<'p> {
    pub fn new(plan: &'p Plan, prefilter: bool) -> Self {
        Worker {
            plan,
            agg: plan.agg.as_ref().map(AggState::new),
            n: Counters::default(),
            finder: match plan.needles.as_slice() {
                _ if !prefilter => None,
                [] => None,
                [one] => Some(Prefilter::One(memmem::Finder::new(one))),
                many => Some(Prefilter::Many(aho_corasick::AhoCorasick::new(many).expect("literal needles"))),
            },
            unicode_esc: memmem::Finder::new(b"\\u"),
        }
    }

    /// Processes a block of complete lines. Stops early once `limit` records were emitted.
    pub fn process<'c>(&mut self, chunk: &'c [u8], out: &mut ChunkOut, limit: Option<usize>) {
        let plan = self.plan;
        let n = &mut self.n;
        let agg = &mut self.agg;
        let mut slots: Vec<Option<&'c [u8]>> = vec![None; plan.trie.nslots()];
        let mut handle = |line: &'c [u8]| handle_line(plan, n, agg, line, &mut slots, out, limit);

        let Some(finder) = &self.finder else {
            for_each_line(chunk, handle);
            return;
        };
        if self.unicode_esc.find(chunk).is_some() {
            // Some line may spell the needle with \u escapes: check per line.
            let esc = &self.unicode_esc;
            for_each_line(chunk, |l| {
                if finder.find(l).is_none() && esc.find(l).is_none() {
                    return true;
                }
                handle(l)
            });
            return;
        }
        // Jump between needle hits; lines without one are never looked at.
        let mut pos = 0;
        while let Some(h) = finder.find(&chunk[pos..]) {
            let hit = pos + h;
            let start = memrchr(b'\n', &chunk[pos..hit]).map_or(pos, |i| pos + i + 1);
            let end = memchr(b'\n', &chunk[hit..]).map_or(chunk.len(), |i| hit + i);
            if !handle(&chunk[start..end]) {
                return;
            }
            pos = end + 1;
            if pos >= chunk.len() {
                break;
            }
        }
    }
}

#[inline]
fn for_each_line<'c>(chunk: &'c [u8], mut f: impl FnMut(&'c [u8]) -> bool) {
    let mut start = 0;
    for nl in memchr_iter(b'\n', chunk) {
        if !f(&chunk[start..nl]) {
            return;
        }
        start = nl + 1;
    }
    if start < chunk.len() {
        f(&chunk[start..]);
    }
}

/// Returns false to stop processing the chunk.
#[inline]
fn handle_line<'a>(
    plan: &Plan,
    n: &mut Counters,
    agg: &mut Option<AggState>,
    line: &'a [u8],
    slots: &mut [Option<&'a [u8]>],
    out: &mut ChunkOut,
    limit: Option<usize>,
) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let start = skip_ws(line, 0);
    match line.get(start) {
        None => return true, // blank
        // Cheap framing check, so even field-free queries (`count`) don't count garbage.
        Some(b'{') if line.trim_ascii_end().last() == Some(&b'}') => {}
        Some(_) => {
            n.malformed += 1;
            return true;
        }
    }
    if !slots.is_empty() {
        slots.fill(None);
        if !plan.trie.extract(line, start, slots) {
            n.malformed += 1;
            return true;
        }
    }
    if let Some(f) = &plan.filter {
        if !truth(f, &Slots(slots)) {
            return true;
        }
    }
    n.matched += 1;
    if let Some(a) = agg {
        a.update(plan.agg.as_ref().unwrap(), slots);
        return true;
    }
    if plan.buffers_records() {
        let mut payload = vec![];
        output::record(plan, line, slots, &mut payload);
        out.items.push(Item { keys: plan.sort_slots.iter().map(|&s| Cell::from_raw(slots[s])).collect(), payload });
        return true;
    }
    output::record(plan, line, slots, &mut out.bytes);
    out.ends.push(out.bytes.len());
    limit.is_none_or(|l| out.ends.len() < l)
}

/// Owns output and cross-input state for one invocation.
pub struct Ctx<'p> {
    pub plan: &'p Plan,
    pub threads: usize,
    pub prefilter: bool,
    agg: Option<AggState>,
    items: Vec<Item>,
    emitted: usize,
    pub matched: u64,
    pub malformed: u64,
    pub bytes: u64,
    /// `-o table` over records: buffered TSV, aligned at the end.
    table_buf: Option<Vec<u8>>,
    stdout: io::Stdout,
}

impl<'p> Ctx<'p> {
    pub fn new(plan: &'p Plan, threads: usize, prefilter: bool) -> Result<Self> {
        let mut ctx = Ctx {
            plan,
            threads,
            prefilter,
            agg: None,
            items: vec![],
            emitted: 0,
            matched: 0,
            malformed: 0,
            bytes: 0,
            table_buf: (plan.agg.is_none() && plan.project.is_some() && plan.format == Format::Table).then(Vec::new),
            stdout: io::stdout(),
        };
        if plan.agg.is_none() {
            if let Some(h) = output::record_header(plan) {
                ctx.write(&h)?;
            }
        }
        Ok(ctx)
    }

    fn write(&mut self, b: &[u8]) -> Result<()> {
        if let Some(buf) = &mut self.table_buf {
            buf.extend_from_slice(b);
            return Ok(());
        }
        write_or_exit(&mut self.stdout.lock(), b)
    }

    fn remaining(&self) -> Option<usize> {
        self.plan.stream_limit().map(|l| l.saturating_sub(self.emitted))
    }

    fn absorb(&mut self, mut w: Worker) {
        self.matched += w.n.matched;
        self.malformed += w.n.malformed;
        w.n = Counters::default();
        if let Some(a) = w.agg.take() {
            match &mut self.agg {
                Some(mine) => mine.merge(a),
                None => self.agg = Some(a),
            }
        }
    }

    /// Writes one chunk's output in input order. Exits the process once a `head` is satisfied.
    fn emit(&mut self, o: ChunkOut) -> Result<()> {
        self.items.extend(o.items);
        let mut n = o.ends.len();
        if n == 0 {
            return Ok(());
        }
        let mut done = false;
        if let Some(rem) = self.remaining() {
            if n >= rem {
                n = rem;
                done = true;
            }
        }
        let cut = if n == 0 { 0 } else { o.ends[n - 1] };
        self.emitted += n;
        self.write(&o.bytes[..cut])?;
        if done {
            self.finish()?;
            std::process::exit(0);
        }
        Ok(())
    }

    /// Runs the plan over one input, in parallel when worthwhile.
    pub fn run(&mut self, src: Source) -> Result<()> {
        if self.remaining() == Some(0) {
            return Ok(());
        }
        let threads = match &src {
            Source::Bytes(b) if b.len() < (1 << 20) => 1,
            _ => self.threads,
        };
        let chunk_size = match &src {
            Source::Bytes(b) => (b.len() / (threads * 16)).clamp(256 << 10, 8 << 20),
            Source::Reader(_) => 1 << 20,
        };
        let mut chunks = Chunker { src, pos: 0, carry: vec![], size: chunk_size };
        if threads <= 1 {
            let mut w = Worker::new(self.plan, self.prefilter);
            while let Some(c) = chunks.next()? {
                self.bytes += c.len() as u64;
                let mut o = ChunkOut::default();
                w.process(&c, &mut o, self.remaining());
                self.emit(o)?;
            }
            self.absorb(w);
            return Ok(());
        }

        let plan = self.plan;
        let prefilter = self.prefilter;
        let limit = self.remaining();
        std::thread::scope(|s| -> Result<()> {
            // Backpressure: a chunk may only be issued while it is fewer than `window` chunks ahead
            // of the last one written. That bounds everything between reading and writing (queued
            // input, chunks being processed, the reorder buffer), so a stalled consumer stalls
            // lq instead of growing its memory toward the size of the output.
            let window = threads * 2;
            let (tok_tx, tok_rx) = bounded::<()>(window);
            for _ in 0..window {
                tok_tx.send(()).unwrap();
            }
            let (tx_in, rx_in) = bounded::<(usize, Chunk)>(window);
            let (tx_out, rx_out) = unbounded::<(usize, ChunkOut)>();
            let producer = s.spawn(move || -> Result<u64> {
                let mut bytes = 0;
                let mut idx = 0;
                while tok_rx.recv().is_ok() {
                    let Some(c) = chunks.next()? else { break };
                    bytes += c.len() as u64;
                    if tx_in.send((idx, c)).is_err() {
                        break;
                    }
                    idx += 1;
                }
                Ok(bytes)
            });
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let rx = rx_in.clone();
                    let tx = tx_out.clone();
                    s.spawn(move || {
                        let mut w = Worker::new(plan, prefilter);
                        for (idx, c) in rx {
                            let mut o = ChunkOut::default();
                            w.process(&c, &mut o, limit);
                            if tx.send((idx, o)).is_err() {
                                break;
                            }
                        }
                        w
                    })
                })
                .collect();
            drop((rx_in, tx_out));

            let mut next = 0;
            let mut pending = BTreeMap::new();
            for (idx, o) in rx_out {
                pending.insert(idx, o);
                while let Some(o) = pending.remove(&next) {
                    next += 1;
                    self.emit(o)?;
                    let _ = tok_tx.send(()); // free a slot for the producer
                }
            }
            for w in workers {
                self.absorb(w.join().expect("worker panicked"));
            }
            self.bytes += producer.join().expect("reader panicked")?;
            Ok(())
        })
    }

    /// Final output: the aggregate table or buffered records, plus warnings.
    pub fn finish(&mut self) -> Result<()> {
        let plan = self.plan;
        if let Some(ap) = &plan.agg {
            let state = self.agg.get_or_insert_with(|| AggState::new(ap));
            let mut rows = state.rows(ap);
            apply_ops(&plan.ops, &mut rows, |r| r);
            let t = output::table(&ap.columns(), &rows, plan.format);
            self.write(&t)?;
        } else if plan.buffers_records() {
            let mut items = std::mem::take(&mut self.items);
            apply_ops(&plan.ops, &mut items, |i| &i.keys);
            for it in items {
                self.write(&it.payload)?;
            }
        }
        if let Some(buf) = self.table_buf.take() {
            write_or_exit(&mut self.stdout.lock(), &output::align_tsv(&buf))?;
        }
        if self.malformed > 0 {
            eprintln!("lq: skipped {} malformed line(s)", self.malformed);
        }
        Ok(())
    }

    /// Tails files, processing appended lines as they arrive. Never returns on success.
    pub fn follow(&mut self, paths: &[PathBuf], from_start: bool, interval: Duration) -> Result<()> {
        if self.plan.buffers_records() {
            anyhow::bail!("record 'sort' needs the whole input, so it cannot be used with --follow");
        }
        if self.table_buf.is_some() {
            anyhow::bail!("-o table aligns columns over the whole output, so it cannot stream; use -o tsv with --follow");
        }
        let mut tails = vec![];
        for p in paths {
            let mut t = Tail::open(p.clone())?;
            if from_start {
                let data = std::fs::read(p)?;
                let end = memrchr(b'\n', &data).map_or(0, |i| i + 1);
                self.run(Source::Bytes(&data[..end]))?;
                t.pos = end as u64;
            } else {
                t.pos = t.file.metadata()?.len();
            }
            tails.push(t);
        }
        let mut w = Worker::new(self.plan, self.prefilter);
        w.agg = self.agg.take().or(w.agg);
        let tty = io::stdout().is_terminal();
        let mut last_render: Option<Instant> = None;
        let mut dirty = true;
        let mut buf = vec![0u8; 1 << 20];
        let mut last_warn = Instant::now();
        let mut warned = 0;
        loop {
            let mut got = false;
            for t in &mut tails {
                while let Some(data) = t.poll(&mut buf)? {
                    got = true;
                    self.bytes += data.len() as u64;
                    let mut o = ChunkOut::default();
                    w.process(&data, &mut o, self.remaining());
                    self.emit(o)?;
                    dirty = true;
                }
            }
            if self.plan.agg.is_some() && dirty && last_render.is_none_or(|t| t.elapsed() >= interval) {
                let ap = self.plan.agg.as_ref().unwrap();
                let mut rows = w.agg.as_mut().unwrap().rows(ap);
                apply_ops(&self.plan.ops, &mut rows, |r| r);
                let mut screen = vec![];
                if tty {
                    screen.extend_from_slice(b"\x1b[H\x1b[2J");
                }
                screen.extend(output::table(&ap.columns(), &rows, self.plan.format));
                if tty {
                    let n = self.matched + w.n.matched;
                    screen.extend(format!("\n\x1b[2m{n} matching records · following {} file(s) · ctrl-c to stop\x1b[0m\n", tails.len()).bytes());
                } else {
                    screen.push(b'\n');
                }
                self.write(&screen)?;
                last_render = Some(Instant::now());
                dirty = false;
            }
            // finish() never runs in follow mode, so report malformed lines as they turn up.
            let malformed = self.malformed + w.n.malformed;
            if malformed > warned && last_warn.elapsed() >= interval {
                eprintln!("lq: skipped {malformed} malformed line(s) so far");
                warned = malformed;
                last_warn = Instant::now();
            }
            if !got {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn apply_ops<T>(ops: &[RowOp], rows: &mut Vec<T>, keys: impl Fn(&T) -> &[Cell]) {
    for op in ops {
        match op {
            RowOp::Where(e) => rows.retain(|r| truth(e, &Row(keys(r)))),
            RowOp::Sort(by) => rows.sort_by(|a, b| {
                let (ka, kb) = (keys(a), keys(b));
                by.iter()
                    .map(|&(i, desc)| {
                        let o = ka[i].total_cmp(&kb[i]);
                        if desc {
                            o.reverse()
                        } else {
                            o
                        }
                    })
                    .find(|o| o.is_ne())
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            RowOp::Head(n) => rows.truncate(*n),
        }
    }
}

fn write_or_exit(w: &mut impl Write, b: &[u8]) -> Result<()> {
    match w.write_all(b).and_then(|_| w.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        r => Ok(r?),
    }
}

pub enum Source<'a> {
    Bytes(&'a [u8]),
    Reader(Box<dyn Read + Send + 'a>),
}

/// A block of whole lines, borrowed from a mapping or read from a stream.
pub enum Chunk<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Chunk<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Chunk::Borrowed(b) => b,
            Chunk::Owned(v) => v,
        }
    }
}

struct Chunker<'a> {
    src: Source<'a>,
    pos: usize,
    carry: Vec<u8>,
    size: usize,
}

impl<'a> Chunker<'a> {
    /// Next block ending on a line boundary. Stream reads return as soon as some data is
    /// available, so piped live input (`tail -f x | lq ...`) is processed with low latency.
    fn next(&mut self) -> io::Result<Option<Chunk<'a>>> {
        match &mut self.src {
            Source::Bytes(b) => {
                let b: &'a [u8] = b;
                if self.pos >= b.len() {
                    return Ok(None);
                }
                let mut end = (self.pos + self.size).min(b.len());
                if end < b.len() {
                    end = memchr(b'\n', &b[end..]).map_or(b.len(), |i| end + i + 1);
                }
                let c = &b[self.pos..end];
                self.pos = end;
                Ok(Some(Chunk::Borrowed(c)))
            }
            Source::Reader(r) => loop {
                let mut buf = std::mem::take(&mut self.carry);
                let old = buf.len();
                buf.resize(old + self.size, 0);
                let n = loop {
                    match r.read(&mut buf[old..]) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        r => break r?,
                    }
                };
                buf.truncate(old + n);
                if n == 0 {
                    return Ok((!buf.is_empty()).then_some(Chunk::Owned(buf)));
                }
                match memrchr(b'\n', &buf[old..]) {
                    Some(i) => {
                        self.carry = buf.split_off(old + i + 1);
                        return Ok(Some(Chunk::Owned(buf)));
                    }
                    None => self.carry = buf,
                }
            },
        }
    }
}

/// One followed file. Survives truncation and rotation (rename + recreate), like `tail -F`.
struct Tail {
    path: PathBuf,
    file: std::fs::File,
    ino: u64,
    pos: u64,
    partial: Vec<u8>,
}

impl Tail {
    fn open(path: PathBuf) -> Result<Tail> {
        use std::os::unix::fs::MetadataExt;
        let file = std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let ino = file.metadata()?.ino();
        Ok(Tail { path, file, ino, pos: 0, partial: vec![] })
    }

    /// Returns newly appended complete lines, if any.
    fn poll(&mut self, buf: &mut [u8]) -> Result<Option<Vec<u8>>> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(buf)?;
        if n == 0 {
            // Drained: check whether the path now points elsewhere or was truncated.
            if let Ok(m) = std::fs::metadata(&self.path) {
                if m.ino() != self.ino || m.len() < self.pos {
                    let rotated = m.ino() != self.ino;
                    eprintln!("lq: {} was {}; reading from the start", self.path.display(), if rotated { "rotated" } else { "truncated" });
                    // An unterminated last line of a rotated file is complete: nothing more will
                    // be appended to it. After truncation it is gone, so drop it.
                    let mut leftover = std::mem::take(&mut self.partial);
                    *self = Tail::open(self.path.clone())?;
                    if rotated && !leftover.is_empty() {
                        leftover.push(b'\n');
                        let mut rest = self.poll(buf)?.unwrap_or_default();
                        leftover.append(&mut rest);
                        return Ok(Some(leftover));
                    }
                    return self.poll(buf);
                }
            }
            return Ok(None);
        }
        self.pos += n as u64;
        self.partial.extend_from_slice(&buf[..n]);
        // May be empty when no line is complete yet; the caller keeps polling until EOF.
        Ok(Some(memrchr(b'\n', &self.partial).map_or_else(Vec::new, |i| {
            let rest = self.partial.split_off(i + 1);
            std::mem::replace(&mut self.partial, rest)
        })))
    }
}
