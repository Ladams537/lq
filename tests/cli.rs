use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn lq() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lq"))
}

fn run(args: &[&str], input: &str) -> (String, String, i32) {
    let mut c = lq().args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    (String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap(), o.status.code().unwrap_or(-1))
}

fn out(args: &[&str], input: &str) -> String {
    let (o, e, code) = run(args, input);
    assert_eq!(code, 0, "stderr: {e}");
    o
}

const LOGS: &str = r#"{"level":"info","svc":"a","lat":10,"msg":"ok"}
{"level":"error","svc":"b","lat":30,"msg":"boom, \"bad\""}

{"level":"error","svc":"a","lat":20,"http":{"status":503}}
not json
{"level":"\u0065rror","svc":"c","lat":5}
{"level":"warn","lat":7}
"#;

#[test]
fn filters_print_raw_lines_in_order_and_report_malformed() {
    let (o, e, _) = run(&[r#"level == "error""#], LOGS);
    let lines: Vec<_> = o.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains(r#""svc":"b""#) && lines[2].contains(r#""svc":"c""#), "{o}");
    // Lines the substring prefilter rules out are never parsed, so they can't be reported.
    assert_eq!(e, "");
    let (_, e, _) = run(&["lat > 0"], LOGS);
    assert!(e.contains("skipped 1 malformed"), "{e}");
}

#[test]
fn prefilter_does_not_hide_unicode_escaped_matches() {
    // Same check through the chunk-level needle path (big input, no \u in most chunks).
    let mut big = String::new();
    for i in 0..60_000 {
        big.push_str(&format!("{{\"level\":\"info\",\"i\":{i}}}\n"));
    }
    big.push_str("{\"level\":\"\\u0065rror\",\"i\":-1}\n");
    big.push_str("{\"level\":\"error\",\"i\":-2}\n");
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.log");
    std::fs::write(&p, &big).unwrap();
    for j in ["-j1", "-j4"] {
        let o = out(&[j, r#"level == "error" | count"#, p.to_str().unwrap()], "");
        assert!(o.ends_with("2\n"), "{j}: {o}");
    }
    assert!(out(&["--no-prefilter", r#"level == "error" | count"#], &big).ends_with("2\n"));
}

#[test]
fn parallel_output_matches_sequential_and_head_stops_early() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.log");
    let mut s = String::new();
    for i in 0..200_000 {
        s.push_str(&format!("{{\"i\":{i},\"odd\":{}}}\n", i % 2 == 1));
    }
    std::fs::write(&p, &s).unwrap();
    let f = p.to_str().unwrap();
    let par = out(&["-j8", "odd", f], "");
    let seq = out(&["-j1", "odd", f], "");
    assert_eq!(par.lines().count(), 100_000);
    assert_eq!(par, seq);
    let head = out(&["-j8", "odd | head 5 | select i", f], "");
    assert_eq!(head, "{\"i\":1}\n{\"i\":3}\n{\"i\":5}\n{\"i\":7}\n{\"i\":9}\n");
}

#[test]
fn aggregations() {
    let o = out(&["-o", "tsv", "count by svc"], LOGS);
    assert_eq!(o, "svc\tcount\n\t1\na\t2\nb\t1\nc\t1\n"); // missing svc groups as null, sorted first
    let o = out(&["-o", "json", "stats count(), sum(lat), avg(lat), min(lat), max(lat), median(lat), distinct(svc) as d"], LOGS);
    assert_eq!(o, "{\"count\":5,\"sum(lat)\":72,\"avg(lat)\":14.4,\"min(lat)\":5,\"max(lat)\":30,\"p50(lat)\":10,\"d\":3}\n");
    let o = out(&["-o", "tsv", "top 1 level"], LOGS);
    assert_eq!(o, "level\tcount\nerror\t3\n");
    let o = out(&["-o", "tsv", "count by level | where count >= 1 and level != \"info\" | sort count desc, level"], LOGS);
    assert_eq!(o, "level\tcount\nerror\t3\nwarn\t1\n");
    let o = out(&["-o", "tsv", "exists(http) | count by http.status"], LOGS);
    assert_eq!(o, "http.status\tcount\n503\t1\n");
    // Aggregate over empty input still yields a row (and exits 1: nothing matched).
    assert_eq!(run(&["-o", "tsv", "count"], ""), ("count\n0\n".into(), "".into(), 1));
}

#[test]
fn projection_formats_and_record_sort() {
    let q = r#"level in ["error", "warn"] | select svc as service, lat, msg | sort lat desc | head 3"#;
    let json = out(&[q], LOGS);
    assert_eq!(
        json,
        "{\"service\":\"b\",\"lat\":30,\"msg\":\"boom, \\\"bad\\\"\"}\n{\"service\":\"a\",\"lat\":20,\"msg\":null}\n{\"service\":null,\"lat\":7,\"msg\":null}\n"
    );
    let csv = out(&["-o", "csv", q], LOGS);
    assert_eq!(csv, "service,lat,msg\nb,30,\"boom, \"\"bad\"\"\"\na,20,\n,7,\n");
}

#[test]
fn string_ops_and_regex() {
    assert_eq!(out(&["-o", "tsv", r#"msg contains "boom" or msg =~ "^o.$" | count"#], LOGS), "count\n2\n");
    assert_eq!(out(&["-o", "tsv", r#"svc startswith "a" and not (lat > 15) | count"#], LOGS), "count\n1\n");
    assert_eq!(out(&["-o", "tsv", r#"svc == null | count"#], LOGS), "count\n1\n");
}

#[test]
fn query_errors_exit_2() {
    let (_, e, code) = run(&["level =="], "");
    assert_eq!(code, 2);
    assert!(e.contains("query error"), "{e}");
    let (_, e, code) = run(&["count by a | sort b"], "");
    assert_eq!(code, 2);
    assert!(e.contains("unknown column 'b'"), "{e}");
}

/// Kills the child when dropped, so a failing assertion can't leak a `lq -f` process.
struct Reaper(std::process::Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl std::ops::Deref for Reaper {
    type Target = std::process::Child;
    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl std::ops::DerefMut for Reaper {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

/// Reads child stdout lines on a thread so tests can wait with a timeout.
fn line_reader(child: &mut std::process::Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines() {
            if tx.send(l.unwrap()).is_err() {
                break;
            }
        }
    });
    rx
}

fn append(p: &Path, s: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

fn expect_line(rx: &mpsc::Receiver<String>, needle: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(l) if l.contains(needle) => return,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    panic!("timed out waiting for {needle:?}");
}

/// `-f` starts at end of file, so lines appended before it is up would be missed. Startup time
/// varies (macOS scans a freshly built binary on first launch), so append markers until one
/// comes through instead of sleeping a fixed amount.
fn wait_until_following(p: &Path, rx: &mpsc::Receiver<String>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        append(p, "{\"level\":\"error\",\"n\":\"ready\"}\n");
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(100)) {
            assert!(l.contains("ready"), "{l}");
            while rx.recv_timeout(Duration::from_millis(200)).is_ok() {} // drain extra markers
            return;
        }
    }
    panic!("lq -f never started following");
}

#[test]
fn follow_tails_appends_partial_lines_truncation_and_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("app.log");
    std::fs::write(&p, "{\"level\":\"error\",\"n\":\"old\"}\n").unwrap();
    let mut child = Reaper(lq().args(["-f", r#"level == "error" | select n"#, p.to_str().unwrap()]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap());
    let rx = line_reader(&mut child);
    wait_until_following(&p, &rx);
    append(&p, "{\"level\":\"info\",\"n\":\"skip\"}\n{\"level\":\"error\",\"n\":\"one\"}\n");
    expect_line(&rx, "\"one\"");
    // A line written in two pieces is only processed once complete.
    append(&p, "{\"level\":\"error\",");
    std::thread::sleep(Duration::from_millis(200));
    append(&p, "\"n\":\"two\"}\n");
    expect_line(&rx, "\"two\"");
    // Truncation.
    std::fs::write(&p, "").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    append(&p, "{\"level\":\"error\",\"n\":\"three\"}\n");
    expect_line(&rx, "\"three\"");
    // Rotation: rename away, create a fresh file at the same path.
    std::fs::rename(&p, dir.path().join("app.log.1")).unwrap();
    append(&p, "{\"level\":\"error\",\"n\":\"four\"}\n");
    expect_line(&rx, "\"four\"");
    assert!(rx.try_iter().all(|l| !l.contains("old") && !l.contains("skip")));
}

#[test]
fn follow_aggregate_rerenders_and_from_start_includes_history() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("app.log");
    std::fs::write(&p, "{\"svc\":\"a\"}\n{\"svc\":\"a\"}\n").unwrap();
    let mut child = Reaper(lq()
        .args(["-f", "--from-start", "--interval", "0.1", "-o", "tsv", "count by svc", p.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap());
    let rx = line_reader(&mut child);
    expect_line(&rx, "a\t2");
    append(&p, "{\"svc\":\"a\"}\n{\"svc\":\"b\"}\n");
    expect_line(&rx, "a\t3");
}

#[test]
fn stdin_streams_with_low_latency() {
    let mut child = Reaper(lq().args(["select n"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
    let rx = line_reader(&mut child);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"{\"n\":1}\n").unwrap();
    stdin.flush().unwrap();
    expect_line(&rx, "{\"n\":1}"); // arrives while stdin is still open
    drop(stdin);
    child.wait().unwrap();
}

// ---- regressions from review ----

fn big_file(dir: &Path, name: &str, lines: usize, f: impl Fn(usize) -> String) -> String {
    let p = dir.join(name);
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(&f(i));
        s.push('\n');
    }
    std::fs::write(&p, s).unwrap();
    p.to_str().unwrap().to_string()
}

#[test]
fn head_counts_records_not_newlines() {
    // A CSV/TSV/JSON record may span lines (CSV) or contain escaped newlines.
    let input = "{\"m\":\"line1\\nline2\",\"x\":1}\n{\"m\":\"b\",\"x\":2}\n";
    assert_eq!(out(&["-o", "csv", "select m, x | head 1"], input), "m,x\n\"line1\nline2\",1\n");
    let dir = tempfile::tempdir().unwrap();
    let f = big_file(dir.path(), "m.log", 300_000, |i| format!("{{\"m\":\"a\\nb{i}\",\"x\":{i}}}"));
    let o = out(&["-j8", "-o", "csv", "select m, x | head 3", &f], "");
    assert_eq!(o, "m,x\n\"a\nb0\",0\n\"a\nb1\",1\n\"a\nb2\",2\n");
}

#[test]
fn unsupported_combinations_are_rejected() {
    let (_, e, code) = run(&["-o", "csv", "level == \"x\""], "");
    assert_eq!(code, 2, "{e}");
    assert!(e.contains("select"), "{e}");
    let (_, e, code) = run(&["select a | where b == 1"], "");
    assert_eq!(code, 2);
    assert!(e.contains("'b'") && e.contains("select"), "{e}");
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.log");
    std::fs::write(&p, "").unwrap();
    let (_, e, code) = run(&["-f", "-o", "table", "select a", p.to_str().unwrap()], "");
    assert_eq!(code, 2, "{e}");
    let (_, e, code) = run(&["-f", "a"], "");
    assert_eq!(code, 2, "{e}");
    assert!(e.contains("--follow"), "{e}");
}

#[test]
fn aggregate_columns_by_function_name() {
    let o = out(&["-o", "tsv", "stats avg(lat), p99.9(lat), median(lat) by svc | where avg(lat) > 6 | sort avg(lat) desc, p50(lat)"], LOGS);
    assert_eq!(o, "svc\tavg(lat)\tp99.9(lat)\tp50(lat)\nb\t30\t30\t30\na\t15\t20\t10\n\t7\t7\t7\n");
    let (_, e, code) = run(&["avg(lat) > 3"], LOGS);
    assert_eq!(code, 2);
    assert!(e.contains("after an aggregation"), "{e}");
}

#[test]
fn percentile_names_lexed_exactly() {
    let o = out(&["--explain", "stats p99.05(x)"], "");
    assert!(o.contains("Pct(\n") && o.contains("99.05"), "{o}");
    assert!(o.contains("\"p99.05(x)\""), "{o}");
}

#[test]
fn field_free_queries_still_reject_broken_lines() {
    let (o, e, _) = run(&["-o", "tsv", "count"], "{broken garbage\n{\"a\":\n{\"ok\":1}\n");
    assert_eq!(o, "count\n1\n");
    assert!(e.contains("skipped 2 malformed"), "{e}");
}

#[test]
fn duplicate_object_keys_first_wins_as_a_whole() {
    let o = out(&["select o.a, o.b"], "{\"o\":{\"a\":1},\"o\":{\"b\":2}}\n");
    assert_eq!(o, "{\"o.a\":1,\"o.b\":null}\n");
}

#[test]
fn exit_status_like_grep() {
    assert_eq!(run(&["a == 1"], "{\"a\":2}\n").2, 1);
    assert_eq!(run(&["a == 1"], "{\"a\":1}\n").2, 0);
    assert_eq!(run(&["a == 1 | count"], "{\"a\":2}\n").2, 1);
}

#[test]
fn record_table_and_tsv_formatting() {
    let o = out(&["-o", "table", "select svc, lat"], LOGS);
    assert_eq!(o.lines().nth(1).unwrap(), "a     10", "{o}"); // numbers right-aligned
    // Backslashes escaped the same way for records and aggregates.
    let input = "{\"k\":\"a\\\\b\"}\n";
    assert_eq!(out(&["-o", "tsv", "select k"], input), "k\na\\\\b\n");
    assert_eq!(out(&["-o", "tsv", "count by k"], input), "k\tcount\na\\\\b\t1\n");
    assert!(out(&["--explain", "level"], "").contains("Key(\"level\")"));
}

#[test]
fn multi_literal_prefilter() {
    let dir = tempfile::tempdir().unwrap();
    let f = big_file(dir.path(), "l.log", 100_000, |i| format!("{{\"l\":\"{}\",\"i\":{i}}}", ["info", "warn", "error", "debug"][i % 4]));
    for q in [r#"l in ["warn", "error"] | count"#, r#"l == "warn" or l == "error" | count"#] {
        let o = out(&[q, &f], "");
        assert_eq!(o.lines().last().unwrap().trim(), "50000", "{q}");
    }
    let o = out(&["--explain", r#"l in ["warn", "error"]"#], "");
    assert!(o.contains("needles"), "{o}");
}

#[test]
fn backpressure_bounds_memory_with_a_stalled_reader() {
    // 200 MB of matching output; the reader stalls, so lq must stop producing rather than buffer.
    let dir = tempfile::tempdir().unwrap();
    let pad = "x".repeat(400);
    let f = big_file(dir.path(), "b.log", 500_000, |i| format!("{{\"i\":{i},\"p\":\"{pad}\"}}"));
    let child = Reaper(lq().args(["-j8", "i >= 0", &f]).stdout(Stdio::piped()).spawn().unwrap());
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(1500));
    let rss = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
    let rss_kb: u64 = String::from_utf8_lossy(&rss.stdout).trim().parse().unwrap();
    // The mapped input (~210 MB) counts once pages are touched; buffered output must not add to it.
    assert!(rss_kb < 120_000, "rss {rss_kb} KB while the reader was stalled");
}

#[test]
fn rotation_keeps_unterminated_last_line() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("app.log");
    std::fs::write(&p, "").unwrap();
    let mut child = Reaper(lq().args(["-f", "select n", p.to_str().unwrap()]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap());
    let rx = line_reader(&mut child);
    wait_until_following(&p, &rx);
    append(&p, "{\"n\":\"tail-without-newline\"}");
    std::thread::sleep(Duration::from_millis(300));
    std::fs::rename(&p, dir.path().join("app.log.1")).unwrap();
    append(&p, "{\"n\":\"new\"}\n");
    expect_line(&rx, "tail-without-newline");
    expect_line(&rx, "new");
}
