# Phase 12 — Code-Quality Review (Maintainability)

**Feature:** jonggrang-tunnel-desktop
**Scope:** the implemented feature surface — `src-tauri/src/{tunnel,commands,lib,main}.rs`,
`src/main.ts`, `src/index.html` (tooling under `.claude/`, `.codex/`, `.opencode/`,
`hooks/` is generated scaffolding and out of review scope).
**Date:** 2026-06-10

## Verdict: PASS — maintainable, ship-ready

The feature code is high quality and easy to maintain. Validation both gates green:

- **Rust pure module:** 51/51 tests pass via the documented standalone workaround
  (`rustc --test --edition 2021 src/tunnel.rs`). Full `cargo test` is blocked locally
  only by the WebKit/GTK build-dep wall (CI covers it).
- **Frontend:** `tsc --noEmit` clean.

Findings below are **non-blocking** polish items. None warrant holding the phase.

## Strengths (keep doing this)

- **Clean layering, honored end to end:** webview → `#[tauri::command]` → `TunnelManager`
  → `ssh -L` children. Each layer only talks to its neighbour.
- **Pure core / impure edge:** spec parsing + port allocation are deterministic, serde-free,
  and exhaustively unit-tested; spawning/probing/reaping are thin impure wrappers. This is
  what makes the test suite meaningful without a live SSH server.
- **RAII teardown** (`Drop for ForwardProcess`) plus the explicit `RunEvent::Exit` hook — the
  only leak path (app `SIGKILL`) is documented, not hidden.
- **Typed errors with actionable `Display`** carried verbatim to the UI (the `0600`/owner
  gotcha surfaces as a `chmod 600` hint). camelCase wire contract is pinned by a serde test.
- **Thorough doc comments** that explain *why* (BatchMode, ExitOnForwardFailure, failure-wins
  classification ordering), not just *what*.

## Findings

### F1 — Dashboard summary link is hardcoded to 7777, ignoring collision bumps (low-medium)
`src/main.ts:49` pins `DASHBOARD_LOCAL_URL = "http://localhost:7777"` and the summary link
(`index.html:253`, wired at `main.ts:480`) always opens that literal. But the backend bumps
the dashboard forward to 7778+ on a 7777 collision (`tunnel.rs:304`, test
`build_plan_dashboard_collision_bumps_up`). The per-forward **card** correctly uses the live
`forward.localUrl`; only the top **summary** link can go stale and open the wrong port.
*Recommend:* derive the summary link/text from the dashboard forward in the latest
`TunnelStatus` (match `containerId === "dashboard"`) instead of the constant.

### F2 — Local-port probe runs while the manager mutex is held (low)
`snapshot()` (`commands.rs:119`) holds the `Mutex<TunnelManager>` across `mgr.health()`, which
calls `probe_local_port()` (a blocking TCP connect) per forward. Loopback refusals return
instantly so this is fast in practice, but synchronous I/O under a global lock that every
command shares is a latent responsiveness footgun as forward counts grow.
*Recommend (if it ever bites):* clone the forward descriptors, drop the lock, then probe.

### F3 — `reserved` is always empty; no live OS re-probe before bind (low, by-design)
`start_tunnel` (`commands.rs:152`) passes an empty `reserved` set, and `TunnelManager::start`
trusts the pure plan rather than re-checking live OS ports. A port occupied between planning
and bind surfaces only as a `Failed` status via `ExitOnForwardFailure`. This is the accepted
tradeoff of the inferred-status design — flagging only because the inline comment ("the
OS-level probe lives in the lifecycle manager") slightly oversells what `start` does.

### F4 — Empty `projectId` yields a `".key"` candidate path (low, cosmetic)
`resolve_default_ssh_key(args.project_id.as_deref().unwrap_or(""))` (`commands.rs:159`)
produces `~/.jonggrang/web/ssh/.key` as the first candidate when no project id is given.
Harmless (it won't exist; resolution falls through to `global.key` → `id_rsa`), but a guard
that skips the per-project candidate on an empty id would read cleaner.

### F5 — `StartTunnelArgs.forwards` doc comment drifts (trivial)
The doc at `commands.rs:51` muses about a dashboard-only mode "once that is supported." Today
the parser requires ≥1 forward and the frontend enforces it; trim the speculative half so the
contract reads unambiguously.

## Cross-cutting note: Rust↔TS constant duplication
`dashboard` / `7777` live independently in `tunnel.rs`, `main.ts`, and `index.html`. This is
inherent to the no-bundler / no-codegen choice (documented in AGENTS.md) and acceptable, but
it is the root cause of F1 — prefer reading these values from backend status at runtime over
re-declaring them in the frontend.
