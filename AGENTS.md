# AGENTS.md — Jonggrang-desktop

> This file is human-curated project knowledge for AI agents.
> Agents may propose updates, but humans approve them.
> Research shows human-written AGENTS.md improves agent success ~4%.

---

## Project Overview

- **Name**: Jonggrang-desktop (`jonggrang-tunnel-desktop`)
- **Type**: Tauri v2 desktop application (macOS + Linux)
- **Stack**: Tauri v2 — TypeScript webview frontend + Rust backend
- **Description**: A GUI front-end for SSH local-port forwarding into a jonggrang
  server. The app owns the forwarding logic by shelling out to the host's
  system `ssh -L <local>:localhost:<remote> user@server`, reusing the user's
  existing keys / agent / `known_hosts` and keeping private keys out of app
  memory. A dashboard webview handles first-launch setup and live tunnel status;
  the dashboard forward defaults to `http://localhost:7777`.

---

## Conventions

### File Structure
```
src/                     # TypeScript webview frontend (static, NO bundler)
├── index.html           # Dashboard markup (served as Tauri frontendDist)
├── main.ts              # UI logic — calls Tauri commands via window.__TAURI__
└── main.js              # tsc emit of main.ts (build artifact; gitignored)

src-tauri/               # Rust backend (Cargo crate)
├── Cargo.toml           # crate = jonggrang_tunnel_lib (lib + bin split)
├── build.rs             # Tauri codegen build script
├── tauri.conf.json      # Tauri v2 config (frontendDist=../src, withGlobalTauri)
├── icons/               # App icons (.png/.ico/.icns)
└── src/
    ├── main.rs          # Thin bin shim → jonggrang_tunnel_lib::run()
    ├── lib.rs           # run(): Tauri builder + invoke_handler + exit hook
    ├── tunnel.rs        # PURE spec parsing/port alloc + TunnelManager lifecycle
    └── commands.rs      # #[tauri::command] bridge + serde DTOs + status events

.github/workflows/
└── build.yml            # mac+linux release build matrix (CI)
```

Notes:
- The frontend is a **static** dir (no Vite/dev server). `tauri.conf.json` sets
  `build.frontendDist = "../src"`, and `app.withGlobalTauri = true` so the
  webview reaches Tauri via `window.__TAURI__` instead of bare ESM imports.
- `tunnel.rs` is kept **std-only / serde-free** so it is unit-testable in
  isolation; serde DTOs and the Tauri boundary live in `commands.rs`.

### Naming Conventions
- Files: `kebab-case.ts`
- Components: `PascalCase.tsx`
- Functions: `camelCase`
- Constants: `UPPER_SNAKE_CASE`
- Database tables: `snake_case`
- API endpoints: `kebab-case`

### Code Patterns
- **Layering**: webview UI → `#[tauri::command]` (commands.rs) → `TunnelManager`
  (tunnel.rs) → spawned system `ssh -L` child processes. Each layer only talks to
  its neighbour.
- **Pure core, impure edge**: tunnel-spec parsing and local-port allocation are
  pure functions (no spawn, no net, no OS probe) in `tunnel.rs`. Keep them that
  way — it is what makes `cargo test` meaningful without a live SSH server.
- **Serde at the boundary only**: `tunnel.rs` stays std-only. DTOs
  (`StartTunnelArgs`, `TunnelStatus`, `ForwardView`) live in `commands.rs` with
  `#[serde(rename_all = "camelCase")]`; convert via `impl From<…>` at the edge.
- **One ssh child per forward** (not a single multi-`-L`) so each forward has
  independent status and teardown.
- **Teardown is RAII**: `impl Drop for ForwardProcess` kills + reaps the child;
  `TunnelManager::stop()` just clears the vec. `lib.rs` also calls `stop()` on
  Tauri `RunEvent::Exit` as the explicit hook; Drop is the backstop.
- **Status is inferred** (not reported): combine ssh `-v` stderr parsing with an
  active loopback port probe, reconciled strongest-first.
- **Events over polling**: every state change emits a `tunnel-status` event; the
  same `TunnelStatus` is both the command return value and the event payload.
- **Frontend**: type-only imports (`import type { … }`) so `tsc` erases them and
  the emitted `main.js` has zero bare ESM specifiers; runtime goes through
  `window.__TAURI__`.

### Testing Conventions
- Framework: Rust built-in `#[test]` (no JS test framework configured).
- Command: `cd src-tauri && cargo test` (runs in CI on both matrix legs).
- Pattern: pure functions are tested directly; FS/OS interactions are tested via
  injected predicates (e.g. `resolve_ssh_key_with(cands, exists_fn)`) so key
  precedence is covered without touching the filesystem.
- **Local gotcha**: full `cargo test` needs the Linux WebKit build deps (see
  Development Setup). Without them, validate a pure module standalone:
  `cd src-tauri && rustc --test --edition 2021 src/tunnel.rs -o /tmp/t && /tmp/t`.

---

## Known Gotchas

- **Linux build-dep wall**: `cargo build`/`tauri build`/full `cargo test` fail
  locally until `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `build-essential`,
  `librsvg2-dev` are installed (tauri/wry link the WebKit/GTK stack; libdbus is
  hit first). CI installs these; on a dev box it is a manual (sudo) prerequisite.
- **No bundler → no bare imports**: the webview cannot resolve
  `@tauri-apps/api/core` at runtime (node_modules is not in `frontendDist`). Use
  `window.__TAURI__` (enabled by `withGlobalTauri`) for runtime, `import type`
  for types only.
- **`tsc` must emit**: `tsconfig` has `noEmit: false` because
  `beforeBuildCommand: "tsc"` both typechecks AND emits `src/main.js` into the
  served dir. `src/*.js` is gitignored (build artifact — never commit it).
- **`-c` flag stripping**: strip a leading `-c` only when it stands alone, NEVER
  when it prefixes a container id like `cache:80` / `-cache:80`.
- **SSH key `0600`/owner**: surfaces twice — a pre-flight `BadPermissions` error
  (with a `chmod 600` hint) and a runtime stderr signal ("bad owner or
  permissions"). The frontend shows the backend's `Err` string verbatim.
- **Only the key PATH is passed to ssh** (`-i <path>`); key contents are never
  read into app memory.
- **App SIGKILL can still leak ssh children** — RAII Drop and the exit hook cover
  stop/normal-exit/panic-unwind, but a hard kill bypasses both.
- **Per-forward Start/Stop is global only**: the backend exposes global
  `start_tunnel`/`stop_tunnel`; per-forward cards expose Open/Copy only. A future
  `stop_forward` command would be needed for per-forward control.

---

## Architecture Decisions

- **Shell out to system `ssh -L`** rather than a native Rust SSH crate (`russh`).
  Reuses the user's keys / agent / `known_hosts`, keeps secrets out of process
  memory, and matches jonggrang's own key-resolution model. The tradeoff —
  inferred connection state and child-process lifecycle management — is accepted.
- **Key-resolution precedence**: `~/.jonggrang/web/ssh/<id>.key` →
  `~/.jonggrang/web/ssh/global.key` → `~/.ssh/id_rsa`, modeling
  `.jonggrang/lib/sandbox.js`. An explicit `keyPath` from the UI overrides this.
- **Port strategy**: the dashboard forward is allocated FIRST and keeps the
  canonical local `7777` (bumping up only on collision); each `-c` forward fans
  out from `7778` via monotonic next-free-port scanning against a reserved set.
- **CI-only macOS builds**: Tauri cannot cross-compile a macOS `.app`/`.dmg` from
  Linux (needs the Apple SDK + codesign on a macOS host), so a
  `macos-latest` + `ubuntu-latest` GitHub Actions matrix is the canonical way to
  produce both platform binaries. The local Linux box builds Linux bundles only.
- **Secret safety**: only the key PATH is passed to ssh (`-i`); key contents are
  never read into the app or agent context.
- **Tauri v2 lib/bin split**: `lib.rs::run()` owns the builder + invoke_handler +
  exit hook; `main.rs` is a thin shim. Tauri pinned to `^2` / `@tauri-apps/api`
  `^2` to avoid v1/v2 config drift.

---

## Dependencies & Integrations

- **System `ssh`** (OpenSSH): spawned as `ssh -i <key> -N -v -o BatchMode=yes -o
  ExitOnForwardFailure=yes -o ServerAliveInterval=15 -L <local>:localhost:<remote>
  user@host`. Must be on `PATH`. No SSH library dependency.
- **SSH keys**: resolved from `~/.jonggrang/web/ssh/` then `~/.ssh/id_rsa` (see
  Architecture Decisions). Only the path is used; contents are never read.
- **Tauri v2** (`@tauri-apps/api` ^2 runtime, `@tauri-apps/cli` ^2 dev,
  `typescript` ^5). The webview integrates via `window.__TAURI__`.
- **GitHub Actions** (`tauri-apps/tauri-action`, `actions/checkout`,
  `actions/setup-node`, `dtolnay/rust-toolchain`, `Swatinem/rust-cache`,
  `actions/upload-artifact`) — produces and publishes the mac/linux binaries.

---

## Development Setup

```bash
# 1. Install the Linux build deps (one-time, sudo) — NOT auto-installed.
#    These satisfy the WebKit/GTK stack tauri/wry link against.
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev libsoup-3.0-dev build-essential librsvg2-dev

# 2. Install the Tauri CLI. It ships as the @tauri-apps/cli dev dependency
#    (invoked via `npm run tauri`), or install the Rust CLI globally:
#    cargo install tauri-cli   # gives `cargo tauri`

# 3. Frontend + Rust deps.
npm install

# 4. Run in dev (typechecks + serves src/ + builds the Rust backend).
npm run dev            # = tauri dev

# 5. Build release bundles (Linux: .deb/.AppImage). macOS bundles only build
#    on a macOS host — use the CI workflow for those (see README).
npm run build          # = tsc && tauri build

# Run Rust tests:
cd src-tauri && cargo test
```

---

## Jonggrang Workflow

Jonggrang uses a **two-phase planning** flow so humans can review and edit a plan before AI decomposes it into tasks.

### Full workflow

```bash
# Phase 1 — generate a human-readable draft plan
jonggrang plan "add JWT authentication"
# → AI writes .jonggrang/plan.md (high-level, no tasks yet)
# → Interactive options:
#     Approve           → run Phase 2 immediately
#     Edit with AI      → describe changes, AI revises plan, loop back
#     Edit in $EDITOR   → open editor, loop back
#     Save draft        → exit, run "jonggrang approve" later
#     Abort             → discard plan.md

# Resume after accidental close:
jonggrang plan
# → no description → shows list of pending + archived plans
# → pick one → shows plan + interactive options again

# Phase 2 — approve plan → decompose into tasks
jonggrang approve
# → AI reads .jonggrang/plan.md → runs `jonggrang task import` to create tasks
# → plan.md is archived to .jonggrang/.output/features/<featureId>/plan.md

# Execute tasks
jonggrang work
```

### Shorthand options

```bash
# Plan + auto-approve + tasks in one shot (skips human review)
jonggrang plan "add JWT auth" --yes

# Deep mode: 3-phase analysis (discovery + brainstorm + condense) → enriched plan
# Adds Affected Areas, Risks, and Alternatives Considered sections to plan.md
jonggrang plan "add JWT auth" --deep

# Deep mode + auto-approve in one shot
jonggrang plan "add JWT auth" --deep --yes

# Full pipeline: plan → approve → execute in one shot
jonggrang work "add JWT auth" --yes

# Execute existing tasks only (skip pending plan warning)
jonggrang work --ignore-plan
```

### Modifying an approved plan

| Situation | Command |
|-----------|---------|
| Add new scope on top of done work | `jonggrang plan "also add rate limiting"` |
| Change remaining pending work | `jonggrang plan "use Passport.js instead"` |
| Undo completed tasks | Not supported — create new tasks to override |

**Rule: completed tasks are immutable.** They reflect real code. Any correction must be a new task that fixes/replaces the previous implementation.

### Plan file format

```markdown
---
feature: jwt-auth
branch: feat/jwt-auth
work_type: MEDIUM
description: JWT authentication with login, register, refresh
created_at: 2026-04-16T10:30:00Z
---

# Plan: JWT Authentication

## Approach
...

## Phases
1. DB schema — users + refresh_tokens
2. Auth service — register/login/refresh
3. JWT middleware
...

## Key Decisions
- Token storage: httpOnly cookie

## Out of Scope
- OAuth, 2FA, email verification
```

---

## Bug Reporting

When you discover a defect **outside the scope of your current task**, report it immediately:

```bash
# Report a bug and create a BUGFIX task in one shot
jonggrang bug "description of what is broken" --feature <feature_id>
# When asked "Create a task now?" → y

# Or save for later (batch convert)
jonggrang bug "description" --feature <feature_id>
# When asked "Create a task now?" → n
jonggrang bug convert --feature <feature_id>   # converts all open bugs to tasks later
```

Get the `feature_id` by running: `jonggrang task show <id>` — look for the `feature_id` field in the output.

**Rules:**
- Do NOT fix out-of-scope bugs inline — stay focused on your current task
- Report real defects only (crashes, wrong return values, broken edge cases)
- Do NOT report style issues, TODOs, or future features — those go in the plan

Bug reports are saved to `.jonggrang/.output/features/<feature_id>/bugs.md` and can be viewed with:
```bash
jonggrang bug list
```

---

## Task Management CLI

Use the `jonggrang task` CLI to manage tasks instead of editing `.jonggrang/jonggrang-tasks.json` directly.

### Commands

```bash
# List & inspect
jonggrang task list                         # list all tasks (JSON output)
jonggrang task list pending                 # filter by status
jonggrang task show task-001                # show task detail
jonggrang task next                         # show next eligible task

# Create & modify
jonggrang task add --title "Add login page" --priority 1
jonggrang task add --title "Write tests" --blocked-by task-001
jonggrang task update task-001 --status in_progress
jonggrang task update task-001 --files src/login.ts,src/login.test.ts

# Complete & block
jonggrang task done task-001                # mark completed + passes=true
jonggrang task block task-002 --reason "Waiting for API spec"

# Remove (cleans up blocked_by refs)
jonggrang task remove task-003
```

### Output

- Default output is **JSON** (machine-readable for agents)
- Add `--pretty` for human-readable table format
- Add `--json` to force JSON when in a TTY

### Available flags for add/update

| Flag | Description |
|------|-------------|
| `--title` | Task title |
| `--desc` | Task description |
| `--priority` | Priority (1 = highest) |
| `--status` | pending, in_progress, completed, blocked, waiting, skipped |
| `--skill` | Skill name |
| `--blocked-by` | Comma-separated dependency task IDs |
| `--files` | Comma-separated file paths |
| `--reason` | Reason (used with `block`) |

---

## Jonggrang Notes

This section is updated by Jonggrang during work sessions.
Human should review and curate periodically.

### Patterns Discovered
<!-- Agent appends here, human curates -->

### Gotchas Discovered
<!-- Agent appends here, human curates -->
