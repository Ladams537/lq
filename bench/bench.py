#!/usr/bin/env python3
"""Benchmark lq against jq (and jaq, if installed) on equivalent workloads.

Every case is first run once per tool with output captured and checked for equivalence, so a
speedup is never reported for a query that returns a different answer. Timings are the median
wall-clock of N runs with output to /dev/null; the input is pre-warmed into the page cache.

usage: bench/bench.py [--lines 1000000] [--runs 5] [--jaq PATH] [--only NAME] [--out bench/RESULTS.md]
"""
import argparse, json, math, os, platform, shutil, statistics, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LQ = os.path.join(ROOT, "target/release/lq")
GEN = os.path.join(ROOT, "target/release/gen-logs")


def lines_equal(a, b):
    return a == b


def num_equal(a, b):
    return math.isclose(float(a.strip()), float(b.strip()), rel_tol=1e-9)


def lq_rows(out):
    return [json.loads(l) for l in out.splitlines() if l.strip()]


def close(x, y):
    if isinstance(x, (int, float)) and isinstance(y, (int, float)):
        return math.isclose(x, y, rel_tol=1e-9, abs_tol=1e-9)
    return x == y


def grouped(keyf, valf):
    """lq JSON rows vs a jq object {key: value}."""
    def check(lq_out, jq_out):
        mine = {keyf(r): valf(r) for r in lq_rows(lq_out)}
        theirs = json.loads(jq_out)
        if set(mine) != set(theirs):
            return False
        return all(
            all(close(mine[k][f], theirs[k][f]) for f in theirs[k]) if isinstance(theirs[k], dict) else close(mine[k], theirs[k])
            for k in theirs
        )
    return check


def cases(sample_id):
    return [
        dict(
            name="filter-eq",
            desc='records where level == "error" (5%)',
            lq=['level == "error"'],
            jq=["-c", 'select(.level == "error")'],
            check=lines_equal,
        ),
        dict(
            name="filter-num",
            desc="records where status >= 500 (~5.5%)",
            lq=["status >= 500"],
            jq=["-c", "select(.status >= 500)"],
            check=lines_equal,
        ),
        dict(
            name="needle",
            desc="find one request_id (1 match)",
            lq=[f'request_id == "{sample_id}"'],
            jq=["-c", f'select(.request_id == "{sample_id}")'],
            check=lines_equal,
        ),
        dict(
            name="count-and",
            desc="count of error AND status>=500",
            lq=["-o", "tsv", 'level == "error" and status >= 500 | count'],
            jq=["-n", 'reduce (inputs | select(.level == "error" and .status >= 500)) as $_ (0; . + 1)'],
            check=lambda a, b: num_equal(a.splitlines()[1], b),
        ),
        dict(
            name="project",
            desc="select 3 fields from every record",
            lq=["select ts, level, msg"],
            jq=["-c", "{ts, level, msg}"],
            check=lines_equal,
        ),
        dict(
            name="group-count",
            desc="count by service",
            lq=["-o", "json", "count by service"],
            jq=["-n", "-c", "reduce inputs as $l ({}; .[$l.service] += 1)"],
            check=grouped(lambda r: r["service"], lambda r: r["count"]),
        ),
        dict(
            name="group-2key",
            desc="count by http.method, status (nested field)",
            lq=["-o", "json", "count by http.method, status"],
            jq=["-n", "-c", 'reduce inputs as $l ({}; .["\\($l.http.method) \\($l.status)"] += 1)'],
            check=grouped(lambda r: f'{r["http.method"]} {r["status"]}', lambda r: r["count"]),
        ),
        dict(
            name="group-stats",
            desc="count, avg, max of latency by service",
            lq=["-o", "json", "stats count() as n, avg(latency_ms) as avg, max(latency_ms) as m by service"],
            jq=[
                "-n",
                "-c",
                "reduce inputs as $l ({}; .[$l.service] |= ((. // {n: 0, s: 0, m: null}) | .n += 1 | .s += $l.latency_ms | .m = ([.m, $l.latency_ms] | max)))"
                " | map_values({n, avg: (.s / .n), m})",
            ],
            check=grouped(lambda r: r["service"], lambda r: r),
        ),
        dict(
            name="p99",
            desc="p99 latency by service (exact, nearest-rank)",
            lq=["-o", "json", "stats p99(latency_ms) as p by service"],
            # (`reduce ... .[k] += [x]` is quadratic in jq, so use the group_by idiom)
            jq=["-n", "-c", "[inputs | [.service, .latency_ms]] | group_by(.[0]) | map({key: .[0][0], value: (map(.[1]) | sort | .[(length * 0.99 | ceil) - 1])}) | from_entries"],
            check=grouped(lambda r: r["service"], lambda r: r["p"]),
        ),
        dict(
            name="regex",
            desc='count msg =~ "timeout|refused"',
            lq=["-o", "tsv", 'msg =~ "timeout|refused" | count'],
            jq=["-n", 'reduce (inputs | select(.msg | test("timeout|refused"))) as $_ (0; . + 1)'],
            check=lambda a, b: num_equal(a.splitlines()[1], b),
        ),
    ]


def run(cmd, stdin_path, out=subprocess.PIPE):
    with open(stdin_path, "rb") as f:
        t = time.perf_counter()
        p = subprocess.run(cmd, stdin=f, stdout=out, stderr=subprocess.PIPE)
        dt = time.perf_counter() - t
    if p.returncode != 0:
        raise RuntimeError(f"{cmd[0]} failed: {p.stderr.decode()[:500]}")
    return dt, (p.stdout.decode() if out == subprocess.PIPE else None)


def timeit(cmd, data, runs):
    times = []
    with open(os.devnull, "wb") as null:
        for _ in range(runs):
            times.append(run(cmd, data, null)[0])
    return statistics.median(times)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lines", type=int, default=1_000_000)
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--jq-runs", type=int, default=3)
    ap.add_argument("--jaq", default=shutil.which("jaq"))
    ap.add_argument("--only")
    ap.add_argument("--out", default=os.path.join(ROOT, "bench/RESULTS.md"))
    a = ap.parse_args()

    data = os.path.join(ROOT, f"bench/data/app-{a.lines}.log")
    if not os.path.exists(data):
        os.makedirs(os.path.dirname(data), exist_ok=True)
        with open(data, "wb") as f:
            subprocess.run([GEN, str(a.lines)], stdout=f, check=True)
    size = os.path.getsize(data)
    with open(data, "rb") as f:  # warm the page cache
        while f.read(1 << 24):
            pass
        f.seek(size // 3 * 2)
        f.readline()
        sample_id = json.loads(f.readline())["request_id"]

    tools = [("jq", "jq", a.jq_runs)]
    if a.jaq:
        tools.append(("jaq", a.jaq, a.jq_runs))

    rows = []
    for c in cases(sample_id):
        if a.only and c["name"] != a.only:
            continue
        lq = [LQ] + c["lq"] + [data]
        lq1 = [LQ, "-j1"] + c["lq"] + [data]
        lq_stdin = [LQ] + c["lq"]
        _, lq_out = run(lq, data)
        r = dict(case=c, lq=timeit(lq, data, a.runs), lq1=timeit(lq1, data, a.runs), lq_stdin=timeit(lq_stdin, data, a.runs))
        for name, exe, runs in tools:
            cmd = [exe] + c["jq"] + [data]
            _, out = run(cmd, data)
            ok = c["check"](lq_out, out)
            if not ok:
                print(f"!! {c['name']}: lq and {name} outputs differ", file=sys.stderr)
            r[name] = timeit(cmd, data, runs)
            r[name + "_ok"] = ok
        rows.append(r)
        print(
            f"{c['name']:<12} lq {r['lq']:.3f}s  lq-j1 {r['lq1']:.3f}s  lq-stdin {r['lq_stdin']:.3f}s  "
            + "  ".join(f"{n} {r[n]:.2f}s ({r[n] / r['lq']:.0f}x, 1-thread {r[n] / r['lq1']:.1f}x){'' if r[n + '_ok'] else ' MISMATCH'}" for n, _, _ in tools),
            flush=True,
        )

    cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip() or platform.processor()
    jqv = subprocess.run(["jq", "--version"], capture_output=True, text=True).stdout.strip()
    jaqv = subprocess.run([a.jaq, "--version"], capture_output=True, text=True).stdout.strip() if a.jaq else None
    md = [
        f"# Benchmark results\n",
        f"- input: {a.lines:,} lines, {size / 1e6:.0f} MB synthetic app logs (`gen-logs {a.lines}`)",
        f"- machine: {cpu}, {os.cpu_count()} cores; {jqv}" + (f", {jaqv}" if jaqv else ""),
        f"- median wall time; lq x{a.runs} runs, jq/jaq x{a.jq_runs}; output to /dev/null; warm page cache",
        f"- every row's output was checked equivalent to jq's (✓) before timing\n",
        "| case | what | lq | lq -j1 | lq (stdin) | jq | vs jq | vs jq, 1 thread |" + (" jaq | vs jaq | vs jaq, 1 thread |" if a.jaq else ""),
        "|---|---|---:|---:|---:|---:|---:|---:|" + ("---:|---:|---:|" if a.jaq else ""),
    ]
    for r in rows:
        line = (
            f"| {r['case']['name']} | {r['case']['desc']} | {r['lq'] * 1000:.0f} ms | {r['lq1'] * 1000:.0f} ms | {r['lq_stdin'] * 1000:.0f} ms "
            f"| {r['jq']:.2f} s | **{r['jq'] / r['lq']:.0f}x** {'✓' if r['jq_ok'] else '✗'} | {r['jq'] / r['lq1']:.1f}x |"
        )
        if a.jaq:
            line += f" {r['jaq']:.2f} s | **{r['jaq'] / r['lq']:.0f}x** {'✓' if r['jaq_ok'] else '✗'} | {r['jaq'] / r['lq1']:.1f}x |"
        md.append(line)
    with open(a.out, "w") as f:
        f.write("\n".join(md) + "\n")
    print(f"\nwrote {a.out}")


if __name__ == "__main__":
    main()
