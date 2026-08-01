//! Drag controller — applies imperative (`show`, `hide`, `toggle-detailed`)
//! and gesture (`drag-begin`, `drag`, `drag-end`) verbs to the bar's visual
//! state, snapping with ease-out-cubic on release.
//!
//! Mirrors `~/projects/swipe-panel/panel-daemon/src/drag.rs`. Differences:
//!   * three discrete steady states, not two — direction at drag-begin
//!     decides the target.
//!   * the "visible margin" is a Cairo-drawn translation, not a layer-shell
//!     `margin(Edge)`. We `queue_draw` per tick.
//!   * per-workspace state map kept here. Wayfire workspace-changed (Phase 4)
//!     calls `set_workspace` to swap the live state.
//!
//! All methods run on the GTK main thread.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::cairo;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{ApplicationWindow, DrawingArea};

use crate::app_grid;
use crate::apps::{self, AppEntry};
use crate::battery::BatteryInfo;
use crate::config::{Animation, Behaviour, Geometry, Terminal};
use crate::cpu::CpuInfo;
use crate::state::{BarState, WorkspaceId};
use crate::wayfire::GridSize;
use crate::zones;

pub const BOTTOM_EDGE_ZONE_ID: &str = "sliver-dock.bottom-edge";
pub const BAR_ZONE_ID: &str = "sliver-dock.bar-closed";
pub const DETAILED_ZONE_ID: &str = "sliver-dock.detailed";
pub const SLIVER_EDGE_ZONE_ID: &str = "sliver-dock.sliver-edge";

/// Drop a tapped-but-unmapped app off the launching set after this long —
/// safety net so a failed launch (no view ever maps) doesn't spin forever.
const LAUNCH_TIMEOUT_MS: u64 = 10_000;
/// One full spinner rotation per this many ms.
const LAUNCH_SPINNER_PERIOD_MS: u64 = 900;
/// Spinner animation tick. ~30 fps is plenty for a rotating arc and keeps
/// `queue_draw` cost modest while the icon is also rendering at the bar's
/// own redraw cadence.
const LAUNCH_SPINNER_TICK_MS: u64 = 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    UpGrid,
}

/// Continuous render parameters derived from steady state ± in-flight drag.
/// `hidden_amount` slides the bar off-screen (1.0 = fully gone). `detailed_amount`
/// drives the split + section extrusion (Phase 3+ uses this; Phase 2 ships a
/// minimal placeholder lift).
#[derive(Clone, Copy, Debug, Default)]
pub struct Visual {
    pub hidden_amount: f64,
    pub detailed_amount: f64,
    pub grid_amount: f64,
}

impl From<BarState> for Visual {
    fn from(s: BarState) -> Self {
        match s {
            BarState::Closed => Visual {
                hidden_amount: 0.0,
                detailed_amount: 0.0,
                grid_amount: 0.0,
            },
            BarState::Hidden => Visual {
                hidden_amount: 1.0,
                detailed_amount: 0.0,
                grid_amount: 0.0,
            },
            BarState::Detailed => Visual {
                hidden_amount: 0.0,
                detailed_amount: 1.0,
                grid_amount: 0.0,
            },
            BarState::AppGrid => Visual {
                hidden_amount: 0.0,
                detailed_amount: 1.0,
                grid_amount: 1.0,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct Session {
    source: BarState,
    target: BarState,
    direction: Direction,
}

pub struct Drag {
    geom: Geometry,
    anim: Animation,
    terminal: Terminal,
    window: ApplicationWindow,
    area: DrawingArea,
    state: Cell<BarState>,
    workspaces: RefCell<HashMap<WorkspaceId, BarState>>,
    current_ws: Cell<WorkspaceId>,
    visual: Cell<Visual>,
    session: Cell<Option<Session>>,
    animation: RefCell<Option<glib::SourceId>>,
    /// Wayfire workspace grid (e.g. 2×2). Populated by the wayfire
    /// subscriber on Connected; `None` while the dock is in degraded mode.
    grid: Cell<Option<GridSize>>,
    /// Wayfire output ID of the focused output. Needed for outbound
    /// `vswitch/set-workspace` calls (target output must be specified).
    /// Updated alongside `grid` from the wayfire subscriber.
    wayfire_output_id: Cell<Option<i64>>,
    /// Workspaces that have at least one mapped toplevel view. Driven by
    /// the wayfire `OpenedWorkspacesChanged` event. The render set is the
    /// union of this with the focused workspace (so the dock is never
    /// empty when on an unoccupied workspace).
    opened_workspaces: RefCell<HashSet<WorkspaceId>>,
    /// Apps to render in the apps section, ordered (pinned first, rest
    /// alphabetical). Set once at startup; will become dynamic in 5e
    /// (running-filter).
    apps: RefCell<Vec<AppEntry>>,
    /// Full app-id → AppEntry registry, including hidden / NoDisplay
    /// entries that don't make it into `apps`. Drives icon-spec lookup
    /// for the workspace montage (Milestone D), which gets app-ids from
    /// Wayfire and needs to resolve them to icon paths regardless of
    /// whether the entry would show in the apps section.
    app_registry: RefCell<HashMap<String, AppEntry>>,
    /// Lazy-populated icon surface cache, keyed by app id. `None` value
    /// means we tried to load the icon and it failed — we cache the miss
    /// so we don't retry on every frame.
    icon_cache: RefCell<HashMap<String, Option<cairo::ImageSurface>>>,
    /// Set of app-ids with at least one mapped toplevel view. Driven by
    /// the wayfire `RunningAppsChanged` event. Apps section filters out
    /// any entry whose id is in this set (v1-plan: "running apps disappear
    /// from this section"). Empty until the first event arrives.
    running: RefCell<HashSet<String>>,
    /// Output pixel size of the focused output. Needed for terminal-layout
    /// geometry math (slot rectangles are computed as fractions of output
    /// size). Set by the wayfire subscriber on Connected.
    output_size: Cell<Option<(i32, i32)>>,
    /// Workspace where the last successful terminal-launch laid out its
    /// views. `Some(_)` means subsequent terminal taps re-focus that
    /// workspace instead of relaunching, provided the terminal app-id is
    /// still in the running set (else we treat it as gone and respawn).
    terminal_workspace: Cell<Option<WorkspaceId>>,
    /// Pending per-view geometries for an in-flight terminal launch. The
    /// daemon enqueues N rectangles when the terminal entry is tapped;
    /// each subsequent `view-mapped` event for the configured terminal
    /// app-id pops the front rectangle and issues `configure-view`.
    terminal_pending: RefCell<VecDeque<(i32, i32, i32, i32)>>,
    /// Latest UPower DisplayDevice snapshot. `None` until the first proxy
    /// reply lands, or if UPower isn't reachable. Read by `draw_info_section`.
    battery: Cell<Option<BatteryInfo>>,
    /// Latest CPU usage sample (1Hz from `cpu::spawn`). `None` before the
    /// second tick (the first records a baseline only).
    cpu: Cell<Option<CpuInfo>>,
    /// Behaviour toggles from config (currently only `hide_when_fullscreen`
    /// is wired). Stored so the fullscreen-set handler can early-out
    /// without touching state when the user has opted out.
    behaviour: Behaviour,
    /// Workspaces that currently host at least one fullscreen toplevel.
    /// Driven by the wayfire `FullscreenWorkspacesChanged` event; the diff
    /// against the next set decides which workspaces enter/leave fullscreen
    /// mode and trigger the force-Hidden / restore flow.
    fullscreen_workspaces: RefCell<HashSet<WorkspaceId>>,
    /// Per-workspace state snapshot captured at the moment a workspace
    /// entered fullscreen — used to restore the bar's state when fullscreen
    /// exits. Only present for workspaces currently in `fullscreen_workspaces`.
    /// If the user manually shows the bar while fullscreen is active, the
    /// per-workspace state map diverges from this snapshot; on exit we
    /// detect that and skip the restore (user override wins).
    pre_fullscreen: RefCell<HashMap<WorkspaceId, BarState>>,
    /// Per-workspace ordered app-id list, MRU-first, deduped by app-id.
    /// Driven by the wayfire `WorkspaceAppMontagesChanged` event;
    /// projected into the per-workspace icon montage by
    /// `panel.rs::draw_workspace_icons`. Truncation to 4 + the `+N`
    /// overflow badge live in the renderer (see `montage_cells` and
    /// `draw_overflow_badge`).
    workspace_app_montages: RefCell<HashMap<WorkspaceId, Vec<String>>>,
    /// App-ids tapped but not yet seen in the `running` set. Drives the
    /// per-icon spinner overlay in `draw_app_icons`. Entries are cleared
    /// when the app appears in `set_running_apps` or when their age
    /// exceeds `LAUNCH_TIMEOUT_MS` (failed-launch safety net).
    launching: RefCell<HashMap<String, Instant>>,
    /// Common start instant for the spinner's rotation angle so every
    /// in-flight spinner stays phase-aligned. `Some(_)` only while the
    /// animation tick is live; reset to `None` when launching drains.
    launching_started_at: Cell<Option<Instant>>,
    launching_animation: RefCell<Option<glib::SourceId>>,
    launch_counts: RefCell<HashMap<String, u64>>,
    grid_state: RefCell<Option<app_grid::GridState>>,
}

impl Drag {
    pub fn new(
        geom: Geometry,
        anim: Animation,
        terminal: Terminal,
        behaviour: Behaviour,
        window: ApplicationWindow,
        area: DrawingArea,
    ) -> Rc<Self> {
        Rc::new(Self {
            geom,
            anim,
            terminal,
            window,
            area,
            state: Cell::new(BarState::Detailed),
            workspaces: RefCell::new(HashMap::new()),
            current_ws: Cell::new(WorkspaceId::ZERO),
            visual: Cell::new(BarState::Detailed.into()),
            session: Cell::new(None),
            animation: RefCell::new(None),
            grid: Cell::new(None),
            wayfire_output_id: Cell::new(None),
            opened_workspaces: RefCell::new(HashSet::new()),
            apps: RefCell::new(Vec::new()),
            app_registry: RefCell::new(HashMap::new()),
            icon_cache: RefCell::new(HashMap::new()),
            running: RefCell::new(HashSet::new()),
            output_size: Cell::new(None),
            terminal_workspace: Cell::new(None),
            terminal_pending: RefCell::new(VecDeque::new()),
            battery: Cell::new(None),
            cpu: Cell::new(None),
            behaviour,
            fullscreen_workspaces: RefCell::new(HashSet::new()),
            pre_fullscreen: RefCell::new(HashMap::new()),
            workspace_app_montages: RefCell::new(HashMap::new()),
            launching: RefCell::new(HashMap::new()),
            launching_started_at: Cell::new(None),
            launching_animation: RefCell::new(None),
            launch_counts: RefCell::new(HashMap::new()),
            grid_state: RefCell::new(None),
        })
    }

    pub fn battery(&self) -> Option<BatteryInfo> {
        self.battery.get()
    }

    pub fn set_battery(&self, info: BatteryInfo) {
        let prev = self.battery.replace(Some(info));
        if prev != Some(info) {
            self.area.queue_draw();
        }
    }

    pub fn cpu(&self) -> Option<CpuInfo> {
        self.cpu.get()
    }

    pub fn set_cpu(&self, info: CpuInfo) {
        let prev = self.cpu.replace(Some(info));
        if prev != Some(info) {
            self.area.queue_draw();
        }
    }

    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    pub fn output_size(&self) -> Option<(i32, i32)> {
        self.output_size.get()
    }

    pub fn set_output_size(&self, size: (i32, i32)) {
        self.output_size.set(Some(size));
    }

    pub fn terminal_workspace(&self) -> Option<WorkspaceId> {
        self.terminal_workspace.get()
    }

    pub fn set_terminal_workspace(&self, ws: Option<WorkspaceId>) {
        self.terminal_workspace.set(ws);
    }

    pub fn enqueue_terminal_slots<I>(&self, slots: I)
    where
        I: IntoIterator<Item = (i32, i32, i32, i32)>,
    {
        let mut q = self.terminal_pending.borrow_mut();
        q.clear();
        q.extend(slots);
    }

    pub fn pop_terminal_slot(&self) -> Option<(i32, i32, i32, i32)> {
        self.terminal_pending.borrow_mut().pop_front()
    }

    pub fn state(&self) -> BarState {
        self.state.get()
    }

    pub fn visual(&self) -> Visual {
        self.visual.get()
    }

    pub fn current_workspace(&self) -> WorkspaceId {
        self.current_ws.get()
    }

    #[allow(dead_code)] // 3x3 mini-grid expansion (deferred touch-drawer plan)
    pub fn grid(&self) -> Option<GridSize> {
        self.grid.get()
    }

    pub fn set_grid(&self, grid: GridSize) {
        self.grid.set(Some(grid));
        self.area.queue_draw();
    }

    pub fn wayfire_output_id(&self) -> Option<i64> {
        self.wayfire_output_id.get()
    }

    pub fn set_wayfire_output_id(&self, id: i64) {
        self.wayfire_output_id.set(Some(id));
    }

    pub fn set_opened_workspaces(&self, set: HashSet<WorkspaceId>) {
        *self.opened_workspaces.borrow_mut() = set;
        self.area.queue_draw();
    }

    /// Snapshot the opened-workspaces set. Cheap clone (≤ grid_w × grid_h
    /// entries — 4 on this user's setup); avoids leaking a `Ref` across
    /// callers that also borrow other Drag fields.
    pub fn opened_workspaces(&self) -> HashSet<WorkspaceId> {
        self.opened_workspaces.borrow().clone()
    }

    /// Workspaces to render in the section: the union of opened workspaces
    /// (those with at least one toplevel view) with the currently focused
    /// workspace, sorted row-major (y first, then x).
    pub fn rendered_workspaces(&self) -> Vec<WorkspaceId> {
        let mut set: HashSet<WorkspaceId> = self.opened_workspaces.borrow().clone();
        set.insert(self.current_ws.get());
        let mut out: Vec<WorkspaceId> = set.into_iter().collect();
        out.sort_by(|a, b| a.y.cmp(&b.y).then_with(|| a.x.cmp(&b.x)));
        out
    }

    pub fn set_apps(&self, apps: Vec<AppEntry>) {
        *self.apps.borrow_mut() = apps;
        self.icon_cache.borrow_mut().clear();
        self.area.queue_draw();
    }

    pub fn set_running_apps(&self, running: HashSet<String>) {
        // Tap → spinner is cleared the moment the launched view shows up
        // in the running set. The animation tick (started by `mark_launching`)
        // self-terminates on the next iteration when the map drains.
        self.launching
            .borrow_mut()
            .retain(|id, _| !running.contains(id));
        *self.running.borrow_mut() = running;
        self.area.queue_draw();
    }

    pub fn running_apps(&self) -> HashSet<String> {
        self.running.borrow().clone()
    }

    /// Replace the per-workspace montage map. `queue_draw` only fires when
    /// the projection actually changes — the subscriber already gates the
    /// event, but a manual equality check keeps `set_workspace_app_montages`
    /// safe to call from anywhere without forcing a redraw.
    pub fn set_workspace_app_montages(&self, map: HashMap<WorkspaceId, Vec<String>>) {
        let mut slot = self.workspace_app_montages.borrow_mut();
        if *slot == map {
            return;
        }
        *slot = map;
        drop(slot);
        self.area.queue_draw();
    }

    /// Ordered app-id list for `ws`, MRU-first, deduped. Empty when the
    /// workspace has no tracked toplevels — the renderer falls back to
    /// the bare-placeholder icon in that case.
    pub fn workspace_app_montage(&self, ws: WorkspaceId) -> Vec<String> {
        self.workspace_app_montages
            .borrow()
            .get(&ws)
            .cloned()
            .unwrap_or_default()
    }

    /// Apps to render in the apps section: pinned-then-alphabetical (set
    /// by `set_apps`) minus any whose id is currently running. Cloned so
    /// callers don't have to juggle two RefCell borrows simultaneously.
    pub fn visible_apps(&self) -> Vec<AppEntry> {
        let apps = self.apps.borrow();
        let running = self.running.borrow();
        apps.iter()
            .filter(|e| !running.contains(&e.id))
            .cloned()
            .collect()
    }

    /// Record that the user just tapped `id` to launch it. The spinner
    /// overlay shows in `draw_app_icons` for any id present in this map.
    /// Cleared when the app appears in the running set (real success) or
    /// when its age exceeds `LAUNCH_TIMEOUT_MS` (failed-launch safety net).
    pub fn mark_launching(self: &Rc<Self>, id: &str) {
        let now = Instant::now();
        self.launching.borrow_mut().insert(id.to_owned(), now);
        if self.launching_started_at.get().is_none() {
            self.launching_started_at.set(Some(now));
        }
        self.start_launching_animation();
        self.area.queue_draw();
    }

    pub fn launching_ids(&self) -> HashSet<String> {
        self.launching.borrow().keys().cloned().collect()
    }

    /// Rotation angle (radians) shared by every in-flight spinner so they
    /// stay phase-aligned. One full rotation per `LAUNCH_SPINNER_PERIOD_MS`.
    /// Zero when no animation is in flight.
    pub fn launching_angle(&self) -> f64 {
        use std::f64::consts::TAU;
        let Some(start) = self.launching_started_at.get() else {
            return 0.0;
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        (elapsed_ms / LAUNCH_SPINNER_PERIOD_MS as f64) * TAU
    }

    fn start_launching_animation(self: &Rc<Self>) {
        if self.launching_animation.borrow().is_some() {
            return;
        }
        let this = Rc::clone(self);
        let id = glib::timeout_add_local(Duration::from_millis(LAUNCH_SPINNER_TICK_MS), move || {
            let now = Instant::now();
            let timeout = Duration::from_millis(LAUNCH_TIMEOUT_MS);
            let still_pending = {
                let mut map = this.launching.borrow_mut();
                map.retain(|_, started| now.duration_since(*started) < timeout);
                !map.is_empty()
            };
            this.area.queue_draw();
            if still_pending {
                glib::ControlFlow::Continue
            } else {
                this.launching_animation.borrow_mut().take();
                this.launching_started_at.set(None);
                glib::ControlFlow::Break
            }
        });
        *self.launching_animation.borrow_mut() = Some(id);
    }

    /// Lazy + cached icon lookup. The first frame after `set_apps` pays the
    /// scan + decode cost (one pixbuf load per visible icon); subsequent
    /// frames hit the cache. Negative results are cached too — a missing
    /// icon shouldn't keep retrying every frame.
    pub fn app_icon(
        &self,
        id: &str,
        icon_spec: Option<&str>,
        size: i32,
    ) -> Option<cairo::ImageSurface> {
        if let Some(cached) = self.icon_cache.borrow().get(id) {
            return cached.clone();
        }
        let surface = icon_spec.and_then(|s| apps::load_icon_surface(s, size));
        self.icon_cache
            .borrow_mut()
            .insert(id.to_owned(), surface.clone());
        surface
    }

    /// Replace the app-id → AppEntry registry. Called once at startup
    /// alongside `set_apps`. Distinct from `apps` because the registry
    /// includes hidden / NoDisplay entries whose icons we still need
    /// to resolve for the workspace montage.
    pub fn set_app_registry(&self, map: HashMap<String, AppEntry>) {
        *self.app_registry.borrow_mut() = map;
    }

    /// Icon lookup by app-id alone. Resolves the icon spec from the
    /// registry, then routes through the shared `icon_cache` so apps
    /// section + workspace montage share decoded surfaces. Unknown ids
    /// (a running app whose .desktop entry wasn't scanned) cache as a
    /// miss so we don't retry on every frame.
    pub fn app_icon_by_id(&self, id: &str, size: i32) -> Option<cairo::ImageSurface> {
        if let Some(cached) = self.icon_cache.borrow().get(id) {
            return cached.clone();
        }
        let spec = self
            .app_registry
            .borrow()
            .get(id)
            .and_then(|e| e.icon.clone());
        let surface = spec.as_deref().and_then(|s| apps::load_icon_surface(s, size));
        self.icon_cache
            .borrow_mut()
            .insert(id.to_owned(), surface.clone());
        surface
    }

    /// JSON-ish summary for the `get-state` IPC verb. Hand-rolled to avoid
    /// pulling in serde_json for one debug verb.
    pub fn dump_state(&self) -> String {
        let v = self.visual.get();
        let ws = self.current_ws.get();
        let map = self.workspaces.borrow();
        let mut entries: Vec<String> = map
            .iter()
            .map(|(k, s)| format!(r#"{{"x":{},"y":{},"state":"{}"}}"#, k.x, k.y, s.as_str()))
            .collect();
        entries.sort();
        format!(
            r#"{{"state":"{}","visual":{{"hidden":{:.3},"detailed":{:.3},"grid":{:.3}}},"workspace":{{"x":{},"y":{}}},"map":[{}]}}"#,
            self.state.get().as_str(),
            v.hidden_amount,
            v.detailed_amount,
            v.grid_amount,
            ws.x,
            ws.y,
            entries.join(",")
        )
    }

    pub fn auto_close_app_grid(&self) -> bool {
        self.behaviour.auto_close_app_grid
    }

    fn cancel_animation(&self) {
        if let Some(id) = self.animation.borrow_mut().take() {
            id.remove();
        }
    }

    fn set_visual(&self, v: Visual) {
        self.visual.set(v);
        self.area.queue_draw();
    }

    fn save_current_workspace(&self) {
        let ws = self.current_ws.get();
        let s = self.state.get();
        self.workspaces.borrow_mut().insert(ws, s);
    }

    fn settle(&self, target: BarState) {
        self.state.set(target);
        self.set_visual(target.into());
        self.save_current_workspace();
        self.apply_visibility();
        if target == BarState::AppGrid {
            self.rebuild_grid();
        }
        update_zone_states(target);
        self.refresh_input_region();
    }

    /// Hidden state unmaps the surface so it doesn't intercept touches; any
    /// other state must be mapped before we redraw.
    fn apply_visibility(&self) {
        let want_visible = !matches!(self.state.get(), BarState::Hidden);
        if self.window.is_visible() != want_visible {
            self.window.set_visible(want_visible);
        }
    }

    /// Pre-show the surface for an upcoming draw — used at drag-begin from
    /// Hidden state so the unhide animation has a mapped surface.
    fn ensure_mapped(&self) {
        if !self.window.is_visible() {
            self.window.set_visible(true);
        }
    }

    /// Constrain the layer-shell surface's input region to the visible
    /// interactive area only. Without this the full transparent window
    /// (full screen width × `margin + bar + content_height` tall) would
    /// intercept clicks meant for the apps below.
    ///
    /// Closed state: from the bar's top edge down to the screen edge —
    /// the bar itself plus the `margin_bottom_px` band beneath it. The
    /// touch-drawer `bar-closed` zone sits over this same region and
    /// catches swipe-up gestures (Closed → Detailed). Tap-through still
    /// works because touch-drawer only commits on motion past
    /// `commit_threshold`.
    ///
    /// Detailed state: covers the bar plus the section content area; the
    /// touch-drawer `sliver-dock.detailed` bidir zone fires `drag-begin
    /// down` for swipe-down → Closed.
    /// Hidden state: surface is unmapped.
    ///
    pub fn refresh_input_region(&self) {
        let Some(surface) = self.window.surface() else { return };
        let win_w = surface.width();
        let win_h = surface.height();
        if win_w <= 0 || win_h <= 0 {
            return;
        }
        let g = &self.geom;
        let bar_w = g.width_px.min(win_w);
        // `win_h` is the real surface height — `grid_height_px` taller than
        // the pre-grid window. The bar is bottom-anchored, so the input
        // region is measured up from the bottom edge. Using the old
        // `margin+default+detailed` constant here pinned the region to the
        // top `~107px` of a 507px surface, leaving the visible bar unclickable.
        let bar_top = win_h - g.margin_bottom_px - g.default_height_px;

        let extend_up = if self.input_should_be_expanded() {
            if self.grid_expand_needed() {
                g.default_height_px + g.grid_height_px
            } else {
                g.detailed_content_height_px
            }
        } else {
            0
        };

        let x = (win_w - bar_w) / 2;
        let y = (bar_top - extend_up).max(0);
        let h = win_h - y;

        let rect = cairo::RectangleInt::new(x, y, bar_w, h);
        let region = cairo::Region::create_rectangle(&rect);
        surface.set_input_region(&region);
    }

    /// Input region grows whenever any part of the detailed view is
    /// visible — including transitions toward / away from Detailed — so
    /// clicks land while the gap is animating, not just after settle.
    fn input_should_be_expanded(&self) -> bool {
        if let Some(s) = self.session.get() {
            return matches!(s.source, BarState::Detailed | BarState::AppGrid)
                || matches!(s.target, BarState::Detailed | BarState::AppGrid);
        }
        matches!(self.state.get(), BarState::Detailed | BarState::AppGrid)
    }

    fn grid_expand_needed(&self) -> bool {
        if let Some(s) = self.session.get() {
            return matches!(s.source, BarState::AppGrid)
                || matches!(s.target, BarState::AppGrid);
        }
        matches!(self.state.get(), BarState::AppGrid)
    }

    #[allow(dead_code)] // Phase 3+: instant-set without animation
    pub fn jump_to(self: &Rc<Self>, target: BarState) {
        self.cancel_animation();
        self.session.set(None);
        self.ensure_mapped();
        self.state.set(target);
        self.set_visual(target.into());
        self.save_current_workspace();
        if target == BarState::AppGrid {
            self.rebuild_grid();
        }
        self.apply_visibility();
        update_zone_states(target);
        self.refresh_input_region();
    }

    pub fn toggle_detailed(self: &Rc<Self>) {
        let next = match self.state.get() {
            BarState::Detailed => BarState::Closed,
            _ => BarState::Detailed,
        };
        self.snap_to_state(next);
    }

    pub fn set_workspace(self: &Rc<Self>, ws: WorkspaceId) {
        if self.current_ws.get() == ws {
            return;
        }
        self.save_current_workspace();
        self.current_ws.set(ws);
        let target = self
            .workspaces
            .borrow()
            .get(&ws)
            .copied()
            .unwrap_or(BarState::Detailed);
        self.snap_to_state(target);
    }

    pub fn load_launches(&self) {
        let counts = app_grid::load_launches();
        *self.launch_counts.borrow_mut() = counts;
    }

    pub fn increment_launch_count(&self, id: &str) {
        {
            let mut counts = self.launch_counts.borrow_mut();
            *counts.entry(id.to_owned()).or_insert(0) += 1;
        }
        if let Err(e) = app_grid::save_launches(&self.launch_counts.borrow()) {
            eprintln!("sliver-dock: failed to save launches: {e}");
        }
    }

    pub fn rebuild_grid(&self) {
        let registry = self.app_registry.borrow();
        let counts = self.launch_counts.borrow();
        if registry.is_empty() {
            return;
        }
        let entries_map: BTreeMap<String, AppEntry> = registry.clone().into_iter().collect();
        let g = &self.geom;
        let bar_w = g.width_px as f64;
        let tip_len = g.default_height_px as f64 * 6.0;
        let grid_w = (bar_w - 2.0 * tip_len).max(0.0);
        let state = app_grid::build_grid(
            &entries_map,
            &counts,
            grid_w,
            g.grid_height_px as f64,
        );
        *self.grid_state.borrow_mut() = Some(state);
    }

    pub fn grid_state(&self) -> Option<app_grid::GridState> {
        self.grid_state.borrow().clone()
    }

    /// Apply the latest fullscreen-workspace set from Wayfire. No-op when
    /// `behaviour.hide_when_fullscreen` is false.
    ///
    /// Diff semantics:
    ///   * **Newly fullscreen ws** — snapshot the workspace's current state
    ///     into `pre_fullscreen` (so we can restore later), force its
    ///     per-workspace state to `Hidden`, and if it's the focused ws,
    ///     snap the live bar to `Hidden`.
    ///   * **Exited fullscreen ws** — if the workspace's *current* state
    ///     is still `Hidden` (meaning the user hasn't manually overridden
    ///     while fullscreen was active), restore the saved state. If the
    ///     user did override, drop the snapshot and leave their choice
    ///     intact. Either way, drop the `pre_fullscreen` entry.
    ///
    /// `save_current_workspace` is called before the diff so the
    /// `workspaces` map reflects the focused workspace's live state — the
    /// snapshot we take is therefore current rather than stale.
    pub fn set_fullscreen_workspaces(self: &Rc<Self>, new_set: HashSet<WorkspaceId>) {
        if !self.behaviour.hide_when_fullscreen {
            return;
        }
        self.save_current_workspace();
        let current_ws = self.current_ws.get();
        let prev_set: HashSet<WorkspaceId> = self.fullscreen_workspaces.borrow().clone();

        let newly: Vec<WorkspaceId> = new_set.difference(&prev_set).copied().collect();
        let exited: Vec<WorkspaceId> = prev_set.difference(&new_set).copied().collect();

        for ws in newly {
            let prior = self
                .workspaces
                .borrow()
                .get(&ws)
                .copied()
                .unwrap_or(BarState::Detailed);
            self.pre_fullscreen.borrow_mut().entry(ws).or_insert(prior);
            self.workspaces.borrow_mut().insert(ws, BarState::Hidden);
            if ws == current_ws && self.state.get() != BarState::Hidden {
                self.snap_to_state(BarState::Hidden);
            }
        }

        for ws in exited {
            let saved = self.pre_fullscreen.borrow_mut().remove(&ws);
            let current = self
                .workspaces
                .borrow()
                .get(&ws)
                .copied()
                .unwrap_or(BarState::Detailed);
            if let Some(restore) = saved {
                if current == BarState::Hidden {
                    self.workspaces.borrow_mut().insert(ws, restore);
                    if ws == current_ws {
                        self.snap_to_state(restore);
                    }
                }
            }
        }

        *self.fullscreen_workspaces.borrow_mut() = new_set;
    }

    pub fn snap_to_state(self: &Rc<Self>, target: BarState) {
        let from = self.visual.get();
        let to = Visual::from(target);
        self.cancel_animation();
        self.session.set(None);
        self.ensure_mapped();
        if (from.hidden_amount - to.hidden_amount).abs() < 1e-6
            && (from.detailed_amount - to.detailed_amount).abs() < 1e-6
            && (from.grid_amount - to.grid_amount).abs() < 1e-6
        {
            self.settle(target);
            return;
        }
        let dur_ms = self.anim.snap_duration_ms.max(1) as f64;
        let start = Instant::now();
        let this = Rc::clone(self);
        let id = glib::timeout_add_local(Duration::from_millis(16), move || {
            let t = (start.elapsed().as_secs_f64() * 1000.0 / dur_ms).min(1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            let v = Visual {
                hidden_amount: lerp(from.hidden_amount, to.hidden_amount, eased),
                detailed_amount: lerp(from.detailed_amount, to.detailed_amount, eased),
                grid_amount: lerp(from.grid_amount, to.grid_amount, eased),
            };
            this.set_visual(v);
            if t >= 1.0 {
                this.animation.borrow_mut().take();
                this.settle(target);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        *self.animation.borrow_mut() = Some(id);
    }

    pub fn begin(self: &Rc<Self>, direction: Direction) {
        let source = self.state.get();
        let target = match (source, direction) {
            (BarState::Closed, Direction::Up) => BarState::Detailed,
            (BarState::Closed, Direction::Down) => BarState::Hidden,
            (BarState::Closed, Direction::UpGrid) => BarState::AppGrid,
            (BarState::Detailed, Direction::Down) => BarState::Closed,
            (BarState::Detailed, Direction::Up) => BarState::AppGrid,
            (BarState::Detailed, Direction::UpGrid) => BarState::AppGrid,
            (BarState::Hidden, Direction::Up) => BarState::Closed,
            (BarState::AppGrid, Direction::Down) => BarState::Detailed,
            _ => return,
        };
        self.cancel_animation();
        self.session.set(Some(Session {
            source,
            target,
            direction,
        }));
        self.ensure_mapped();
        if target == BarState::AppGrid {
            self.rebuild_grid();
        }
        self.set_visual(Visual::from(source));
        self.refresh_input_region();
    }

    pub fn update(self: &Rc<Self>, progress: f64) {
        let Some(s) = self.session.get() else { return };
        let p = forward_progress(s.direction, progress);
        let from = Visual::from(s.source);
        let to = Visual::from(s.target);
        let v = Visual {
            hidden_amount: lerp(from.hidden_amount, to.hidden_amount, p),
            detailed_amount: lerp(from.detailed_amount, to.detailed_amount, p),
            grid_amount: lerp(from.grid_amount, to.grid_amount, p),
        };
        self.set_visual(v);
    }

    pub fn end(self: &Rc<Self>, progress: f64, velocity_px_per_ms: f64) {
        let Some(s) = self.session.get() else { return };
        let p = forward_progress(s.direction, progress);
        let v_forward = forward_velocity(s.direction, velocity_px_per_ms);

        // Snap decision tier (cloned from swipe-panel/drag.rs:148):
        //   1. fling — go with velocity sign
        //   2. sub-fling motion — bias threshold toward direction of motion
        //   3. stationary — neutral 0.5 midpoint
        let target = if v_forward.abs() > self.anim.fling_threshold_px_per_ms {
            if v_forward > 0.0 { s.target } else { s.source }
        } else {
            const DIRECTION_BIAS: f64 = 0.05;
            let snap_threshold = if v_forward > DIRECTION_BIAS {
                0.2
            } else if v_forward < -DIRECTION_BIAS {
                0.8
            } else {
                0.5
            };
            if p > snap_threshold { s.target } else { s.source }
        };
        self.snap_to_state(target);
    }
}

/// touch-drawer reports progress relative to its zone direction:
///   * Down zones:  0.0 (initial)  →  1.0 (committed)        — use as-is
///   * Up zones:    1.0 (initial)  →  0.0 (committed)        — invert
///
/// We normalize so that 0.0 always means "still at source" and 1.0 always
/// means "fully at target". See touch-drawer/src/main.cpp:286-298 for the
/// plugin-side calculation.
fn forward_progress(dir: Direction, p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    match dir {
        Direction::Down => p,
        Direction::Up | Direction::UpGrid => 1.0 - p,
    }
}

/// touch-drawer reports raw screen-Y velocity: positive = downward
/// (see touch-drawer/src/main.cpp:320). For "forward toward target":
///   * Down zones:  positive velocity = forward
///   * Up zones:    negative velocity = forward (flip the sign)
fn forward_velocity(dir: Direction, v: f64) -> f64 {
    match dir {
        Direction::Down => v,
        Direction::Up | Direction::UpGrid => -v,
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Flip touch-drawer zones based on the steady state. At most one is
/// enabled at a time:
///   * Closed   → bar zone (swipe up over the bar opens Detailed)
///   * Hidden   → bottom-edge zone (swipe up at screen edge unhides)
///   * Detailed → detailed bidir zone (swipe down anywhere on the
///     expanded window closes to Closed; up arm is reserved for a
///     future workspaces-section grid-expand gesture)
fn update_zone_states(state: BarState) {
    zones::set_enabled(BAR_ZONE_ID, matches!(state, BarState::Closed));
    zones::set_enabled(BOTTOM_EDGE_ZONE_ID, matches!(state, BarState::Hidden));
    zones::set_enabled(DETAILED_ZONE_ID, false);
    zones::set_enabled(
        SLIVER_EDGE_ZONE_ID,
        matches!(state, BarState::Closed | BarState::Detailed | BarState::AppGrid),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_progress_down_unchanged() {
        assert!((forward_progress(Direction::Down, 0.0) - 0.0).abs() < 1e-9);
        assert!((forward_progress(Direction::Down, 0.5) - 0.5).abs() < 1e-9);
        assert!((forward_progress(Direction::Down, 1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn forward_progress_up_inverted() {
        assert!((forward_progress(Direction::Up, 0.0) - 1.0).abs() < 1e-9);
        assert!((forward_progress(Direction::Up, 0.5) - 0.5).abs() < 1e-9);
        assert!((forward_progress(Direction::Up, 1.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn forward_progress_up_grid_inverted() {
        assert!((forward_progress(Direction::UpGrid, 0.0) - 1.0).abs() < 1e-9);
        assert!((forward_progress(Direction::UpGrid, 0.5) - 0.5).abs() < 1e-9);
        assert!((forward_progress(Direction::UpGrid, 1.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn forward_progress_clamped() {
        assert!((forward_progress(Direction::Down, -0.5) - 0.0).abs() < 1e-9);
        assert!((forward_progress(Direction::Down, 1.5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn forward_velocity_down_unchanged() {
        assert!((forward_velocity(Direction::Down, 10.0) - 10.0).abs() < 1e-9);
        assert!((forward_velocity(Direction::Down, -3.0) - (-3.0)).abs() < 1e-9);
    }

    #[test]
    fn forward_velocity_up_negated() {
        assert!((forward_velocity(Direction::Up, 10.0) - (-10.0)).abs() < 1e-9);
        assert!((forward_velocity(Direction::Up, -3.0) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn forward_velocity_up_grid_negated() {
        assert!((forward_velocity(Direction::UpGrid, 5.0) - (-5.0)).abs() < 1e-9);
    }

    #[test]
    fn lerp_boundaries() {
        assert!((lerp(0.0, 10.0, 0.0) - 0.0).abs() < 1e-9);
        assert!((lerp(0.0, 10.0, 1.0) - 10.0).abs() < 1e-9);
        assert!((lerp(0.0, 10.0, 0.5) - 5.0).abs() < 1e-9);
        assert!((lerp(5.0, 15.0, 0.25) - 7.5).abs() < 1e-9);
    }

    #[test]
    fn direction_equality() {
        assert_eq!(Direction::Up, Direction::Up);
        assert_ne!(Direction::Up, Direction::Down);
        assert_ne!(Direction::Up, Direction::UpGrid);
        assert_ne!(Direction::Down, Direction::UpGrid);
    }
}
