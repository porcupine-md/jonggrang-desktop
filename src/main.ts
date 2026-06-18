/**
 * Jonggrang Tunnel — webview frontend.
 *
 * This module is the dashboard UI. It contains NO tunnel logic of its own: it
 * only drives the Rust `#[tauri::command]` handlers
 * (`start_tunnel` / `stop_tunnel` / `tunnel_status` / `list_forwards` /
 * `open_dashboard`) and listens for the live `tunnel-status` event to keep the
 * view in sync.
 *
 * It manages a list of saved **connections** (one per jonggrang server). Each
 * connection is forwarded independently on its own dashboard port — the backend
 * keys every command by `connectionId` — so several servers can run at once.
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

/** Event emitted by the backend after every tunnel state change. */
const TUNNEL_STATUS_EVENT = "tunnel-status";
/** localStorage key holding the saved connections (paths only, no secrets). */
const STORAGE_KEY = "jonggrang.tunnel.connections.v1";
/** Older single-connection key, migrated into one connection on first load. */
const LEGACY_STORAGE_KEY = "jonggrang.tunnel.config.v1";
/** Container id the backend uses for the always-present dashboard forward. */
const DASHBOARD_CONTAINER_ID = "dashboard";
/** Default SSH port (the jonggrang server's sshd); mirrors the backend default. */
const DEFAULT_SSH_PORT = 2222;
/** Default dashboard forward port (local + remote); mirrors the backend default. */
const DEFAULT_DASHBOARD_PORT = 7777;
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
  connectionId: string;
  running: boolean;
  forwards: ForwardView[];
}

// ---- persisted connections -------------------------------------------------

interface ContainerForward {
  containerId: string;
  remotePort: number;
}

interface TunnelConnection {
  /** Stable id; doubles as the backend `connectionId`. */
  id: string;
  /** Display name (defaults to the target). */
  name: string;
  target: string;
  port: number;
  dashboardPort: number;
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

/** A stable, collision-free connection id. */
function newConnectionId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") {
    return `conn-${c.randomUUID()}`;
  }
  // Fallback for older webviews — only needs in-app uniqueness.
  return `conn-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

// ---- forward / connection validation helpers -------------------------------

function isContainerForward(f: unknown): f is ContainerForward {
  return (
    !!f &&
    typeof (f as ContainerForward).containerId === "string" &&
    typeof (f as ContainerForward).remotePort === "number"
  );
}

function asValidPort(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= 65535
    ? value
    : fallback;
}

// ---- config persistence ----------------------------------------------------

/** Coerce a loosely-typed parsed object into a well-formed connection. */
function normalizeConnection(raw: Partial<TunnelConnection>): TunnelConnection | null {
  if (typeof raw.target !== "string" || !Array.isArray(raw.forwards)) {
    return null;
  }
  return {
    id: typeof raw.id === "string" && raw.id ? raw.id : newConnectionId(),
    name: typeof raw.name === "string" && raw.name ? raw.name : raw.target,
    target: raw.target,
    port: asValidPort(raw.port, DEFAULT_SSH_PORT),
    dashboardPort: asValidPort(raw.dashboardPort, DEFAULT_DASHBOARD_PORT),
    keyPath: typeof raw.keyPath === "string" ? raw.keyPath : "",
    projectId: typeof raw.projectId === "string" ? raw.projectId : "",
    forwards: raw.forwards.filter(isContainerForward),
  };
}

/** Migrate a legacy single-connection config into a one-element list. */
function migrateLegacyConfig(): TunnelConnection[] {
  const raw = window.localStorage.getItem(LEGACY_STORAGE_KEY);
  if (!raw) {
    return [];
  }
  try {
    const conn = normalizeConnection(JSON.parse(raw) as Partial<TunnelConnection>);
    return conn ? [conn] : [];
  } catch {
    return [];
  }
}

function readConnections(): TunnelConnection[] {
  const raw = window.localStorage.getItem(STORAGE_KEY);
  if (!raw) {
    const migrated = migrateLegacyConfig();
    if (migrated.length) {
      saveConnections(migrated);
      window.localStorage.removeItem(LEGACY_STORAGE_KEY);
    }
    return migrated;
  }
  try {
    const parsed = JSON.parse(raw) as Partial<TunnelConnection>[];
    if (!Array.isArray(parsed)) {
      return [];
    }
    return parsed
      .map((c) => normalizeConnection(c))
      .filter((c): c is TunnelConnection => c !== null);
  } catch {
    return [];
  }
}

function saveConnections(connections: TunnelConnection[]): void {
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(connections));
}

/**
 * The next free dashboard port at or above 7777, skipping ports already taken
 * by other connections, so a freshly-added connection auto-assigns a distinct
 * port. The field stays editable.
 */
function nextDashboardPort(connections: TunnelConnection[], excludeId?: string): number {
  const used = new Set(
    connections.filter((c) => c.id !== excludeId).map((c) => c.dashboardPort),
  );
  let port = DEFAULT_DASHBOARD_PORT;
  while (used.has(port) && port < 65535) {
    port += 1;
  }
  return port;
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

/** Read + validate the setup form into a connection (keeping the given id). */
function readSetupForm(id: string): TunnelConnection {
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
  // Dashboard port is optional in the form — a blank field falls back to 7777.
  const dashboardText = byId<HTMLInputElement>("dashboard-port-input").value.trim();
  const dashboardPort =
    dashboardText === "" ? DEFAULT_DASHBOARD_PORT : Number(dashboardText);
  if (
    !Number.isInteger(dashboardPort) ||
    dashboardPort < 1 ||
    dashboardPort > 65535
  ) {
    throw new Error(
      `Invalid dashboard port: "${dashboardText}" (expected 1-65535).`,
    );
  }
  const name = byId<HTMLInputElement>("name-input").value.trim();
  // Container forwards are optional — with none, the backend still opens the
  // always-present dashboard forward.
  const forwards = collectForwards();
  return {
    id,
    name: name || target,
    target,
    port,
    dashboardPort,
    keyPath: byId<HTMLInputElement>("key-input").value.trim(),
    projectId: byId<HTMLInputElement>("project-input").value.trim(),
    forwards,
  };
}

function populateSetupForm(connection: TunnelConnection | null): void {
  byId<HTMLInputElement>("name-input").value = connection?.name ?? "";
  byId<HTMLInputElement>("target-input").value = connection?.target ?? "";
  byId<HTMLInputElement>("port-input").value = String(
    connection?.port ?? DEFAULT_SSH_PORT,
  );
  byId<HTMLInputElement>("dashboard-port-input").value = String(
    connection?.dashboardPort ?? nextDashboardPort(connections),
  );
  byId<HTMLInputElement>("key-input").value = connection?.keyPath ?? "";
  byId<HTMLInputElement>("project-input").value = connection?.projectId ?? "";
  byId("forward-rows").replaceChildren();
  const seeds = connection?.forwards.length ? connection.forwards : [undefined];
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

/** Open one connection's jonggrang dashboard in its own in-app webview window. */
async function openDashboardWindow(connectionId: string): Promise<void> {
  await invokeCommand<void>("open_dashboard", { connectionId });
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

function renderForwardCard(
  connection: TunnelConnection,
  forward: ForwardView,
): HTMLElement {
  const card = document.createElement("div");
  card.className = "forward-card";

  const meta = document.createElement("div");
  meta.className = "meta";

  const name = document.createElement("div");
  name.className = "name";
  name.textContent = forward.containerId;
  const isDashboard = forward.containerId === DASHBOARD_CONTAINER_ID;
  if (isDashboard) {
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
    const opened = isDashboard
      ? openDashboardWindow(connection.id)
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

/** Render one connection panel (header + actions + live forward list). */
function renderConnectionCard(connection: TunnelConnection): HTMLElement {
  const status = statuses.get(connection.id);
  const running = status?.running ?? false;

  const panel = document.createElement("div");
  panel.className = "panel connection";

  // --- header (name / target / dashboard link / running pill) ---
  const head = document.createElement("div");
  head.className = "conn-head";

  const meta = document.createElement("div");
  meta.className = "conn-meta";

  const nameEl = document.createElement("div");
  nameEl.className = "conn-name";
  nameEl.textContent = connection.name;

  const sub = document.createElement("div");
  sub.className = "conn-sub";
  sub.textContent = `${connection.target} · SSH ${connection.port}`;

  const dashUrl = `http://localhost:${connection.dashboardPort}`;
  const dashLink = document.createElement("a");
  dashLink.className = "url";
  dashLink.href = dashUrl;
  dashLink.textContent = dashUrl;
  dashLink.addEventListener("click", (event) => {
    event.preventDefault();
    openDashboardWindow(connection.id).catch((err) => showError(String(err)));
  });

  meta.append(nameEl, sub, dashLink);

  const pill = document.createElement("span");
  pill.className = `running-pill${running ? " on" : ""}`;
  pill.textContent = running ? "running" : "idle";

  head.append(meta, pill);

  // --- action buttons ---
  const actions = document.createElement("div");
  actions.className = "toolbar conn-actions";

  const startBtn = document.createElement("button");
  startBtn.className = "primary";
  startBtn.textContent = "Start";
  startBtn.disabled = running;
  startBtn.addEventListener("click", () => {
    startConnection(connection).catch((err) => showError(String(err)));
  });

  const stopBtn = document.createElement("button");
  stopBtn.className = "danger";
  stopBtn.textContent = "Stop";
  stopBtn.disabled = !running;
  stopBtn.addEventListener("click", () => {
    stopConnection(connection).catch((err) => showError(String(err)));
  });

  const refreshBtn = document.createElement("button");
  refreshBtn.className = "icon";
  refreshBtn.textContent = "Refresh";
  refreshBtn.addEventListener("click", () => {
    refreshConnection(connection).catch((err) => showError(String(err)));
  });

  const editBtn = document.createElement("button");
  editBtn.className = "icon";
  editBtn.textContent = "Edit";
  editBtn.addEventListener("click", () => showSetup(connection));

  const removeBtn = document.createElement("button");
  removeBtn.className = "icon danger";
  removeBtn.textContent = "Remove";
  removeBtn.addEventListener("click", () => {
    removeConnection(connection).catch((err) => showError(String(err)));
  });

  const spacer = document.createElement("span");
  spacer.className = "spacer";

  actions.append(startBtn, stopBtn, refreshBtn, spacer, editBtn, removeBtn);

  // --- forward list ---
  const list = document.createElement("div");
  list.className = "forward-list";
  const forwards = status?.forwards ?? [];
  if (forwards.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = 'No active forwards. Press "Start" to open the tunnel.';
    list.append(empty);
  } else {
    for (const forward of forwards) {
      list.append(renderForwardCard(connection, forward));
    }
  }

  panel.append(head, actions, list);
  return panel;
}

function renderList(): void {
  const list = byId("connection-list");
  list.replaceChildren();
  setHidden(byId("list-empty"), connections.length > 0);
  for (const connection of connections) {
    list.append(renderConnectionCard(connection));
  }
}

// ---- command drivers -------------------------------------------------------

async function startConnection(connection: TunnelConnection): Promise<void> {
  clearError();
  try {
    // Matches StartTunnelArgs (camelCase serde fields). Empty optionals are
    // omitted so the backend falls back to its key-resolution precedence.
    const args: Record<string, unknown> = {
      connectionId: connection.id,
      target: connection.target,
      forwards: buildForwardsSpec(connection.forwards),
      port: connection.port,
      dashboardPort: connection.dashboardPort,
    };
    if (connection.keyPath) {
      args.keyPath = connection.keyPath;
    }
    if (connection.projectId) {
      args.projectId = connection.projectId;
    }
    const status = await invokeCommand<TunnelStatus>("start_tunnel", { args });
    statuses.set(status.connectionId, status);
    renderList();
  } catch (err) {
    // Backend errors arrive as plain strings — including the SSH key
    // 0600/owner permission gotcha — and are shown verbatim to the user.
    showError(String(err));
  }
}

async function stopConnection(connection: TunnelConnection): Promise<void> {
  clearError();
  const status = await invokeCommand<TunnelStatus>("stop_tunnel", {
    connectionId: connection.id,
  });
  statuses.set(status.connectionId, status);
  renderList();
}

async function refreshConnection(connection: TunnelConnection): Promise<void> {
  const status = await invokeCommand<TunnelStatus>("tunnel_status", {
    connectionId: connection.id,
  });
  statuses.set(status.connectionId, status);
  renderList();
}

async function refreshAll(): Promise<void> {
  await Promise.all(
    connections.map((c) =>
      refreshConnection(c).catch((err) => showError(String(err))),
    ),
  );
}

async function removeConnection(connection: TunnelConnection): Promise<void> {
  clearError();
  // Tear the tunnel down first so removing a card never leaks `ssh` children.
  await invokeCommand<TunnelStatus>("stop_tunnel", {
    connectionId: connection.id,
  }).catch(() => {
    /* stopping is best-effort during removal */
  });
  statuses.delete(connection.id);
  connections = connections.filter((c) => c.id !== connection.id);
  saveConnections(connections);
  renderList();
}

// ---- view switching --------------------------------------------------------

function showList(): void {
  renderList();
  setHidden(byId("setup-view"), true);
  setHidden(byId("list-view"), false);
}

function showSetup(connection: TunnelConnection | null): void {
  editingId = connection?.id ?? null;
  byId("setup-title").textContent = connection
    ? "Edit connection"
    : "Add connection";
  populateSetupForm(connection);
  setHidden(byId("list-view"), true);
  setHidden(byId("setup-view"), false);
}

// ---- wiring ----------------------------------------------------------------

let connections: TunnelConnection[] = [];
const statuses = new Map<string, TunnelStatus>();
/** Id of the connection being edited, or null when adding a new one. */
let editingId: string | null = null;
let unlistenStatus: UnlistenFn | null = null;

async function subscribeToStatusEvents(): Promise<void> {
  if (unlistenStatus) {
    return;
  }
  unlistenStatus = await getTauri().event.listen<TunnelStatus>(
    TUNNEL_STATUS_EVENT,
    (event) => {
      statuses.set(event.payload.connectionId, event.payload);
      // Only re-render when the list is the visible view.
      if (!byId("list-view").classList.contains("hidden")) {
        renderList();
      }
    },
  );
}

function wireSetupForm(): void {
  byId("add-forward").addEventListener("click", () => addForwardRow());
  byId("key-browse").addEventListener("click", () => {
    pickKeyPath().catch((err) => showError(String(err)));
  });
  byId("setup-cancel").addEventListener("click", () => {
    clearError();
    showList();
  });
  byId<HTMLFormElement>("setup-form").addEventListener("submit", (event) => {
    event.preventDefault();
    clearError();
    try {
      const id = editingId ?? newConnectionId();
      const connection = readSetupForm(id);
      const existing = connections.findIndex((c) => c.id === id);
      if (existing >= 0) {
        connections[existing] = connection;
      } else {
        connections.push(connection);
      }
      saveConnections(connections);
      editingId = null;
      showList();
    } catch (err) {
      showError(err instanceof Error ? err.message : String(err));
    }
  });
}

function wireListView(): void {
  byId("add-connection").addEventListener("click", () => showSetup(null));
  byId("refresh-all").addEventListener("click", () => {
    refreshAll().catch((err) => showError(String(err)));
  });
  byId("error-dismiss").addEventListener("click", () => clearError());
}

async function init(): Promise<void> {
  wireSetupForm();
  wireListView();

  try {
    await subscribeToStatusEvents();
  } catch (err) {
    showError(`Failed to subscribe to live status: ${String(err)}`);
  }

  connections = readConnections();
  showList();
  // Reflect the current backend state (tunnels survive webview reloads).
  refreshAll().catch((err) => showError(String(err)));
}

window.addEventListener("DOMContentLoaded", () => {
  init().catch((err) => showError(String(err)));
});

export {};
