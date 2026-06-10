# Phase 17 — Completion Report

**Feature:** jonggrang-tunnel-desktop
**Branch:** `feat/jonggrang-tunnel-desktop`
**Date:** 2026-06-10

## Task status
All 8 tasks `completed` with `passes=true` (task-001 … task-008). `jonggrang task next` → null.

## Final verification
- Pure-core suite (std-only `tunnel.rs`, runnable without WebKit deps):
  `rustc --test --edition 2021 src/tunnel.rs` → **55 passed / 0 failed**.
- Full `cargo test` (adds 5 `commands.rs` serde DTO tests + `lib.rs`) requires the
  Linux WebKit/GTK stack — CI-only by design (see AGENTS.md "Linux build-dep wall").
  Runs on both legs of the mac+linux matrix in `.github/workflows/build.yml`.
- Line coverage on the pure core measured previously at 91.10% (threshold 80% → PASS).

## Cleanup
- Committed `package-lock.json` + `src-tauri/Cargo.lock` (commit `6a9f7f7`).
  **Required:** CI runs `npm ci`, which fails without `package-lock.json`;
  `Swatinem/rust-cache` keys off `Cargo.lock`. Both were untracked before this phase.
- Removed a stray `*.profraw` coverage artifact before committing.
- Working tree clean except `.jonggrang/` (orchestrator-managed state, tracked on
  `main`, left to the jonggrang CLI — not folded into this code branch).

## PR — BLOCKED (environment)
Could not push or open a PR from this environment:
- `git ls-remote origin` → `Host key verification failed` (github.com not in
  known_hosts; no push credentials in this sandbox).
- `gh pr list` → repository `porcupine-md/jonggrang-desktop` does not resolve via API.

The branch is merge-ready. To open the PR from an authorized host:
```bash
git push -u origin feat/jonggrang-tunnel-desktop
gh pr create --base main --head feat/jonggrang-tunnel-desktop \
  --title "feat: Jonggrang tunnel desktop app (Tauri v2)" \
  --body "GUI SSH local-port forwarding into a jonggrang server. See AGENTS.md / README."
```

## Result
Verification PASS, cleanup done, branch ready. PR creation deferred to an
authorized environment (remote unreachable from sandbox).
