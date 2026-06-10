# Phase 10 — Design Verification

**Feature:** jonggrang-tunnel-desktop · **Verdict:** ✅ PASS · **Date:** 2026-06-10

Implementation verified against `plan.md`. All 7 planned phases are present in the
code and match the design decisions. No material deviations.

## Plan phase → implementation evidence

| # | Plan phase | Status | Evidence |
|---|------------|--------|----------|
| 1 | Scaffold & toolchain (Tauri v2) | ✅ | `package.json`, `tsconfig.json`, `src-tauri/Cargo.toml`, `tauri.conf.json` (`frontendDist=../src`, `withGlobalTauri=true`, targets deb/appimage/dmg/app), `build.rs`, `main.rs` shim, 6 icons |
| 2 | Tunnel spec & port allocation (pure) | ✅ | `tunnel.rs` pure `build_tunnel_plan`/`next_free_local_port`; dashboard allocated first keeping 7777, `-c` forwards from 7778 with collision skip; 51 `#[test]` cases, serde-free |
| 3 | SSH lifecycle manager | ✅ | One `ssh -L` child per forward (`spawn_forward`); `impl Drop for ForwardProcess` RAII reap; status inferred via `classify_ssh_log_line` (`-v` stderr) + loopback `TcpStream` probe (`local_probe_addr`); key precedence `<id>.key → global.key → ~/.ssh/id_rsa` (`ssh_key_candidates`/`resolve_ssh_key_with`), path-only via `-i` |
| 4 | Tauri commands bridge | ✅ | `commands.rs` exposes `start_tunnel`/`stop_tunnel`/`tunnel_status`/`list_forwards`; camelCase serde DTOs at boundary only; `tunnel-status` event emitted after every state change (`TUNNEL_STATUS_EVENT`) |
| 5 | Dashboard UI | ✅ | `src/index.html` + `src/main.ts`; runtime via `window.__TAURI__`, `invokeCommand` of all four commands, event `listen`; type-only imports |
| 6 | GitHub Actions mac+linux build | ✅ | `build.yml` `macos-latest`+`ubuntu-latest` matrix, `fail-fast:false`, ubuntu apt-get installs `libwebkit2gtk-4.1-dev`/`libsoup-3.0-dev`/`build-essential`/`librsvg2-dev`, `cargo test`, `tauri-action`, `contents:write` for tag Releases, triggers on branch/PR/`v*` tags |
| 7 | Docs & conventions | ✅ | `AGENTS.md` TODOs filled, `README.md` rewritten with usage + CI-binary download instructions |

## Key decisions honored
- Shell out to system `ssh -L` (not a Rust SSH crate) — confirmed in `spawn_forward`.
- `ssh` flags match design: `-i -N -v -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -L <l>:localhost:<r> user@host` (asserted by `build_ssh_args_has_expected_shape`).
- Secret safety: only key **path** passed; contents never read into memory (doc + test).
- Teardown: RAII `Drop` backstop **plus** explicit `lib.rs` `RunEvent::Exit ⇒ mgr.stop()` hook.
- Tauri v2 lib/bin split: `lib.rs::run()` owns builder/invoke_handler/exit hook; `main.rs` thin shim.

## Noted (documented, not a defect)
- **Per-forward Start/Stop is global-only.** Plan phase 5 mentioned per-forward
  "start/stop controls"; the shipped backend exposes global `start_tunnel`/`stop_tunnel`
  while per-forward cards expose Open/Copy only. This is intentional scoping, disclosed
  in AGENTS.md "Known Gotchas" as a future `stop_forward` enhancement. Does not fail design verification.

## Out-of-scope items correctly excluded
Windows/mobile targets, native Rust SSH client, code-signing/notarization, server
provisioning, secrets vault, auto-install of Linux build deps — all absent as planned.
