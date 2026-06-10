---
feature: jonggrang-tunnel-desktop
branch: feat/jonggrang-tunnel-desktop
work_type: LARGE
description: Tauri v2 desktop app (macOS + Linux) providing a GUI for SSH tunneling into a jonggrang server
created_at: 2026-06-10T08:34:24.518Z
depth: deep
---

# Plan: Jonggrang Tunnel Desktop App

## Approach
Build a greenfield Tauri v2 desktop application (macOS + Linux) that acts as a GUI front-end for SSH local-port forwarding into a jonggrang server. Rather than wrapping a non-existent `jonggrang web tunnel` CLI, the app owns the forwarding logic by shelling out to the host's system `ssh -L <local>:localhost:<remote> user@server` — reusing the user's existing keys/agent/known_hosts exactly as jonggrang's own tooling does and keeping private keys out of app memory. The Rust backend runs a stateful child-process lifecycle manager (spawn/track/health/kill) plus pure, testable tunnel-spec parsing and local-port allocation; a TypeScript webview dashboard handles first-launch setup and live status, defaulting the dashboard forward to `http://localhost:7777`. Distributable macOS and Linux binaries are produced by a dedicated GitHub Actions build workflow (a `macos-latest` + `ubuntu-latest` matrix), since Tauri cannot cross-compile macOS from Linux — this CI is the canonical way to obtain both platform binaries and uploads them as build artifacts.

## Phases
1. Scaffold & toolchain — Create the Tauri v2 project skeleton (package.json, tsconfig.json, src-tauri/Cargo.toml, tauri.conf.json, main.rs, icons), pin Tauri v2 + `@tauri-apps/api` v2, install `@tauri-apps/cli` dev dep, document missing Linux build deps as a setup prerequisite, and update .gitignore.
2. Tunnel spec & port allocation (pure logic) — Parse `<user>@<server>` and the `-c <cid>:port,<cid2>:port` list into a typed tunnel plan; allocate distinct local ports (dashboard defaults to 7777) with collision avoidance. Cover with `cargo test`.
3. SSH tunnel lifecycle manager (tunnel.rs) — Spawn `ssh -L` child(ren), track PIDs, infer connection state (stderr parse + local-port probe), expose health, and guarantee teardown on stop and on app exit. Resolve the SSH key with jonggrang's precedence.
4. Tauri commands bridge (commands.rs) — Expose `#[tauri::command]` handlers for start/stop/status/list to the webview and emit live state events to the frontend.
5. Dashboard UI (src/) — First-launch setup form (server, key picker, container forwards) and a live status view showing each forward, its local URL (dashboard → http://localhost:7777), and start/stop controls. Match AGENTS.md naming conventions.
6. GitHub Actions build for mac + linux binaries — Add `.github/workflows/build.yml` that builds the app for **both macOS and Linux** using `tauri-apps/tauri-action` over a `macos-latest` + `ubuntu-latest` matrix. The Ubuntu job first installs the Linux build deps (`libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `build-essential`, `librsvg2-dev`); both jobs check out, set up Node + a Rust toolchain, cache cargo/npm, run `cargo test`, build the release bundles (`.dmg`/`.app` on macOS, `.deb`/`.AppImage` on Linux), and upload them via `actions/upload-artifact` (and attach them to a GitHub Release on tag pushes). Triggers: push/PR on the feature branch plus version tags.
7. Docs & conventions — Fill in AGENTS.md TODOs (stack/structure/setup/architecture), rewrite README.md with usage + build instructions (including how to download the CI-built mac/linux binaries from the GitHub Actions artifacts/releases), and record key decisions.

## Key Decisions
- SSH transport: shell out to system `ssh -L` rather than a native Rust SSH crate — reuses user keys/agent/known_hosts, keeps secrets out of app memory, matches jonggrang's own key-resolution model; accept stderr-parsing/lifecycle complexity as the tradeoff.
- Tauri version: target Tauri v2 with `@tauri-apps/api` v2; pin versions to avoid v1/v2 config drift.
- Key resolution precedence: `~/.jonggrang/web/ssh/<id>.key` → `~/.jonggrang/web/ssh/global.key` → `~/.ssh/id_rsa`, modeling `.jonggrang/lib/sandbox.js`; surface the `0600`/owner gotcha in UI errors.
- Port strategy: dashboard forward defaults to local 7777; each `-c` entry gets a distinct auto-allocated local port with collision detection; UI shows the resulting URLs.
- Binary builds via GitHub Actions: a dedicated `build.yml` workflow using `tauri-apps/tauri-action` is the canonical way to produce both macOS and Linux binaries. A `macos-latest` + `ubuntu-latest` matrix is required because Tauri cannot cross-compile macOS from Linux (needs Apple SDK/codesign); the local box only builds Linux. The workflow uploads platform bundles as artifacts and attaches them to GitHub Releases on tag pushes.
- Testability: keep tunnel-spec parsing and port allocation as pure Rust functions so `cargo test` covers them without live SSH; live forwarding stays integration-level. CI runs `cargo test` on both matrix legs.
- Secret safety: never read private-key contents into app/context; pass only the key path to the spawned ssh process, honoring the harness's sensitive-file hooks.

## Affected Areas
- Create (Rust backend): `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, `src-tauri/build.rs`, `src-tauri/src/main.rs`, `src-tauri/src/tunnel.rs`, `src-tauri/src/commands.rs`, `src-tauri/icons/`
- Create (frontend): `src/index.html`, `src/main.ts`
- Create (tooling/CI): `package.json`, `tsconfig.json`, `.github/workflows/build.yml` (GitHub Action that builds mac + linux binaries)
- Update: `AGENTS.md` (fill stack/structure/setup/architecture TODOs), `README.md` (usage + build instructions, plus how to fetch CI-built mac/linux binaries), `.gitignore` (add `node_modules/`, `target/`, `dist/`, `src-tauri/target/`)
- Reference only (do NOT modify): `.jonggrang/lib/sandbox.js` (SSH key resolution model), `.jonggrang/jonggrang.json` (project config)

## Risks
- No upstream tunnel command exists: `jonggrang web tunnel` is not a real subcommand — the app must implement forwarding itself. Mitigation: own the `ssh -L` logic; treat the command form as target UX, not a dependency.
- Connection state is inferred, not reported: shelling out means state must be derived from ssh stderr parsing + local-port probing, which is brittle. Mitigation: combine stderr `-v` parsing with active port probes and surface clear status to the UI.
- Child-process leaks: spawned ssh children must be reaped on stop, app crash, and exit. Mitigation: track PIDs and guarantee teardown hooks on app exit.
- Tauri cannot cross-compile macOS from Linux (needs Apple SDK/codesign). Mitigation: the GitHub Actions build workflow uses a macos-latest runner for the macOS binary; local box builds Linux only.
- CI build environment drift: the Ubuntu runner needs the same Linux build deps as a local box, and runner images change over time. Mitigation: pin the action versions and explicitly `apt-get install` `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `build-essential`, `librsvg2-dev` in the Linux job; cache cargo/npm to keep builds reproducible.
- Missing local Linux build deps: `webkit2gtk-4.1/4.0` and `libsoup-3.0` are absent, so `tauri dev`/`build` fail until installed. Mitigation: document `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `build-essential`, `librsvg2-dev` as a manual (possibly-sudo) setup prerequisite.
- Secret handling: SSH involves credentials and the harness has sensitive-file/command hooks. Mitigation: pass only the key path to ssh; never read key contents into app or context.
- Tauri v1/v2 config drift: mixing versions breaks config. Mitigation: pin Tauri v2 and `@tauri-apps/api` v2 explicitly.

## Alternatives Considered
- Option 2 — Native Rust SSH library (`russh`): not chosen for v1; would require reimplementing auth, agent integration, and known_hosts handling — high effort and risk, diverges from jonggrang's key resolution, and pulls credentials into process memory against the secret-handling guidance.
- Option 3 — Thin wrapper over a `jonggrang web tunnel` CLI: a non-starter because the subcommand does not exist upstream (`jonggrang web` is only a local Kanban dashboard server); there is nothing to wrap.
- Build distribution — manual local builds only: rejected in favor of the GitHub Actions matrix, since macOS binaries cannot be produced on the local Linux box and CI gives reproducible, downloadable artifacts for both platforms.

## Out of Scope
- Implementing a native `jonggrang web tunnel` CLI subcommand upstream.
- A native/pure-Rust SSH client implementation (Option 2 deferred).
- Windows builds (the GitHub Action matrix and feature target macOS + Linux only).
- Mobile (iOS/Android) targets.
- Code-signing/notarization of the macOS binary and Apple Developer account setup (CI produces unsigned bundles for now).
- Managing or provisioning the remote jonggrang server, containers, or remote services themselves — the app only forwards ports.
- Persisting/syncing credentials or a secrets vault beyond pointing ssh at an existing key path; no key generation or storage.
- Auto-installing the missing Linux system build dependencies on a local developer box (manual setup prerequisite; CI installs them in the workflow).
- A full frontend test framework (optional Vitest only if a UI framework warrants it).

## Dependencies
- Host toolchain verified present: node v24.16.0, npm 11.13.0, cargo 1.96.0, rustc 1.96.0, system `ssh` (OpenSSH 9.6p1) for `ssh -L`.
- Missing and required before local build/run: `cargo tauri` / `@tauri-apps/cli`, `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev` (and related Linux build deps).
- SSH key-resolution model from `.jonggrang/lib/sandbox.js` (precedence + `0600`/owner gotcha).
- AGENTS.md naming conventions: files `kebab-case.ts`, components `PascalCase.tsx`, functions `camelCase`, constants `UPPER_SNAKE_CASE`.
- `tauri-apps/tauri-action` GitHub Action (pinned) for the mac + linux binary build workflow, plus `actions/checkout`, `actions/setup-node`, a Rust toolchain action, and `actions/upload-artifact` for publishing the built binaries.
- GitHub-hosted `macos-latest` and `ubuntu-latest` runners for the build matrix.
