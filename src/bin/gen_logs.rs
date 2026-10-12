//! Deterministic synthetic JSON-Lines application logs for benchmarks and tests.
//!
//! usage: gen-logs [LINES] [SEED] > app.log
//!
//! With `--rate N`, writes N lines/second forever (for trying out `lq --follow`).

use std::io::{BufWriter, Write};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
    /// Picks by cumulative weight table.
    fn weighted<'a, T>(&mut self, xs: &'a [(T, u32)]) -> &'a T {
        let total: u32 = xs.iter().map(|x| x.1).sum();
        let mut r = self.below(total as u64) as u32;
        for (x, w) in xs {
            if r < *w {
                return x;
            }
            r -= w;
        }
        &xs[0].0
    }
}

const SERVICES: &[&str] = &["api-gateway", "auth", "billing", "catalog", "checkout", "inventory", "notifications", "search", "users", "recommendations"];
const LEVELS: &[(&str, u32)] = &[("debug", 20), ("info", 64), ("warn", 11), ("error", 5)];
const STATUS: &[(u32, u32)] = &[(200, 780), (201, 60), (204, 30), (301, 10), (400, 30), (401, 15), (403, 10), (404, 50), (429, 5), (500, 7), (502, 2), (503, 1)];
const METHODS: &[(&str, u32)] = &[("GET", 70), ("POST", 18), ("PUT", 6), ("DELETE", 4), ("PATCH", 2)];
const PATHS: &[&str] = &["/v1/users/{id}", "/v1/orders", "/v1/orders/{id}", "/v1/cart", "/v1/search", "/v1/products/{id}", "/v1/login", "/v1/payments", "/healthz", "/v1/recommendations"];
const MSGS: &[&str] = &[
    "request completed",
    "cache miss for key user:{id}",
    "upstream responded slowly",
    "connection refused by upstream",
    "retrying request (attempt 2 of 3)",
    "user \\\"{id}\\\" logged in",
    "payment authorized",
    "timeout waiting for db connection",
    "validation failed: field \\\"email\\\" is invalid",
    "rate limit exceeded",
    "feature flag evaluated",
    "background job finished in {id}ms",
    "café order synced ☕",
    "path C:\\\\temp\\\\upload.tmp cleaned",
];
const ERRORS: &[&str] = &["TimeoutError", "ConnectionRefused", "ValidationError", "NullPointerException", "DeadlineExceeded"];

fn civil(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ts(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let (y, mo, d) = civil(days);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

fn line(r: &mut Rng, i: u64, now_ms: i64, out: &mut String) {
    use std::fmt::Write;
    let level = *r.weighted(LEVELS);
    let service = *r.pick(SERVICES);
    let status = match level {
        "error" => *r.pick(&[500, 502, 503, 504]),
        _ => *r.weighted(STATUS),
    };
    let method = *r.weighted(METHODS);
    let id = r.below(100_000);
    let path = r.pick(PATHS).replace("{id}", &id.to_string());
    let msg = r.pick(MSGS).replace("{id}", &id.to_string());
    // Log-normal-ish latency, rounded to µs.
    let n = (-2.0 * r.unit().max(1e-12).ln()).sqrt() * (std::f64::consts::TAU * r.unit()).cos();
    let latency = ((3.0 + 1.1 * n).exp() * 1000.0).round() / 1000.0;
    let _ = write!(
        out,
        r#"{{"ts":"{}","level":"{level}","service":"{service}","host":"web-{:02}","msg":"{msg}","request_id":"{:016x}","user_id":{},"latency_ms":{latency},"status":{status},"http":{{"method":"{method}","path":"{path}","bytes":{}}}"#,
        ts(now_ms),
        r.below(24),
        r.next() ^ i,
        r.below(50_000),
        r.below(200_000),
    );
    if r.below(4) == 0 {
        let _ = write!(out, r#","tags":["{}","{}"]"#, r.pick(&["beta", "canary", "eu", "us", "mobile"]), r.pick(&["web", "ios", "android"]));
    }
    if level == "error" {
        let _ = write!(
            out,
            r#","error":{{"type":"{}","message":"upstream {} failed after {}ms","retryable":{}}}"#,
            r.pick(ERRORS),
            r.pick(SERVICES),
            r.below(5000),
            r.below(2) == 0
        );
    }
    if r.below(10) == 0 {
        let _ = write!(out, r#","trace_id":"{:032x}""#, (r.next() as u128) << 64 | r.next() as u128);
    }
    out.push_str("}\n");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdout = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    let mut s = String::with_capacity(512);
    if args.first().map(String::as_str) == Some("--rate") {
        let rate: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(10);
        let mut r = Rng(1);
        for i in 0.. {
            s.clear();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
            line(&mut r, i, now, &mut s);
            if stdout.write_all(s.as_bytes()).and_then(|_| stdout.flush()).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs_f64(1.0 / rate as f64));
        }
    }
    let lines: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(1_000_000);
    let seed: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(42);
    let mut r = Rng(seed);
    let mut now = 1_791_763_200_000i64; // 2026-10-12T00:00:00Z
    for i in 0..lines {
        s.clear();
        now += r.below(4) as i64;
        line(&mut r, i, now, &mut s);
        if stdout.write_all(s.as_bytes()).is_err() {
            return;
        }
    }
    let _ = stdout.flush();
}
