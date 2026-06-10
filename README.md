# Jonggrang Tunnel Desktop

A small **Tauri v2** desktop app (macOS + Linux) that gives you a GUI for SSH
local-port forwarding into a **jonggrang** server. Instead of memorising
`ssh -L` invocations, you point the app at a server, pick your containers, and it
spawns and supervises the tunnels for you — surfacing live per-forward status and
clickable local URLs.

The app does **not** ship its own SSH implementation. It shells out to your
system `ssh`, so it reuses your existing keys, `ssh-agent`, and `known_hosts`,
and never reads private-key contents into memory.

---

## How it works

```
Dashboard webview (src/)  ──invoke──▶  Tauri commands (commands.rs)
                                              │
                                              ▼
                                    TunnelManager (tunnel.rs)
                                              │  spawns one child per forward
                                              ▼
                            system `ssh -L <local>:localhost:<remote> user@host`
```

- The dashboard webview drives everything through Tauri commands
  (`start_tunnel` / `stop_tunnel` / `tunnel_status` / `list_forwards`).
- `TunnelManager` spawns one `ssh -L` child **per forward**, infers connection
  state (ssh `-v` stderr parsing + active loopback port probe), and guarantees
  teardown on stop and on app exit.
- A `tunnel-status` event is emitted on every state change, so the UI never polls.

---

## Usage

### First-launch setup

On first launch the dashboard shows a setup form. Provide:

- **Server** — `user@host` (e.g. `deploy@my-jonggrang-box`).
- **SSH key path** *(optional)* — leave blank to use the default resolution
  order (see below). Only the **path** is used; the key is never read by the app.
- **Project id** *(optional)* — used to resolve a per-project key.
- **Forwards** — a comma-separated `-c <container>:<remotePort>` list, e.g.
  `-c api:8080,db:5432`.

Your config is saved to `localStorage`, so later launches skip straight to the
live status view.

### Forwards and ports

- The **dashboard** forward is always created and keeps the canonical local port
  **7777** — open it at **http://localhost:7777** (bumps to the next free port
  only if 7777 is already taken).
- Each `-c` forward gets its own auto-allocated local port starting from `7778`,
  with collision avoidance. The UI shows the resulting `http://localhost:<port>`
  for each forward, with **Open** and **Copy** actions.

> Start/Stop are **global** (all forwards together). Per-forward cards currently
> expose Open/Copy only.

### SSH key resolution

When no key path is given, keys are resolved with this precedence (modeling
jonggrang's `sandbox.js`):

1. `~/.jonggrang/web/ssh/<projectId>.key`
2. `~/.jonggrang/web/ssh/global.key`
3. `~/.ssh/id_rsa`

If a key has loose permissions you'll see an error like *"bad owner or
permissions"* — fix it with `chmod 600 <key>`.

---

## Download a pre-built binary

CI builds macOS and Linux bundles on every push. You don't need a local Rust /
WebKit toolchain just to run the app.

### From a tagged Release (recommended)

Tagged versions (`v*`) attach their bundles to a GitHub Release:

1. Go to **[Releases](https://github.com/porcupine-md/jonggrang-desktop/releases)**.
2. Download the asset for your platform:
   - **macOS** — `.dmg` (or `.app`)
   - **Linux** — `.AppImage` (portable) or `.deb` (Debian/Ubuntu)

### From a CI run (any branch / PR)

Every workflow run uploads the bundles as artifacts, even without a release tag:

1. Open the **[Actions](https://github.com/porcupine-md/jonggrang-desktop/actions)**
   tab and click the latest **build** run.
2. Scroll to **Artifacts** and download:
   - `bundles-macos-latest` → `.dmg` / `.app`
   - `bundles-ubuntu-latest` → `.deb` / `.AppImage`

> Artifacts are zipped by GitHub and expire after the repo's retention window.
> The bundles are currently **unsigned** (no Apple notarization), so macOS may
> require a right-click → Open / Gatekeeper override on first launch.

---

## Build locally

### Prerequisites

- **Node** (used for the TypeScript frontend tooling) and **npm**.
- **Rust** stable + Cargo.
- **System `ssh`** on your `PATH` (OpenSSH).
- **Tauri CLI** — ships as the `@tauri-apps/cli` dev dependency (run via
  `npm run tauri`), or install the Rust CLI globally with
  `cargo install tauri-cli` to get `cargo tauri`.

### Linux build dependencies (manual, one-time)

Tauri/wry link against the WebKit/GTK stack, which is **not** installed by
default. Install it before building or `cargo build` / `tauri build` will fail:

```bash
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev \
  libsoup-3.0-dev \
  build-essential \
  librsvg2-dev
```

### Run / build

```bash
npm install        # frontend + Tauri CLI deps

npm run dev        # tauri dev — typechecks, serves src/, builds the Rust backend
npm run build      # tsc && tauri build — Linux .deb/.AppImage bundles

cd src-tauri && cargo test    # run the Rust unit tests
```

> **macOS bundles cannot be built on Linux.** Tauri needs the Apple SDK +
> codesign tooling that only exists on a macOS host, so there is no Linux→macOS
> cross-compile path. Use the CI workflow (which runs a `macos-latest` runner) to
> produce macOS binaries — see *Download a pre-built binary* above.

Built bundles land in `src-tauri/target/release/bundle/`.

---

## Key decisions

- **Shell out to system `ssh -L`**, not a native Rust SSH library — reuses your
  keys / agent / `known_hosts` and keeps private keys out of app memory.
- **Key precedence** `~/.jonggrang/web/ssh/<id>.key` → `global.key` →
  `~/.ssh/id_rsa`; an explicit key path overrides it.
- **Port strategy**: dashboard pinned to local `7777`; other forwards
  auto-allocate from `7778` with collision avoidance.
- **CI-only macOS builds**: a `macos-latest` + `ubuntu-latest` Actions matrix is
  the canonical source of both platform binaries (no cross-compile).
- **Secret safety**: only the key *path* is ever passed to ssh; contents are
  never read.

See **[AGENTS.md](./AGENTS.md)** for the full architecture, conventions, and
gotchas.

---

## Project layout

```
src/           TypeScript webview frontend (static, no bundler)
src-tauri/     Rust backend — tunnel.rs (pure logic + lifecycle), commands.rs (bridge)
.github/workflows/build.yml   mac + linux release build matrix
```
