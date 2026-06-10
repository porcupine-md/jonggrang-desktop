/**
 * Jonggrang Tunnel — webview frontend (task-006).
 *
 * This module is the dashboard UI. It contains NO tunnel logic of its own: it
 * only drives the Rust `#[tauri::command]` handlers from task-005
 * (`start_tunnel` / `stop_tunnel` / `tunnel_status` / `list_forwards`) and
 * listens for the live `tunnel-status` event to keep the view in sync.
 *
 * ## How the Tauri API is reached
 * The project uses a no-bundler static frontend (`frontendDist: "../src"`), so
 * bare ESM specifiers like `@tauri-apps/api/core` cannot be resolved by the
 * webview at runtime. Instead we use the same `@tauri-apps/api` functions via
 * the `window.__TAURI__` global that Tauri injects when
 * `app.withGlobalTauri = true` (set in tauri.conf.json). The `import type`
 * queries below pull in the real `@tauri-apps/api` types (erased at compile
 * time) so `invoke`/`listen` stay fully typed without a runtime import.
 */

type InvokeFn = typeof import("@tauri-apps/api/core").invoke;
type ListenFn = typeof import("@tauri-apps/api/event").listen;
type UnlistenFn = () => void;

declare global {
  interface Window {
    __TAURI__?: {
      core: { invoke: InvokeFn };
      event: { listen: ListenFn };
      /** Optional dialog plugin — only used for the key "Browse…" button if present. */
      dialog?: {
        open: (
          opts?: Record<string, unknown>,
        ) => Promise<string | string[] | null>;
      };
      /** Optional opener plugin — used to open local URLs externally if present. */
      opener?: { openUrl: (url: string) => Promise<void> };
    };
  }
}

// ---- constants -------------------------------------------------------------

/** Event emitted by the backend after every tunnel state change (task-005). */
const TUNNEL_STATUS_EVENT = "tunnel-status";
/** localStorage key holding the persisted setup config (paths only, no secrets). */
const STORAGE_KEY = "jonggrang.tunnel.config.v1";
/** Container id the backend uses for the always-present dashboard forward. */
const DASHBOARD_CONTAINER_ID = "dashboard";
/** Default SSH port (the jonggrang server's sshd); mirrors the backend default. */
const DEFAULT_SSH_PORT = 2222;
/** Human-readable labels for the backend's lowercase status wire strings. */
const STATUS_LABELS: Record<string, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  failed: "Failed",
  stopped: "Stopped",
};

// ---- backend DTOs (mirror commands.rs serde output) ------------------------

interface ForwardView {
  containerId: string;
  remotePort: number;
  localPort: number;
  localUrl: string;
  pid: number | null;
  status: string;
}

interface TunnelStatus {
  running: boolean;
  forwards: ForwardView[];
}

// ---- persisted setup config ------------------------------------------------

interface ContainerForward {
  containerId: string;
  remotePort: number;
}

interface TunnelConfig {
  target: string;
  port: number;
  keyPath: string;
  projectId: string;
  forwards: ContainerForward[];
}

// ---- Tauri access helpers --------------------------------------------------

function getTauri(): NonNullable<Window["__TAURI__"]> {
  const tauri = window.__TAURI__;
  if (!tauri) {
    throw new Error(
      "Tauri API is unavailable. Run this inside the desktop app " +
        "(withGlobalTauri must be enabled), not a plain browser.",
    );
  }
  return tauri;
}

async function invokeCommand<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return getTauri().core.invoke<T>(command, args);
}

// ---- small DOM helpers -----------------------------------------------------

function byId<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) {
    throw new Error(`missing DOM element #${id}`);
  }
  return node as T;
}

function setHidden(node: HTMLElement, hidden: boolean): void {
  node.classList.toggle("hidden", hidden);
}

// ---- config persistence ----------------------------------------------------

function readConfig(): TunnelConfig | null {
  const raw = window.localStorage.getItem(STORAGE_KEY);
  if (!raw) {
    return null;
  }
  try {
    const parsed = JSON.parse(raw) as Partial<TunnelConfig>;
    if (typeof parsed.target !== "string" || !Array.isArray(parsed.forwards)) {
      return null;
    }
    return {
      target: parsed.target,
      port:
        typeof parsed.port === "number" && Number.isInteger(parsed.port)
          ? parsed.port
          : DEFAULT_SSH_PORT,
      keyPath: typeof parsed.keyPath === "string" ? parsed.keyPath : "",
      projectId: typeof parsed.projectId === "string" ? parsed.projectId : "",
      forwards: parsed.forwards.filter(
        (f): f is ContainerForward =>
          !!f &&
          typeof f.containerId === "string" &&
          typeof f.remotePort === "number",
      ),
    };
  } catch {
    return null;
  }
}

function saveConfig(config: TunnelConfig): void {
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(config));
}

/**
 * Assemble the raw forward spec string the backend parser expects, e.g.
 * `-c web:8080,db:5432`. The dashboard forward is added by the backend and is
 * intentionally NOT included here. Container forwards are optional: with none
 * configured this returns an empty string, which the backend treats as a
 * dashboard-only tunnel.
 */
function buildForwardsSpec(forwards: ContainerForward[]): string {
  if (forwards.length === 0) {
    return "";
  }
  const parts = forwards.map((f) => `${f.containerId}:${f.remotePort}`);
  return `-c ${parts.join(",")}`;
}

// ---- error banner ----------------------------------------------------------

function showError(message: string): void {
  byId("error-text").textContent = message;
  setHidden(byId("error-banner"), false);
}

function clearError(): void {
  setHidden(byId("error-banner"), true);
}

// ---- setup form ------------------------------------------------------------

function addForwardRow(seed?: ContainerForward): void {
  const row = document.createElement("div");
  row.className = "row forward-row";

  const cid = document.createElement("input");
  cid.type = "text";
  cid.className = "cid";
  cid.placeholder = "container-id";
  cid.value = seed?.containerId ?? "";

  const port = document.createElement("input");
  port.type = "text";
  port.className = "port";
  port.placeholder = "remote port";
  port.value = seed ? String(seed.remotePort) : "";

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "icon danger";
  remove.textContent = "Remove";
  remove.addEventListener("click", () => row.remove());

  row.append(cid, port, remove);
  byId("forward-rows").append(row);
}

/** Read + validate the container-forward rows from the setup form. */
function collectForwards(): ContainerForward[] {
  const rows = Array.from(
    byId("forward-rows").querySelectorAll<HTMLDivElement>(".forward-row"),
  );
  const forwards: ContainerForward[] = [];
  for (const row of rows) {
    const cidInput = row.querySelector<HTMLInputElement>(".cid");
    const portInput = row.querySelector<HTMLInputElement>(".port");
    const containerId = cidInput?.value.trim() ?? "";
    const portText = portInput?.value.trim() ?? "";
    if (!containerId && !portText) {
      continue; // skip blank rows
    }
    if (!containerId || /[\s:,]/.test(containerId)) {
      throw new Error(`Invalid container id: "${containerId || "(empty)"}"`);
    }
    const remotePort = Number(portText);
    if (!Number.isInteger(remotePort) || remotePort < 1 || remotePort > 65535) {
      throw new Error(
        `Invalid remote port for "${containerId}": "${portText}" (expected 1-65535)`,
      );
    }
    forwards.push({ containerId, remotePort });
  }
  return forwards;
}

function readSetupForm(): TunnelConfig {
  const target = byId<HTMLInputElement>("target-input").value.trim();
  if (!/^[^@\s]+@[^@\s]+$/.test(target)) {
    throw new Error('Server target must look like "<user>@<server>".');
  }
  // SSH port is optional in the form — a blank field falls back to 2222.
  const portText = byId<HTMLInputElement>("port-input").value.trim();
  const port = portText === "" ? DEFAULT_SSH_PORT : Number(portText);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`Invalid SSH port: "${portText}" (expected 1-65535).`);
  }
  // Container forwards are optional — with none, the backend still opens the
  // always-present dashboard forward (http://localhost:7777).
  const forwards = collectForwards();
  return {
    target,
    port,
    keyPath: byId<HTMLInputElement>("key-input").value.trim(),
    projectId: byId<HTMLInputElement>("project-input").value.trim(),
    forwards,
  };
}

function populateSetupForm(config: TunnelConfig | null): void {
  byId<HTMLInputElement>("target-input").value = config?.target ?? "";
  byId<HTMLInputElement>("port-input").value = String(
    config?.port ?? DEFAULT_SSH_PORT,
  );
  byId<HTMLInputElement>("key-input").value = config?.keyPath ?? "";
  byId<HTMLInputElement>("project-input").value = config?.projectId ?? "";
  byId("forward-rows").replaceChildren();
  const seeds = config?.forwards.length ? config.forwards : [undefined];
  for (const seed of seeds) {
    addForwardRow(seed);
  }
}

// ---- key picker (optional dialog plugin) -----------------------------------

async function pickKeyPath(): Promise<void> {
  const dialog = getTauri().dialog;
  if (!dialog) {
    showError(
      "Native file picker is unavailable; type the SSH key path manually.",
    );
    return;
  }
  // Only the chosen PATH is read back — the key contents are never loaded.
  const picked = await dialog.open({
    multiple: false,
    directory: false,
    title: "Select SSH private key",
  });
  if (typeof picked === "string") {
    byId<HTMLInputElement>("key-input").value = picked;
  }
}

// ---- live status view ------------------------------------------------------

function statusLabel(status: string): string {
  return STATUS_LABELS[status] ?? status;
}

/**
 * Open the jonggrang dashboard inside its own in-app webview window (at
 * http://localhost:7777) via the backend `open_dashboard` command, instead of
 * handing it off to an external browser.
 */
async function openDashboardWindow(): Promise<void> {
  await invokeCommand<void>("open_dashboard");
}

async function openLocalUrl(url: string): Promise<void> {
  // Prefer the opener plugin (opens in the system browser) when present;
  // otherwise fall back to a normal window.open.
  const opener = getTauri().opener;
  if (opener) {
    await opener.openUrl(url);
    return;
  }
  window.open(url, "_blank");
}

function renderForwardCard(forward: ForwardView): HTMLElement {
  const card = document.createElement("div");
  card.className = "forward-card";

  const meta = document.createElement("div");
  meta.className = "meta";

  const name = document.createElement("div");
  name.className = "name";
  name.textContent = forward.containerId;
  if (forward.containerId === DASHBOARD_CONTAINER_ID) {
    const tag = document.createElement("span");
    tag.className = "tag";
    tag.textContent = "dashboard";
    name.append(tag);
  }

  const link = document.createElement("a");
  link.className = "url";
  link.href = forward.localUrl;
  link.textContent = forward.localUrl;
  link.addEventListener("click", (event) => {
    event.preventDefault();
    // The dashboard forward opens in-app; other forwards open externally.
    const opened =
      forward.containerId === DASHBOARD_CONTAINER_ID
        ? openDashboardWindow()
        : openLocalUrl(forward.localUrl);
    opened.catch((err) => showError(String(err)));
  });

  const ports = document.createElement("div");
  ports.className = "ports";
  ports.textContent = `local ${forward.localPort} → remote ${forward.remotePort}${
    forward.pid != null ? ` · pid ${forward.pid}` : ""
  }`;

  meta.append(name, link, ports);

  const copy = document.createElement("button");
  copy.className = "icon";
  copy.textContent = "Copy";
  copy.addEventListener("click", () => {
    navigator.clipboard?.writeText(forward.localUrl).catch(() => {
      /* clipboard is best-effort */
    });
  });

  const badge = document.createElement("span");
  badge.className = `badge ${forward.status}`;
  badge.textContent = statusLabel(forward.status);

  card.append(meta, copy, badge);
  return card;
}

function applyStatus(status: TunnelStatus): void {
  const pill = byId("running-pill");
  pill.textContent = status.running ? "running" : "idle";
  pill.classList.toggle("on", status.running);

  byId<HTMLButtonElement>("start-btn").disabled = status.running;
  byId<HTMLButtonElement>("stop-btn").disabled = !status.running;

  const list = byId("forward-list");
  list.replaceChildren();
  if (status.forwards.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = 'No active forwards. Press "Start" to open the tunnel.';
    list.append(empty);
    return;
  }
  for (const forward of status.forwards) {
    list.append(renderForwardCard(forward));
  }
}

// ---- command drivers -------------------------------------------------------

async function startTunnel(config: TunnelConfig): Promise<void> {
  clearError();
  try {
    // Matches StartTunnelArgs (camelCase serde fields). Empty optionals are
    // omitted so the backend falls back to its key-resolution precedence.
    const args: Record<string, unknown> = {
      target: config.target,
      forwards: buildForwardsSpec(config.forwards),
      port: config.port,
    };
    if (config.keyPath) {
      args.keyPath = config.keyPath;
    }
    if (config.projectId) {
      args.projectId = config.projectId;
    }
    const status = await invokeCommand<TunnelStatus>("start_tunnel", { args });
    applyStatus(status);
  } catch (err) {
    // Backend errors arrive as plain strings — including the SSH key
    // 0600/owner permission gotcha — and are shown verbatim to the user.
    showError(String(err));
  }
}

async function stopTunnel(): Promise<void> {
  clearError();
  try {
    const status = await invokeCommand<TunnelStatus>("stop_tunnel");
    applyStatus(status);
  } catch (err) {
    showError(String(err));
  }
}

async function refreshStatus(): Promise<void> {
  try {
    const status = await invokeCommand<TunnelStatus>("tunnel_status");
    applyStatus(status);
  } catch (err) {
    showError(String(err));
  }
}

// ---- view switching --------------------------------------------------------

function showSetup(config: TunnelConfig | null): void {
  populateSetupForm(config);
  setHidden(byId("dashboard-view"), true);
  setHidden(byId("setup-view"), false);
}

function showDashboard(config: TunnelConfig): void {
  byId("summary-target").textContent = config.target;
  setHidden(byId("setup-view"), true);
  setHidden(byId("dashboard-view"), false);
  // Reflect the current backend state immediately (it survives webview reloads).
  refreshStatus();
}

// ---- wiring ----------------------------------------------------------------

let activeConfig: TunnelConfig | null = null;
let unlistenStatus: UnlistenFn | null = null;

async function subscribeToStatusEvents(): Promise<void> {
  if (unlistenStatus) {
    return;
  }
  unlistenStatus = await getTauri().event.listen<TunnelStatus>(
    TUNNEL_STATUS_EVENT,
    (event) => applyStatus(event.payload),
  );
}

function wireSetupForm(): void {
  byId("add-forward").addEventListener("click", () => addForwardRow());
  byId("key-browse").addEventListener("click", () => {
    pickKeyPath().catch((err) => showError(String(err)));
  });
  byId<HTMLFormElement>("setup-form").addEventListener("submit", (event) => {
    event.preventDefault();
    clearError();
    try {
      activeConfig = readSetupForm();
      saveConfig(activeConfig);
      showDashboard(activeConfig);
    } catch (err) {
      showError(err instanceof Error ? err.message : String(err));
    }
  });
}

function wireDashboard(): void {
  byId("start-btn").addEventListener("click", () => {
    if (activeConfig) {
      startTunnel(activeConfig);
    }
  });
  byId("stop-btn").addEventListener("click", () => {
    stopTunnel();
  });
  byId("refresh-btn").addEventListener("click", () => {
    refreshStatus();
  });
  byId("edit-btn").addEventListener("click", () => {
    showSetup(activeConfig);
  });
  byId("summary-dashboard").addEventListener("click", (event) => {
    event.preventDefault();
    openDashboardWindow().catch((err) => showError(String(err)));
  });
  byId("error-dismiss").addEventListener("click", () => clearError());
}

async function init(): Promise<void> {
  wireSetupForm();
  wireDashboard();

  try {
    await subscribeToStatusEvents();
  } catch (err) {
    showError(`Failed to subscribe to live status: ${String(err)}`);
  }

  activeConfig = readConfig();
  if (activeConfig) {
    showDashboard(activeConfig);
  } else {
    showSetup(null);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  init().catch((err) => showError(String(err)));
});

export {};
