# tests: lessons learned

### Flaky "tail -f"-style test after a rebuild (macOS first-launch latency)
* **Context:** lq's `--follow` integration test spawned `lq -f`, slept 300 ms, then appended lines. It failed intermittently, and only on the first run after `cargo` rebuilt the binary.
* **Root Cause:** macOS scans a newly built executable on its first launch, which can push startup past a fixed sleep. A follower that starts at EOF then misses the lines appended before it was ready.
* **Fix:** Never sleep a fixed time to wait for a spawned follower. Do a readiness handshake: keep appending marker lines until one shows up on the child's stdout, drain the extras, then run the real assertions.
