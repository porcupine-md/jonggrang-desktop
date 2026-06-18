//! Tauri command bridge between the webview and the SSH tunnel lifecycle
//! manager.
//!
//! This module is the only place `tauri` and the pure tunnel logic meet. It
//! exposes five `#[tauri::command]` handlers — [`start_tunnel`], [`stop_tunnel`],
//! [`tunnel_status`], [`list_forwards`], and [`open_dashboard`] — that drive a
//! shared [`SharedTunnelRegistry`] held as Tauri managed state, and it emits a
//! live [`TUNNEL_STATUS_EVENT`] after every state change so the UI can update
//! without polling.
//!
//! ## Multiple connections
//! Every command is scoped to a `connectionId`: the registry keeps one
//! independent tunnel (its own `ssh -L` children + dashboard port) per saved
//! jonggrang server, so several servers can be forwarded at the same time. The
//! emitted status carries its `connectionId` so the webview can update just the
//! matching connection.
//!
//! ## Serialization boundary
//! The lifecycle types in [`crate::tunnel`] are deliberately `std`-only (no
//! `serde`) so they stay unit-testable in isolation. Rather than bolt `serde`
//! onto them, this module owns small serde-derived **DTOs** ([`TunnelStatus`],
//! [`ForwardView`]) and converts at the boundary. Every command returns
//! `Result<_, String>`: the `Err` string is the `Display` of the underlying
//! typed error (`TunnelSpecError` / `SshKeyError` / `TunnelError`), which already
//! carries actionable, user-readable text — including the `0600` SSH-key
//! permission gotcha surfaced by [`crate::tunnel::TunnelManager::start`].

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::tunnel::{
    build_plan_from_spec, resolve_default_ssh_key, ForwardHealth, SharedTunnelRegistry,
    DASHBOARD_CONTAINER_ID, DASHBOARD_LOCAL_PORT,
};

/// Name of the event emitted to the frontend whenever a connection's tunnel
/// state changes. The payload is a serialized [`TunnelStatus`] (carrying its
/// `connectionId`). The webview listens via `listen('tunnel-status', ...)` so it
/// never has to poll.
pub const TUNNEL_STATUS_EVENT: &str = "tunnel-status";

// ---- inputs ----------------------------------------------------------------

/// Arguments for [`start_tunnel`], received from the webview as a JSON object.
///
/// `target` and `forwards` are the raw `<user>@<host>` and `-c <cid>:port,...`
/// strings; they are parsed and allocated by the pure parsing logic. The SSH key
/// is resolved by **path only** (its contents are never read): an explicit
/// `keyPath` overrides resolution, otherwise the jonggrang precedence is used
/// with `projectId`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartTunnelArgs {
    /// Stable id of the saved connection this tunnel belongs to. Each connection
    /// runs independently, so this keys it in the registry.
    pub connection_id: String,
    /// The `<user>@<host>` SSH target.
    pub target: String,
    /// The forward spec, e.g. `-c web:8080,db:5432` (the leading `-c` is
    /// optional). **Optional**: an empty or omitted string yields a
    /// dashboard-only tunnel (just the always-present dashboard forward), parsed
    /// via [`crate::tunnel::parse_forwards_optional`].
    #[serde(default)]
    pub forwards: String,
    /// SSH port to connect on. **Optional**: omitted/null falls back to the
    /// [`crate::tunnel::DEFAULT_SSH_PORT`] (2222) the jonggrang server's sshd
    /// listens on.
    #[serde(default)]
    pub port: Option<u16>,
    /// Port for the always-present dashboard forward — used as both its local
    /// and remote port. **Optional**: omitted/null falls back to the canonical
    /// [`crate::tunnel::DASHBOARD_LOCAL_PORT`] (7777).
    #[serde(default)]
    pub dashboard_port: Option<u16>,
    /// jonggrang project id used to resolve `~/.jonggrang/web/ssh/<id>.key`
    /// first, then `global.key`, then `~/.ssh/id_rsa`. Optional.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Explicit SSH private-key path that bypasses precedence resolution.
    /// Optional; only the path is used, never the file contents.
    #[serde(default)]
    pub key_path: Option<String>,
}

// ---- outputs ---------------------------------------------------------------

/// A serde-serializable view of one forward's health, mirroring
/// [`ForwardHealth`] but with the status rendered to its stable lowercase string
/// (`connecting` / `connected` / `failed` / `stopped`) for the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardView {
    /// Container id (or `dashboard` for the always-present dashboard forward).
    pub container_id: String,
    /// Remote port on the server side.
    pub remote_port: u16,
    /// Local port bound on this machine.
    pub local_port: u16,
    /// The `http://localhost:<local_port>` URL the user opens in a browser.
    pub local_url: String,
    /// OS pid of the tracked `ssh` child, if still spawned.
    pub pid: Option<u32>,
    /// Inferred connection state as a stable lowercase string.
    pub status: String,
}

impl From<ForwardHealth> for ForwardView {
    fn from(h: ForwardHealth) -> Self {
        ForwardView {
            container_id: h.container_id,
            remote_port: h.remote_port,
            local_port: h.local_port,
            local_url: h.local_url,
            pid: h.pid,
            // `ForwardStatus`' Display is the stable wire string.
            status: h.status.to_string(),
        }
    }
}

/// The full state of one connection's tunnel handed to the webview: which
/// connection it is, whether anything is running, and the per-forward health
/// (dashboard first). Used both as the return value of the commands and as the
/// [`TUNNEL_STATUS_EVENT`] payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    /// The connection this status belongs to.
    pub connection_id: String,
    /// Whether any forward child is currently tracked for this connection.
    pub running: bool,
    /// Health snapshot for every forward (dashboard first).
    pub forwards: Vec<ForwardView>,
}

// ---- helpers ---------------------------------------------------------------

/// Take a fresh status snapshot for one connection, converting lifecycle health
/// into serde DTOs. An unknown connection reports `running: false` with no
/// forwards. Returns a user-readable error if the registry mutex was poisoned by
/// a panic in another thread.
fn snapshot(registry: &SharedTunnelRegistry, connection_id: &str) -> Result<TunnelStatus, String> {
    let mut reg = registry
        .lock()
        .map_err(|_| "tunnel registry is unavailable (lock poisoned)".to_string())?;
    let running = reg.is_running(connection_id);
    let forwards = reg
        .health(connection_id)
        .unwrap_or_default()
        .into_iter()
        .map(ForwardView::from)
        .collect();
    Ok(TunnelStatus {
        connection_id: connection_id.to_string(),
        running,
        forwards,
    })
}

/// Emit the live status event to the frontend. Emission failures are
/// non-fatal — the command's return value still carries the authoritative
/// status — so they are intentionally swallowed.
fn emit_status(app: &AppHandle, status: &TunnelStatus) {
    let _ = app.emit(TUNNEL_STATUS_EVENT, status);
}

/// A safe, unique webview window label for a connection's dashboard window.
/// Tauri window labels are restricted to a small character set, so any other
/// character in the connection id is mapped to `_`.
fn dashboard_window_label(connection_id: &str) -> String {
    let safe: String = connection_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("dashboard-{safe}")
}

// ---- commands --------------------------------------------------------------

/// Start (or restart) the tunnel for a connection from a raw target + forward
/// spec.
///
/// Builds the plan with the pure parsing logic — reserving the local ports every
/// *other* live connection already holds, so this tunnel never collides with
/// them — resolves the SSH key by path, then spawns one `ssh -L` child per
/// forward via the registry. `start` validates key permissions first, so the
/// `0600`/owner gotcha surfaces here as a clear error string. On success the new
/// status is returned **and** broadcast via [`TUNNEL_STATUS_EVENT`].
#[tauri::command]
pub fn start_tunnel(
    app: AppHandle,
    registry: State<'_, SharedTunnelRegistry>,
    args: StartTunnelArgs,
) -> Result<TunnelStatus, String> {
    if args.connection_id.trim().is_empty() {
        return Err("connectionId is required".to_string());
    }

    // Reserve the local ports held by every *other* connection so this tunnel's
    // dashboard/forwards never reuse them (the dashboard bumps up off a clash).
    let reserved: HashSet<u16> = {
        let mut reg = registry
            .lock()
            .map_err(|_| "tunnel registry is unavailable (lock poisoned)".to_string())?;
        reg.local_ports_excluding(&args.connection_id)
    };

    let plan = build_plan_from_spec(
        &args.target,
        &args.forwards,
        args.port,
        args.dashboard_port,
        &reserved,
    )
    .map_err(|e| e.to_string())?;

    // Resolve the key by PATH only — never read its contents into the app.
    let key_path: PathBuf = match args.key_path {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => resolve_default_ssh_key(args.project_id.as_deref().unwrap_or(""))
            .map_err(|e| e.to_string())?,
    };

    {
        let mut reg = registry
            .lock()
            .map_err(|_| "tunnel registry is unavailable (lock poisoned)".to_string())?;
        // `start` checks the key permissions (the 0600 gotcha) before spawning;
        // its error Display is already an actionable, user-readable message.
        reg.start(&args.connection_id, &plan, &key_path)
            .map_err(|e| e.to_string())?;
    }

    let status = snapshot(registry.inner(), &args.connection_id)?;
    emit_status(&app, &status);
    Ok(status)
}

/// Stop one connection's tunnel, killing and reaping every tracked `ssh` child.
/// Idempotent — stopping an already-idle (or unknown) connection succeeds.
/// Broadcasts the resulting (empty) status via [`TUNNEL_STATUS_EVENT`].
#[tauri::command]
pub fn stop_tunnel(
    app: AppHandle,
    registry: State<'_, SharedTunnelRegistry>,
    connection_id: String,
) -> Result<TunnelStatus, String> {
    {
        let mut reg = registry
            .lock()
            .map_err(|_| "tunnel registry is unavailable (lock poisoned)".to_string())?;
        reg.stop(&connection_id);
    }
    let status = snapshot(registry.inner(), &connection_id)?;
    emit_status(&app, &status);
    Ok(status)
}

/// Return the current state of one connection's forwards (including local URLs),
/// reconciling each child's exit state, an active local-port probe, and stderr
/// signals. This is the pull counterpart to the pushed [`TUNNEL_STATUS_EVENT`].
#[tauri::command]
pub fn tunnel_status(
    registry: State<'_, SharedTunnelRegistry>,
    connection_id: String,
) -> Result<TunnelStatus, String> {
    snapshot(registry.inner(), &connection_id)
}

/// Return just one connection's per-forward views (dashboard first), without the
/// top-level `running` flag — a convenience for views that only render the
/// forward list.
#[tauri::command]
pub fn list_forwards(
    registry: State<'_, SharedTunnelRegistry>,
    connection_id: String,
) -> Result<Vec<ForwardView>, String> {
    Ok(snapshot(registry.inner(), &connection_id)?.forwards)
}

/// Resolve the URL one connection's dashboard is reachable at. When that tunnel
/// is running we use its dashboard forward's actual local URL (in case the port
/// was bumped on a collision), otherwise we fall back to the canonical 7777.
fn dashboard_local_url(registry: &SharedTunnelRegistry, connection_id: &str) -> String {
    if let Ok(mut reg) = registry.lock() {
        if let Some(h) = reg
            .health(connection_id)
            .unwrap_or_default()
            .into_iter()
            .find(|h| h.container_id == DASHBOARD_CONTAINER_ID)
        {
            return h.local_url;
        }
    }
    format!("http://localhost:{DASHBOARD_LOCAL_PORT}")
}

/// Open one connection's jonggrang dashboard in its own in-app webview window so
/// the user can view it without leaving the desktop app. The window is created
/// from Rust (so no extra JS-side ACL permission is needed) and labeled per
/// connection; if it is already open we just focus it instead of stacking
/// duplicates.
#[tauri::command]
pub fn open_dashboard(
    app: AppHandle,
    registry: State<'_, SharedTunnelRegistry>,
    connection_id: String,
) -> Result<(), String> {
    let label = dashboard_window_label(&connection_id);
    if let Some(existing) = app.get_webview_window(&label) {
        existing.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    let url = dashboard_local_url(registry.inner(), &connection_id);
    let parsed = url
        .parse()
        .map_err(|_| format!("invalid dashboard URL: {url}"))?;
    WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(parsed))
        .title(format!("Jonggrang Dashboard ({connection_id})"))
        .inner_size(1100.0, 820.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::{ForwardStatus, DASHBOARD_CONTAINER_ID};

    fn health(status: ForwardStatus) -> ForwardHealth {
        ForwardHealth {
            container_id: DASHBOARD_CONTAINER_ID.to_string(),
            remote_port: 7777,
            local_port: 7777,
            local_url: "http://localhost:7777".to_string(),
            pid: Some(4242),
            status,
        }
    }

    #[test]
    fn forward_view_renders_status_as_wire_string() {
        let view = ForwardView::from(health(ForwardStatus::Connected));
        assert_eq!(view.status, "connected");
        assert_eq!(view.container_id, DASHBOARD_CONTAINER_ID);
        assert_eq!(view.local_url, "http://localhost:7777");
        assert_eq!(view.pid, Some(4242));
    }

    #[test]
    fn forward_view_covers_every_status_variant() {
        assert_eq!(ForwardView::from(health(ForwardStatus::Connecting)).status, "connecting");
        assert_eq!(ForwardView::from(health(ForwardStatus::Connected)).status, "connected");
        assert_eq!(ForwardView::from(health(ForwardStatus::Failed)).status, "failed");
        assert_eq!(ForwardView::from(health(ForwardStatus::Stopped)).status, "stopped");
    }

    #[test]
    fn tunnel_status_serializes_to_camel_case_json() {
        let status = TunnelStatus {
            connection_id: "conn-1".to_string(),
            running: true,
            forwards: vec![ForwardView::from(health(ForwardStatus::Connected))],
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["connectionId"], "conn-1");
        assert_eq!(json["running"], true);
        let fwd = &json["forwards"][0];
        // camelCase rename must hold on the wire so the TS frontend can rely on it.
        assert_eq!(fwd["containerId"], DASHBOARD_CONTAINER_ID);
        assert_eq!(fwd["remotePort"], 7777);
        assert_eq!(fwd["localPort"], 7777);
        assert_eq!(fwd["localUrl"], "http://localhost:7777");
        assert_eq!(fwd["pid"], 4242);
        assert_eq!(fwd["status"], "connected");
    }

    #[test]
    fn start_tunnel_args_deserializes_camel_case_with_optionals() {
        let args: StartTunnelArgs = serde_json::from_str(
            r#"{"connectionId":"conn-1","target":"deploy@srv","forwards":"-c web:8080","port":2022,"dashboardPort":8888,"projectId":"proj-1","keyPath":"/k/id"}"#,
        )
        .unwrap();
        assert_eq!(args.connection_id, "conn-1");
        assert_eq!(args.target, "deploy@srv");
        assert_eq!(args.forwards, "-c web:8080");
        assert_eq!(args.port, Some(2022));
        assert_eq!(args.dashboard_port, Some(8888));
        assert_eq!(args.project_id.as_deref(), Some("proj-1"));
        assert_eq!(args.key_path.as_deref(), Some("/k/id"));
    }

    #[test]
    fn start_tunnel_args_optionals_default_to_none() {
        let args: StartTunnelArgs = serde_json::from_str(
            r#"{"connectionId":"conn-2","target":"deploy@srv","forwards":"web:80"}"#,
        )
        .unwrap();
        assert_eq!(args.connection_id, "conn-2");
        assert!(args.port.is_none());
        assert!(args.dashboard_port.is_none());
        assert!(args.project_id.is_none());
        assert!(args.key_path.is_none());
    }

    #[test]
    fn dashboard_window_label_is_unique_and_sanitized() {
        assert_eq!(dashboard_window_label("conn-1"), "dashboard-conn-1");
        // Characters outside the safe set collapse to underscores.
        assert_eq!(dashboard_window_label("a@b c/d"), "dashboard-a_b_c_d");
        // Distinct connection ids yield distinct labels (no window stacking).
        assert_ne!(
            dashboard_window_label("conn-1"),
            dashboard_window_label("conn-2")
        );
    }
}
