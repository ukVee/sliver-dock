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
use std::time::{Duration, Instant};

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

/// Icon-space quantisation resolution (slice 007). The workspace montage
/// is drawn inside a `panel::ICON_SIZE` (41 px) square, and its letterboxed
/// content box is at most that in either axis — so a geometry change
/// smaller than 1/41 of the screen (~31 px at 1280 wide) can never move a
/// cell edge by a whole icon pixel. Quantising the projection to this grid
/// *before* the change comparison is what keeps an interactive drag-resize
/// (hundreds of events, a few px apart) down to a handful of redraws.
/// Pinned against `panel::ICON_SIZE` by a test there.
pub const MONTAGE_QUANT_STEPS: i32 = 41;

/// One montage record reduced to what the icon can actually draw: identity
/// plus the cell's edges in icon-space steps. Comparing these instead of
/// raw pixels is the change gate for [`Event::WorkspaceViewsChanged`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuantView {
    id: i64,
    app_id: String,
    /// (left, top, right, bottom) in `MONTAGE_QUANT_STEPS` units.
    cell: (i32, i32, i32, i32),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Read one frame, but give up if `wait` expires **before any byte of it
/// arrives**. `Ok(None)` means the wait ran out at a clean frame boundary —
/// the caller uses that tick to flush a deferred geometry burst
/// ([`GeometryBuffer`]) and then comes back to read.
///
/// The first byte commits us: a half-read length prefix or body cannot be
/// abandoned without desyncing the stream, so the socket goes back to
/// blocking the moment anything is read. The timeout is always cleared
/// again before returning, so `read_frame`'s blocking contract still holds
/// for every other caller of this stream.
fn read_frame_timeout(stream: &mut UnixStream, wait: Duration) -> std::io::Result<Option<Value>> {
    // A zero timeout means "block forever" to the kernel — never pass one.
    stream.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
    let mut len_buf = [0u8; 4];
    let mut got = 0usize;
    while got < 4 {
        match stream.read(&mut len_buf[got..]) {
            Ok(0) => {
                stream.set_read_timeout(None)?;
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            Ok(n) => {
                got += n;
                stream.set_read_timeout(None)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if got == 0 && is_timeout(&e) => {
                stream.set_read_timeout(None)?;
                return Ok(None);
            }
            Err(e) => {
                stream.set_read_timeout(None)?;
                return Err(e);
            }
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        .map(Some)
}

/// A read timeout surfaces as `WouldBlock` (EAGAIN) on Unix; `TimedOut` is
/// accepted too so the helper isn't platform-fragile.
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
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
    let state = match fetch_output_state() {
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

    // Everything the event loop mutates lives in one `Tracker`. Its view
    // map + stacking order are seeded from `list-views`; the stacking order
    // of that array was pinned empirically on 2026-09-01 (within one
    // workspace the topmost view comes later — a freshly mapped + raised
    // window landed at the end).
    let (views_by_id, stack) = seed_from_list_views(
        fetch_view_list().unwrap_or_default(),
        state.output_size,
        state.workspace,
    );
    let mut t = Tracker {
        state,
        views_by_id,
        stack,
        geom: GeometryBuffer::default(),
        sent_views: HashMap::new(),
        needs_resync: false,
    };

    let _ = tx.send_blocking(Event::OpenedWorkspacesChanged(opened_set(&t.views_by_id)));
    let _ = tx.send_blocking(Event::RunningAppsChanged(running_apps_set(&t.views_by_id)));
    let _ = tx.send_blocking(Event::FullscreenWorkspacesChanged(fullscreen_set(
        &t.views_by_id,
    )));
    let views = views_in_order(&t.views_by_id, &t.stack);
    t.sent_views = quantise_views(&views, t.state.output_size);
    let _ = tx.send_blocking(Event::WorkspaceViewsChanged(views));

    loop {
        // Flush first: a burst that went quiet (or hit the defer cap) is
        // applied before we go back to waiting for the next frame.
        if t.geom.flush_due(Instant::now()) {
            flush_geometry(&mut t, tx);
        }
        // An idle desktop must cost this thread zero wakeups (milestone
        // criterion 8), so the read blocks indefinitely unless a geometry
        // burst is actually pending — only then does it get a deadline.
        let frame = match t.geom.wait_before_flush(Instant::now()) {
            Some(wait) => read_frame_timeout(&mut stream, wait)?,
            None => Some(read_frame(&mut stream)?),
        };
        if let Some(frame) = frame
            && let Some(name) = frame.get("event").and_then(|v| v.as_str())
        {
            process_event(name, &frame, &mut t, tx);
        }
        // The `list-views` round-trip a workspace switch asks for lives
        // here, not in `process_event` — keeping IPC out of the event
        // handler is what makes the frame-race logic unit-testable. It is
        // one short one-shot call on its own connection, bounded by a
        // switch (a human keypress), so the pause in reading this stream
        // is sub-millisecond and can't back the event socket up.
        if t.take_needs_resync() {
            match fetch_view_list() {
                Ok(views) => t.apply_list_views(views, tx),
                Err(e) => eprintln!("sliver-dock: workspace-switch resync: {e}"),
            }
        }
    }
}

/// Build the view map + per-workspace stacking order from a `list-views`
/// response. Array order is stacking order within a workspace, bottom →
/// top. Used for the initial seed and for the post-workspace-switch
/// reconcile, so both agree by construction.
fn seed_from_list_views(
    views: Vec<Value>,
    output_size: (i32, i32),
    focused: WorkspaceId,
) -> (HashMap<i64, ViewInfo>, HashMap<WorkspaceId, Vec<i64>>) {
    let mut views_by_id: HashMap<i64, ViewInfo> = HashMap::new();
    let mut stack: HashMap<WorkspaceId, Vec<i64>> = HashMap::new();
    for v in views {
        if let Some((id, info)) = extract_view(&v, output_size, focused) {
            stack.entry(info.workspace).or_default().push(id);
            views_by_id.insert(id, info);
        }
    }
    (views_by_id, stack)
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

/// Quantise one workspace-local rect to icon-space cell edges (left, top,
/// right, bottom) — the cell as the montage can actually draw it. An
/// `output_size` of zero (the pre-`Connected` race) disables quantisation
/// and the raw edges are compared instead, which is only ever *more*
/// sensitive, never less.
fn quantise_rect(r: ViewRect, output_size: (i32, i32)) -> (i32, i32, i32, i32) {
    let (ow, oh) = output_size;
    let q = |v: i32, dim: i32| -> i32 {
        if dim > 0 {
            ((v as i64 * MONTAGE_QUANT_STEPS as i64).div_euclid(dim as i64)) as i32
        } else {
            v
        }
    };
    (
        q(r.x, ow),
        q(r.y, oh),
        q(r.x.saturating_add(r.w), ow),
        q(r.y.saturating_add(r.h), oh),
    )
}

/// Reduce a whole projection to its icon-space form. Identity (id, app-id)
/// is compared exactly; only geometry is quantised.
fn quantise_views(
    views: &HashMap<WorkspaceId, Vec<MontageView>>,
    output_size: (i32, i32),
) -> HashMap<WorkspaceId, Vec<QuantView>> {
    views
        .iter()
        .map(|(ws, recs)| {
            let q = recs
                .iter()
                .map(|mv| QuantView {
                    id: mv.id,
                    app_id: mv.app_id.clone(),
                    cell: quantise_rect(mv.rect, output_size),
                })
                .collect();
            (*ws, q)
        })
        .collect()
}

/// Idle gap that ends a geometry burst. Long enough that a retile's N
/// back-to-back events coalesce into one update, short enough to be
/// invisible in a 41 px miniature.
const GEOMETRY_DEBOUNCE: Duration = Duration::from_millis(60);

/// Hard cap on deferral. A continuous interactive resize never opens the
/// idle gap, so without this the miniature would freeze until the drag
/// ended; with it, it updates ~4×/s while the drag runs.
const GEOMETRY_MAX_DEFER: Duration = Duration::from_millis(250);

/// Deferred `view-geometry-changed` payloads (slice 007).
///
/// Geometry events are never applied where they arrive. Two reasons, both
/// from the 2026-05-13 capture in
/// `docs/wayfire-inter-workspace-events.md`:
///
/// 1. **The frame race.** A plain workspace switch fires one geometry event
///    per existing view, already expressed in the NEW focused-relative
///    frame, *before* `wset-workspace-changed` moves our idea of the
///    focused workspace. Applying those in place recomputes a wrong
///    workspace for every view on the system. Buffering lets the switch
///    invalidate them retroactively (`take_dirty` → one `list-views`
///    reconcile). That is option (a), defer-and-reconcile, of the slice:
///    chosen over the frame-independent update because the workspace-local
///    rect is frame-invariant but the *workspace assignment* is not, and
///    only the reconcile keeps that honest — at one round-trip per switch
///    instead of N misattributed updates.
/// 2. **Bursts.** A retile emits N events back-to-back; one flush per burst
///    is one channel send and one redraw instead of N.
#[derive(Default)]
struct GeometryBuffer {
    /// Latest payload per view id — within a burst the last one wins.
    pending: HashMap<i64, Value>,
    /// Arrival of the burst's first event (bounds the deferral).
    first_at: Option<Instant>,
    /// Arrival of its most recent event (opens the idle gap).
    last_at: Option<Instant>,
    /// Any geometry buffered since the last `wset-workspace-changed`. Arms
    /// the reconcile even when the burst already drained through the
    /// max-defer cap, so a cap-flush that landed mid-switch is still
    /// repaired by the switch that follows it.
    dirty: bool,
}

impl GeometryBuffer {
    fn buffer(&mut self, id: i64, view: Value, now: Instant) {
        self.pending.insert(id, view);
        self.first_at.get_or_insert(now);
        self.last_at = Some(now);
        self.dirty = true;
    }

    /// Forget any pending payload for `id` — used by the paths that carry
    /// fresher truth for the same view (unmap, fullscreen toggle,
    /// inter-workspace move) so a late flush can't overwrite them.
    fn forget(&mut self, id: i64) {
        self.pending.remove(&id);
        if self.pending.is_empty() {
            self.first_at = None;
            self.last_at = None;
        }
    }

    /// Mark geometry as in flight without buffering anything — for a path
    /// that applied a possibly frame-stale geometry itself and wants the
    /// next workspace switch to reconcile.
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// How long a read may block before the pending burst must be flushed.
    /// `None` = nothing pending, block indefinitely.
    fn wait_before_flush(&self, now: Instant) -> Option<Duration> {
        let first = self.first_at?;
        let last = self.last_at?;
        let idle = (last + GEOMETRY_DEBOUNCE).saturating_duration_since(now);
        let cap = (first + GEOMETRY_MAX_DEFER).saturating_duration_since(now);
        Some(idle.min(cap).max(Duration::from_millis(1)))
    }

    fn flush_due(&self, now: Instant) -> bool {
        match (self.first_at, self.last_at) {
            (Some(first), Some(last)) => {
                now >= last + GEOMETRY_DEBOUNCE || now >= first + GEOMETRY_MAX_DEFER
            }
            _ => false,
        }
    }

    /// Drain the burst for application.
    fn take(&mut self) -> HashMap<i64, Value> {
        self.first_at = None;
        self.last_at = None;
        std::mem::take(&mut self.pending)
    }

    /// Drop whatever is buffered and report whether any geometry has been
    /// in flight since the last workspace switch — the caller reconciles
    /// when it has.
    fn take_dirty(&mut self) -> bool {
        self.take();
        std::mem::replace(&mut self.dirty, false)
    }
}

/// The subscriber's mutable world: output state, tracked views, the
/// per-workspace stacking order, the deferred-geometry buffer, and the
/// last projection actually sent.
struct Tracker {
    state: OutputState,
    views_by_id: HashMap<i64, ViewInfo>,
    stack: HashMap<WorkspaceId, Vec<i64>>,
    geom: GeometryBuffer,
    /// Quantised form of the last `WorkspaceViewsChanged` payload sent.
    /// The gate compares against what the renderer actually got, not
    /// against a per-event snapshot — which is what makes it correct
    /// across a debounced burst, where every intermediate state between
    /// the first buffered event and the flush was never drawn at all.
    sent_views: HashMap<WorkspaceId, Vec<QuantView>>,
    /// Set by a workspace switch that invalidated buffered geometry; the
    /// event loop owns the `list-views` round-trip it asks for.
    needs_resync: bool,
}

impl Tracker {
    fn take_needs_resync(&mut self) -> bool {
        std::mem::replace(&mut self.needs_resync, false)
    }

    /// Rebuild the view map + stacking order from a `list-views` response
    /// against the current focused workspace. The post-switch reconcile:
    /// authoritative for both geometry and workspace assignment, and a
    /// no-op emission-wise when (as on a plain switch) nothing actually
    /// moved.
    fn apply_list_views(&mut self, views: Vec<Value>, tx: &async_channel::Sender<Event>) {
        let before_opened = opened_set(&self.views_by_id);
        let before_running = running_apps_set(&self.views_by_id);
        let before_fullscreen = fullscreen_set(&self.views_by_id);
        let (views_by_id, stack) =
            seed_from_list_views(views, self.state.output_size, self.state.workspace);
        self.views_by_id = views_by_id;
        self.stack = stack;
        self.emit_derived_if_changed(&before_opened, &before_running, &before_fullscreen, tx);
    }

    /// Emit `WorkspaceViewsChanged` only when the projection changes *as
    /// drawn* — the comparison runs in icon space (see
    /// [`MONTAGE_QUANT_STEPS`]), so sub-icon-pixel jitter never reaches the
    /// GTK side, while the payload itself still carries exact rects.
    fn emit_views_if_changed(&mut self, tx: &async_channel::Sender<Event>) {
        let views = views_in_order(&self.views_by_id, &self.stack);
        let quantised = quantise_views(&views, self.state.output_size);
        if quantised != self.sent_views {
            self.sent_views = quantised;
            let _ = tx.send_blocking(Event::WorkspaceViewsChanged(views));
        }
    }

    /// Emit OpenedWorkspacesChanged / RunningAppsChanged /
    /// FullscreenWorkspacesChanged / WorkspaceViewsChanged only when the
    /// derived sets actually change — each is recomputed cheap, and
    /// skipping no-op events keeps the GTK-side handler from re-rendering
    /// for nothing.
    fn emit_derived_if_changed(
        &mut self,
        before_opened: &HashSet<WorkspaceId>,
        before_running: &HashSet<String>,
        before_fullscreen: &HashSet<WorkspaceId>,
        tx: &async_channel::Sender<Event>,
    ) {
        let opened = opened_set(&self.views_by_id);
        if opened != *before_opened {
            let _ = tx.send_blocking(Event::OpenedWorkspacesChanged(opened));
        }
        let running = running_apps_set(&self.views_by_id);
        if running != *before_running {
            let _ = tx.send_blocking(Event::RunningAppsChanged(running));
        }
        let fullscreen = fullscreen_set(&self.views_by_id);
        if fullscreen != *before_fullscreen {
            let _ = tx.send_blocking(Event::FullscreenWorkspacesChanged(fullscreen));
        }
        self.emit_views_if_changed(tx);
    }
}

/// Apply a drained geometry burst. Runs only when no workspace switch is in
/// flight (a switch drops the buffer instead), so `state.workspace` is
/// trustworthy here and `extract_view` may be used in full — which is what
/// keeps a view dragged across a workspace boundary (no
/// `view-workspace-changed` for a plain move) correctly re-homed.
fn flush_geometry(t: &mut Tracker, tx: &async_channel::Sender<Event>) {
    let pending = t.geom.take();
    if pending.is_empty() {
        return;
    }
    let before_opened = opened_set(&t.views_by_id);
    let before_running = running_apps_set(&t.views_by_id);
    let before_fullscreen = fullscreen_set(&t.views_by_id);
    for (id, view) in pending {
        // Creation is view-mapped's job and removal is view-unmapped's, so
        // a view that left the map while its geometry sat buffered must
        // never be resurrected here.
        if !t.views_by_id.contains_key(&id) {
            continue;
        }
        let Some((_, info)) = extract_view(&view, t.state.output_size, t.state.workspace) else {
            // Now unmapped / sticky / off-layer: leave removal to the
            // event that owns it.
            continue;
        };
        let workspace = info.workspace;
        let old_ws = t.views_by_id.insert(id, info).map(|v| v.workspace);
        stack_rehome(&mut t.stack, old_ws, workspace, id);
    }
    t.emit_derived_if_changed(&before_opened, &before_running, &before_fullscreen, tx);
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

/// Re-home `id` from `old` to `new` in the stacking map. For the paths
/// that can discover a workspace change without a `view-workspace-changed`
/// event — the geometry flush (a floating window dragged across the
/// boundary) and the fullscreen re-extract. A view that didn't change
/// workspace keeps its stacking position: a resize must not raise a window.
fn stack_rehome(
    stack: &mut HashMap<WorkspaceId, Vec<i64>>,
    old: Option<WorkspaceId>,
    new: WorkspaceId,
    id: i64,
) {
    if old == Some(new) {
        return;
    }
    if let Some(old) = old
        && let Some(list) = stack.get_mut(&old)
    {
        list.retain(|&x| x != id);
    }
    stack.retain(|_, ids| !ids.is_empty());
    stack_push_top(stack, new, id);
}

fn process_event(
    name: &str,
    frame: &Value,
    t: &mut Tracker,
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
            t.state.workspace = workspace;
            t.state.output_id = output_id;
            let _ = tx.send_blocking(Event::WorkspaceChanged {
                workspace,
                output_id,
            });
            // The frame race (docs/wayfire-inter-workspace-events.md §1):
            // every geometry event that preceded this one was already in
            // the NEW focused-relative frame while `state.workspace` still
            // held the old one, so none of it can be attributed to a
            // workspace. Drop the burst and re-derive the whole map from
            // one `list-views` against the now-correct focus — the loop
            // owns the round-trip.
            if t.geom.take_dirty() {
                t.needs_resync = true;
            }
        }
        "view-mapped" => {
            let Some(view) = frame.get("view") else { return };
            let before_opened = opened_set(&t.views_by_id);
            let before_running = running_apps_set(&t.views_by_id);
            let before_fullscreen = fullscreen_set(&t.views_by_id);
            if let Some((id, info)) = extract_view(view, t.state.output_size, t.state.workspace) {
                let app_id = info.app_id.clone();
                let ws = info.workspace;
                t.views_by_id.insert(id, info);
                // A newly mapped view goes on top of its workspace's stack
                // (verified live 2026-09-01: the new window lands at the
                // end of `list-views`). Wayfire follows with a
                // `view-focused` shortly after, which reaffirms the same
                // position via `stack_push_top`'s dedupe.
                stack_push_top(&mut t.stack, ws, id);
                t.emit_derived_if_changed(&before_opened, &before_running, &before_fullscreen, tx);
                let _ = tx.send_blocking(Event::ViewMapped { id, app_id });
            }
        }
        "view-unmapped" => {
            let Some(view) = frame.get("view") else { return };
            let Some(id) = view.get("id").and_then(|v| v.as_i64()) else { return };
            let before_opened = opened_set(&t.views_by_id);
            let before_running = running_apps_set(&t.views_by_id);
            let before_fullscreen = fullscreen_set(&t.views_by_id);
            // Anything buffered for a view that just died would otherwise
            // be applied by the next flush; `flush_geometry` refuses to
            // resurrect it, but dropping it here keeps the burst honest.
            t.geom.forget(id);
            if t.views_by_id.remove(&id).is_some() {
                stack_remove_anywhere(&mut t.stack, id);
                t.emit_derived_if_changed(&before_opened, &before_running, &before_fullscreen, tx);
            }
        }
        // View toggled fullscreen / windowed. Wayfire 0.10.1 emits this as
        // `view-fullscreen` (not `-changed`); the payload carries the full
        // view record with the post-toggle `fullscreen` flag. Re-extract so
        // any workspace coords also stay consistent.
        "view-fullscreen" => {
            let Some(view) = frame.get("view") else { return };
            let before_fullscreen = fullscreen_set(&t.views_by_id);
            if let Some((id, info)) = extract_view(view, t.state.output_size, t.state.workspace) {
                // This payload carries post-toggle geometry, so it
                // supersedes anything buffered for the same view — drop
                // that rather than let a late flush overwrite the
                // fullscreen rect (slice 007: the fullscreen path and the
                // geometry path must not fight over `views_by_id`).
                t.geom.forget(id);
                // Wayfire un-fullscreens a view as part of a
                // `super+shift+arrow` move (capture case 3a), which puts
                // this event inside the same frame race as the geometry
                // burst: arm the reconcile so the switch that follows
                // re-derives the workspace authoritatively.
                t.geom.mark_dirty();
                let workspace = info.workspace;
                let old_ws = t.views_by_id.insert(id, info).map(|v| v.workspace);
                stack_rehome(&mut t.stack, old_ws, workspace, id);
                let after_fullscreen = fullscreen_set(&t.views_by_id);
                if after_fullscreen != before_fullscreen {
                    let _ =
                        tx.send_blocking(Event::FullscreenWorkspacesChanged(after_fullscreen));
                }
                // Fullscreen resizes the view, so the miniature changes
                // with it — before slice 007 this path left the montage
                // stale until some other event happened to redraw it.
                t.emit_views_if_changed(tx);
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
            let Some(info) = t.views_by_id.get(&id) else { return };
            let ws = info.workspace;
            stack_push_top(&mut t.stack, ws, id);
            t.emit_views_if_changed(tx);
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
            let before_opened = opened_set(&t.views_by_id);
            let before_running = running_apps_set(&t.views_by_id);
            let before_fullscreen = fullscreen_set(&t.views_by_id);
            // `to` is authoritative for this view, so it supersedes any
            // geometry buffered for it during the move.
            t.geom.forget(id);
            // Read the source workspace BEFORE overwriting views_by_id so
            // we know which stack to evict from. Falls back to a full sweep
            // if the id isn't tracked (subscriber raced ahead of map).
            let old_ws = t.views_by_id.get(&id).map(|v| v.workspace);
            t.views_by_id.insert(
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
                    if let Some(list) = t.stack.get_mut(&old) {
                        list.retain(|&x| x != id);
                    }
                    t.stack.retain(|_, ids| !ids.is_empty());
                }
                Some(_) => {}
                None => stack_remove_anywhere(&mut t.stack, id),
            }
            // A super+shift+arrow move implies focus follows the view to
            // the destination; push onto the top of the destination's stack.
            stack_push_top(&mut t.stack, workspace, id);
            t.emit_derived_if_changed(&before_opened, &before_running, &before_fullscreen, tx);
        }
        // A view's geometry changed: a retile, a resize, a move — or (case
        // 1 of the capture) the pure frame shift of a workspace switch,
        // which is indistinguishable from the payload alone. Nothing is
        // applied here: the event is buffered, disambiguated by what
        // follows it, and flushed once the burst goes quiet. See
        // [`GeometryBuffer`].
        "view-geometry-changed" => {
            let Some(view) = frame.get("view") else { return };
            let Some(id) = view.get("id").and_then(|v| v.as_i64()) else { return };
            // Creation is view-mapped's job and removal is
            // view-unmapped's, so geometry for anything we don't already
            // track (layer-shell surfaces, sticky views, whatever
            // `extract_view` filters) is noise.
            if !t.views_by_id.contains_key(&id) {
                return;
            }
            t.geom.buffer(id, view.clone(), Instant::now());
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

    // ---- slice 007: live geometry (frame race, coalescing, quantise) ---

    const WS0: WorkspaceId = WorkspaceId { x: 0, y: 0 };
    const WS1: WorkspaceId = WorkspaceId { x: 1, y: 0 };

    /// A tracker seeded from `views`, focused on (0,0), with the sent-state
    /// primed exactly as `subscribe_once` primes it after the initial
    /// emissions.
    fn tracker(views: &[Value]) -> Tracker {
        let (views_by_id, stack) = seed_from_list_views(views.to_vec(), OUT, WS0);
        let mut t = Tracker {
            state: OutputState {
                workspace: WS0,
                grid: GridSize { width: 2, height: 2 },
                output_id: 1,
                output_size: OUT,
            },
            views_by_id,
            stack,
            geom: GeometryBuffer::default(),
            sent_views: HashMap::new(),
            needs_resync: false,
        };
        t.sent_views = quantise_views(&views_in_order(&t.views_by_id, &t.stack), OUT);
        t
    }

    fn geometry_event(v: &Value) -> Value {
        json!({ "event": "view-geometry-changed", "view": v })
    }

    /// Count (and drain) the `WorkspaceViewsChanged` events on the channel.
    fn views_emitted(rx: &async_channel::Receiver<Event>) -> usize {
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, Event::WorkspaceViewsChanged(_)) {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn quantise_rect_buckets_screen_px_into_icon_px() {
        // 1280 wide over 41 icon px → one icon px ≈ 31.2 screen px.
        let step = 1280 / MONTAGE_QUANT_STEPS; // 31
        let a = quantise_rect(ViewRect { x: 0, y: 0, w: 640, h: 853 }, OUT);
        assert_eq!(a, (0, 0, 20, 41));
        // A move smaller than one icon px lands in the same bucket…
        let jitter = quantise_rect(
            ViewRect { x: step - 2, y: 0, w: 640, h: 853 },
            OUT,
        );
        assert_eq!(jitter.0, a.0);
        // …and one that clears it does not.
        let moved = quantise_rect(ViewRect { x: step + 1, y: 0, w: 640, h: 853 }, OUT);
        assert_eq!(moved.0, 1);
        // Unknown output size (pre-Connected race): raw edges, never a
        // divide-by-zero.
        assert_eq!(
            quantise_rect(ViewRect { x: 5, y: 7, w: 10, h: 20 }, (0, 0)),
            (5, 7, 15, 27)
        );
    }

    /// The trap this slice exists to dodge, in executable form: during a
    /// workspace switch the geometry payload is already in the NEW frame
    /// while `state.workspace` still holds the OLD one, so extracting it in
    /// place invents a workspace that does not exist. Same payload, same
    /// view, two different answers depending only on the focus in effect.
    #[test]
    fn geometry_in_a_stale_frame_would_misattribute_the_workspace() {
        // A view really sitting on (0,0) at local x=5, reported after focus
        // moved to (1,0): 5 + (0 − 1)·1280 = −1275.
        let v = view(4, "foot", -1275, 5, 629, 843);
        let (_, stale) = extract_view(&v, OUT, WS0).unwrap();
        assert_eq!(stale.workspace, WorkspaceId { x: -1, y: 0 }, "the trap");
        let (_, correct) = extract_view(&v, OUT, WS1).unwrap();
        assert_eq!(correct.workspace, WS0);
        // The workspace-local rect is frame-invariant either way — that is
        // why only the *assignment* needs the reconcile.
        assert_eq!(stale.rect, correct.rect);
        assert_eq!(stale.rect.x, 5);
    }

    #[test]
    fn geometry_events_are_buffered_not_applied() {
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843)]);
        let resized = view(4, "foot", 5, 5, 1270, 843);
        process_event("view-geometry-changed", &geometry_event(&resized), &mut t, &tx);
        // Nothing applied, nothing emitted — it is deferred, not dropped.
        assert_eq!(t.views_by_id[&4].rect.w, 629);
        assert_eq!(views_emitted(&rx), 0);
        assert_eq!(t.geom.pending.len(), 1);
        flush_geometry(&mut t, &tx);
        assert_eq!(t.views_by_id[&4].rect.w, 1270);
        assert_eq!(views_emitted(&rx), 1);
    }

    /// The measurement behind the milestone's low-power criterion: the same
    /// burst, applied per event vs coalesced. 16 events → 16 emissions
    /// before, 1 after.
    #[test]
    fn a_retile_burst_emits_once_instead_of_once_per_event() {
        // Two windows sliding apart in 8 steps each — a simple-tile retile.
        let burst: Vec<Value> = (1..=8)
            .flat_map(|i| {
                [
                    view(4, "foot", 5, 5, 629 - i * 40, 843),
                    view(8, "foot", 644 - i * 40, 5, 631 + i * 40, 843),
                ]
            })
            .collect();
        assert_eq!(burst.len(), 16);

        // Un-coalesced: flush after every event (what applying geometry
        // where it arrives amounts to).
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843), view(8, "foot", 644, 5, 631, 843)]);
        for v in &burst {
            process_event("view-geometry-changed", &geometry_event(v), &mut t, &tx);
            flush_geometry(&mut t, &tx);
        }
        assert_eq!(views_emitted(&rx), 16, "per-event baseline");

        // Coalesced: one flush when the burst goes quiet.
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843), view(8, "foot", 644, 5, 631, 843)]);
        for v in &burst {
            process_event("view-geometry-changed", &geometry_event(v), &mut t, &tx);
        }
        flush_geometry(&mut t, &tx);
        assert_eq!(views_emitted(&rx), 1, "debounced");
        // The coalesced result is the burst's final state, not a partial one.
        assert_eq!(t.views_by_id[&4].rect.w, 629 - 320);
        assert_eq!(t.views_by_id[&8].rect.x, 644 - 320);
    }

    /// An interactive drag-resize moves a few px per event. Below one icon
    /// pixel nothing the renderer could draw has changed, so nothing is
    /// emitted; the step that crosses a bucket emits once.
    #[test]
    fn sub_icon_pixel_jitter_never_emits() {
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843)]);
        // x = 10, 15, 20, 25: the cell's edges stay in the same icon px.
        for x in [10, 15, 20, 25] {
            let v = view(4, "foot", x, 5, 629, 843);
            process_event("view-geometry-changed", &geometry_event(&v), &mut t, &tx);
            flush_geometry(&mut t, &tx);
            // The exact rect is kept current even while nothing is sent…
            assert_eq!(t.views_by_id[&4].rect.x, x);
        }
        assert_eq!(views_emitted(&rx), 0, "sub-icon-pixel moves");
        // …and the move that clears an icon px does emit.
        let v = view(4, "foot", 30, 5, 629, 843);
        process_event("view-geometry-changed", &geometry_event(&v), &mut t, &tx);
        flush_geometry(&mut t, &tx);
        assert_eq!(views_emitted(&rx), 1);
    }

    #[test]
    fn workspace_switch_drops_the_buffer_and_arms_the_reconcile() {
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843)]);
        // The switch's geometry burst: already in the (1,0) frame while we
        // still believe (0,0) is focused.
        let poisoned = view(4, "foot", -1275, 5, 629, 843);
        process_event("view-geometry-changed", &geometry_event(&poisoned), &mut t, &tx);
        assert_eq!(t.geom.pending.len(), 1);

        process_event(
            "wset-workspace-changed",
            &json!({
                "event": "wset-workspace-changed",
                "new-workspace": { "x": 1, "y": 0 },
                "output": 1,
            }),
            &mut t,
            &tx,
        );
        assert!(t.geom.pending.is_empty(), "buffer dropped");
        assert!(t.take_needs_resync(), "reconcile armed");
        assert_eq!(t.state.workspace, WS1);
        // Crucially: the view was NOT misattributed to the phantom (−1,0).
        assert_eq!(t.views_by_id[&4].workspace, WS0);
        assert_eq!(views_emitted(&rx), 0);

        // The reconcile re-derives the same answer from `list-views` in the
        // new frame — a plain switch moved nothing, so it emits nothing.
        t.apply_list_views(vec![poisoned], &tx);
        assert_eq!(t.views_by_id[&4].workspace, WS0);
        assert_eq!(t.views_by_id[&4].rect.x, 5);
        assert_eq!(views_emitted(&rx), 0, "a plain switch is a no-op");
    }

    #[test]
    fn flush_never_resurrects_an_unmapped_view() {
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843)]);
        let v = view(4, "foot", 5, 5, 1270, 843);
        process_event("view-geometry-changed", &geometry_event(&v), &mut t, &tx);
        process_event(
            "view-unmapped",
            &json!({ "event": "view-unmapped", "view": { "id": 4 } }),
            &mut t,
            &tx,
        );
        assert!(t.geom.pending.is_empty(), "pending dropped with the view");
        flush_geometry(&mut t, &tx);
        assert!(!t.views_by_id.contains_key(&4), "no resurrection");
        assert!(t.stack.is_empty());
        let _ = views_emitted(&rx);
    }

    #[test]
    fn fullscreen_supersedes_buffered_geometry() {
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843)]);
        // A windowed resize is in flight when the view goes fullscreen.
        let windowed = view(4, "foot", 5, 5, 700, 843);
        process_event("view-geometry-changed", &geometry_event(&windowed), &mut t, &tx);
        let mut fs = view(4, "foot", 0, 0, 1280, 853);
        fs["fullscreen"] = json!(true);
        process_event(
            "view-fullscreen",
            &json!({ "event": "view-fullscreen", "view": fs }),
            &mut t,
            &tx,
        );
        assert!(t.geom.pending.is_empty());
        assert_eq!(t.views_by_id[&4].rect.w, 1280);
        assert!(t.views_by_id[&4].fullscreen);
        // The montage tracks the fullscreen resize, not just the set.
        assert_eq!(views_emitted(&rx), 1);
        // A late flush can no longer put the windowed rect back.
        flush_geometry(&mut t, &tx);
        assert_eq!(t.views_by_id[&4].rect.w, 1280);
    }

    #[test]
    fn geometry_flush_rehomes_a_view_that_crossed_a_workspace_boundary() {
        let (tx, rx) = async_channel::unbounded();
        let mut t = tracker(&[view(4, "foot", 5, 5, 629, 843), view(8, "foot", 644, 5, 631, 843)]);
        // id=4 dragged one workspace right (no view-workspace-changed for a
        // plain drag): reported at x = 5 + 1280.
        let dragged = view(4, "foot", 1285, 5, 629, 843);
        process_event("view-geometry-changed", &geometry_event(&dragged), &mut t, &tx);
        flush_geometry(&mut t, &tx);
        assert_eq!(t.views_by_id[&4].workspace, WS1);
        assert_eq!(t.views_by_id[&4].rect.x, 5, "workspace-local again");
        assert_eq!(t.stack[&WS0], vec![8], "evicted from the old stack");
        assert_eq!(t.stack[&WS1], vec![4]);
        assert_eq!(views_emitted(&rx), 1);
    }

    #[test]
    fn stack_rehome_leaves_a_plain_resize_where_it_was() {
        let mut stack = HashMap::from([(WS0, vec![4, 8, 9])]);
        // Same workspace: a resize must never raise a window.
        stack_rehome(&mut stack, Some(WS0), WS0, 4);
        assert_eq!(stack[&WS0], vec![4, 8, 9]);
        // Different workspace: evicted from the old, on top of the new.
        stack_rehome(&mut stack, Some(WS0), WS1, 4);
        assert_eq!(stack[&WS0], vec![8, 9]);
        assert_eq!(stack[&WS1], vec![4]);
        // Untracked source: just lands on top of the destination.
        stack_rehome(&mut stack, None, WS1, 12);
        assert_eq!(stack[&WS1], vec![4, 12]);
    }

    // ---- slice 007: the deferral clock + the frame-boundary read -------

    #[test]
    fn nothing_pending_means_no_wakeup() {
        let now = Instant::now();
        let mut geom = GeometryBuffer::default();
        assert!(geom.wait_before_flush(now).is_none(), "block indefinitely");
        assert!(!geom.flush_due(now));

        geom.buffer(4, json!({}), now);
        // Mid-burst: wait, don't flush.
        assert!(!geom.flush_due(now));
        assert!(geom.wait_before_flush(now).unwrap() <= GEOMETRY_DEBOUNCE);
        // The idle gap ends the burst.
        assert!(geom.flush_due(now + GEOMETRY_DEBOUNCE));
        geom.take();
        assert!(geom.wait_before_flush(now).is_none());
    }

    #[test]
    fn a_never_quiet_burst_still_flushes_at_the_cap() {
        let start = Instant::now();
        let mut geom = GeometryBuffer::default();
        geom.buffer(4, json!({}), start);
        // An event every 10ms keeps re-opening the idle gap…
        let mut now = start;
        for _ in 0..10 {
            now += Duration::from_millis(10);
            geom.buffer(4, json!({}), now);
            assert!(!geom.flush_due(now));
        }
        // …until the max-defer cap fires anyway, measured from the FIRST
        // event of the burst.
        let capped = start + GEOMETRY_MAX_DEFER;
        assert!(geom.flush_due(capped));
        assert!(geom.wait_before_flush(now).unwrap() <= GEOMETRY_MAX_DEFER);
    }

    #[test]
    fn read_frame_timeout_gives_up_only_at_a_frame_boundary() {
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        // Nothing written: the wait expires cleanly and the socket is left
        // blocking for the next read.
        assert!(read_frame_timeout(&mut a, Duration::from_millis(5))
            .unwrap()
            .is_none());
        assert!(a.read_timeout().unwrap().is_none(), "timeout cleared");
        // A whole frame written: read in full.
        write_frame(&mut b, &json!({ "event": "view-focused", "id": 4 })).unwrap();
        let frame = read_frame_timeout(&mut a, Duration::from_millis(500))
            .unwrap()
            .expect("frame");
        assert_eq!(frame.get("event").unwrap(), "view-focused");
        assert!(a.read_timeout().unwrap().is_none());
    }
}
