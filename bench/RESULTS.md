# Benchmark results

- input: 1,000,000 lines, 281 MB synthetic app logs (`gen-logs 1000000`)
- machine: Apple M2, 8 cores; jq-1.7.1-apple, jaq 3.1.1
- median wall time; lq x7 runs, jq/jaq x3; output to /dev/null; warm page cache
- every row's output was checked equivalent to jq's (✓) before timing

| case | what | lq | lq -j1 | lq (stdin) | jq | vs jq | vs jq, 1 thread | jaq | vs jaq | vs jaq, 1 thread |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| filter-eq | records where level == "error" (5%) | 21 ms | 75 ms | 29 ms | 4.31 s | **201x** ✓ | 57.5x | 2.38 s | **111x** ✓ | 31.7x |
| filter-num | records where status >= 500 (~5.5%) | 49 ms | 208 ms | 56 ms | 4.42 s | **91x** ✓ | 21.3x | 2.41 s | **49x** ✓ | 11.6x |
| needle | find one request_id (1 match) | 21 ms | 73 ms | 29 ms | 3.90 s | **183x** ✓ | 53.5x | 2.22 s | **105x** ✓ | 30.5x |
| count-and | count of error AND status>=500 | 25 ms | 93 ms | 30 ms | 3.89 s | **157x** ✓ | 41.8x | 2.31 s | **94x** ✓ | 24.8x |
| project | select 3 fields from every record | 48 ms | 167 ms | 52 ms | 6.46 s | **135x** ✓ | 38.8x | 4.39 s | **92x** ✓ | 26.4x |
| group-count | count by service | 32 ms | 116 ms | 38 ms | 6.11 s | **188x** ✓ | 52.9x | 2.17 s | **67x** ✓ | 18.7x |
| group-2key | count by http.method, status (nested field) | 57 ms | 246 ms | 62 ms | 6.93 s | **121x** ✓ | 28.2x | 3.00 s | **52x** ✓ | 12.2x |
| group-stats | count, avg, max of latency by service | 61 ms | 247 ms | 69 ms | 12.48 s | **204x** ✓ | 50.6x | 5.20 s | **85x** ✓ | 21.1x |
| p99 | p99 latency by service (exact, nearest-rank) | 68 ms | 243 ms | 69 ms | 7.08 s | **104x** ✓ | 29.1x | 3.98 s | **58x** ✓ | 16.4x |
| regex | count msg =~ "timeout|refused" | 58 ms | 174 ms | 72 ms | 5.92 s | **102x** ✓ | 34.1x | 5.17 s | **89x** ✓ | 29.8x |
