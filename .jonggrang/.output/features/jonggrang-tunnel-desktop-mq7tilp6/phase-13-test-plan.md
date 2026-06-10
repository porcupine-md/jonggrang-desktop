# Phase 13 — Test Plan & Strategy

**Feature:** jonggrang-tunnel-desktop
**Date:** 2026-06-10
**Status:** Plan complete (strategy only — no tests authored in this phase)

---

## 1. Strategy

The architecture was deliberately built **pure-core / impure-edge** (see AGENTS.md
"Pure core, impure edge"). That makes the testing strategy a direct consequence of
the layering:

| Layer | File | Testability | Approach |
|-------|------|-------------|----------|
| Spec parse + port alloc + key resolve + log classify | `tunnel.rs` (pure fns) | Unit, no I/O | `#[test]` directly — **done** |
| Child-process lifecycle (spawn/probe/reap/status) | `tunnel.rs` (impure) | Integration | Inject fakes / local sockets — **gap** |
| serde DTOs + `From` conversions | `commands.rs` | Unit, no Tauri | `#[test]` on conversions — **done** |
| `#[tauri::command]` handlers + events | `commands.rs` | Integration | Needs `AppHandle`/`State` — **gap (accept)** |
| Webview UI | `src/main.ts` | E2E only | **Out of scope** (per plan) |

**Test command (canonical):** `cd src-tauri && cargo test` — runs in CI on both
matrix legs (`build.yml`).

**Local gotcha:** full `cargo test` links the WebKit/GTK stack and fails on a dev box
without the Linux build deps. Validate the pure module standalone without them:
`cd src-tauri && rustc --test --edition 2021 src/tunnel.rs -o /tmp/t && /tmp/t`
(this is how the 51 `tunnel.rs` tests have been verified through phases 8–12).

---

## 2. Current coverage (baseline — already implemented)

**`tunnel.rs` — 51 tests**, covering:
- `parse_target`: valid, trims ws, empty/missing-`@`/multiple-`@`/empty-user/empty-host/host-with-space errors.
- `parse_forwards`: single, leading-flag strip, multiple, inner ws, the `-c` vs `cache:80`
  container-id gotcha, empty/empty-entry/missing-colon/empty-container/invalid-port errors.
- `next_free_local_port`: preferred-when-free, skips-used, exhaustion error.
- `build_plan`: dashboard default 7777, distinct ports, all-distinct, dashboard-collision-bump,
  collision avoidance vs reserved + each other, end-to-end, target-error propagation, dashboard-first ordering.
- Key resolution: jonggrang precedence, project-first, global→default fallback, none-existing not-found listing,
  mode `0600` safety, `BadPermissions`→`chmod 600` hint, not-found lists searched paths.
- `classify_ssh_log_line`: established, key-permission gotcha, auth/host failures, ignores noise,
  failure-wins-over-established-on-same-line, actionable failure messages.
- `build_ssh_args`: expected shape + flag mapping.

**`commands.rs` — 5 tests**, covering:
- `ForwardView` renders status as wire string + covers every `ForwardStatus` variant.
- `TunnelStatus` serializes to camelCase JSON.
- `StartTunnelArgs` deserializes camelCase with optionals + optionals default to `None`.

Verdict: **all pure logic is covered.** The gaps below are all at the impure edge.

---

## 3. Gaps & prioritized test backlog

### P1 — `probe_local_port` against a real loopback socket (NEW, recommended)
The port probe is the load-bearing half of status inference, yet it has no test.
It is cheaply testable without ssh:
- **G1.1** bind a `std::net::TcpListener` on an ephemeral `127.0.0.1:0`, assert `probe_local_port`
  reports it reachable; drop the listener, assert it reports unreachable.
- **G1.2** an unbound high port reports unreachable (no hang — confirms the connect timeout is bounded).

*Why P1:* pure-ish, fast, deterministic, no ssh dependency; closes the highest-value gap.

### P2 — `TunnelManager` lifecycle without a live SSH server (NEW, recommended)
`start`/`stop`/`spawn_forward` currently hard-code `Command::new("ssh")`. To test the
spawn→track→reap state machine and the "failed start never leaks a partial set" guarantee,
spawn a **stand-in child** instead of real ssh:
- **G2.1** `stop()` is idempotent on an idle manager (no child) — testable today, no refactor.
- **G2.2** *(needs small seam)* inject the spawn program (e.g. `spawn_forward_with(program, …)` or a
  `TUNNEL_SSH_BIN` override) so a test points at `sleep`/a fake script: assert N forwards → N tracked
  pids, then `stop()` reaps all (pids gone, `forwards` empty).
- **G2.3** partial-failure: make the 2nd spawn fail (program that exits non-zero / bad path) and assert
  `self.forwards` stays untouched and the first child was reaped (no leak).

*Why P2:* this is the riskiest subsystem (child-process leaks were a named Risk). Requires a
1-line injection seam; keep it std-only so it stays in the `rustc --test` standalone path.

### P3 — `ForwardProcess::current_status` reconciliation (NEW, optional)
The strongest-first reconciliation (`try_wait` exit → port probe → last stderr signal) has no
direct test. Reachable via G2's fake-child seam: a child that exits non-zero → `Failed`; a child
holding a bound port → `Connected`; a live child with no bound port → `Connecting`.

### P4 — Tauri command handlers `start_tunnel`/`stop_tunnel`/`tunnel_status`/`list_forwards` (ACCEPT GAP)
These need a constructed `AppHandle`/`State` and emit `tunnel-status` events. Tauri's test harness
(`tauri::test::mock_builder`) exists but pulls in the WebKit link wall, so these cannot run in the
standalone path and add little beyond what the DTO + manager tests already cover. **Recommendation:
do not test directly.** The handlers are thin: lock → delegate to `TunnelManager` → `snapshot` →
`emit_status`; both ends are covered by P2 (manager) and the existing serde tests (snapshot DTO).

### P5 — Frontend `src/main.ts` (OUT OF SCOPE)
Per plan "Out of Scope": no full frontend test framework (Vitest only if a UI framework warrants it,
which it does not — static webview, `window.__TAURI__`). Pure helpers (`buildForwardsSpec`,
`readConfig`/`saveConfig`) *would* be unit-testable if a harness were ever added; note them as
the seam, but **leave untested for v1.** Manual smoke via the dashboard remains the acceptance path.

---

## 4. Regression / known-issue coverage

- **F1 (code-quality review):** `DASHBOARD_LOCAL_URL` is hardcoded `:7777` in `main.ts`, so the
  summary link ignores a 7777→7778 collision bump. The **backend** bump is already covered by
  `build_plan_dashboard_collision_bumps_up`. The frontend mismatch is only observable via E2E and
  stays uncovered under P5 — track it as a known limitation, not a unit-test target.
- **`-c` strip gotcha:** covered (`parse_forwards_container_starting_with_c_is_not_flag`). Keep this
  test as the canonical guard — it is the highest-regret edge.
- **SSH key `0600`/owner:** pre-flight covered (`mode_safe_only_…`, `bad_permissions_error_mentions_chmod_600`);
  the *runtime* stderr signal is covered by `classify_detects_key_permission_gotcha`. Both halves guarded.

---

## 5. Acceptance criteria for the test phase

1. `cd src-tauri && cargo test` green in CI (both matrix legs) — **already enforced by `build.yml`**.
2. Pure modules pass the standalone `rustc --test src/tunnel.rs` path on a dev box without WebKit deps.
3. If P1–P2 are implemented: no new `unsafe`, no real network/ssh dependency, tests deterministic
   (ephemeral ports, fake child program) and runnable in the standalone path.
4. No test reads private-key **contents** (only paths) — preserves the secret-safety invariant.

---

## 6. Recommendation

Coverage of the testable surface (all pure logic) is **already complete and green**. The remaining
gaps are impure-edge integration concerns. Recommended next-phase work, smallest-seam-first:

1. **P1** `probe_local_port` socket tests — no refactor, pure win.
2. **P2** `TunnelManager` lifecycle via an injectable spawn program — one seam, closes the
   child-leak risk, stays std-only.
3. **P3** ride P2's seam if time permits.
4. **P4/P5** explicitly accepted as gaps (Tauri-harness/WebKit cost and out-of-scope frontend).

Estimated new tests if P1–P3 land: ~8–10, all in the `tunnel.rs` standalone path. No change to the
canonical `cargo test` command or CI.
