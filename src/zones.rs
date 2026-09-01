//! Outbound IPC: register touch hot-zones with the touch-drawer Wayfire
//! plugin. Mirrors `~/projects/swipe-panel/panel-daemon/src/zones.rs`. One
//! connection per command, errors logged and swallowed — the dock must
//! still boot if the plugin is down.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

thread_local! {
    /// Last enabled value actually sent per zone id. `update_zone_states`
    /// flips four flags on every state settle and most settles change
    /// nothing — each `send` is a fresh connect → write → close on
    /// `touch-drawer.sock`, so skipping an unchanged write is a real power
    /// win on this fanless device (milestone slice 001, low-power item).
    /// Deliberately NOT seeded at registration: if the register write
    /// failed (plugin down at boot), the first settle must still re-send.
    static LAST_ENABLED: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    /// Last bounds tuple actually sent per zone id, same rationale.
    static LAST_BOUNDS: RefCell<HashMap<String, (i32, i32, i32, i32)>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    /// Unidir down — commits only on signed downward motion (`dy >=
    /// commit_threshold`). Used by the broad detailed zone (the close
    /// gesture) and by the sliver-edge zone in Closed/AppGrid (slice 002).
    Down,
    Up,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Down => "down",
            Direction::Up => "up",
        }
    }
}

pub fn touch_drawer_socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime.join("touch-drawer.sock")
}

fn send(line: &str) -> std::io::Result<()> {
    let mut s = UnixStream::connect(touch_drawer_socket_path())?;
    s.write_all(line.as_bytes())?;
    Ok(())
}

/// Register a unidirectional zone. Returns whether the write reached the
/// plugin — callers that track a last-sent signature (like the
/// sliver-edge re-register) must only record success, so a plugin that was
/// down mid-flight gets the next settle re-sending and self-healing.
#[allow(clippy::too_many_arguments)]
pub fn register(
    id: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    dir: Direction,
    reply_sock: &Path,
    begin_arg: &str,
    enabled: bool,
    travel_px: Option<i32>,
) -> bool {
    let mut line = format!(
        "register {} {} {} {} {} {} {} {} {}",
        id,
        x,
        y,
        w,
        h,
        dir.as_str(),
        reply_sock.display(),
        begin_arg,
        if enabled { "true" } else { "false" },
    );
    if let Some(t) = travel_px {
        line.push(' ');
        line.push_str(&t.to_string());
    }
    send_tracked(&line, "register")
}

/// Register a bidirectional zone — touch-drawer commits on either sign of
/// Δy past `commit_threshold` and emits `drag-begin <begin_arg_up>` for
/// upward motion or `drag-begin <begin_arg_down>` for downward. Returns
/// whether the write reached the plugin, same failure-aware contract as
/// [`register`].
///
/// `travel_px` overrides touch-drawer's default progress denominator
/// (`screen_height / 3`). Pass the zone's own height for thin overlays so
/// a swipe across the available pixels reaches progress 1.0.
#[allow(clippy::too_many_arguments)]
pub fn register_bidir(
    id: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    reply_sock: &Path,
    begin_arg_up: &str,
    begin_arg_down: &str,
    enabled: bool,
    travel_px: Option<i32>,
) -> bool {
    let mut line = format!(
        "register {} {} {} {} {} bidir {} {} {} {}",
        id,
        x,
        y,
        w,
        h,
        reply_sock.display(),
        begin_arg_up,
        begin_arg_down,
        if enabled { "true" } else { "false" },
    );
    if let Some(t) = travel_px {
        line.push(' ');
        line.push_str(&t.to_string());
    }
    send_tracked(&line, "register-bidir")
}

/// Send `line`, returning whether it reached the plugin. On failure the
/// caller must NOT record it as the last-sent value — the next state settle
/// re-sends and self-heals, which is how the dock recovers a plugin that
/// was down mid-flight.
fn send_tracked(line: &str, op: &str) -> bool {
    match send(line) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("sliver-dock: zones {op}: {e}");
            false
        }
    }
}

pub fn set_enabled(id: &str, enabled: bool) {
    let last = LAST_ENABLED.with(|m| m.borrow().get(id).copied());
    if last == Some(enabled) {
        return;
    }
    let line = format!(
        "set-enabled {} {}",
        id,
        if enabled { "true" } else { "false" }
    );
    if send_tracked(&line, "set-enabled") {
        LAST_ENABLED.with(|m| m.borrow_mut().insert(id.to_owned(), enabled));
    }
}

pub fn update_bounds(id: &str, x: i32, y: i32, w: i32, h: i32) {
    let bounds = (x, y, w, h);
    let last = LAST_BOUNDS.with(|m| m.borrow().get(id).copied());
    if last == Some(bounds) {
        return;
    }
    let line = format!("update-bounds {} {} {} {} {}", id, x, y, w, h);
    if send_tracked(&line, "update-bounds") {
        LAST_BOUNDS.with(|m| m.borrow_mut().insert(id.to_owned(), bounds));
    }
}
