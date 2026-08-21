//! Wayfire IPC client.
//!
//! Protocol (verified against Wayfire 0.10.1-746bc7e9 with the `ipc-rules`
//! plugin loaded — see `docs/wayfire-integration.md` for the full method
//! / event surface):
//!
//!     frame  := <u32 LE length><JSON body of `length` bytes>
//!     req    := {"method": "<name>", "data": {...}?}
//!     resp   := {"result": "ok", ...} | {"error": "...", "method": "..."}
//!     event  := {"event": "<name>", ...}   (only on subscribed connections)
//!
//! Two connections per session:
//!   * **Subscriber**: long-lived. Sends one `window-rules/events/watch`
//!     request, expects an `{"result":"ok"}` ack, then reads event frames
//!     until the connection drops. Reconnects with exponential backoff.
//!   * **Command channel**: short-lived per-call. `set_workspace` and
//!     similar issue a fresh connection, write one frame, read one frame.
//!
//! Failure model:
//!   * Socket missing → log once, run in degraded mode (no events,
//!     `set_workspace` returns Err).
//!   * Subscriber disconnects mid-session → backoff + reconnect + emit a
//!     `Reconnected` event so the daemon resyncs initial state.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::state::WorkspaceId;

/// Resolve the Wayfire IPC socket path. Recent Wayfire versions set
/// `WAYFIRE_SOCKET` on every spawned client — that's the most reliable
/// source. We fall back to scanning `$XDG_RUNTIME_DIR` for a name that
/// looks like a wayfire socket (the trailing-dash quirk on this user's
/// system means we can't pattern-match strictly).
pub fn socket_path() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("WAYFIRE_SOCKET") {
        return Some(PathBuf::from(v));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let entries = std::fs::read_dir(&runtime).ok()?;
    for e in entries.flatten() {
        let name = e.file_name();
        let s = name.to_string_lossy();
        if s.starts_with("wayfire") && (s.ends_with(".sock") || s.ends_with(".socket")) {
            return Some(e.path());
        }
    }
    None
}

pub fn log_status() {
    match socket_path() {
        Some(p) => eprintln!("sliver-dock: wayfire IPC socket at {}", p.display()),
        None => eprintln!(
            "sliver-dock: no wayfire IPC socket found — running in degraded \
             mode (no workspace tracking)"
        ),
    }
}

/// Events emitted by the subscriber thread to the GTK main loop.
#[derive(Debug, Clone)]
pub enum Event {
    /// Initial-state snapshot after (re)connect. Carries the focused
    /// workspace + the grid size + the output pixel size (needed for
    /// terminal-layout geometry math, etc.).
    Connected {
        workspace: WorkspaceId,
        grid: GridSize,
        output_id: i64,
        output_size: (i32, i32),
    },
    /// Workspace switched on a wset.
    WorkspaceChanged {
        workspace: WorkspaceId,
        output_id: i64,
    },
    /// Set of workspaces with at least one toplevel view assigned.
    /// Refreshed whenever a view is mapped / unmapped.
    OpenedWorkspacesChanged(HashSet<WorkspaceId>),
    /// Set of distinct app-ids with at least one mapped toplevel view.
    /// Refreshed alongside `OpenedWorkspacesChanged`. Empty app-ids are
    /// filtered out — they can never match a desktop-entry id.
    RunningAppsChanged(HashSet<String>),
    /// Set of workspaces that currently host at least one fullscreen
    /// toplevel view. Drives `cfg.behaviour.hide_when_fullscreen`: when a
    /// workspace enters this set, the daemon forces it to `Hidden`; when
    /// it leaves the set the prior state is restored.
    FullscreenWorkspacesChanged(HashSet<WorkspaceId>),
    /// Per-workspace ordered list of app-ids backing the workspace icon
    /// montage. Order is most-recently-focused first, deduped by app-id
    /// (multiple firefox windows collapse to one slot). Workspaces with
    /// no toplevel views are absent from the map; truncation to 4 +
    /// "+N" overflow lives in the renderer.
    WorkspaceAppMontagesChanged(HashMap<WorkspaceId, Vec<String>>),
    /// Toplevel view just mapped. Carries id + app-id so the daemon can
    /// pair the new view with a pending action (e.g. terminal layout
    /// slot consumption). Fires for every toplevel map.
    ViewMapped { id: i64, app_id: String },
    /// Subscriber lost the connection. The daemon should expect a
    /// `Connected` once reconnect succeeds.
    Disconnected,
}

#[derive(Debug, Clone, Copy)]
pub struct GridSize {
    pub width: i32,
    pub height: i32,
}

// ---- frame I/O ----------------------------------------------------------

fn read_frame(stream: &mut UnixStream) -> std::io::Result<Value> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_frame(stream: &mut UnixStream, body: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(body)?;
    let len = bytes.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn one_shot_call(method: &str, data: Option<Value>) -> std::io::Result<Value> {
    let path = socket_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "WAYFIRE_SOCKET not set")
    })?;
    let mut stream = UnixStream::connect(path)?;
    let mut req = json!({ "method": method });
    if let Some(d) = data {
        req["data"] = d;
    }
    write_frame(&mut stream, &req)?;
    read_frame(&mut stream)
}

// ---- public commands ---------------------------------------------------

#[allow(dead_code)] // Phase 4c: called from the click-to-switch handler
pub fn set_workspace(ws: WorkspaceId, output_id: i64) -> std::io::Result<()> {
    let resp = one_shot_call(
        "vswitch/set-workspace",
        Some(json!({ "x": ws.x, "y": ws.y, "output-id": output_id })),
    )?;
    if resp.get("result").and_then(|v| v.as_str()) == Some("ok") {
        Ok(())
    } else {
        Err(std::io::Error::other(
            format!("set-workspace failed: {resp}"),
        ))
    }
}

/// Position + resize a view by id. Geometry is interpreted in
/// *current-workspace* output coords (Wayfire's convention) — for v1's
/// terminal layout we always switch focus to the target workspace before
/// issuing this, so the values match a freshly-spawned view's frame of
/// reference.
pub fn configure_view(view_id: i64, x: i32, y: i32, w: i32, h: i32) -> std::io::Result<()> {
    let resp = one_shot_call(
        "window-rules/configure-view",
        Some(json!({
            "id": view_id,
            "geometry": { "x": x, "y": y, "width": w, "height": h },
        })),
    )?;
    if resp.get("result").and_then(|v| v.as_str()) == Some("ok") {
        Ok(())
    } else {
        Err(std::io::Error::other(
            format!("configure-view failed: {resp}"),
        ))
    }
}

struct OutputState {
    workspace: WorkspaceId,
    grid: GridSize,
    output_id: i64,
    output_size: (i32, i32),
}

fn fetch_output_state() -> std::io::Result<OutputState> {
    let resp = one_shot_call("window-rules/get-focused-output", None)?;
    let info = resp
        .get("info")
        .ok_or_else(|| std::io::Error::other("no info in response"))?;
    let ws = info
        .get("workspace")
        .ok_or_else(|| std::io::Error::other("no workspace field"))?;
    let id = info
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| std::io::Error::other("no output id"))?;
    let geom = info
        .get("geometry")
        .ok_or_else(|| std::io::Error::other("no geometry field"))?;
    let workspace = WorkspaceId {
        x: ws.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        y: ws.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
    };
    let grid = GridSize {
        width: ws.get("grid_width").and_then(|v| v.as_i64()).unwrap_or(1) as i32,
        height: ws.get("grid_height").and_then(|v| v.as_i64()).unwrap_or(1) as i32,
    };
    let output_size = (
        geom.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        geom.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
    );
    Ok(OutputState {
        workspace,
        grid,
        output_id: id,
        output_size,
    })
}

fn fetch_view_list() -> std::io::Result<Vec<Value>> {
    let resp = one_shot_call("window-rules/list-views", None)?;
    match resp {
        Value::Array(a) => Ok(a),
        _ => Err(std::io::Error::other(
            "list-views: expected array",
        )),
    }
}

/// Per-view tracking info: the workspace the view's center sits on, plus
/// its app-id (xdg-toplevel app_id, ideally matching a `.desktop` file
/// stem). `app_id` may be empty for views that don't set one (some
/// xwayland-bridged X clients).
#[derive(Debug, Clone)]
struct ViewInfo {
    workspace: WorkspaceId,
    app_id: String,
    fullscreen: bool,
}

/// Compute which workspace + app-id a view contributes to tracking. Returns
/// `None` for non-toplevel views (layer != "workspace"), unmapped views,
/// or sticky views (which appear on every workspace).
///
/// Wayfire reports view geometry in *focused-workspace-relative* output
/// coords. The view's center divided by output dimensions gives the
/// workspace offset from the focused workspace; absolute = focused +
/// offset. `div_euclid` handles negative coordinates correctly (a view
/// at x = -1275 on a 1280-wide output is one workspace to the left).
fn extract_view(
    view: &Value,
    output_size: (i32, i32),
    focused: WorkspaceId,
) -> Option<(i64, ViewInfo)> {
    let id = view.get("id")?.as_i64()?;
    let layer = view.get("layer").and_then(|v| v.as_str()).unwrap_or("");
    if layer != "workspace" {
        return None;
    }
    if !view.get("mapped").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    if view.get("sticky").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    let geom = view.get("geometry")?;
    let vx = geom.get("x")?.as_i64()? as i32;
    let vy = geom.get("y")?.as_i64()? as i32;
    let vw = geom.get("width")?.as_i64()? as i32;
    let vh = geom.get("height")?.as_i64()? as i32;
    let (ow, oh) = output_size;
    if ow <= 0 || oh <= 0 {
        return None;
    }
    let cx = vx + vw / 2;
    let cy = vy + vh / 2;
    let workspace = WorkspaceId {
        x: focused.x + cx.div_euclid(ow),
        y: focused.y + cy.div_euclid(oh),
    };
    let app_id = view
        .get("app-id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let fullscreen = view
        .get("fullscreen")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Some((
        id,
        ViewInfo {
            workspace,
            app_id,
            fullscreen,
        },
    ))
}

// ---- subscriber thread --------------------------------------------------

/// Spawn a background thread that maintains the events subscription.
/// On every successful (re)connect it issues a `Connected` event with the
/// focused workspace + grid size, then forwards each `wset-workspace-changed`
/// as a `WorkspaceChanged`. On disconnect it emits `Disconnected`, sleeps
/// (exponential backoff capped at 30s), and retries.
pub fn spawn_subscriber(tx: async_channel::Sender<Event>) {
    thread::spawn(move || run_subscriber(tx));
}

fn run_subscriber(tx: async_channel::Sender<Event>) {
    let mut backoff_ms: u64 = 500;
    loop {
        match subscribe_once(&tx) {
            Ok(()) => {
                // Clean disconnect (read returned EOF). Reset backoff since
                // we held a healthy connection for some time.
                backoff_ms = 500;
            }
            Err(e) => {
                eprintln!("sliver-dock: wayfire subscriber: {e}");
            }
        }
        let _ = tx.send_blocking(Event::Disconnected);

        thread::sleep(Duration::from_millis(backoff_ms));
        backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
    }
}

fn subscribe_once(tx: &async_channel::Sender<Event>) -> std::io::Result<()> {
    let path = socket_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "WAYFIRE_SOCKET not set")
    })?;
    let mut stream = UnixStream::connect(&path)?;
    write_frame(&mut stream, &json!({ "method": "window-rules/events/watch" }))?;
    let ack = read_frame(&mut stream)?;
    if ack.get("result").and_then(|v| v.as_str()) != Some("ok") {
        return Err(std::io::Error::other(
            format!("subscribe ack: {ack}"),
        ));
    }

    // Initial state on a separate one-shot. If it fails, log and
    // continue without it — events alone keep us live.
    let mut state = match fetch_output_state() {
        Ok(s) => {
            let _ = tx.send_blocking(Event::Connected {
                workspace: s.workspace,
                grid: s.grid,
                output_id: s.output_id,
                output_size: s.output_size,
            });
            s
        }
        Err(e) => {
            eprintln!("sliver-dock: wayfire initial state: {e}");
            // Fall through with a placeholder; events still flow.
            OutputState {
                workspace: WorkspaceId::ZERO,
                grid: GridSize { width: 1, height: 1 },
                output_id: 0,
                output_size: (0, 0),
            }
        }
    };

    let mut views_by_id: HashMap<i64, ViewInfo> = HashMap::new();
    // Per-workspace most-recently-focused id list, head = MRU. Initial
    // population from `list-views` is order-arbitrary — historical focus
    // isn't queryable — so the first few `view-focused` events sort it.
    let mut lru: HashMap<WorkspaceId, Vec<i64>> = HashMap::new();
    if let Ok(views) = fetch_view_list() {
        for v in views {
            if let Some((id, info)) = extract_view(&v, state.output_size, state.workspace) {
                lru.entry(info.workspace).or_default().push(id);
                views_by_id.insert(id, info);
            }
        }
    }
    let _ = tx.send_blocking(Event::OpenedWorkspacesChanged(opened_set(&views_by_id)));
    let _ = tx.send_blocking(Event::RunningAppsChanged(running_apps_set(&views_by_id)));
    let _ = tx.send_blocking(Event::FullscreenWorkspacesChanged(fullscreen_set(
        &views_by_id,
    )));
    let _ = tx.send_blocking(Event::WorkspaceAppMontagesChanged(montages_from(
        &views_by_id,
        &lru,
    )));

    loop {
        let frame = read_frame(&mut stream)?;
        let Some(name) = frame.get("event").and_then(|v| v.as_str()) else {
            continue;
        };
        process_event(name, &frame, &mut state, &mut views_by_id, &mut lru, tx);
    }
}

fn opened_set(views_by_id: &HashMap<i64, ViewInfo>) -> HashSet<WorkspaceId> {
    views_by_id.values().map(|v| v.workspace).collect()
}

fn running_apps_set(views_by_id: &HashMap<i64, ViewInfo>) -> HashSet<String> {
    views_by_id
        .values()
        .filter(|v| !v.app_id.is_empty())
        .map(|v| v.app_id.clone())
        .collect()
}

fn fullscreen_set(views_by_id: &HashMap<i64, ViewInfo>) -> HashSet<WorkspaceId> {
    views_by_id
        .values()
        .filter(|v| v.fullscreen)
        .map(|v| v.workspace)
        .collect()
}

/// Project (views_by_id, per-workspace LRU) → per-workspace ordered app-id
/// lists. Multiple views with the same app-id (two firefox windows on one
/// ws) collapse to a single slot keyed by first MRU occurrence. Empty
/// app-ids are filtered out — they can't match a .desktop entry. Untracked
/// ids in the LRU (race with view-unmapped) are skipped.
fn montages_from(
    views_by_id: &HashMap<i64, ViewInfo>,
    lru: &HashMap<WorkspaceId, Vec<i64>>,
) -> HashMap<WorkspaceId, Vec<String>> {
    let mut out: HashMap<WorkspaceId, Vec<String>> = HashMap::new();
    for (ws, ids) in lru {
        let mut apps: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for id in ids {
            let Some(info) = views_by_id.get(id) else { continue };
            if info.app_id.is_empty() {
                continue;
            }
            if seen.insert(info.app_id.clone()) {
                apps.push(info.app_id.clone());
            }
        }
        if !apps.is_empty() {
            out.insert(*ws, apps);
        }
    }
    out
}

/// Emit OpenedWorkspacesChanged / RunningAppsChanged / FullscreenWorkspacesChanged
/// / WorkspaceAppMontagesChanged only when the derived sets actually change —
/// the whole map is recomputed cheap, and skipping no-op events keeps the
/// GTK-side handler from re-rendering for nothing.
fn emit_derived_if_changed(
    before_opened: &HashSet<WorkspaceId>,
    before_running: &HashSet<String>,
    before_fullscreen: &HashSet<WorkspaceId>,
    before_montages: &HashMap<WorkspaceId, Vec<String>>,
    views_by_id: &HashMap<i64, ViewInfo>,
    lru: &HashMap<WorkspaceId, Vec<i64>>,
    tx: &async_channel::Sender<Event>,
) {
    let opened = opened_set(views_by_id);
    if opened != *before_opened {
        let _ = tx.send_blocking(Event::OpenedWorkspacesChanged(opened));
    }
    let running = running_apps_set(views_by_id);
    if running != *before_running {
        let _ = tx.send_blocking(Event::RunningAppsChanged(running));
    }
    let fullscreen = fullscreen_set(views_by_id);
    if fullscreen != *before_fullscreen {
        let _ = tx.send_blocking(Event::FullscreenWorkspacesChanged(fullscreen));
    }
    let montages = montages_from(views_by_id, lru);
    if montages != *before_montages {
        let _ = tx.send_blocking(Event::WorkspaceAppMontagesChanged(montages));
    }
}

/// Move `id` to the head of `lru[ws]`, creating the entry if absent.
/// Idempotent — re-prepending an existing id collapses to a single front
/// position (the `retain` strips the old occurrence first).
fn lru_promote(lru: &mut HashMap<WorkspaceId, Vec<i64>>, ws: WorkspaceId, id: i64) {
    let entry = lru.entry(ws).or_default();
    entry.retain(|&x| x != id);
    entry.insert(0, id);
}

/// Drop `id` from every workspace's LRU. view-unmapped is the only caller —
/// we don't trust the cached workspace (a race between an inter-workspace
/// move and unmap could leave the old ws stale), so sweep all of them.
/// Empty per-ws lists are pruned so `montages_from` doesn't carry ghost
/// entries.
fn lru_remove_anywhere(lru: &mut HashMap<WorkspaceId, Vec<i64>>, id: i64) {
    lru.retain(|_, ids| {
        ids.retain(|&x| x != id);
        !ids.is_empty()
    });
}

fn process_event(
    name: &str,
    frame: &Value,
    state: &mut OutputState,
    views_by_id: &mut HashMap<i64, ViewInfo>,
    lru: &mut HashMap<WorkspaceId, Vec<i64>>,
    tx: &async_channel::Sender<Event>,
) {
    match name {
        "wset-workspace-changed" => {
            let Some(new) = frame.get("new-workspace") else { return };
            let workspace = WorkspaceId {
                x: new.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                y: new.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            };
            let output_id = frame.get("output").and_then(|v| v.as_i64()).unwrap_or(0);
            state.workspace = workspace;
            state.output_id = output_id;
            let _ = tx.send_blocking(Event::WorkspaceChanged {
                workspace,
                output_id,
            });
        }
        "view-mapped" => {
            let Some(view) = frame.get("view") else { return };
            let before_opened = opened_set(views_by_id);
            let before_running = running_apps_set(views_by_id);
            let before_fullscreen = fullscreen_set(views_by_id);
            let before_montages = montages_from(views_by_id, lru);
            if let Some((id, info)) = extract_view(view, state.output_size, state.workspace) {
                let app_id = info.app_id.clone();
                let ws = info.workspace;
                views_by_id.insert(id, info);
                // A newly mapped view is implicitly focused; Wayfire follows
                // with a `view-focused` shortly after, which would reaffirm
                // this position. Prepending here covers the gap.
                lru_promote(lru, ws, id);
                emit_derived_if_changed(
                    &before_opened,
                    &before_running,
                    &before_fullscreen,
                    &before_montages,
                    views_by_id,
                    lru,
                    tx,
                );
                let _ = tx.send_blocking(Event::ViewMapped { id, app_id });
            }
        }
        "view-unmapped" => {
            let Some(view) = frame.get("view") else { return };
            let Some(id) = view.get("id").and_then(|v| v.as_i64()) else { return };
            let before_opened = opened_set(views_by_id);
            let before_running = running_apps_set(views_by_id);
            let before_fullscreen = fullscreen_set(views_by_id);
            let before_montages = montages_from(views_by_id, lru);
            if views_by_id.remove(&id).is_some() {
                lru_remove_anywhere(lru, id);
                emit_derived_if_changed(
                    &before_opened,
                    &before_running,
                    &before_fullscreen,
                    &before_montages,
                    views_by_id,
                    lru,
                    tx,
                );
            }
        }
        // View toggled fullscreen / windowed. Wayfire 0.10.1 emits this as
        // `view-fullscreen` (not `-changed`); the payload carries the full
        // view record with the post-toggle `fullscreen` flag. Re-extract so
        // any workspace coords also stay consistent.
        "view-fullscreen" => {
            let Some(view) = frame.get("view") else { return };
            let before_fullscreen = fullscreen_set(views_by_id);
            if let Some((id, info)) = extract_view(view, state.output_size, state.workspace) {
                views_by_id.insert(id, info);
                let after_fullscreen = fullscreen_set(views_by_id);
                if after_fullscreen != before_fullscreen {
                    let _ =
                        tx.send_blocking(Event::FullscreenWorkspacesChanged(after_fullscreen));
                }
            }
        }
        // Focus change. Promotes the focused view to head of its workspace's
        // LRU so the montage projection picks it up. Defensive on the
        // payload shape: try `view.id` then a bare `id` field. If the id
        // isn't tracked (the focused surface isn't a toplevel we care about
        // — layer-shell client, sticky, etc.), drop silently.
        "view-focused" => {
            let id_opt = frame
                .get("view")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_i64())
                .or_else(|| frame.get("id").and_then(|v| v.as_i64()));
            let Some(id) = id_opt else { return };
            let Some(info) = views_by_id.get(&id) else { return };
            let ws = info.workspace;
            let before_montages = montages_from(views_by_id, lru);
            lru_promote(lru, ws, id);
            let after_montages = montages_from(views_by_id, lru);
            if after_montages != before_montages {
                let _ = tx.send_blocking(Event::WorkspaceAppMontagesChanged(after_montages));
            }
        }
        // Inter-workspace view move via super+shift+arrow. Wayfire emits this
        // BEFORE the accompanying `wset-workspace-changed`, so `state.workspace`
        // is still the *source* — but `view.geometry` is already in the
        // *destination* focused-relative frame. That mismatch means
        // `extract_view` would compute the source workspace. Use the event's
        // `to` field instead: B.1 verification (2026-05-13) confirmed `to`
        // carries the destination coords. The `from` field is unreliable in
        // this Wayfire build (both `from` and `to` report the destination);
        // findings live in `docs/wayfire-inter-workspace-events.md`.
        "view-workspace-changed" => {
            let Some(view) = frame.get("view") else { return };
            let Some(id) = view.get("id").and_then(|v| v.as_i64()) else { return };
            let Some(to) = frame.get("to") else { return };
            let workspace = WorkspaceId {
                x: to.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                y: to.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            };
            let app_id = view
                .get("app-id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let fullscreen = view
                .get("fullscreen")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let before_opened = opened_set(views_by_id);
            let before_running = running_apps_set(views_by_id);
            let before_fullscreen = fullscreen_set(views_by_id);
            let before_montages = montages_from(views_by_id, lru);
            // Read the source workspace BEFORE overwriting views_by_id so
            // we know which LRU to evict from. Falls back to a full sweep
            // if the id isn't tracked (subscriber raced ahead of map).
            let old_ws = views_by_id.get(&id).map(|v| v.workspace);
            views_by_id.insert(
                id,
                ViewInfo {
                    workspace,
                    app_id,
                    fullscreen,
                },
            );
            match old_ws {
                Some(old) if old != workspace => {
                    if let Some(list) = lru.get_mut(&old) {
                        list.retain(|&x| x != id);
                    }
                    lru.retain(|_, ids| !ids.is_empty());
                }
                Some(_) => {}
                None => lru_remove_anywhere(lru, id),
            }
            // A super+shift+arrow move implies focus follows the view to
            // the destination; promote to head so the destination's
            // montage prepends it.
            lru_promote(lru, workspace, id);
            emit_derived_if_changed(
                &before_opened,
                &before_running,
                &before_fullscreen,
                &before_montages,
                views_by_id,
                lru,
                tx,
            );
        }
        _ => {}
    }
}
