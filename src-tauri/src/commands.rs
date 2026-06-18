//! Tauri command bridge between the webview and the SSH tunnel lifecycle
//! manager (task-005).
//!
//! This module is the only place `tauri` and the pure tunnel logic meet. It
//! exposes four `#[tauri::command]` handlers — [`start_tunnel`], [`stop_tunnel`],
//! [`tunnel_status`], and [`list_forwards`] — that drive a shared
//! [`SharedTunnelManager`] held as Tauri managed state, and it emits a live
//! [`TUNNEL_STATUS_EVENT`] after every state change so the UI can update without
//! polling.
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
    build_plan_from_spec, resolve_default_ssh_key, ForwardHealth, SharedTunnelManager,
    DASHBOARD_CONTAINER_ID, DASHBOARD_LOCAL_PORT,
};

/// Window label for the in-app dashboard webview opened by [`open_dashboard`].
const DASHBOARD_WINDOW_LABEL: &str = "dashboard";

/// Name of the event emitted to the frontend whenever the tunnel state changes.
/// The payload is a serialized [`TunnelStatus`]. The webview listens via
/// `listen('tunnel-status', ...)` so it never has to poll.
pub const TUNNEL_STATUS_EVENT: &str = "tunnel-status";

// ---- inputs ----------------------------------------------------------------

/// Arguments for [`start_tunnel`], received from the webview as a JSON object.
///
/// `target` and `forwards` are the raw `<user>@<host>` and `-c <cid>:port,...`
/// strings; they are parsed and allocated by the pure task-003 logic. The SSH
/// key is resolved by **path only** (its contents are never read): an explicit
/// `keyPath` overrides resolution, otherwise the jonggrang precedence is used
/// with `projectId`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartTunnelArgs {
    /// The `<user>@<host>` SSH target.
    pub target: String,
    /// The forward spec, e.g. `-c web:8080,db:5432` (the leading `-c` is
    /// optional). **Optional**: an empty or omitted string yields a
    /// dashboard-only tunnel (just the always-present 7777 forward), parsed via
    /// [`crate::tunnel::parse_forwards_optional`].
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

/// The full tunnel state handed to the webview: whether anything is running and
/// the per-forward health (dashboard first). Used both as the return value of
/// the commands and as the [`TUNNEL_STATUS_EVENT`] payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    /// Whether any forward child is currently tracked.
    pub running: bool,
    /// Health snapshot for every forward (dashboard first).
    pub forwards: Vec<ForwardView>,
}

// ---- helpers ---------------------------------------------------------------

/// Take a fresh status snapshot from the manager, converting lifecycle health
/// into serde DTOs. Returns a user-readable error if the manager mutex was
/// poisoned by a panic in another thread.
fn snapshot(manager: &SharedTunnelManager) -> Result<TunnelStatus, String> {
    let mut mgr = manager
        .lock()
        .map_err(|_| "tunnel manager is unavailable (lock poisoned)".to_string())?;
    let running = mgr.is_running();
    let forwards = mgr.health().into_iter().map(ForwardView::from).collect();
    Ok(TunnelStatus { running, forwards })
}

/// Emit the live status event to the frontend. Emission failures are
/// non-fatal — the command's return value still carries the authoritative
/// status — so they are intentionally swallowed.
fn emit_status(app: &AppHandle, status: &TunnelStatus) {
    let _ = app.emit(TUNNEL_STATUS_EVENT, status);
}

// ---- commands --------------------------------------------------------------

/// Start (or restart) the tunnel from a raw target + forward spec.
///
/// Builds the plan with the pure task-003 logic, resolves the SSH key by path,
/// then spawns one `ssh -L` child per forward via the lifecycle manager.
/// `start` validates key permissions first, so the `0600`/owner gotcha surfaces
/// here as a clear error string. On success the new status is returned **and**
/// broadcast via [`TUNNEL_STATUS_EVENT`].
#[tauri::command]
pub fn start_tunnel(
    app: AppHandle,
    manager: State<'_, SharedTunnelManager>,
    args: StartTunnelArgs,
) -> Result<TunnelStatus, String> {
    // No externally-reserved ports to avoid — the OS-level probe lives in the
    // lifecycle manager; here we just allocate distinct local ports purely.
    let reserved: HashSet<u16> = HashSet::new();
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
        let mut mgr = manager
            .lock()
            .map_err(|_| "tunnel manager is unavailable (lock poisoned)".to_string())?;
        // `start` checks the key permissions (the 0600 gotcha) before spawning;
        // its error Display is already an actionable, user-readable message.
        mgr.start(&plan, &key_path).map_err(|e| e.to_string())?;
    }

    let status = snapshot(manager.inner())?;
    emit_status(&app, &status);
    Ok(status)
}

/// Stop the tunnel, killing and reaping every tracked `ssh` child. Idempotent —
/// stopping an already-idle tunnel succeeds. Broadcasts the resulting (empty)
/// status via [`TUNNEL_STATUS_EVENT`].
#[tauri::command]
pub fn stop_tunnel(
    app: AppHandle,
    manager: State<'_, SharedTunnelManager>,
) -> Result<TunnelStatus, String> {
    {
        let mut mgr = manager
            .lock()
            .map_err(|_| "tunnel manager is unavailable (lock poisoned)".to_string())?;
        mgr.stop();
    }
    let status = snapshot(manager.inner())?;
    emit_status(&app, &status);
    Ok(status)
}

/// Return the current state of every forward (including local URLs), reconciling
/// each child's exit state, an active local-port probe, and stderr signals. This
/// is the pull counterpart to the pushed [`TUNNEL_STATUS_EVENT`].
#[tauri::command]
pub fn tunnel_status(manager: State<'_, SharedTunnelManager>) -> Result<TunnelStatus, String> {
    snapshot(manager.inner())
}

/// Return just the per-forward views (dashboard first), without the top-level
/// `running` flag — a convenience for views that only render the forward list.
#[tauri::command]
pub fn list_forwards(
    manager: State<'_, SharedTunnelManager>,
) -> Result<Vec<ForwardView>, String> {
    Ok(snapshot(manager.inner())?.forwards)
}

/// Resolve the URL the dashboard is reachable at. When the tunnel is running we
/// use the dashboard forward's actual local URL (in case 7777 was bumped on a
/// port collision), otherwise we fall back to the canonical 7777.
fn dashboard_local_url(manager: &SharedTunnelManager) -> String {
    if let Ok(mut mgr) = manager.lock() {
        if let Some(h) = mgr
            .health()
            .into_iter()
            .find(|h| h.container_id == DASHBOARD_CONTAINER_ID)
        {
            return h.local_url;
        }
    }
    format!("http://localhost:{DASHBOARD_LOCAL_PORT}")
}

/// Open the jonggrang dashboard in its own in-app webview window so the user can
/// view it at `http://localhost:7777` without leaving the desktop app. The
/// window is created from Rust (so no extra JS-side ACL permission is needed);
/// if it is already open we just focus it instead of stacking duplicates.
#[tauri::command]
pub fn open_dashboard(
    app: AppHandle,
    manager: State<'_, SharedTunnelManager>,
) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window(DASHBOARD_WINDOW_LABEL) {
        existing.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    let url = dashboard_local_url(manager.inner());
    let parsed = url
        .parse()
        .map_err(|_| format!("invalid dashboard URL: {url}"))?;
    WebviewWindowBuilder::new(&app, DASHBOARD_WINDOW_LABEL, WebviewUrl::External(parsed))
        .title("Jonggrang Dashboard")
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
            running: true,
            forwards: vec![ForwardView::from(health(ForwardStatus::Connected))],
        };
        let json = serde_json::to_value(&status).unwrap();
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
            r#"{"target":"deploy@srv","forwards":"-c web:8080","port":2022,"dashboardPort":8888,"projectId":"proj-1","keyPath":"/k/id"}"#,
        )
        .unwrap();
        assert_eq!(args.target, "deploy@srv");
        assert_eq!(args.forwards, "-c web:8080");
        assert_eq!(args.port, Some(2022));
        assert_eq!(args.dashboard_port, Some(8888));
        assert_eq!(args.project_id.as_deref(), Some("proj-1"));
        assert_eq!(args.key_path.as_deref(), Some("/k/id"));
    }

    #[test]
    fn start_tunnel_args_optionals_default_to_none() {
        let args: StartTunnelArgs =
            serde_json::from_str(r#"{"target":"deploy@srv","forwards":"web:80"}"#).unwrap();
        assert!(args.port.is_none());
        assert!(args.dashboard_port.is_none());
        assert!(args.project_id.is_none());
        assert!(args.key_path.is_none());
    }
}
