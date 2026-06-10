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

/// Convenience: parse a `<user>@<host>` target string and a `-c ...` forward
/// string and build a plan in one call. Useful for the Tauri command bridge,
/// which receives both as raw strings from the webview.
pub fn build_plan_from_spec(
    target: &str,
    forwards: &str,
    reserved: &HashSet<u16>,
) -> Result<TunnelPlan, TunnelSpecError> {
    let target = parse_target(target)?;
    let forwards = parse_forwards(forwards)?;
    build_tunnel_plan(target, forwards, reserved)
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
