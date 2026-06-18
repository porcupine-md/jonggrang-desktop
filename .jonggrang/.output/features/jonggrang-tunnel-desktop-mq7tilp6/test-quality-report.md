# Phase 16 — Test Quality Report

**Feature:** jonggrang-tunnel-desktop
**Date:** 2026-06-10
**Phase purpose:** No low-value tests, correct assertions
**Verdict:** ✅ PASS — verification-only, no source changes required

## Scope reviewed

| File | Module | Tests |
|------|--------|-------|
| `src-tauri/src/tunnel.rs` | `mod tests` (pure spec/port core) | 31 |
| `src-tauri/src/tunnel.rs` | `mod lifecycle_tests` (key/log/manager) | 24 |
| `src-tauri/src/commands.rs` | `mod tests` (serde DTO boundary) | 5 |
| **Total** | | **60** |

Standalone run of the pure core (`rustc --test src/tunnel.rs`): **55 passed,
0 failed**. The 5 `commands.rs` DTO tests require the full WebKit/GTK stack to
link (not installable locally; covered in CI) — they are simple serde
round-trips with no logic to regress.

## Quality assessment

**No low-value tests found.** Every test exercises a distinct behavior:

- **Behavior, not implementation** — tests assert observable outputs (parsed
  structs, allocated ports, error variants, wire JSON, child PIDs/status), never
  internal call sequences.
- **Correct, specific assertions** — equality against exact expected values
  (e.g. deterministic port fan-out `7778/7779/7780`, `127.0.0.1:7777` probe
  addr, camelCase wire keys), not just `is_ok()`/non-null smoke checks.
- **Edge cases + documented gotchas covered** — `-c` flag stripping vs. a
  container literally named `-cache` (the AGENTS.md gotcha), 0600 permission
  boundaries, failure-wins-over-established on a pathological ssh log line, port
  exhaustion, collision avoidance against reserved + sibling forwards.
- **FS/OS isolated via injection** — `resolve_ssh_key_with(cands, exists_fn)`
  and `start_with(plan, key, spawn_fn)` let key precedence and the full
  spawn/track/reap lifecycle be tested without a live SSH server.
- **Concurrency tests are race-aware** — `current_status_reports_failed...`
  polls with a bounded loop (200×10ms) so a regression fails fast instead of
  hanging; the bound is documented. `probe_bound`/`probe_unbound` are split with
  a comment explaining why pairing them would race on kernel state.

### Apparent overlaps — checked, all justified

- `forward_status_display_is_stable` (Display impl) vs.
  `forward_view_covers_every_status_variant` (From conversion) — different code
  paths producing the same wire strings; both contracts worth pinning.
- `build_plan_default_dashboard_port` asserts both `DASHBOARD_LOCAL_PORT` **and**
  literal `7777` — intentional: pins the literal so a silent constant change is
  caught.
- `manager_is_send_and_sync` — compile-time assertion; valuable for a type held
  across the Tauri command boundary (threaded).

## Conclusion

The suite is well-targeted: high assertion specificity, meaningful edge-case and
gotcha coverage, no tautological or redundant tests. No remediation needed.
`jonggrang task next` → null (phase-driven; no task with a `files[]` array). No
source files changed.
