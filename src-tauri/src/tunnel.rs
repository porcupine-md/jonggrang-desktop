//! Pure tunnel-spec parsing and local-port allocation for the Jonggrang Tunnel
//! desktop app.
//!
//! This module is intentionally **pure**: every function is a deterministic
//! transformation over its inputs with no process spawning, no networking, and
//! no OS-level port probing. That keeps the interesting logic — parsing a
//! `<user>@<server>` target, parsing a `-c <cid>:port,...` forward list, and
//! assigning each forward a distinct local port — fully unit-testable under
//! `cargo test` without a live SSH connection.
//!
//! The stateful child-process lifecycle (spawning `ssh -L`, tracking PIDs,
//! probing whether a local port is actually bound, teardown on exit) is the
//! concern of the lifecycle manager added in task-004; nothing here touches the
//! OS. To stay self-contained and testable in isolation, this module depends
//! only on `std` (no `serde`/`tauri`); serialization for the Tauri bridge is
//! layered on in a later task if needed.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Default **local** port for the jonggrang dashboard forward. The UI surfaces
/// the dashboard at `http://localhost:7777`, matching the feature plan.
pub const DASHBOARD_LOCAL_PORT: u16 = 7777;

/// **Remote** port the dashboard listens on inside the jonggrang server. The
/// dashboard maps local 7777 → remote 7777 by default.
pub const DASHBOARD_REMOTE_PORT: u16 = 7777;

/// Starting point for auto-allocating local ports for `-c` container forwards.
/// Sits just above [`DASHBOARD_LOCAL_PORT`] so the dashboard keeps the canonical
/// 7777 and container forwards fan out above it. Allocation still skips any port
/// already in use, so this is only the *preferred* base, not a guarantee.
pub const FORWARD_LOCAL_PORT_BASE: u16 = 7778;

/// Synthetic container id used for the always-present dashboard forward, so it
/// can flow through the same [`AllocatedForward`] machinery as `-c` entries.
pub const DASHBOARD_CONTAINER_ID: &str = "dashboard";

/// Server-side host the dashboard forward connects to. The jonggrang dashboard
/// runs on the docker host, which is reachable as `host.docker.internal` from
/// inside the jonggrang server container, so the dashboard maps to
/// `-L <local>:host.docker.internal:<remote>` rather than `localhost`.
pub const DASHBOARD_FORWARD_HOST: &str = "host.docker.internal";

/// Server-side host every other (`-c` container) forward connects to. These
/// services listen on the server's own loopback, so they map to
/// `-L <local>:localhost:<remote>`.
pub const DEFAULT_FORWARD_HOST: &str = "localhost";

/// A parsed `<user>@<server>` SSH target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelTarget {
    /// The login user (left of the `@`).
    pub user: String,
    /// The server host (right of the `@`).
    pub host: String,
}

impl TunnelTarget {
    /// Render the target back into the canonical `user@host` form that is
    /// handed to `ssh` by the lifecycle manager.
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// A single requested forward parsed from the `-c <cid>:port` list: a container
/// id plus the remote port to forward. The local port is assigned later by
/// [`build_tunnel_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    /// The jonggrang container id to forward into.
    pub container_id: String,
    /// The port the service listens on *inside* the container / on the server.
    pub remote_port: u16,
}

/// A forward that has been assigned a concrete, distinct local port. This is
/// what the lifecycle manager turns into an `ssh -L <local>:localhost:<remote>`
/// flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatedForward {
    /// Container id (or [`DASHBOARD_CONTAINER_ID`] for the dashboard forward).
    pub container_id: String,
    /// Remote port on the server side.
    pub remote_port: u16,
    /// Distinct local port bound on the user's machine.
    pub local_port: u16,
}

impl AllocatedForward {
    /// The `localhost:<local_port>` URL authority a user opens in their browser.
    pub fn local_url(&self) -> String {
        format!("http://localhost:{}", self.local_port)
    }

    /// The host on the *server* side that `ssh` connects this forward to. The
    /// dashboard lives on the docker host ([`DASHBOARD_FORWARD_HOST`]); every
    /// container forward resolves against the server's own loopback
    /// ([`DEFAULT_FORWARD_HOST`]).
    pub fn forward_host(&self) -> &'static str {
        if self.container_id == DASHBOARD_CONTAINER_ID {
            DASHBOARD_FORWARD_HOST
        } else {
            DEFAULT_FORWARD_HOST
        }
    }
}

/// A fully resolved plan: where to connect, plus every forward with a distinct
/// local port. The dashboard forward is always present and listed first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelPlan {
    /// The `user@host` to SSH into.
    pub target: TunnelTarget,
    /// The always-present dashboard forward (local 7777 → remote 7777 by
    /// default, bumped only on collision).
    pub dashboard: AllocatedForward,
    /// The container forwards, in request order, each with a distinct local port.
    pub forwards: Vec<AllocatedForward>,
}

impl TunnelPlan {
    /// Iterate over every forward in the plan — the dashboard first, then the
    /// container forwards — so the lifecycle manager can build one `ssh -L` per
    /// entry without special-casing the dashboard.
    pub fn all_forwards(&self) -> impl Iterator<Item = &AllocatedForward> {
        std::iter::once(&self.dashboard).chain(self.forwards.iter())
    }

    /// The set of local ports this plan occupies (dashboard + all forwards).
    pub fn local_ports(&self) -> Vec<u16> {
        self.all_forwards().map(|f| f.local_port).collect()
    }
}

/// Typed errors produced while parsing tunnel specs or allocating ports. All
/// variants are recoverable user-input problems, surfaced to the UI as
/// actionable messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelSpecError {
    /// The target string was empty or whitespace-only.
    EmptyTarget,
    /// The target had no `@` separating user and host (or had more than one).
    MalformedTarget(String),
    /// The user portion (left of `@`) was empty.
    EmptyUser,
    /// The host portion (right of `@`) was empty.
    EmptyHost,
    /// The forward list was empty after stripping an optional leading `-c`.
    EmptyForwardList,
    /// A forward entry was empty (e.g. a stray `,,` or trailing comma).
    EmptyForwardEntry,
    /// A forward entry was not of the form `<cid>:<port>`.
    MalformedForward(String),
    /// A forward entry had an empty container id (e.g. `:8080`).
    EmptyContainerId,
    /// A forward entry's port was not a valid 1..=65535 TCP port.
    InvalidPort(String),
    /// No free local port could be found at or above the preferred port (the
    /// 1..=65535 range above `preferred` was exhausted).
    NoFreePort(u16),
}

impl fmt::Display for TunnelSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TunnelSpecError::EmptyTarget => write!(f, "target is empty"),
            TunnelSpecError::MalformedTarget(s) => {
                write!(f, "target '{}' is not of the form <user>@<host>", s)
            }
            TunnelSpecError::EmptyUser => write!(f, "target user (before '@') is empty"),
            TunnelSpecError::EmptyHost => write!(f, "target host (after '@') is empty"),
            TunnelSpecError::EmptyForwardList => write!(f, "forward list is empty"),
            TunnelSpecError::EmptyForwardEntry => write!(f, "forward list has an empty entry"),
            TunnelSpecError::MalformedForward(s) => {
                write!(f, "forward '{}' is not of the form <container>:<port>", s)
            }
            TunnelSpecError::EmptyContainerId => write!(f, "forward has an empty container id"),
            TunnelSpecError::InvalidPort(s) => {
                write!(f, "'{}' is not a valid TCP port (1..=65535)", s)
            }
            TunnelSpecError::NoFreePort(p) => {
                write!(f, "no free local port available at or above {}", p)
            }
        }
    }
}

impl Error for TunnelSpecError {}

/// Parse a `<user>@<server>` target into a typed [`TunnelTarget`].
///
/// Surrounding whitespace is trimmed. Exactly one `@` must be present, and
/// neither side may be empty; otherwise a typed [`TunnelSpecError`] is returned.
pub fn parse_target(input: &str) -> Result<TunnelTarget, TunnelSpecError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(TunnelSpecError::EmptyTarget);
    }
    // Require exactly one '@' so neither the user nor the host can smuggle one in.
    if trimmed.matches('@').count() != 1 {
        return Err(TunnelSpecError::MalformedTarget(trimmed.to_string()));
    }
    let (user, host) = trimmed
        .split_once('@')
        .ok_or_else(|| TunnelSpecError::MalformedTarget(trimmed.to_string()))?;
    let user = user.trim();
    let host = host.trim();
    if user.is_empty() {
        return Err(TunnelSpecError::EmptyUser);
    }
    if host.is_empty() {
        return Err(TunnelSpecError::EmptyHost);
    }
    // A host with internal whitespace is never valid.
    if host.split_whitespace().count() != 1 {
        return Err(TunnelSpecError::MalformedTarget(trimmed.to_string()));
    }
    Ok(TunnelTarget {
        user: user.to_string(),
        host: host.to_string(),
    })
}

/// Parse a forward list of the form `-c <cid>:port,<cid2>:port` into a typed
/// `Vec<Forward>`.
///
/// A leading `-c` token (the form used on the jonggrang command line) is
/// optional and stripped if present. Entries are comma-separated; each must be
/// `<container>:<port>` with a non-empty container id and a 1..=65535 port.
pub fn parse_forwards(input: &str) -> Result<Vec<Forward>, TunnelSpecError> {
    let mut spec = input.trim();
    // Strip an optional leading `-c` flag token.
    if let Some(rest) = spec.strip_prefix("-c") {
        // Only treat `-c` as a flag when it stands alone (followed by space/end),
        // never when it is the start of a container id like "-cache:80".
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            spec = rest.trim();
        }
    }
    if spec.is_empty() {
        return Err(TunnelSpecError::EmptyForwardList);
    }

    let mut forwards = Vec::new();
    for raw in spec.split(',') {
        let entry = raw.trim();
        if entry.is_empty() {
            return Err(TunnelSpecError::EmptyForwardEntry);
        }
        let (cid, port_str) = entry
            .split_once(':')
            .ok_or_else(|| TunnelSpecError::MalformedForward(entry.to_string()))?;
        let cid = cid.trim();
        let port_str = port_str.trim();
        if cid.is_empty() {
            return Err(TunnelSpecError::EmptyContainerId);
        }
        let remote_port = parse_port(port_str)?;
        forwards.push(Forward {
            container_id: cid.to_string(),
            remote_port,
        });
    }
    Ok(forwards)
}

/// Parse a single TCP port, rejecting non-numeric input and port 0.
fn parse_port(s: &str) -> Result<u16, TunnelSpecError> {
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(TunnelSpecError::InvalidPort(s.to_string())),
        Ok(port) => Ok(port),
    }
}

/// Find the next free local port at or above `preferred`, skipping any port in
/// `in_use`.
///
/// This is the **pure** half of port allocation: it reasons only about the
/// supplied `in_use` set, never the OS. The lifecycle manager is responsible
/// for real OS-level probing before binding. Allocation is monotonic upward; if
/// every port from `preferred` through 65535 is occupied, a
/// [`TunnelSpecError::NoFreePort`] is returned.
pub fn next_free_local_port(
    preferred: u16,
    in_use: &HashSet<u16>,
) -> Result<u16, TunnelSpecError> {
    // Port 0 is the "any port" sentinel and never a valid bind target; start at
    // 1 if a caller passes it.
    let mut candidate = preferred.max(1);
    loop {
        if !in_use.contains(&candidate) {
            return Ok(candidate);
        }
        candidate = candidate
            .checked_add(1)
            .ok_or(TunnelSpecError::NoFreePort(preferred))?;
    }
}

/// Build a [`TunnelPlan`] that assigns every forward a **distinct** local port.
///
/// The dashboard forward is allocated first, preferring [`DASHBOARD_LOCAL_PORT`]
/// (7777) and falling back upward on collision. Each container forward is then
/// allocated a distinct local port starting from [`FORWARD_LOCAL_PORT_BASE`],
/// skipping anything already taken — both ports passed in via `reserved` and
/// ports handed to earlier forwards in this same plan.
///
/// `reserved` is an externally-known in-use set (e.g. ports other tunnels hold);
/// it is treated as read-only and copied internally.
pub fn build_tunnel_plan(
    target: TunnelTarget,
    forwards: Vec<Forward>,
    reserved: &HashSet<u16>,
) -> Result<TunnelPlan, TunnelSpecError> {
    let mut in_use = reserved.clone();

    // Dashboard first, so it keeps the canonical 7777 whenever it is free.
    let dashboard_local = next_free_local_port(DASHBOARD_LOCAL_PORT, &in_use)?;
    in_use.insert(dashboard_local);
    let dashboard = AllocatedForward {
        container_id: DASHBOARD_CONTAINER_ID.to_string(),
        remote_port: DASHBOARD_REMOTE_PORT,
        local_port: dashboard_local,
    };

    let mut allocated = Vec::with_capacity(forwards.len());
    let mut preferred = FORWARD_LOCAL_PORT_BASE;
    for forward in forwards {
        let local_port = next_free_local_port(preferred, &in_use)?;
        in_use.insert(local_port);
        allocated.push(AllocatedForward {
            container_id: forward.container_id,
            remote_port: forward.remote_port,
            local_port,
        });
        // Continue searching just above the port we just took so the next
        // forward fans out predictably instead of re-scanning from the base.
        preferred = local_port
            .checked_add(1)
            .ok_or(TunnelSpecError::NoFreePort(local_port))?;
    }

    Ok(TunnelPlan {
        target,
        dashboard,
        forwards: allocated,
    })
}

/// Like [`parse_forwards`] but treats an empty (or bare `-c`) spec as "no
/// container forwards" instead of an error, so a **dashboard-only** tunnel is
/// valid. Any non-empty spec is delegated to [`parse_forwards`], so malformed
/// entries still surface their precise [`TunnelSpecError`].
pub fn parse_forwards_optional(input: &str) -> Result<Vec<Forward>, TunnelSpecError> {
    let mut spec = input.trim();
    // Mirror `parse_forwards`' optional leading `-c` handling before deciding
    // whether the spec is genuinely empty.
    if let Some(rest) = spec.strip_prefix("-c") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            spec = rest.trim();
        }
    }
    if spec.is_empty() {
        return Ok(Vec::new());
    }
    parse_forwards(input)
}

/// Convenience: parse a `<user>@<host>` target string and a `-c ...` forward
/// string and build a plan in one call. Useful for the Tauri command bridge,
/// which receives both as raw strings from the webview. The forward spec is
/// **optional** — an empty string yields a dashboard-only plan (just the
/// always-present 7777 forward).
pub fn build_plan_from_spec(
    target: &str,
    forwards: &str,
    reserved: &HashSet<u16>,
) -> Result<TunnelPlan, TunnelSpecError> {
    let target = parse_target(target)?;
    let forwards = parse_forwards_optional(forwards)?;
    build_tunnel_plan(target, forwards, reserved)
}

// =====================================================================
// SSH tunnel lifecycle manager (task-004)
// =====================================================================
//
// Everything below turns a pure [`TunnelPlan`] into running `ssh -L` child
// processes, tracks their state, and guarantees they are reaped. It stays
// `std`-only (process/net/thread/fs) so the module keeps compiling and testing
// in isolation via `rustc --test` without the `tauri` system-dependency wall.
//
// The pieces that can be made pure — SSH key precedence resolution, key
// permission checks, `ssh -v` stderr classification, and `ssh` argument
// construction — are factored into free functions with their own unit tests.
// The genuinely impure parts (spawning, port probing, reaping) are thin wrappers
// over those pure helpers and are not exercised against a live SSH server.

// ---- SSH key resolution ----------------------------------------------------

/// Errors resolving or validating the SSH private key used by the tunnel.
///
/// The key is only ever referenced by **path** — its contents are never read
/// into memory; `ssh` is handed the path via `-i` and does the reading itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshKeyError {
    /// None of the precedence-ordered candidate paths existed.
    NotFound {
        /// The candidate paths that were searched, in precedence order.
        searched: Vec<PathBuf>,
    },
    /// The key file exists but has group/world-accessible permissions, which
    /// OpenSSH rejects with "bad owner or permissions" (the 0600 gotcha).
    BadPermissions {
        /// The offending key path.
        path: PathBuf,
        /// The file's unix mode bits (permission portion).
        mode: u32,
    },
    /// The key path exists but its metadata could not be read.
    Unreadable {
        /// The key path whose metadata could not be stat'd.
        path: PathBuf,
    },
}

impl fmt::Display for SshKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshKeyError::NotFound { searched } => {
                let list = searched
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "no SSH key found; searched (in order): {}. Add a key at \
                     ~/.jonggrang/web/ssh/<id>.key, ~/.jonggrang/web/ssh/global.key, \
                     or ~/.ssh/id_rsa",
                    list
                )
            }
            SshKeyError::BadPermissions { path, mode } => write!(
                f,
                "SSH key '{}' has insecure permissions {:#o}: it is accessible by \
                 group or others, so ssh will reject it with \"bad owner or \
                 permissions\". Fix it with: chmod 600 '{}' (and ensure you own the file)",
                path.display(),
                mode & 0o777,
                path.display()
            ),
            SshKeyError::Unreadable { path } => {
                write!(f, "SSH key '{}' could not be read", path.display())
            }
        }
    }
}

impl Error for SshKeyError {}

/// Build the precedence-ordered list of candidate SSH key paths for a project,
/// modeling `resolveProjectSshKey` in `.jonggrang/lib/sandbox.js`:
///
/// 1. per-project — `<home>/.jonggrang/web/ssh/<project_id>.key`
/// 2. global      — `<home>/.jonggrang/web/ssh/global.key`
/// 3. default     — `<home>/.ssh/id_rsa`
pub fn ssh_key_candidates(project_id: &str, home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".jonggrang")
            .join("web")
            .join("ssh")
            .join(format!("{}.key", project_id)),
        home.join(".jonggrang").join("web").join("ssh").join("global.key"),
        home.join(".ssh").join("id_rsa"),
    ]
}

/// Resolve the SSH key by returning the first candidate for which `exists`
/// returns true, or [`SshKeyError::NotFound`] listing everything searched.
///
/// The existence predicate is injected so precedence can be unit-tested without
/// touching the filesystem.
pub fn resolve_ssh_key_with<F>(candidates: &[PathBuf], exists: F) -> Result<PathBuf, SshKeyError>
where
    F: Fn(&Path) -> bool,
{
    for candidate in candidates {
        if exists(candidate) {
            return Ok(candidate.clone());
        }
    }
    Err(SshKeyError::NotFound {
        searched: candidates.to_vec(),
    })
}

/// Resolve the SSH key for `project_id` under `home`, hitting the real
/// filesystem. A candidate counts only if it is an existing regular file
/// (matching the `existsSync && isFile()` check in `sandbox.js`).
pub fn resolve_ssh_key(project_id: &str, home: &Path) -> Result<PathBuf, SshKeyError> {
    let candidates = ssh_key_candidates(project_id, home);
    resolve_ssh_key_with(&candidates, |p| p.is_file())
}

/// The user's home directory from `$HOME`, used by [`resolve_default_ssh_key`].
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Convenience: resolve the SSH key for `project_id` under the current user's
/// `$HOME`. Returns [`SshKeyError::NotFound`] (with an empty search list) if
/// `$HOME` is unset.
pub fn resolve_default_ssh_key(project_id: &str) -> Result<PathBuf, SshKeyError> {
    match home_dir() {
        Some(home) => resolve_ssh_key(project_id, &home),
        None => Err(SshKeyError::NotFound { searched: vec![] }),
    }
}

/// Pure permission predicate: an SSH private key is safe only when no group or
/// "other" permission bits are set (mode `& 0o077 == 0`), e.g. `0600`/`0400`.
/// This mirrors the constraint OpenSSH enforces before it will use a key.
pub fn mode_is_ssh_safe(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// Validate that the key at `path` is safe for `ssh` to use, surfacing the
/// `0600`/owner gotcha as a clear, actionable [`SshKeyError::BadPermissions`].
///
/// On non-unix platforms permission bits are not meaningful, so this is a no-op
/// that simply confirms the file is readable.
#[cfg(unix)]
pub fn check_key_permissions(path: &Path) -> Result<(), SshKeyError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|_| SshKeyError::Unreadable {
        path: path.to_path_buf(),
    })?;
    let mode = meta.permissions().mode();
    if mode_is_ssh_safe(mode) {
        Ok(())
    } else {
        Err(SshKeyError::BadPermissions {
            path: path.to_path_buf(),
            mode,
        })
    }
}

/// Non-unix fallback: only confirm the key file is stat-able.
#[cfg(not(unix))]
pub fn check_key_permissions(path: &Path) -> Result<(), SshKeyError> {
    std::fs::metadata(path)
        .map(|_| ())
        .map_err(|_| SshKeyError::Unreadable {
            path: path.to_path_buf(),
        })
}

// ---- ssh -v stderr classification ------------------------------------------

/// A reason an `ssh` forward failed, derived from its `-v` stderr. Each carries
/// a human-readable [`fmt::Display`] message suitable for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshFailure {
    /// Authentication was rejected (wrong/declined key, `Permission denied`).
    AuthDenied,
    /// The key was refused for insecure permissions/ownership (the 0600 gotcha).
    KeyPermissions,
    /// The server could not be reached (DNS, refused, timeout, no route).
    HostUnreachable,
    /// Host-key verification failed (`known_hosts` mismatch / unknown host).
    HostKeyVerification,
    /// The local forward port could not be bound (already in use / rejected).
    ForwardingRejected,
}

impl fmt::Display for SshFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            SshFailure::AuthDenied => {
                "authentication failed — the SSH key was declined by the server"
            }
            SshFailure::KeyPermissions => {
                "ssh refused the key for bad owner or permissions — chmod 600 the key file"
            }
            SshFailure::HostUnreachable => {
                "could not reach the server — check the host, network, and that sshd is running"
            }
            SshFailure::HostKeyVerification => {
                "host key verification failed — the server's host key is unknown or changed"
            }
            SshFailure::ForwardingRejected => {
                "local port forwarding could not be established — the local port may be in use"
            }
        };
        f.write_str(msg)
    }
}

/// A meaningful state signal extracted from a single line of `ssh -v` stderr.
/// Lines that carry no lifecycle signal classify to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshLogSignal {
    /// The connection authenticated and local forwarding is in effect.
    ForwardingEstablished,
    /// A terminal failure with a categorized reason.
    Failed(SshFailure),
}

/// Classify a single line of `ssh -v` stderr into an optional [`SshLogSignal`].
///
/// Matching is case-insensitive against the stable phrases OpenSSH emits.
/// Failures are checked before success so a late error wins. This is pure and
/// the primary unit-tested status helper.
pub fn classify_ssh_log_line(line: &str) -> Option<SshLogSignal> {
    let l = line.to_ascii_lowercase();

    // Failures first — a fatal line should never be masked by an earlier
    // "established"-looking line.
    if l.contains("bad owner or permissions") || l.contains("unprotected private key file") {
        return Some(SshLogSignal::Failed(SshFailure::KeyPermissions));
    }
    if l.contains("permission denied")
        || l.contains("too many authentication failures")
        || l.contains("no more authentication methods")
    {
        return Some(SshLogSignal::Failed(SshFailure::AuthDenied));
    }
    if l.contains("host key verification failed") {
        return Some(SshLogSignal::Failed(SshFailure::HostKeyVerification));
    }
    if l.contains("could not resolve hostname")
        || l.contains("connection refused")
        || l.contains("connection timed out")
        || l.contains("operation timed out")
        || l.contains("no route to host")
        || l.contains("network is unreachable")
    {
        return Some(SshLogSignal::Failed(SshFailure::HostUnreachable));
    }
    if l.contains("address already in use")
        || l.contains("cannot listen to port")
        || l.contains("could not request local forwarding")
        || l.contains("bind: ")
    {
        return Some(SshLogSignal::Failed(SshFailure::ForwardingRejected));
    }

    // Success signals — `-v` prints these once the session is usable.
    if l.contains("entering interactive session")
        || l.contains("authenticated to ")
        || l.contains("local forwarding listening on")
        || l.contains("forwarding port")
    {
        return Some(SshLogSignal::ForwardingEstablished);
    }

    None
}

// ---- ssh argument construction ---------------------------------------------

/// Build the full `ssh` argument vector (everything after the program name) for
/// one forward, of the form:
///
/// `-i <key> -N -v -o BatchMode=yes -o ExitOnForwardFailure=yes \
///  -o ServerAliveInterval=15 -L <local>:<forward-host>:<remote> <user>@<host>`
///
/// The forward host is the dashboard's docker host for the dashboard forward and
/// the server loopback for container forwards — see [`AllocatedForward::forward_host`].
///
/// `BatchMode=yes` keeps a GUI app from hanging on a password prompt (key-only),
/// and `ExitOnForwardFailure=yes` makes `ssh` exit (rather than linger) if the
/// forward can't be set up, which the status inference relies on. Only the key
/// **path** is passed — never its contents. Pure and unit-tested.
pub fn build_ssh_args(target: &TunnelTarget, key_path: &Path, forward: &AllocatedForward) -> Vec<String> {
    vec![
        "-i".to_string(),
        key_path.to_string_lossy().into_owned(),
        "-N".to_string(),
        "-v".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-L".to_string(),
        format!("{}:{}:{}", forward.local_port, forward.forward_host(), forward.remote_port),
        target.ssh_target(),
    ]
}

// ---- local-port probing ----------------------------------------------------

/// The loopback address a forward's local listener is probed at.
pub fn local_probe_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// How long to wait for the loopback connect when probing a local port.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Actively probe whether something is listening on `127.0.0.1:<port>` by
/// attempting a short-timeout TCP connect. A successful connect means `ssh` has
/// bound the local forward; failure means it has not (yet). This is the active
/// half of status inference and is intentionally not unit-tested against a live
/// server.
pub fn probe_local_port(port: u16) -> bool {
    TcpStream::connect_timeout(&local_probe_addr(port), PROBE_TIMEOUT).is_ok()
}

// ---- status enum & health --------------------------------------------------

/// The inferred connection state of a single forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardStatus {
    /// The `ssh` child is running but the local port is not yet bound / the
    /// session has not authenticated.
    Connecting,
    /// The local port is bound and reachable — the forward is usable.
    Connected,
    /// The `ssh` child exited non-zero or reported a fatal error.
    Failed,
    /// The forward was deliberately stopped (or never started).
    Stopped,
}

impl fmt::Display for ForwardStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ForwardStatus::Connecting => "connecting",
            ForwardStatus::Connected => "connected",
            ForwardStatus::Failed => "failed",
            ForwardStatus::Stopped => "stopped",
        })
    }
}

/// A point-in-time health snapshot for one forward, suitable for handing to the
/// webview (the Tauri command bridge in task-005 adds serde on top).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardHealth {
    /// Container id (or [`DASHBOARD_CONTAINER_ID`]).
    pub container_id: String,
    /// Remote port on the server side.
    pub remote_port: u16,
    /// Local port bound on this machine.
    pub local_port: u16,
    /// The `http://localhost:<local_port>` URL the user opens.
    pub local_url: String,
    /// OS pid of the tracked `ssh` child, if still spawned.
    pub pid: Option<u32>,
    /// The inferred connection state.
    pub status: ForwardStatus,
}

// ---- per-forward child process ---------------------------------------------

/// One spawned `ssh -L` child plus the bookkeeping needed to infer its state
/// and to reap it. A background thread parses its `-v` stderr into `status`.
struct ForwardProcess {
    forward: AllocatedForward,
    pid: Option<u32>,
    child: Child,
    /// Latest signal observed from stderr; reconciled with port probing and the
    /// child's exit status in [`ForwardProcess::current_status`].
    status: Arc<Mutex<ForwardStatus>>,
    reader: Option<JoinHandle<()>>,
}

impl ForwardProcess {
    /// Derive the current status by reconciling three signals, strongest first:
    /// the child's exit state (`try_wait`), an active local-port probe, then the
    /// stderr-observed status.
    fn current_status(&mut self) -> ForwardStatus {
        match self.child.try_wait() {
            // Child has exited: a clean exit means it was stopped/torn down, a
            // non-zero exit means it failed.
            Ok(Some(exit)) => {
                return if exit.success() {
                    ForwardStatus::Stopped
                } else {
                    ForwardStatus::Failed
                };
            }
            // Could not determine — treat as failed rather than silently OK.
            Err(_) => return ForwardStatus::Failed,
            // Still running — fall through to probing.
            Ok(None) => {}
        }

        // Running: an open loopback port is the authoritative "Connected" signal.
        if probe_local_port(self.forward.local_port) {
            return ForwardStatus::Connected;
        }

        // Running but not yet bound: trust a fatal stderr signal, else still
        // connecting (a thread-observed "established" without a bound port means
        // it is mid-handshake).
        match *self.status.lock().expect("status mutex poisoned") {
            ForwardStatus::Failed => ForwardStatus::Failed,
            _ => ForwardStatus::Connecting,
        }
    }

    /// Kill the child and reap it so no zombie/leaked `ssh` remains, then join
    /// the stderr reader thread (which ends on stderr EOF once the child dies).
    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ForwardProcess {
    fn drop(&mut self) {
        // RAII teardown: dropping a ForwardProcess always reaps its child. This
        // is what guarantees no `ssh` leaks when the manager is dropped on app
        // exit (or while unwinding from a panic).
        self.kill_and_reap();
    }
}

/// Spawn the stderr reader thread for a forward: it parses each `-v` line and
/// folds meaningful signals into the shared `status`.
fn spawn_status_reader(stderr: ChildStderr, status: Arc<Mutex<ForwardStatus>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(signal) = classify_ssh_log_line(&line) {
                let mut current = status.lock().expect("status mutex poisoned");
                *current = match signal {
                    SshLogSignal::ForwardingEstablished => ForwardStatus::Connected,
                    SshLogSignal::Failed(_) => ForwardStatus::Failed,
                };
            }
        }
        // Stderr EOF means the child closed it (exited). Leave the last observed
        // status in place; `current_status` reconciles via `try_wait`/probe.
    })
}

/// Spawn a single `ssh -L` child for one forward, wiring up its stderr reader.
fn spawn_forward(
    target: &TunnelTarget,
    key_path: &Path,
    forward: &AllocatedForward,
) -> std::io::Result<ForwardProcess> {
    let args = build_ssh_args(target, key_path, forward);
    let mut command = Command::new("ssh");
    command.args(&args);
    spawn_forward_process(command, forward.clone())
}

/// Spawn `command` as a forward child — wiring stdio (null stdin/stdout, piped
/// stderr) and the `-v` stderr status reader — and wrap it in a
/// [`ForwardProcess`] bound to `forward`.
///
/// Factoring the raw spawn out of [`spawn_forward`] (which fixes the program to
/// `ssh`) gives the lifecycle tests a seam to inject a stand-in child program
/// and exercise the spawn→track→reap state machine without a live SSH server.
fn spawn_forward_process(
    mut command: Command,
    forward: AllocatedForward,
) -> std::io::Result<ForwardProcess> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = Some(child.id());
    let status = Arc::new(Mutex::new(ForwardStatus::Connecting));
    let reader = child
        .stderr
        .take()
        .map(|err| spawn_status_reader(err, Arc::clone(&status)));
    Ok(ForwardProcess {
        forward,
        pid,
        child,
        status,
        reader,
    })
}

// ---- lifecycle errors & manager --------------------------------------------

/// Errors starting a tunnel: either the key was unusable, or a child failed to
/// spawn.
#[derive(Debug)]
pub enum TunnelError {
    /// SSH key resolution or permission validation failed.
    Key(SshKeyError),
    /// Spawning an `ssh` child failed (e.g. `ssh` not on `PATH`).
    Spawn(std::io::Error),
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TunnelError::Key(e) => write!(f, "{}", e),
            TunnelError::Spawn(e) => write!(f, "failed to spawn ssh: {}", e),
        }
    }
}

impl Error for TunnelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            TunnelError::Key(e) => Some(e),
            TunnelError::Spawn(e) => Some(e),
        }
    }
}

impl From<SshKeyError> for TunnelError {
    fn from(e: SshKeyError) -> Self {
        TunnelError::Key(e)
    }
}

/// Stateful manager of the `ssh -L` children for one tunnel.
///
/// One child is spawned per forward (dashboard + each `-c` entry), so each
/// forward has independent status and teardown. The manager is `Send + Sync`
/// (all fields are), so it is meant to be held behind a `Mutex` as Tauri managed
/// state; see [`SharedTunnelManager`].
///
/// **Teardown is guaranteed two ways**: [`TunnelManager::stop`] kills and reaps
/// every child on demand, and every child is also reaped via `Drop` (RAII) when
/// the manager is dropped — which the Tauri layer ensures happens on app exit by
/// calling `stop()` from `RunEvent::Exit`/`ExitRequested` (and the drop catches
/// panics). The only thing that can leak is `SIGKILL` of the app itself, which
/// no in-process hook can prevent.
pub struct TunnelManager {
    forwards: Vec<ForwardProcess>,
}

/// The intended shared form of the manager: held behind a `Mutex` (so it is
/// mutable from `&self` Tauri commands) inside an `Arc` (so the exit handler and
/// command handlers share it).
pub type SharedTunnelManager = Arc<Mutex<TunnelManager>>;

impl TunnelManager {
    /// Create an idle manager with no children spawned.
    pub fn new() -> Self {
        TunnelManager { forwards: Vec::new() }
    }

    /// Start the tunnel: validate the key's permissions, tear down any existing
    /// children, then spawn one `ssh -L` child per forward in `plan`.
    ///
    /// Only the key **path** is passed to `ssh` (via `-i`); its contents are
    /// never read here. If any child fails to spawn, the ones already spawned in
    /// this call are reaped (via their `Drop`) before the error is returned, so a
    /// failed start never leaks a partial set of children.
    pub fn start(&mut self, plan: &TunnelPlan, key_path: &Path) -> Result<(), TunnelError> {
        self.start_with(plan, key_path, spawn_forward)
    }

    /// The spawn machinery behind [`TunnelManager::start`], parameterized by the
    /// per-forward spawn function so tests can inject a stand-in child program.
    /// The production path passes [`spawn_forward`] (real `ssh`); the contract is
    /// identical either way — validate the key, tear down any previous children,
    /// then spawn one child per forward, never leaking a partial set on failure.
    fn start_with<F>(
        &mut self,
        plan: &TunnelPlan,
        key_path: &Path,
        spawn: F,
    ) -> Result<(), TunnelError>
    where
        F: Fn(&TunnelTarget, &Path, &AllocatedForward) -> std::io::Result<ForwardProcess>,
    {
        check_key_permissions(key_path)?;
        // Tear down anything from a previous start first.
        self.stop();

        // Spawn into a local vec so an early failure drops (and reaps) the
        // partial set without ever touching `self.forwards`.
        let mut spawned = Vec::new();
        for forward in plan.all_forwards() {
            let process = spawn(&plan.target, key_path, forward).map_err(TunnelError::Spawn)?;
            spawned.push(process);
        }
        self.forwards = spawned;
        Ok(())
    }

    /// Whether any forward child is currently tracked.
    pub fn is_running(&self) -> bool {
        !self.forwards.is_empty()
    }

    /// A health snapshot for every tracked forward (status + local URL + pid).
    pub fn health(&mut self) -> Vec<ForwardHealth> {
        self.forwards
            .iter_mut()
            .map(|fp| ForwardHealth {
                container_id: fp.forward.container_id.clone(),
                remote_port: fp.forward.remote_port,
                local_port: fp.forward.local_port,
                local_url: fp.forward.local_url(),
                pid: fp.pid,
                status: fp.current_status(),
            })
            .collect()
    }

    /// Kill and reap every tracked child. Idempotent: draining the vec drops
    /// each [`ForwardProcess`], whose `Drop` does the kill/reap, so afterwards no
    /// `ssh` child remains.
    pub fn stop(&mut self) {
        self.forwards.clear();
    }
}

impl Default for TunnelManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_use(ports: &[u16]) -> HashSet<u16> {
        ports.iter().copied().collect()
    }

    // ---- parse_target -----------------------------------------------------

    #[test]
    fn parse_target_valid() {
        let t = parse_target("deploy@example.com").unwrap();
        assert_eq!(t.user, "deploy");
        assert_eq!(t.host, "example.com");
        assert_eq!(t.ssh_target(), "deploy@example.com");
    }

    #[test]
    fn parse_target_trims_whitespace() {
        let t = parse_target("  alice@10.0.0.1  ").unwrap();
        assert_eq!(t.user, "alice");
        assert_eq!(t.host, "10.0.0.1");
    }

    #[test]
    fn parse_target_empty_is_error() {
        assert_eq!(parse_target(""), Err(TunnelSpecError::EmptyTarget));
        assert_eq!(parse_target("   "), Err(TunnelSpecError::EmptyTarget));
    }

    #[test]
    fn parse_target_missing_at_is_error() {
        assert_eq!(
            parse_target("nohostsep"),
            Err(TunnelSpecError::MalformedTarget("nohostsep".to_string()))
        );
    }

    #[test]
    fn parse_target_multiple_at_is_error() {
        assert_eq!(
            parse_target("a@b@c"),
            Err(TunnelSpecError::MalformedTarget("a@b@c".to_string()))
        );
    }

    #[test]
    fn parse_target_empty_user_is_error() {
        assert_eq!(parse_target("@host"), Err(TunnelSpecError::EmptyUser));
    }

    #[test]
    fn parse_target_empty_host_is_error() {
        assert_eq!(parse_target("user@"), Err(TunnelSpecError::EmptyHost));
    }

    #[test]
    fn parse_target_host_with_space_is_error() {
        assert_eq!(
            parse_target("user@bad host"),
            Err(TunnelSpecError::MalformedTarget("user@bad host".to_string()))
        );
    }

    // ---- parse_forwards ---------------------------------------------------

    #[test]
    fn parse_forwards_single() {
        let f = parse_forwards("web:8080").unwrap();
        assert_eq!(
            f,
            vec![Forward {
                container_id: "web".to_string(),
                remote_port: 8080,
            }]
        );
    }

    #[test]
    fn parse_forwards_strips_leading_flag() {
        let f = parse_forwards("-c web:8080").unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].container_id, "web");
        assert_eq!(f[0].remote_port, 8080);
    }

    #[test]
    fn parse_forwards_multiple() {
        let f = parse_forwards("-c web:8080,db:5432,cache:6379").unwrap();
        assert_eq!(
            f,
            vec![
                Forward { container_id: "web".to_string(), remote_port: 8080 },
                Forward { container_id: "db".to_string(), remote_port: 5432 },
                Forward { container_id: "cache".to_string(), remote_port: 6379 },
            ]
        );
    }

    #[test]
    fn parse_forwards_tolerates_inner_whitespace() {
        let f = parse_forwards("-c  web : 8080 , db : 5432 ").unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0], Forward { container_id: "web".to_string(), remote_port: 8080 });
        assert_eq!(f[1], Forward { container_id: "db".to_string(), remote_port: 5432 });
    }

    #[test]
    fn parse_forwards_container_starting_with_c_is_not_flag() {
        // "cache:80" must not have its leading 'c' eaten as the `-c` flag, and a
        // container literally named with a leading dash like "-cache" is kept.
        let f = parse_forwards("cache:80").unwrap();
        assert_eq!(f[0].container_id, "cache");
        let f2 = parse_forwards("-cache:80").unwrap();
        assert_eq!(f2[0].container_id, "-cache");
    }

    #[test]
    fn parse_forwards_empty_is_error() {
        assert_eq!(parse_forwards(""), Err(TunnelSpecError::EmptyForwardList));
        assert_eq!(parse_forwards("-c"), Err(TunnelSpecError::EmptyForwardList));
        assert_eq!(parse_forwards("-c   "), Err(TunnelSpecError::EmptyForwardList));
    }

    #[test]
    fn parse_forwards_empty_entry_is_error() {
        assert_eq!(
            parse_forwards("web:8080,,db:5432"),
            Err(TunnelSpecError::EmptyForwardEntry)
        );
        assert_eq!(
            parse_forwards("web:8080,"),
            Err(TunnelSpecError::EmptyForwardEntry)
        );
    }

    #[test]
    fn parse_forwards_missing_colon_is_error() {
        assert_eq!(
            parse_forwards("web8080"),
            Err(TunnelSpecError::MalformedForward("web8080".to_string()))
        );
    }

    #[test]
    fn parse_forwards_empty_container_is_error() {
        assert_eq!(parse_forwards(":8080"), Err(TunnelSpecError::EmptyContainerId));
    }

    #[test]
    fn parse_forwards_invalid_port_is_error() {
        assert_eq!(
            parse_forwards("web:notaport"),
            Err(TunnelSpecError::InvalidPort("notaport".to_string()))
        );
        assert_eq!(
            parse_forwards("web:0"),
            Err(TunnelSpecError::InvalidPort("0".to_string()))
        );
        assert_eq!(
            parse_forwards("web:70000"),
            Err(TunnelSpecError::InvalidPort("70000".to_string()))
        );
    }

    // ---- next_free_local_port --------------------------------------------

    #[test]
    fn next_free_local_port_returns_preferred_when_free() {
        assert_eq!(next_free_local_port(7777, &in_use(&[])).unwrap(), 7777);
    }

    #[test]
    fn next_free_local_port_skips_used() {
        let used = in_use(&[7777, 7778, 7779]);
        assert_eq!(next_free_local_port(7777, &used).unwrap(), 7780);
    }

    #[test]
    fn next_free_local_port_exhaustion_is_error() {
        // Everything from 65534..=65535 taken, preferred 65534 → no room above.
        let used = in_use(&[65534, 65535]);
        assert_eq!(
            next_free_local_port(65534, &used),
            Err(TunnelSpecError::NoFreePort(65534))
        );
    }

    // ---- build_tunnel_plan -----------------------------------------------

    fn target() -> TunnelTarget {
        TunnelTarget { user: "deploy".to_string(), host: "srv".to_string() }
    }

    #[test]
    fn build_plan_default_dashboard_port() {
        let plan = build_tunnel_plan(target(), vec![], &in_use(&[])).unwrap();
        assert_eq!(plan.dashboard.local_port, DASHBOARD_LOCAL_PORT);
        assert_eq!(plan.dashboard.remote_port, DASHBOARD_REMOTE_PORT);
        assert_eq!(plan.dashboard.local_port, 7777);
        assert_eq!(plan.dashboard.container_id, DASHBOARD_CONTAINER_ID);
        assert!(plan.forwards.is_empty());
        assert_eq!(plan.dashboard.local_url(), "http://localhost:7777");
    }

    #[test]
    fn build_plan_single_forward_distinct_port() {
        let forwards = parse_forwards("-c web:8080").unwrap();
        let plan = build_tunnel_plan(target(), forwards, &in_use(&[])).unwrap();
        assert_eq!(plan.dashboard.local_port, 7777);
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].container_id, "web");
        assert_eq!(plan.forwards[0].remote_port, 8080);
        assert_eq!(plan.forwards[0].local_port, FORWARD_LOCAL_PORT_BASE);
        assert_ne!(plan.forwards[0].local_port, plan.dashboard.local_port);
    }

    #[test]
    fn build_plan_multiple_forwards_all_distinct() {
        let forwards = parse_forwards("-c web:8080,db:5432,cache:6379").unwrap();
        let plan = build_tunnel_plan(target(), forwards, &in_use(&[])).unwrap();
        let locals = plan.local_ports();
        // Dashboard + 3 forwards = 4 ports, all distinct.
        assert_eq!(locals.len(), 4);
        let distinct: HashSet<u16> = locals.iter().copied().collect();
        assert_eq!(distinct.len(), 4, "all local ports must be distinct");
        // Deterministic fan-out: 7777 dashboard, then 7778/7779/7780.
        assert_eq!(plan.dashboard.local_port, 7777);
        assert_eq!(plan.forwards[0].local_port, 7778);
        assert_eq!(plan.forwards[1].local_port, 7779);
        assert_eq!(plan.forwards[2].local_port, 7780);
    }

    #[test]
    fn build_plan_dashboard_collision_bumps_up() {
        // 7777 already reserved → dashboard must move to the next free port.
        let plan = build_tunnel_plan(target(), vec![], &in_use(&[7777])).unwrap();
        assert_eq!(plan.dashboard.local_port, 7778);
    }

    #[test]
    fn build_plan_collision_avoidance_against_reserved_and_each_other() {
        // Reserve the canonical dashboard port and the first two forward ports.
        let reserved = in_use(&[7777, 7778, 7779]);
        let forwards = parse_forwards("-c web:8080,db:5432").unwrap();
        let plan = build_tunnel_plan(target(), forwards, &reserved).unwrap();
        // Dashboard bumped to 7780 (7777..7779 reserved).
        assert_eq!(plan.dashboard.local_port, 7780);
        // Forwards must avoid reserved AND the dashboard's 7780 → 7781, 7782.
        assert_eq!(plan.forwards[0].local_port, 7781);
        assert_eq!(plan.forwards[1].local_port, 7782);
        let locals = plan.local_ports();
        let distinct: HashSet<u16> = locals.iter().copied().collect();
        assert_eq!(distinct.len(), locals.len(), "no port may collide");
        // None of the allocated ports may be in the reserved set.
        for p in &locals {
            assert!(!reserved.contains(p), "allocated port {} collided with reserved", p);
        }
    }

    #[test]
    fn parse_forwards_optional_treats_empty_as_no_forwards() {
        // Empty / whitespace / bare `-c` are all valid dashboard-only specs.
        assert_eq!(parse_forwards_optional(""), Ok(vec![]));
        assert_eq!(parse_forwards_optional("   "), Ok(vec![]));
        assert_eq!(parse_forwards_optional("-c"), Ok(vec![]));
        assert_eq!(parse_forwards_optional("-c   "), Ok(vec![]));
    }

    #[test]
    fn parse_forwards_optional_still_parses_and_validates_entries() {
        let f = parse_forwards_optional("-c web:8080").unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].container_id, "web");
        assert_eq!(f[0].remote_port, 8080);
        // A genuinely malformed entry must still error, not be swallowed.
        assert_eq!(
            parse_forwards_optional("web:notaport"),
            Err(TunnelSpecError::InvalidPort("notaport".to_string()))
        );
    }

    #[test]
    fn build_plan_from_spec_allows_dashboard_only() {
        // No `-c` forwards → a valid plan with just the dashboard forward.
        let plan = build_plan_from_spec("deploy@srv", "", &in_use(&[])).unwrap();
        assert_eq!(plan.dashboard.local_port, 7777);
        assert!(plan.forwards.is_empty());
    }

    #[test]
    fn build_plan_from_spec_end_to_end() {
        let plan =
            build_plan_from_spec("deploy@srv.example.com", "-c api:3000", &in_use(&[])).unwrap();
        assert_eq!(plan.target.ssh_target(), "deploy@srv.example.com");
        assert_eq!(plan.dashboard.local_port, 7777);
        assert_eq!(plan.forwards[0].container_id, "api");
        assert_eq!(plan.forwards[0].remote_port, 3000);
        assert_eq!(plan.forwards[0].local_port, 7778);
    }

    #[test]
    fn build_plan_from_spec_propagates_target_error() {
        assert_eq!(
            build_plan_from_spec("bogus", "-c web:80", &in_use(&[])),
            Err(TunnelSpecError::MalformedTarget("bogus".to_string()))
        );
    }

    #[test]
    fn all_forwards_yields_dashboard_first() {
        let forwards = parse_forwards("-c web:8080").unwrap();
        let plan = build_tunnel_plan(target(), forwards, &in_use(&[])).unwrap();
        let ids: Vec<&str> = plan.all_forwards().map(|f| f.container_id.as_str()).collect();
        assert_eq!(ids, vec![DASHBOARD_CONTAINER_ID, "web"]);
    }

    #[test]
    fn error_display_is_human_readable() {
        assert_eq!(TunnelSpecError::EmptyTarget.to_string(), "target is empty");
        assert!(TunnelSpecError::InvalidPort("x".to_string())
            .to_string()
            .contains("not a valid TCP port"));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn target() -> TunnelTarget {
        TunnelTarget {
            user: "deploy".to_string(),
            host: "srv.example.com".to_string(),
        }
    }

    // ---- SSH key resolution & precedence ---------------------------------

    #[test]
    fn key_candidates_follow_jonggrang_precedence() {
        let home = Path::new("/home/dev");
        let cands = ssh_key_candidates("proj-42", home);
        assert_eq!(
            cands,
            vec![
                PathBuf::from("/home/dev/.jonggrang/web/ssh/proj-42.key"),
                PathBuf::from("/home/dev/.jonggrang/web/ssh/global.key"),
                PathBuf::from("/home/dev/.ssh/id_rsa"),
            ]
        );
    }

    #[test]
    fn resolve_prefers_project_key_first() {
        let cands = ssh_key_candidates("proj", Path::new("/h"));
        // Every candidate "exists": the highest-precedence one must win.
        let got = resolve_ssh_key_with(&cands, |_| true).unwrap();
        assert_eq!(got, PathBuf::from("/h/.jonggrang/web/ssh/proj.key"));
    }

    #[test]
    fn resolve_falls_back_to_global_then_default() {
        let cands = ssh_key_candidates("proj", Path::new("/h"));
        let global = PathBuf::from("/h/.jonggrang/web/ssh/global.key");
        let default = PathBuf::from("/h/.ssh/id_rsa");

        // Project key missing → global wins.
        let got = resolve_ssh_key_with(&cands, |p| p != cands[0].as_path()).unwrap();
        assert_eq!(got, global);

        // Project + global missing → default id_rsa wins.
        let got = resolve_ssh_key_with(&cands, |p| p == default.as_path()).unwrap();
        assert_eq!(got, default);
    }

    #[test]
    fn resolve_none_existing_is_not_found_listing_all() {
        let cands = ssh_key_candidates("proj", Path::new("/h"));
        let err = resolve_ssh_key_with(&cands, |_| false).unwrap_err();
        match err {
            SshKeyError::NotFound { searched } => assert_eq!(searched, cands),
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    // ---- key permission (0600) gotcha ------------------------------------

    #[test]
    fn mode_safe_only_without_group_or_other_bits() {
        assert!(mode_is_ssh_safe(0o600));
        assert!(mode_is_ssh_safe(0o400));
        assert!(!mode_is_ssh_safe(0o644));
        assert!(!mode_is_ssh_safe(0o640));
        assert!(!mode_is_ssh_safe(0o606));
        assert!(!mode_is_ssh_safe(0o660));
    }

    #[test]
    fn bad_permissions_error_mentions_chmod_600() {
        let err = SshKeyError::BadPermissions {
            path: PathBuf::from("/h/.ssh/id_rsa"),
            mode: 0o644,
        };
        let msg = err.to_string();
        assert!(msg.contains("bad owner or permissions"));
        assert!(msg.contains("chmod 600"));
        assert!(msg.contains("/h/.ssh/id_rsa"));
    }

    #[test]
    fn not_found_error_lists_searched_paths() {
        let err = SshKeyError::NotFound {
            searched: ssh_key_candidates("p", Path::new("/h")),
        };
        let msg = err.to_string();
        assert!(msg.contains("/h/.jonggrang/web/ssh/p.key"));
        assert!(msg.contains("global.key"));
        assert!(msg.contains("id_rsa"));
    }

    // ---- ssh -v stderr classification ------------------------------------

    #[test]
    fn classify_detects_established() {
        assert_eq!(
            classify_ssh_log_line("debug1: Entering interactive session."),
            Some(SshLogSignal::ForwardingEstablished)
        );
        assert_eq!(
            classify_ssh_log_line("debug1: Authenticated to srv ([10.0.0.1]:22)."),
            Some(SshLogSignal::ForwardingEstablished)
        );
        assert_eq!(
            classify_ssh_log_line("debug1: Local forwarding listening on 127.0.0.1 port 7777."),
            Some(SshLogSignal::ForwardingEstablished)
        );
    }

    #[test]
    fn classify_detects_key_permission_gotcha() {
        assert_eq!(
            classify_ssh_log_line(
                "Permissions 0644 for '/h/.ssh/id_rsa' are too open. bad owner or permissions"
            ),
            Some(SshLogSignal::Failed(SshFailure::KeyPermissions))
        );
        assert_eq!(
            classify_ssh_log_line("UNPROTECTED PRIVATE KEY FILE!"),
            Some(SshLogSignal::Failed(SshFailure::KeyPermissions))
        );
    }

    #[test]
    fn classify_detects_auth_and_host_failures() {
        assert_eq!(
            classify_ssh_log_line("deploy@srv: Permission denied (publickey)."),
            Some(SshLogSignal::Failed(SshFailure::AuthDenied))
        );
        assert_eq!(
            classify_ssh_log_line("ssh: Could not resolve hostname srv: Name or service not known"),
            Some(SshLogSignal::Failed(SshFailure::HostUnreachable))
        );
        assert_eq!(
            classify_ssh_log_line("connect to host srv port 22: Connection refused"),
            Some(SshLogSignal::Failed(SshFailure::HostUnreachable))
        );
        assert_eq!(
            classify_ssh_log_line("Host key verification failed."),
            Some(SshLogSignal::Failed(SshFailure::HostKeyVerification))
        );
        assert_eq!(
            classify_ssh_log_line("bind: Address already in use"),
            Some(SshLogSignal::Failed(SshFailure::ForwardingRejected))
        );
        assert_eq!(
            classify_ssh_log_line("Could not request local forwarding."),
            Some(SshLogSignal::Failed(SshFailure::ForwardingRejected))
        );
    }

    #[test]
    fn classify_ignores_noise_lines() {
        assert_eq!(classify_ssh_log_line("debug1: Reading configuration data"), None);
        assert_eq!(classify_ssh_log_line(""), None);
        assert_eq!(classify_ssh_log_line("debug2: resolving \"srv\" port 22"), None);
    }

    #[test]
    fn classify_failure_wins_over_established_on_same_line() {
        // A pathological line containing both phrases must classify as failure.
        let line = "Entering interactive session ... Permission denied";
        assert_eq!(
            classify_ssh_log_line(line),
            Some(SshLogSignal::Failed(SshFailure::AuthDenied))
        );
    }

    #[test]
    fn ssh_failure_messages_are_actionable() {
        assert!(SshFailure::KeyPermissions.to_string().contains("chmod 600"));
        assert!(SshFailure::AuthDenied.to_string().contains("declined"));
        assert!(SshFailure::HostUnreachable.to_string().contains("reach"));
    }

    // ---- ssh argument construction ---------------------------------------

    #[test]
    fn build_ssh_args_has_expected_shape() {
        let fwd = AllocatedForward {
            container_id: DASHBOARD_CONTAINER_ID.to_string(),
            remote_port: 7777,
            local_port: 7777,
        };
        let args = build_ssh_args(&target(), Path::new("/h/.ssh/id_rsa"), &fwd);

        // Key passed by path only, via -i, never its contents.
        let i = args.iter().position(|a| a == "-i").expect("missing -i");
        assert_eq!(args[i + 1], "/h/.ssh/id_rsa");

        assert!(args.contains(&"-N".to_string()), "must include -N (no remote command)");
        assert!(args.contains(&"-v".to_string()), "must include -v for status parsing");
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        // The dashboard forward targets the docker host, not loopback.
        assert!(args.contains(&"7777:host.docker.internal:7777".to_string()));

        // The -L value follows the -L flag.
        let l = args.iter().position(|a| a == "-L").expect("missing -L");
        assert_eq!(args[l + 1], "7777:host.docker.internal:7777");

        // Target is the final argument.
        assert_eq!(args.last().unwrap(), "deploy@srv.example.com");
    }

    #[test]
    fn build_ssh_args_maps_distinct_ports() {
        let fwd = AllocatedForward {
            container_id: "web".to_string(),
            remote_port: 8080,
            local_port: 7778,
        };
        let args = build_ssh_args(&target(), Path::new("/k"), &fwd);
        assert!(args.contains(&"7778:localhost:8080".to_string()));
    }

    #[test]
    fn forward_host_is_docker_host_only_for_dashboard() {
        let dashboard = AllocatedForward {
            container_id: DASHBOARD_CONTAINER_ID.to_string(),
            remote_port: 7777,
            local_port: 7777,
        };
        assert_eq!(dashboard.forward_host(), "host.docker.internal");

        let container = AllocatedForward {
            container_id: "web".to_string(),
            remote_port: 8080,
            local_port: 7778,
        };
        assert_eq!(container.forward_host(), "localhost");
    }

    // ---- status enum / probing -------------------------------------------

    #[test]
    fn forward_status_display_is_stable() {
        assert_eq!(ForwardStatus::Connecting.to_string(), "connecting");
        assert_eq!(ForwardStatus::Connected.to_string(), "connected");
        assert_eq!(ForwardStatus::Failed.to_string(), "failed");
        assert_eq!(ForwardStatus::Stopped.to_string(), "stopped");
    }

    #[test]
    fn probe_addr_is_loopback() {
        let addr = local_probe_addr(7777);
        assert_eq!(addr.to_string(), "127.0.0.1:7777");
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn probe_unbound_port_is_false() {
        // Reserve a port via a listener to learn a free one, drop it, then probe:
        // nothing is listening, so the probe must report false — no live SSH or
        // server required.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!probe_local_port(port));
    }

    // ---- manager basics & Send+Sync --------------------------------------

    #[test]
    fn new_manager_is_idle() {
        let mgr = TunnelManager::new();
        assert!(!mgr.is_running());
    }

    #[test]
    fn stop_on_idle_manager_is_noop() {
        let mut mgr = TunnelManager::new();
        mgr.stop();
        assert!(!mgr.is_running());
        assert!(mgr.health().is_empty());
    }

    #[test]
    fn manager_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TunnelManager>();
        assert_send_sync::<SharedTunnelManager>();
    }

    // ---- probe against a real loopback socket (P1) -----------------------

    #[test]
    fn probe_bound_port_is_true() {
        // A live loopback listener must be reported reachable — the positive
        // counterpart to `probe_unbound_port_is_false`, exercising the active
        // half of status inference with no SSH server. (The negative direction
        // is covered deterministically by `probe_unbound_port_is_false`; pairing
        // it here would race on lingering kernel state from this probe's connect.)
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe_local_port(port), "a bound loopback port must probe true");
    }

    // ---- TunnelManager lifecycle via injected stand-in child (P2/P3) -----

    /// Create a real `0600` key file so `start_with`'s permission pre-flight
    /// passes. The path is only stat'd, never spawned against — the tests below
    /// inject a stand-in program in place of `ssh`.
    fn temp_key_0600(tag: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir()
            .join(format!("jonggrang-test-key-{}-{}", std::process::id(), tag));
        std::fs::write(&path, b"not-a-real-key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    /// A stand-in spawn fn that runs a long-lived `sleep` (ignores its args,
    /// binds no port) in place of `ssh`, giving the manager real PIDs to track.
    fn spawn_sleeper(
        _t: &TunnelTarget,
        _k: &Path,
        fwd: &AllocatedForward,
    ) -> std::io::Result<ForwardProcess> {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        spawn_forward_process(cmd, fwd.clone())
    }

    fn plan_with(forward_spec: &str) -> TunnelPlan {
        build_plan_from_spec("deploy@srv.example.com", forward_spec, &HashSet::new()).unwrap()
    }

    #[test]
    fn start_tracks_one_child_per_forward_and_stop_reaps_all() {
        // dashboard + 2 container forwards = 3 children.
        let plan = plan_with("-c web:8080,db:5432");
        let key = temp_key_0600("track");
        let mut mgr = TunnelManager::new();

        mgr.start_with(&plan, &key, spawn_sleeper).unwrap();
        assert!(mgr.is_running());

        let health = mgr.health();
        assert_eq!(health.len(), 3, "one tracked child per forward");
        assert!(health.iter().all(|h| h.pid.is_some()), "every forward has a live pid");
        // A long-lived sleeper binds no port and emits no `-v` signal → the
        // status stays Connecting (never spuriously Connected).
        assert!(health.iter().all(|h| h.status == ForwardStatus::Connecting));

        mgr.stop();
        assert!(!mgr.is_running(), "stop drains every tracked child");
        assert!(mgr.health().is_empty());

        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn failed_spawn_never_leaks_a_partial_set() {
        // The dashboard child spawns fine; the next forward's program does not
        // exist, so its spawn() errors → start must surface Spawn and leave
        // `forwards` empty (the already-spawned dashboard child is dropped and
        // reaped, never retained as a partial set).
        let plan = plan_with("-c web:8080");
        let key = temp_key_0600("partial");
        let mut mgr = TunnelManager::new();

        let result = mgr.start_with(&plan, &key, |_t, _k, fwd: &AllocatedForward| {
            if fwd.container_id == DASHBOARD_CONTAINER_ID {
                let mut cmd = Command::new("sleep");
                cmd.arg("30");
                spawn_forward_process(cmd, fwd.clone())
            } else {
                spawn_forward_process(
                    Command::new("/nonexistent/jonggrang-fake-ssh-bin"),
                    fwd.clone(),
                )
            }
        });

        assert!(matches!(result, Err(TunnelError::Spawn(_))));
        assert!(!mgr.is_running(), "a failed start must not retain a partial set");
        assert!(mgr.health().is_empty());

        // The manager remains usable: a clean start afterwards works.
        mgr.start_with(&plan, &key, spawn_sleeper).unwrap();
        assert!(mgr.is_running());
        mgr.stop();

        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn current_status_reports_failed_when_child_exits_nonzero() {
        // `false` exits non-zero immediately; current_status must reconcile the
        // child's exit (via try_wait) to Failed, overriding the initial
        // Connecting. Polled with a bound so a regression cannot hang the suite.
        let plan = plan_with("-c web:8080");
        let key = temp_key_0600("failed");
        let mut mgr = TunnelManager::new();

        mgr.start_with(&plan, &key, |_t, _k, fwd: &AllocatedForward| {
            spawn_forward_process(Command::new("false"), fwd.clone())
        })
        .unwrap();

        let mut all_failed = false;
        for _ in 0..200 {
            if mgr.health().iter().all(|h| h.status == ForwardStatus::Failed) {
                all_failed = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(all_failed, "exited-nonzero children must reconcile to Failed");

        mgr.stop();
        let _ = std::fs::remove_file(&key);
    }
}
