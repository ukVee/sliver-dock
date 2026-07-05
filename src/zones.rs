//! Outbound IPC: register touch hot-zones with the touch-drawer Wayfire
//! plugin. Mirrors `~/projects/swipe-panel/panel-daemon/src/zones.rs`. One
//! connection per command, errors logged and swallowed — the dock must
//! still boot if the plugin is down.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    #[allow(dead_code)] // protocol-complete; no unidir-down zone today
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

fn send_or_log(line: &str, op: &str) {
    if let Err(e) = send(line) {
        eprintln!("sliver-dock: zones {op}: {e}");
    }
}

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
) {
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
    send_or_log(&line, "register");
}

/// Register a bidirectional zone — touch-drawer commits on either sign of
/// Δy past `commit_threshold` and emits `drag-begin <begin_arg_up>` for
/// upward motion or `drag-begin <begin_arg_down>` for downward.
///
/// `travel_px` overrides touch-drawer's default progress denominator
/// (`screen_height / 3`). Pass the zone's own height for thin overlays so
/// a swipe across the available pixels reaches progress 1.0.
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
) {
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
    send_or_log(&line, "register-bidir");
}

pub fn set_enabled(id: &str, enabled: bool) {
    let line = format!(
        "set-enabled {} {}",
        id,
        if enabled { "true" } else { "false" }
    );
    send_or_log(&line, "set-enabled");
}

pub fn update_bounds(id: &str, x: i32, y: i32, w: i32, h: i32) {
    let line = format!("update-bounds {} {} {} {} {}", id, x, y, w, h);
    send_or_log(&line, "update-bounds");
}
