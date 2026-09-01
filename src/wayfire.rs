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

/// A toplevel view's geometry in **workspace-local** output coords — the
/// view's rect on its own workspace (0..output_w, 0..output_h), independent
/// of where that workspace sits relative to the focused one. Computed in
/// `extract_view` as ints; the renderer (slice 006) owns normalising these
/// to cell space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One window's montage record: identity + workspace-local rect. Plain
/// struct, no serde — it crosses only the in-process event channel.
/// Ordered lists of these (per workspace, stacking order) are the payload
/// of [`Event::WorkspaceViewsChanged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MontageView {
    pub id: i64,
    pub app_id: String,
    pub rect: ViewRect,
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
    /// Per-workspace ordered toplevel-view records backing the workspace
    /// icon montage — **one record per window, no app-id dedupe** (slice
    /// 005): two terminals on one workspace contribute two records. Empty
    /// app-ids are kept — the cell renders a fallback glyph, never
    /// nothing. Order is **stacking order, bottom → top** (topmost last),
    /// empirically pinned against `list-views` on 2026-09-01 (a freshly
    /// mapped + raised view lands at the end of the array), so slice 006
    /// can paint bottom-to-top and let the topmost window win an overlap.
    /// Workspaces with no toplevel views are absent from the map; the
    /// interim 1/2/3/4-cell partition + `+N` badge lives in the renderer.
    WorkspaceViewsChanged(HashMap<WorkspaceId, Vec<MontageView>>),
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
/// stem) and its workspace-local rect. `app_id` may be empty for views
/// that don't set one (some xwayland-bridged X clients) — those are still
/// tracked; the renderer falls back to a generic glyph.
#[derive(Debug, Clone)]
struct ViewInfo {
    workspace: WorkspaceId,
    app_id: String,
    fullscreen: bool,
    rect: ViewRect,
}

/// Read a geometry object (`{x, y, width, height}`) from a JSON value.
fn geometry_rect(geom: &Value) -> Option<ViewRect> {
    Some(ViewRect {
        x: geom.get("x")?.as_i64()? as i32,
        y: geom.get("y")?.as_i64()? as i32,
        w: geom.get("width")?.as_i64()? as i32,
        h: geom.get("height")?.as_i64()? as i32,
    })
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
    let g = geometry_rect(geom)?;
    let (ow, oh) = output_size;
    if ow <= 0 || oh <= 0 {
        return None;
    }
    let cx = g.x + g.w / 2;
    let cy = g.y + g.h / 2;
    let workspace = WorkspaceId {
        x: focused.x + cx.div_euclid(ow),
        y: focused.y + cy.div_euclid(oh),
    };
    // Wayfire reports geometry in focused-workspace-relative output coords.
    // Workspace-local coords = geom − (ws − focused)·output_size per axis.
    // Because `workspace` came from `div_euclid` over the same axis, the
    // subtraction is exact and always lands in [0, output): focused (0,0),
    // view on ws(1,0) at x=1285 → local x = 1285 − 1280 = 5; view one
    // workspace LEFT at x=−1275 → ws(−1,0) → local x = −1275 + 1280 = 5.
    let rect = ViewRect {
        x: g.x - (workspace.x - focused.x) * ow,
        y: g.y - (workspace.y - focused.y) * oh,
        w: g.w,
        h: g.h,
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
            rect,
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
    // Per-workspace stacking order of tracked view ids, bottom → top
    // (topmost LAST). Seeded from `list-views`, whose order was pinned
    // empirically on 2026-09-01: within one workspace the topmost view
    // comes later in the array — a freshly mapped + raised window landed
    // at the end. Maintained by view-mapped (append on top), view-focused
    // (move to top — Wayfire raises on focus), view-unmapped (remove
    // anywhere), and view-workspace-changed (remove from old ws, push onto
    // the new one).
    let mut stack: HashMap<WorkspaceId, Vec<i64>> = HashMap::new();
    if let Ok(views) = fetch_view_list() {
        for v in views {
            if let Some((id, info)) = extract_view(&v, state.output_size, state.workspace) {
                stack.entry(info.workspace).or_default().push(id);
                views_by_id.insert(id, info);
            }
        }
    }
    let _ = tx.send_blocking(Event::OpenedWorkspacesChanged(opened_set(&views_by_id)));
    let _ = tx.send_blocking(Event::RunningAppsChanged(running_apps_set(&views_by_id)));
    let _ = tx.send_blocking(Event::FullscreenWorkspacesChanged(fullscreen_set(
        &views_by_id,
    )));
    let _ = tx.send_blocking(Event::WorkspaceViewsChanged(views_in_order(
        &views_by_id,
        &stack,
    )));

    loop {
        let frame = read_frame(&mut stream)?;
        let Some(name) = frame.get("event").and_then(|v| v.as_str()) else {
            continue;
        };
        process_event(name, &frame, &mut state, &mut views_by_id, &mut stack, tx);
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

/// Project (views_by_id, per-workspace stack) → per-workspace ordered
/// `MontageView` lists. One record per tracked view — NO app-id dedupe
/// (slice 005): two windows of the same app contribute two records, and an
/// empty app-id still gets a record (the renderer draws a fallback glyph,
/// never a blank cell). Order is stacking order bottom → top (topmost
/// last). Untracked ids in the stack (race with view-unmapped) are
/// skipped; workspaces that end up empty are omitted from the map.
fn views_in_order(
    views_by_id: &HashMap<i64, ViewInfo>,
    stack: &HashMap<WorkspaceId, Vec<i64>>,
) -> HashMap<WorkspaceId, Vec<MontageView>> {
    let mut out: HashMap<WorkspaceId, Vec<MontageView>> = HashMap::new();
    for (ws, ids) in stack {
        let mut recs: Vec<MontageView> = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(info) = views_by_id.get(id) else { continue };
            recs.push(MontageView {
                id: *id,
                app_id: info.app_id.clone(),
                rect: info.rect,
            });
        }
        if !recs.is_empty() {
            out.insert(*ws, recs);
        }
    }
    out
}

/// Emit OpenedWorkspacesChanged / RunningAppsChanged / FullscreenWorkspacesChanged
/// / WorkspaceViewsChanged only when the derived sets actually change —
/// the whole map is recomputed cheap, and skipping no-op events keeps the
/// GTK-side handler from re-rendering for nothing.
///
/// The WorkspaceViews comparison is the honest whole-projection equality:
/// the payload now carries per-view geometry, so any rect change counts as
/// a change here. Slice 007 owns coalescing; until then the cost is one
/// small map rebuild per view event (tens of records), still negligible.
fn emit_derived_if_changed(
    before_opened: &HashSet<WorkspaceId>,
    before_running: &HashSet<String>,
    before_fullscreen: &HashSet<WorkspaceId>,
    before_views: &HashMap<WorkspaceId, Vec<MontageView>>,
    views_by_id: &HashMap<i64, ViewInfo>,
    stack: &HashMap<WorkspaceId, Vec<i64>>,
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
    let views = views_in_order(views_by_id, stack);
    if views != *before_views {
        let _ = tx.send_blocking(Event::WorkspaceViewsChanged(views));
    }
}

/// Push `id` onto the top (end) of `stack[ws]` — stacking order, bottom →
/// top. Idempotent-ish: re-pushing an id already in the list first removes
/// the old occurrence, so a list never carries duplicates.
fn stack_push_top(stack: &mut HashMap<WorkspaceId, Vec<i64>>, ws: WorkspaceId, id: i64) {
    let entry = stack.entry(ws).or_default();
    entry.retain(|&x| x != id);
    entry.push(id);
}

/// Drop `id` from every workspace's stack. view-unmapped is the only caller —
/// we don't trust the cached workspace (a race between an inter-workspace
/// move and unmap could leave the old ws stale), so sweep all of them.
/// Empty per-ws lists are pruned so `views_in_order` doesn't carry ghost
/// entries.
fn stack_remove_anywhere(stack: &mut HashMap<WorkspaceId, Vec<i64>>, id: i64) {
    stack.retain(|_, ids| {
        ids.retain(|&x| x != id);
        !ids.is_empty()
    });
}

fn process_event(
    name: &str,
    frame: &Value,
    state: &mut OutputState,
    views_by_id: &mut HashMap<i64, ViewInfo>,
    stack: &mut HashMap<WorkspaceId, Vec<i64>>,
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
            let before_views = views_in_order(views_by_id, stack);
            if let Some((id, info)) = extract_view(view, state.output_size, state.workspace) {
                let app_id = info.app_id.clone();
                let ws = info.workspace;
                views_by_id.insert(id, info);
                // A newly mapped view goes on top of its workspace's stack
                // (verified live 2026-09-01: the new window lands at the
                // end of `list-views`). Wayfire follows with a
                // `view-focused` shortly after, which reaffirms the same
                // position via `stack_push_top`'s dedupe.
                stack_push_top(stack, ws, id);
                emit_derived_if_changed(
                    &before_opened,
                    &before_running,
                    &before_fullscreen,
                    &before_views,
                    views_by_id,
                    stack,
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
            let before_views = views_in_order(views_by_id, stack);
            if views_by_id.remove(&id).is_some() {
                stack_remove_anywhere(stack, id);
                emit_derived_if_changed(
                    &before_opened,
                    &before_running,
                    &before_fullscreen,
                    &before_views,
                    views_by_id,
                    stack,
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
        // Focus change. Wayfire raises a view when it gains focus (the
        // default focus policy), so a focused view is — or just became — the
        // top of its workspace's stack: move it to the end of the list so
        // the montage order tracks stacking. Defensive on the payload
        // shape: try `view.id` then a bare `id` field. If the id isn't
        // tracked (the focused surface isn't a toplevel we care about —
        // layer-shell client, sticky, etc.), drop silently.
        "view-focused" => {
            let id_opt = frame
                .get("view")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_i64())
                .or_else(|| frame.get("id").and_then(|v| v.as_i64()));
            let Some(id) = id_opt else { return };
            let Some(info) = views_by_id.get(&id) else { return };
            let ws = info.workspace;
            let before_views = views_in_order(views_by_id, stack);
            stack_push_top(stack, ws, id);
            let after_views = views_in_order(views_by_id, stack);
            if after_views != before_views {
                let _ = tx.send_blocking(Event::WorkspaceViewsChanged(after_views));
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
            // The event's geometry is destination-relative (frame note
            // above) and the destination IS the view's workspace, so the
            // workspace-local rect is the reported geometry unchanged:
            // local = geom − (ws − focused)·size with ws == focused == `to`.
            let rect = view
                .get("geometry")
                .and_then(geometry_rect)
                .unwrap_or(ViewRect { x: 0, y: 0, w: 0, h: 0 });
            let before_opened = opened_set(views_by_id);
            let before_running = running_apps_set(views_by_id);
            let before_fullscreen = fullscreen_set(views_by_id);
            let before_views = views_in_order(views_by_id, stack);
            // Read the source workspace BEFORE overwriting views_by_id so
            // we know which stack to evict from. Falls back to a full sweep
            // if the id isn't tracked (subscriber raced ahead of map).
            let old_ws = views_by_id.get(&id).map(|v| v.workspace);
            views_by_id.insert(
                id,
                ViewInfo {
                    workspace,
                    app_id,
                    fullscreen,
                    rect,
                },
            );
            match old_ws {
                Some(old) if old != workspace => {
                    if let Some(list) = stack.get_mut(&old) {
                        list.retain(|&x| x != id);
                    }
                    stack.retain(|_, ids| !ids.is_empty());
                }
                Some(_) => {}
                None => stack_remove_anywhere(stack, id),
            }
            // A super+shift+arrow move implies focus follows the view to
            // the destination; push onto the top of the destination's stack.
            stack_push_top(stack, workspace, id);
            emit_derived_if_changed(
                &before_opened,
                &before_running,
                &before_fullscreen,
                &before_views,
                views_by_id,
                stack,
                tx,
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Screen 1280×853, grid 2×2, focused (0,0) — the live setup the
    /// values were recorded against on 2026-09-01.
    const OUT: (i32, i32) = (1280, 853);

    fn view(id: i64, app: &str, x: i32, y: i32, w: i32, h: i32) -> Value {
        json!({
            "id": id,
            "layer": "workspace",
            "mapped": true,
            "sticky": false,
            "app-id": app,
            "fullscreen": false,
            "geometry": { "x": x, "y": y, "width": w, "height": h },
        })
    }

    fn info(ws: WorkspaceId, app: &str, rect: ViewRect) -> ViewInfo {
        ViewInfo {
            workspace: ws,
            app_id: app.to_owned(),
            fullscreen: false,
            rect,
        }
    }

    // ---- slice 005: workspace assignment + workspace-local rect --------

    #[test]
    fn extract_view_live_focused_workspace_geometry_is_local() {
        // Live: id=4 foot (5,5,629,843) and id=8 foot (644,5,631,843) sit
        // on the focused ws(0,0) — their geometry IS their local rect.
        let (_, i4) = extract_view(&view(4, "foot", 5, 5, 629, 843), OUT, WorkspaceId::ZERO)
            .expect("id=4 tracked");
        assert_eq!(i4.workspace, WorkspaceId { x: 0, y: 0 });
        assert_eq!(i4.rect, ViewRect { x: 5, y: 5, w: 629, h: 843 });

        let (_, i8) = extract_view(&view(8, "foot", 644, 5, 631, 843), OUT, WorkspaceId::ZERO)
            .expect("id=8 tracked");
        assert_eq!(i8.workspace, WorkspaceId { x: 0, y: 0 });
        assert_eq!(i8.rect, ViewRect { x: 644, y: 5, w: 631, h: 843 });
    }

    #[test]
    fn extract_view_live_neighbour_workspaces_become_local() {
        // id=27 foot (1285,5,1270,843) → ws(1,0), local (5,5,1270,843).
        let (_, i27) =
            extract_view(&view(27, "foot", 1285, 5, 1270, 843), OUT, WorkspaceId::ZERO)
                .expect("id=27 tracked");
        assert_eq!(i27.workspace, WorkspaceId { x: 1, y: 0 });
        assert_eq!(i27.rect, ViewRect { x: 5, y: 5, w: 1270, h: 843 });

        // id=10 firefox (5,858,1270,843) → ws(0,1), local (5,5,1270,843).
        let (_, i10) =
            extract_view(&view(10, "firefox", 5, 858, 1270, 843), OUT, WorkspaceId::ZERO)
                .expect("id=10 tracked");
        assert_eq!(i10.workspace, WorkspaceId { x: 0, y: 1 });
        assert_eq!(i10.rect, ViewRect { x: 5, y: 5, w: 1270, h: 843 });

        // id=6 foot (1285,858,1270,843) → ws(1,1).
        let (_, i6) =
            extract_view(&view(6, "foot", 1285, 858, 1270, 843), OUT, WorkspaceId::ZERO)
                .expect("id=6 tracked");
        assert_eq!(i6.workspace, WorkspaceId { x: 1, y: 1 });
        assert_eq!(i6.rect, ViewRect { x: 5, y: 5, w: 1270, h: 843 });
    }

    #[test]
    fn extract_view_negative_offsets_wrap_within_the_output() {
        // One workspace LEFT of focused: x=−1275 → ws(−1,0) via div_euclid
        // (not truncation), local x = −1275 − (−1)·1280 = 5.
        let (_, i) = extract_view(&view(9, "foot", -1275, 5, 1280, 843), OUT, WorkspaceId::ZERO)
            .expect("left view tracked");
        assert_eq!(i.workspace, WorkspaceId { x: -1, y: 0 });
        assert_eq!(i.rect, ViewRect { x: 5, y: 5, w: 1280, h: 843 });
    }

    #[test]
    fn extract_view_two_workspaces_away_localizes_against_focused() {
        // Focused (2,0), view on ws(0,0) at x=−2560: local = −2560 − (0−2)·1280 = 0.
        let focused = WorkspaceId { x: 2, y: 0 };
        let (_, i) = extract_view(&view(11, "foot", -2560, 0, 1280, 853), OUT, focused)
            .expect("two-left view tracked");
        assert_eq!(i.workspace, WorkspaceId { x: 0, y: 0 });
        assert_eq!(i.rect, ViewRect { x: 0, y: 0, w: 1280, h: 853 });
    }

    #[test]
    fn extract_view_skips_non_workspace_layers_and_unmapped() {
        let layer_shell = json!({
            "id": 51, "layer": "top", "mapped": true, "sticky": false,
            "geometry": { "x": 0, "y": 107, "width": 1280, "height": 746 },
        });
        assert!(extract_view(&layer_shell, OUT, WorkspaceId::ZERO).is_none());
        let unmapped = json!({
            "id": 52, "layer": "workspace", "mapped": false, "sticky": false,
            "geometry": { "x": 0, "y": 0, "width": 10, "height": 10 },
        });
        assert!(extract_view(&unmapped, OUT, WorkspaceId::ZERO).is_none());
    }

    // ---- slice 005: one record per window, stacking order --------------

    #[test]
    fn views_in_order_keeps_duplicates_and_empty_app_ids() {
        let ws = WorkspaceId::ZERO;
        let mut by_id = HashMap::new();
        by_id.insert(
            4,
            info(ws, "foot", ViewRect { x: 5, y: 5, w: 629, h: 843 }),
        );
        by_id.insert(
            8,
            info(ws, "foot", ViewRect { x: 644, y: 5, w: 631, h: 843 }),
        );
        by_id.insert(
            9,
            info(ws, "", ViewRect { x: 0, y: 0, w: 1280, h: 843 }),
        );
        // Stacking bottom → top: 4 (bottom), 9, 8 (topmost).
        let stack = HashMap::from([(ws, vec![4, 9, 8])]);
        let out = views_in_order(&by_id, &stack);
        let recs = &out[&ws];
        assert_eq!(recs.len(), 3, "no app-id dedupe, no empty-app-id drop");
        assert_eq!(recs[0].id, 4);
        assert_eq!(recs[1].id, 9);
        assert_eq!(recs[2].id, 8);
        assert_eq!(recs[1].app_id, "", "empty app-id kept, keyed by view id");
        assert_eq!(recs[2].rect, ViewRect { x: 644, y: 5, w: 631, h: 843 });
    }

    #[test]
    fn views_in_order_skips_untracked_ids_and_omits_empty_workspaces() {
        let ws = WorkspaceId::ZERO;
        let mut by_id = HashMap::new();
        by_id.insert(
            4,
            info(ws, "foot", ViewRect { x: 0, y: 0, w: 10, h: 10 }),
        );
        // 99 raced ahead of view-mapped; the only tracked id is 4.
        let stack = HashMap::from([(ws, vec![99, 4])]);
        let out = views_in_order(&by_id, &stack);
        assert_eq!(out[&ws].len(), 1);
        assert_eq!(out[&ws][0].id, 4);

        // A workspace whose ids all resolve to nothing is omitted entirely.
        let stack = HashMap::from([
            (ws, vec![98]),
            (WorkspaceId { x: 1, y: 0 }, vec![97]),
        ]);
        assert!(views_in_order(&by_id, &stack).is_empty());
    }

    #[test]
    fn stack_push_top_dedupes_and_moves_to_end() {
        let ws = WorkspaceId::ZERO;
        let mut stack = HashMap::from([(ws, vec![4, 9, 8])]);
        // Re-push an existing id: moves to the top (end), no duplicate.
        stack_push_top(&mut stack, ws, 9);
        assert_eq!(stack[&ws], vec![4, 8, 9]);
        // Fresh id: appended.
        stack_push_top(&mut stack, ws, 12);
        assert_eq!(stack[&ws], vec![4, 8, 9, 12]);
    }

    #[test]
    fn stack_remove_anywhere_sweeps_all_workspaces() {
        let ws_a = WorkspaceId { x: 0, y: 0 };
        let ws_b = WorkspaceId { x: 1, y: 0 };
        let mut stack = HashMap::from([(ws_a, vec![4, 9]), (ws_b, vec![8])]);
        stack_remove_anywhere(&mut stack, 9);
        assert_eq!(stack[&ws_a], vec![4]);
        assert_eq!(stack[&ws_b], vec![8]);
        // Removing the last id of a workspace prunes the entry.
        stack_remove_anywhere(&mut stack, 8);
        assert!(!stack.contains_key(&ws_b));
    }
}
