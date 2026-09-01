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
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::cairo;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{ApplicationWindow, DrawingArea};

use crate::apps::{self, AppEntry};
use crate::astro;
use crate::backdrop;
use crate::battery::BatteryInfo;
use crate::categories::Planet;
use crate::config::{Animation, Astro, Behaviour, Geometry, Terminal};
use crate::launches;
use crate::cpu::CpuInfo;
use crate::state::{BarState, WorkspaceId};
use crate::wayfire::GridSize;
use crate::zones;

pub const BOTTOM_EDGE_ZONE_ID: &str = "sliver-dock.bottom-edge";
pub const BAR_ZONE_ID: &str = "sliver-dock.bar-closed";
pub const DETAILED_ZONE_ID: &str = "sliver-dock.detailed";
pub const SLIVER_EDGE_ZONE_ID: &str = "sliver-dock.sliver-edge";
/// The quick-launcher apps section zone — the narrow rect that must win
/// over the broad detailed zone it sits inside (registration order).
pub const APPS_ZONE_ID: &str = "sliver-dock.apps";

/// Drop a tapped-but-unmapped app off the launching set after this long —
/// safety net so a failed launch (no view ever maps) doesn't spin forever.
const LAUNCH_TIMEOUT_MS: u64 = 10_000;
/// One full spinner rotation per this many ms.
const LAUNCH_SPINNER_PERIOD_MS: u64 = 900;
/// Spinner animation tick. ~30 fps is plenty for a rotating arc and keeps
/// `queue_draw` cost modest while the icon is also rendering at the bar's
/// own redraw cadence.
const LAUNCH_SPINNER_TICK_MS: u64 = 33;

/// Frame cadence while the astro backdrop has something moving — the same
/// 16ms the open/close animation runs at (note 010 slice 5: "cheap ambient
/// animation, 16ms ticker reused").
///
/// It is only ever armed at this rate *during* a shooting star. Between them
/// the timer sleeps to the next one instead of polling, because the starfield
/// is static and there is nothing to ask: an idle 60Hz tick measured ~7% of a
/// core on this device, all of it spent deciding not to draw.
const ASTRO_FRAME_MS: u64 = 16;

/// Ceiling on an idle sleep. The gap between shooting stars is usually the
/// whole `shooting_star_period_s`; capping the nap keeps the animation from
/// parking indefinitely if that ever computes wrong, at a cost of one wakeup
/// every two seconds.
const ASTRO_IDLE_SLEEP_MAX_MS: u64 = 2_000;

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

#[derive(Clone)]
struct Session {
    source: BarState,
    target: BarState,
    direction: Direction,
    /// The touch-drawer zone id that matched, from the optional `zone=`
    /// token on `drag-begin`. `None` when the running plugin predates the
    /// token (slice 003 protocol extension) — the gesture still works, only
    /// the sample ring can't name the zone.
    zone: Option<String>,
}

/// One completed gesture, captured at `drag-end` — the per-gesture sample
/// the fling-threshold tune (slice 003) needs. Kept in a bounded in-memory
/// ring surfaced through `get-state`, so the human collects real flicks
/// with `nc` instead of grep'ing the journal. No per-event cost: one push
/// per released gesture, no timers, no disk writes.
#[derive(Clone, Debug)]
pub struct GestureSample {
    /// Zone id that matched, or "-" when unknown (pre-token plugin).
    pub zone: String,
    pub direction: Direction,
    /// State the gesture began in — with `direction` this pins the
    /// routing arm, which disambiguates zone pairs sharing an arm.
    pub source: BarState,
    /// Forward-normalized progress at release — the tier's input.
    pub progress: f64,
    /// Raw release velocity, px/ms, positive = down — the fling tier's
    /// input (`|v|` vs `fling_threshold_px_per_ms`).
    pub velocity: f64,
    /// Monotonic ms since daemon start, to correlate with the plugin's
    /// own `drag-end` journal line.
    pub ts_ms: u64,
    /// The snap decision: committed to target (true) or bounced back to
    /// the source (false). The diagnosis gold: which flicks bounced.
    pub committed: bool,
}

/// Ring cap — 64 gestures is a full sampling session (the deferred
/// verification asks for 10× per gesture, four gestures) with room to
/// spare, at ~100 bytes an entry.
const GESTURE_RING_CAP: usize = 64;

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
    session: RefCell<Option<Session>>,
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
    /// Lazy-populated icon surface cache, keyed by **app id and pixel size**.
    /// A `None` value means we tried to load the icon and it failed — we cache
    /// the miss so we don't retry on every frame.
    ///
    /// The size is part of the key because consumers ask for different ones:
    /// the dock row loads at its cell width, the workspace montage at
    /// `ICON_SIZE`, and each astro planet at its own globe icon size (which
    /// scales with the planet). Keyed by id alone, whichever consumer drew
    /// first would pin the resolution for every other one.
    icon_cache: RefCell<HashMap<(String, i32), Option<cairo::ImageSurface>>>,
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
    /// Astro-view planets — the launchable roster grouped by category, ordered
    /// most-launched-first within each. Set once at startup by
    /// `panel::load_apps_into`.
    planets: RefCell<Vec<Planet>>,
    /// Astro sizing / seed / ring-budget knobs.
    astro: Astro,
    /// The laid-out sky, cached against the rect it was solved for. Layout is
    /// deterministic, so this is pure memoisation — it exists because the
    /// astro view redraws on a 16ms ticker and re-solving placement (and
    /// re-cloning every globe's app list) per frame would be wasted work on a
    /// Surface Go 3. Dropped whenever the roster changes; re-solved when the
    /// surface resizes.
    sky: RefCell<Option<Rc<astro::Sky>>>,
    /// The ASCII backdrop behind the planets, cached against its rect on the
    /// same terms as `sky`. It also memoises a baked glyph layer internally,
    /// so dropping this entry is what forces a re-bake on a surface resize.
    backdrop: RefCell<Option<Rc<backdrop::Backdrop>>>,
    /// The astro view's ambient animation, live only while the view is on
    /// screen. `Some` ⇔ a wakeup is armed.
    astro_animation: RefCell<Option<glib::SourceId>>,
    /// Whether the previous ambient step had a shooting star in flight, so
    /// the step after one lands can redraw once to clear it.
    astro_was_animating: Cell<bool>,
    /// Start of the backdrop's own monotonic clock. Taken once at
    /// construction rather than at each open, so the sky keeps running while
    /// the launcher is closed and a meteor doesn't fire at the same beat
    /// after every swipe-up.
    created_at: Instant,
    /// Bounded ring of completed gestures ([`GestureSample`]) — the slice
    /// 003 instrumentation, surfaced through `get-state`.
    gesture_ring: RefCell<VecDeque<GestureSample>>,
}

impl Drag {
    pub fn new(
        geom: Geometry,
        anim: Animation,
        terminal: Terminal,
        behaviour: Behaviour,
        astro: Astro,
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
            session: RefCell::new(None),
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
            planets: RefCell::new(Vec::new()),
            astro,
            sky: RefCell::new(None),
            backdrop: RefCell::new(None),
            astro_animation: RefCell::new(None),
            astro_was_animating: Cell::new(false),
            created_at: Instant::now(),
            gesture_ring: RefCell::new(VecDeque::new()),
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

    pub fn set_planets(&self, planets: Vec<Planet>) {
        *self.planets.borrow_mut() = planets;
        *self.sky.borrow_mut() = None;
        self.area.queue_draw();
    }

    /// Seconds on the backdrop's ambient clock. Monotonic and process-wide —
    /// see `created_at`.
    pub fn astro_clock(&self) -> f64 {
        self.created_at.elapsed().as_secs_f64()
    }

    /// The backdrop generated for `rect`, built on first use and on any change
    /// of rect. Same memoisation contract as [`Self::sky`]: generation is
    /// deterministic, so this is pure caching — but it also holds the baked
    /// glyph layer, which is what the 16ms redraw actually depends on.
    pub fn backdrop(&self, rect: astro::SkyRect) -> Rc<backdrop::Backdrop> {
        let cached = self.backdrop.borrow().clone();
        if let Some(b) = cached.filter(|b| b.rect == rect) {
            return b;
        }
        let built = Rc::new(backdrop::Backdrop::build(&self.astro, rect));
        *self.backdrop.borrow_mut() = Some(Rc::clone(&built));
        built
    }

    /// Run the ambient animation while the astro view is on screen, and only
    /// then. Started and stopped from `set_visual`, so it follows the view
    /// through drags, snaps and workspace switches alike rather than needing
    /// a hook at each entry point.
    ///
    /// The tick redraws only when the backdrop has something in flight; an
    /// open-but-idle astro view issues no `queue_draw` at all.
    fn start_astro_animation(self: &Rc<Self>) {
        if self.astro_animation.borrow().is_some() {
            return;
        }
        self.astro_was_animating.set(false);
        self.arm_astro_frame(Duration::from_millis(ASTRO_FRAME_MS));
    }

    /// Arm the next ambient wakeup. The cadence is the *result* of each step:
    /// frame rate while a shooting star crosses, a sleep to the next one when
    /// the sky is still, and nothing at all when shooting stars are off.
    ///
    /// The source is only replaced when the cadence changes — a whole meteor
    /// runs on one repeating source, so re-arming is a per-transition cost,
    /// not a per-frame one.
    fn arm_astro_frame(self: &Rc<Self>, delay: Duration) {
        let this = Rc::clone(self);
        let delay_ms = delay.as_millis();
        let id = glib::timeout_add_local(delay, move || match this.astro_frame() {
            Some(next) if next.as_millis() == delay_ms => glib::ControlFlow::Continue,
            Some(next) => {
                // Stores the newly armed source id over this one *before* this
                // source is dropped, so what's stored is always live and
                // `stop_astro_animation` never removes a dead id.
                this.arm_astro_frame(next);
                glib::ControlFlow::Break
            }
            None => {
                this.astro_animation.borrow_mut().take();
                glib::ControlFlow::Break
            }
        });
        *self.astro_animation.borrow_mut() = Some(id);
    }

    /// One ambient step: redraw if the backdrop has — or just had — something
    /// moving, and report how long until it is worth looking again. `None`
    /// retires the animation entirely.
    fn astro_frame(&self) -> Option<Duration> {
        let Some(backdrop) = self.backdrop.borrow().clone() else {
            // The view hasn't drawn its first frame, so there is no backdrop
            // to ask yet. Come back at frame rate; the first draw builds it.
            return Some(Duration::from_millis(ASTRO_FRAME_MS));
        };
        let t = self.astro_clock();
        let animating = backdrop.is_animating(t);
        // The trailing frame after `astro_was_animating` is what *clears* the
        // last streak. Without it the meteor's final position stays painted
        // until something else happens to redraw the dock.
        if animating || self.astro_was_animating.get() {
            self.area.queue_draw();
        }
        self.astro_was_animating.set(animating);

        let wait_s = backdrop.seconds_until_next(t)?;
        let ms = (wait_s * 1000.0).clamp(0.0, ASTRO_IDLE_SLEEP_MAX_MS as f64) as u64;
        Some(Duration::from_millis(ms.max(ASTRO_FRAME_MS)))
    }

    fn stop_astro_animation(&self) {
        if let Some(id) = self.astro_animation.borrow_mut().take() {
            id.remove();
        }
    }

    /// The sky laid out for `rect`, solving it on first use and on any change
    /// of rect. Shared by the renderer, the tap handler and `get-state`, so
    /// all three read one layout rather than three that could drift.
    ///
    /// This is the accessor slice 4 needed — not the bare `planets()` the
    /// slice-3 handoff predicted. Layout lives behind this method so the
    /// roster and the geometry can't be read apart and re-solved twice; a
    /// public `planets()` would have no caller.
    pub fn sky(&self, rect: astro::SkyRect) -> Rc<astro::Sky> {
        let cached = self.sky.borrow().clone();
        if let Some(sky) = cached.filter(|s| s.rect == rect) {
            return sky;
        }
        let built = Rc::new(astro::build_sky(
            &self.planets.borrow(),
            &self.astro,
            rect,
        ));
        *self.sky.borrow_mut() = Some(Rc::clone(&built));
        built
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
        let key = (id.to_owned(), size);
        if let Some(cached) = self.icon_cache.borrow().get(&key) {
            return cached.clone();
        }
        let surface = icon_spec.and_then(|s| apps::load_icon_surface(s, size));
        self.icon_cache.borrow_mut().insert(key, surface.clone());
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
        let key = (id.to_owned(), size);
        if let Some(cached) = self.icon_cache.borrow().get(&key) {
            return cached.clone();
        }
        let spec = self
            .app_registry
            .borrow()
            .get(id)
            .and_then(|e| e.icon.clone());
        let surface = spec.as_deref().and_then(|s| apps::load_icon_surface(s, size));
        self.icon_cache.borrow_mut().insert(key, surface.clone());
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
        // Live surface dimensions — the compositor-assigned size, which is the
        // single source of truth for all vertical geometry (astro sizes the
        // surface `screen_h - D` tall). Exposed so geometry can be verified
        // without a throwaway probe, as
        // `incident-20260801-app-grid-invisible-stale-surface-height` needed.
        let (sw, sh) = self
            .window
            .surface()
            .map(|s| (s.width(), s.height()))
            .unwrap_or((0, 0));
        // Planet roster, in render order. Same rationale as `surface` above:
        // screencopy is unavailable in this compositor session, so the astro
        // view's inputs have to be verifiable over IPC.
        let planets: Vec<String> = self
            .planets
            .borrow()
            .iter()
            .map(|p| {
                format!(
                    r#"{{"key":"{}","name":"{}","apps":{}}}"#,
                    json_escape(&p.key),
                    json_escape(&p.display_name),
                    p.apps.len()
                )
            })
            .collect();
        // Gesture sample ring (slice 003 instrumentation) — the fling-tune
        // numbers, read back with `nc` after a sampling session.
        let gestures: Vec<String> = self
            .gesture_ring
            .borrow()
            .iter()
            .map(|g| {
                format!(
                    r#"{{"zone":"{}","dir":"{}","source":"{}","p":{:.3},"v":{:.3},"ts_ms":{},"committed":{}}}"#,
                    json_escape(&g.zone),
                    direction_str(g.direction),
                    g.source.as_str(),
                    g.progress,
                    g.velocity,
                    g.ts_ms,
                    g.committed,
                )
            })
            .collect();
        format!(
            r#"{{"state":"{}","visual":{{"hidden":{:.3},"detailed":{:.3},"grid":{:.3}}},"surface":{{"w":{},"h":{}}},"workspace":{{"x":{},"y":{}}},"map":[{}],"gestures":[{}],"planets":[{}],"sky":{}}}"#,
            self.state.get().as_str(),
            v.hidden_amount,
            v.detailed_amount,
            v.grid_amount,
            sw,
            sh,
            ws.x,
            ws.y,
            entries.join(","),
            gestures.join(","),
            planets.join(","),
            self.dump_sky(),
        )
    }

    /// The solved astro layout, or `{"built":false}` before the view has been
    /// drawn once (the sky is solved lazily, against the real surface). This
    /// is the only way to check planet placement on this device: screencopy
    /// fails in this compositor session, so open the astro view over IPC
    /// (`drag-begin up-grid` → `drag 1` → `drag-end 1 1`) and read it back
    /// here.
    fn dump_sky(&self) -> String {
        let Some(sky) = self.sky.borrow().clone() else {
            return r#"{"built":false}"#.to_owned();
        };
        let bodies: Vec<String> = sky
            .planets
            .iter()
            .map(|p| {
                format!(
                    r#"{{"key":"{}","x":{:.1},"y":{:.1},"r":{:.1},"icon":{:.1},"rings":{},"front":{},"back":{}}}"#,
                    json_escape(&p.key),
                    p.cx,
                    p.cy,
                    p.radius,
                    p.icon_px,
                    p.front_rings,
                    p.placed_icons(0.0).len(),
                    p.back_count(0.0),
                )
            })
            .collect();
        format!(
            r#"{{"built":true,"rect":{{"x":{:.1},"y":{:.1},"w":{:.1},"h":{:.1}}},"bodies":[{}]}}"#,
            sky.rect.x,
            sky.rect.y,
            sky.rect.w,
            sky.rect.h,
            bodies.join(",")
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

    /// Set the render parameters and keep the ambient animation in step with
    /// them. Every path that changes the view — drag, snap, jump, settle —
    /// goes through here, so the backdrop's ticker starts and stops in one
    /// place instead of being hooked onto each of them.
    fn set_visual(self: &Rc<Self>, v: Visual) {
        self.visual.set(v);
        if v.grid_amount > 0.001 {
            self.start_astro_animation();
        } else {
            self.stop_astro_animation();
        }
        self.area.queue_draw();
    }

    fn save_current_workspace(&self) {
        let ws = self.current_ws.get();
        let s = self.state.get();
        self.workspaces.borrow_mut().insert(ws, s);
    }

    fn settle(self: &Rc<Self>, target: BarState) {
        self.state.set(target);
        self.set_visual(target.into());
        self.save_current_workspace();
        self.apply_visibility();
        update_zone_states(target, &self.geom);
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
        // `win_h` is the real surface height (astro sizes it `screen_h - D`).
        // The bar is bottom-anchored, so the input region is measured up from
        // the bottom edge — never from a `margin+default+detailed(+grid)`
        // constant, which pinned the region to the top of the surface and left
        // the visible bar unclickable
        // (`incident-20260801-app-grid-invisible-stale-surface-height`).
        let bar_top = win_h - g.margin_bottom_px - g.default_height_px;

        let extend_up = if self.input_should_be_expanded() {
            if self.grid_expand_needed() {
                // AppGrid: the astro rect fills the whole surface above the
                // bar (screen `[D, h-D]`); cover it entirely. `bar_top` clamps
                // the region's top to the surface top (y=0).
                bar_top.max(0)
            } else {
                g.detailed_content_height_px
            }
        } else {
            0
        };

        let (x, y, bar_w, h) = input_region_rect(win_w, win_h, g, extend_up);

        let rect = cairo::RectangleInt::new(x, y, bar_w, h);
        let region = cairo::Region::create_rectangle(&rect);
        surface.set_input_region(&region);
    }

    /// Input region grows whenever any part of the detailed view is
    /// visible — including transitions toward / away from Detailed — so
    /// clicks land while the gap is animating, not just after settle.
    fn input_should_be_expanded(&self) -> bool {
        if let Some(s) = self.session.borrow().as_ref() {
            return matches!(s.source, BarState::Detailed | BarState::AppGrid)
                || matches!(s.target, BarState::Detailed | BarState::AppGrid);
        }
        matches!(self.state.get(), BarState::Detailed | BarState::AppGrid)
    }

    fn grid_expand_needed(&self) -> bool {
        if let Some(s) = self.session.borrow().as_ref() {
            return matches!(s.source, BarState::AppGrid)
                || matches!(s.target, BarState::AppGrid);
        }
        matches!(self.state.get(), BarState::AppGrid)
    }

    #[allow(dead_code)] // Phase 3+: instant-set without animation
    pub fn jump_to(self: &Rc<Self>, target: BarState) {
        self.cancel_animation();
        *self.session.borrow_mut() = None;
        self.ensure_mapped();
        self.state.set(target);
        self.set_visual(target.into());
        self.save_current_workspace();
        self.apply_visibility();
        update_zone_states(target, &self.geom);
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
        let counts = launches::load();
        *self.launch_counts.borrow_mut() = counts;
    }

    pub fn launch_counts(&self) -> HashMap<String, u64> {
        self.launch_counts.borrow().clone()
    }

    pub fn increment_launch_count(&self, id: &str) {
        {
            let mut counts = self.launch_counts.borrow_mut();
            *counts.entry(id.to_owned()).or_insert(0) += 1;
        }
        if let Err(e) = launches::save(&self.launch_counts.borrow()) {
            eprintln!("sliver-dock: failed to save launches: {e}");
        }
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
            if let Some(restore) = saved
                && current == BarState::Hidden {
                    self.workspaces.borrow_mut().insert(ws, restore);
                    if ws == current_ws {
                        self.snap_to_state(restore);
                    }
                }
        }

        *self.fullscreen_workspaces.borrow_mut() = new_set;
    }

    pub fn snap_to_state(self: &Rc<Self>, target: BarState) {
        let from = self.visual.get();
        let to = Visual::from(target);
        self.cancel_animation();
        *self.session.borrow_mut() = None;
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

    pub fn begin(self: &Rc<Self>, direction: Direction, zone: Option<String>) {
        let source = self.state.get();
        let Some(target) = begin_target(source, direction) else {
            return;
        };
        self.cancel_animation();
        *self.session.borrow_mut() = Some(Session {
            source,
            target,
            direction,
            zone,
        });
        self.ensure_mapped();
        self.set_visual(Visual::from(source));
        self.refresh_input_region();
    }

    pub fn update(self: &Rc<Self>, progress: f64) {
        let Some((direction, source, target)) = self
            .session
            .borrow()
            .as_ref()
            .map(|s| (s.direction, s.source, s.target))
        else {
            return;
        };
        let p = forward_progress(direction, progress);
        let from = Visual::from(source);
        let to = Visual::from(target);
        let v = Visual {
            hidden_amount: lerp(from.hidden_amount, to.hidden_amount, p),
            detailed_amount: lerp(from.detailed_amount, to.detailed_amount, p),
            grid_amount: lerp(from.grid_amount, to.grid_amount, p),
        };
        self.set_visual(v);
    }

    pub fn end(self: &Rc<Self>, progress: f64, velocity_px_per_ms: f64) {
        let (zone, direction, source, target) = match self.session.borrow().as_ref() {
            Some(s) => (s.zone.clone(), s.direction, s.source, s.target),
            None => return,
        };
        let p = forward_progress(direction, progress);
        let v_forward = forward_velocity(direction, velocity_px_per_ms);

        // Record the sample FIRST — it describes the gesture itself, not
        // the state the snap is about to move us into. The zone defaults
        // to "-" for pre-token plugins; the human's `buffering zone=`
        // journal line still names it in that case.
        let sample = GestureSample {
            zone: zone.unwrap_or_else(|| "-".to_owned()),
            direction,
            source,
            progress: p,
            velocity: velocity_px_per_ms,
            ts_ms: self.created_at.elapsed().as_millis() as u64,
            committed: commits_to_target(p, v_forward, self.anim.fling_threshold_px_per_ms),
        };
        {
            let mut ring = self.gesture_ring.borrow_mut();
            if ring.len() >= GESTURE_RING_CAP {
                ring.pop_front();
            }
            ring.push_back(sample);
        }

        let target = if commits_to_target(p, v_forward, self.anim.fling_threshold_px_per_ms) {
            target
        } else {
            source
        };
        self.snap_to_state(target);
    }
}

/// The snap-decision tier, extracted pure (slice 003) so the flick cases at
/// low progress are unit-testable without a GTK window:
///
///   1. fling — `|v_forward| > fling_threshold_px_per_ms`: commit with the
///      velocity sign. A quick flick commits regardless of how far the
///      finger travelled; this tier is the zone-independent mechanism the
///      "quick swipe" requirement (locked 2026-09-01) rides on.
///   2. sub-fling motion — biased midpoint tier toward the direction of
///      motion (±0.05 velocity bias).
///   3. stationary — neutral 0.5 midpoint.
///
/// Returns `true` when the drag commits to the target, `false` when it
/// bounces back to the source.
fn commits_to_target(p: f64, v_forward: f64, fling_threshold_px_per_ms: f64) -> bool {
    if v_forward.abs() > fling_threshold_px_per_ms {
        v_forward > 0.0
    } else {
        const DIRECTION_BIAS: f64 = 0.05;
        let snap_threshold = if v_forward > DIRECTION_BIAS {
            0.2
        } else if v_forward < -DIRECTION_BIAS {
            0.8
        } else {
            0.5
        };
        p > snap_threshold
    }
}

/// Pure input-region rectangle for a given surface size, geometry and
/// upward expansion — the shared derivation behind `Drag::refresh_input_region`
/// and the zone/input agreement test, so the two can't drift apart.
///
/// The bar is bottom-anchored; the region is measured up from the bottom edge
/// and widened by `extend_up` (the detailed content height, or everything up
/// to the surface top in AppGrid). Same "real surface height is the source of
/// truth" rule as the draw path.
pub(crate) fn input_region_rect(win_w: i32, win_h: i32, g: &Geometry, extend_up: i32) -> (i32, i32, i32, i32) {
    let bar_w = g.width_px.min(win_w);
    let bar_top = win_h - g.margin_bottom_px - g.default_height_px;
    let x = (win_w - bar_w) / 2;
    let y = (bar_top - extend_up).max(0);
    let h = win_h - y;
    (x, y, bar_w, h)
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

/// The `(source state, gesture direction) → target state` routing table.
/// Pure so the whole gesture contract is unit-testable; `begin` resolves
/// through it and starts no session when it returns `None`.
///
/// Slice 002 removals, locked with the user 2026-09-01:
///   * `(Closed, UpGrid) → AppGrid` — gone. Closed-up unambiguously means
///     Detailed; reaching astro from Closed costs one extra swipe (up →
///     Detailed, up again from the apps section or astro bar).
///   * `(Detailed, Up) → AppGrid` — gone. No zone sends a plain `up` over
///     Detailed any more: the broad detailed zone is unidir-down (up never
///     commits) and the two astro-opening zones both send `up-grid`. Astro
///     opens from the apps section or the astro sliver bar only.
fn begin_target(source: BarState, direction: Direction) -> Option<BarState> {
    match (source, direction) {
        (BarState::Closed, Direction::Up) => Some(BarState::Detailed),
        (BarState::Closed, Direction::Down) => Some(BarState::Hidden),
        (BarState::Detailed, Direction::Down) => Some(BarState::Closed),
        (BarState::Detailed, Direction::UpGrid) => Some(BarState::AppGrid),
        (BarState::Hidden, Direction::Up) => Some(BarState::Closed),
        (BarState::AppGrid, Direction::Down) => Some(BarState::Detailed),
        _ => None,
    }
}

/// The intended enabled-zone set per steady state — the single source of
/// truth. (There used to be two hand-written copies of this logic, one here
/// and one in `panel.rs`'s map callback; they drifted — both hardcoded the
/// detailed zone to `false` — and the drift is exactly what killed the
/// Detailed swipe-down gesture. The map callback now calls
/// [`update_zone_states`] instead of re-deriving the set.)
///
///   * Closed   → bar zone (up opens Detailed, down hides) + sliver-edge
///     (down hides — unidir, see [`panel::sync_sliver_edge_zone`])
///   * Hidden   → bottom-edge zone (up unhides)
///   * Detailed → sliver-edge (up-grid opens AppGrid from the astro bar,
///     down closes) + apps zone (up-grid opens AppGrid from the apps
///     section, down closes) + broad detailed zone (unidir down closes)
///   * AppGrid  → sliver-edge (unidir down closes) + detailed zone extended
///     over the astro body (unidir down returns to Detailed)
fn intended_zones(state: BarState) -> [(&'static str, bool); 5] {
    [
        (BAR_ZONE_ID, matches!(state, BarState::Closed)),
        (BOTTOM_EDGE_ZONE_ID, matches!(state, BarState::Hidden)),
        (
            DETAILED_ZONE_ID,
            matches!(state, BarState::Detailed | BarState::AppGrid),
        ),
        (
            SLIVER_EDGE_ZONE_ID,
            matches!(state, BarState::Closed | BarState::Detailed | BarState::AppGrid),
        ),
        (APPS_ZONE_ID, matches!(state, BarState::Detailed)),
    ]
}

/// Single-zone lookup over [`intended_zones`] — for registration-time and
/// test callers that want one flag instead of the whole set.
pub(crate) fn zone_enabled(state: BarState, zone: &str) -> bool {
    intended_zones(state)
        .into_iter()
        .find(|(id, _)| *id == zone)
        .map(|(_, enabled)| enabled)
        .unwrap_or(false)
}

/// Flip touch-drawer zones based on the steady state. At most the intended
/// set per state is enabled — see [`intended_zones`] for the table.
///
/// The state-dependent zones (detailed + sliver-edge) re-register their
/// bounds on every state settle: the sliver-edge rides the detailed lift,
/// and the detailed zone covers the astro body in AppGrid. Without this
/// the swipe zones stayed pinned to the Closed geometry
/// (`update_zone_bounds` only fires on map + monitor-change). The
/// sliver-edge's *arms* are state-dependent too — Closed/AppGrid keep only
/// the down arm, Detailed carries the up-grid arm — so its registration is
/// re-sent when the arm signature changes (see
/// [`panel::sync_sliver_edge_zone`]).
pub(crate) fn update_zone_states(state: BarState, geom: &Geometry) {
    for (id, enabled) in intended_zones(state) {
        zones::set_enabled(id, enabled);
    }
    if let Some((sw, sh)) = crate::panel::current_screen_size() {
        let (x, y, w, h) = crate::panel::detailed_zone_bounds(sw, sh, geom, state);
        zones::update_bounds(DETAILED_ZONE_ID, x, y, w, h);
        let (x, y, w, h) = crate::panel::sliver_edge_zone_bounds(sw, sh, geom, state);
        zones::update_bounds(SLIVER_EDGE_ZONE_ID, x, y, w, h);
        let (x, y, w, h) = crate::panel::apps_zone_bounds(sw, sh, geom);
        zones::update_bounds(APPS_ZONE_ID, x, y, w, h);
        crate::panel::sync_sliver_edge_zone(sw, sh, geom, state);
    }
}

/// Minimal JSON string escaping for `dump_state`. Category keys and planet
/// display names come from `.desktop` files and `categories.toml`, so they
/// can carry quotes, backslashes or control characters that would otherwise
/// produce unparseable `get-state` output.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The touch-drawer begin-arg spelling of a direction — the `dir=` field
/// in the gesture ring matches the plugin's `drag-begin` line.
fn direction_str(d: Direction) -> &'static str {
    match d {
        Direction::Up => "up",
        Direction::Down => "down",
        Direction::UpGrid => "up-grid",
    }
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

    // ---- slice 001: per-state enabled-zone table ----------------------

    #[test]
    fn intended_zones_have_no_dead_registered_zone() {
        // Every zone id we register must be enabled in at least one state —
        // the slice-001 regression the two hardcoded `false` calls caused
        // (the detailed zone was registered and then permanently dead).
        let ids = [
            BAR_ZONE_ID,
            BOTTOM_EDGE_ZONE_ID,
            DETAILED_ZONE_ID,
            SLIVER_EDGE_ZONE_ID,
            APPS_ZONE_ID,
        ];
        let states = [
            BarState::Closed,
            BarState::Hidden,
            BarState::Detailed,
            BarState::AppGrid,
        ];
        for id in ids {
            assert!(
                states.iter().any(|&s| zone_enabled(s, id)),
                "{id} is registered but never enabled in any state"
            );
        }
    }

    #[test]
    fn closed_enables_only_bar_and_sliver_edge() {
        let s = BarState::Closed;
        assert!(zone_enabled(s, BAR_ZONE_ID));
        assert!(zone_enabled(s, SLIVER_EDGE_ZONE_ID), "down-only arm in Closed");
        assert!(!zone_enabled(s, BOTTOM_EDGE_ZONE_ID));
        assert!(!zone_enabled(s, DETAILED_ZONE_ID));
        assert!(!zone_enabled(s, APPS_ZONE_ID));
    }

    #[test]
    fn hidden_enables_only_bottom_edge() {
        let s = BarState::Hidden;
        assert!(zone_enabled(s, BOTTOM_EDGE_ZONE_ID));
        assert!(!zone_enabled(s, BAR_ZONE_ID));
        assert!(!zone_enabled(s, DETAILED_ZONE_ID));
        assert!(!zone_enabled(s, SLIVER_EDGE_ZONE_ID));
        assert!(!zone_enabled(s, APPS_ZONE_ID));
    }

    #[test]
    fn detailed_enables_detailed_sliver_edge_and_apps() {
        let s = BarState::Detailed;
        assert!(zone_enabled(s, DETAILED_ZONE_ID), "the revived zone, down-only");
        assert!(zone_enabled(s, SLIVER_EDGE_ZONE_ID), "astro bar up-grid arm");
        assert!(zone_enabled(s, APPS_ZONE_ID), "apps section up-grid arm");
        assert!(!zone_enabled(s, BAR_ZONE_ID));
        assert!(!zone_enabled(s, BOTTOM_EDGE_ZONE_ID));
    }

    #[test]
    fn app_grid_enables_detailed_and_sliver_edge() {
        let s = BarState::AppGrid;
        assert!(zone_enabled(s, DETAILED_ZONE_ID), "down-close zone");
        assert!(zone_enabled(s, SLIVER_EDGE_ZONE_ID), "down-only close handle");
        assert!(!zone_enabled(s, BAR_ZONE_ID));
        assert!(!zone_enabled(s, BOTTOM_EDGE_ZONE_ID));
        assert!(!zone_enabled(s, APPS_ZONE_ID), "no astro-opening zone in AppGrid");
    }

    #[test]
    fn unknown_zone_id_is_never_enabled() {
        assert!(!zone_enabled(BarState::Detailed, "sliver-dock.nope"));
    }

    // ---- slice 002: gesture routing table -----------------------------

    #[test]
    fn begin_target_closed_up_opens_detailed_never_grid() {
        assert_eq!(begin_target(BarState::Closed, Direction::Up), Some(BarState::Detailed));
        // The removed coin-flip arm: Closed-up-grid must not reach AppGrid.
        assert_eq!(begin_target(BarState::Closed, Direction::UpGrid), None);
    }

    #[test]
    fn begin_target_down_steps_one_state_back() {
        assert_eq!(begin_target(BarState::Closed, Direction::Down), Some(BarState::Hidden));
        assert_eq!(begin_target(BarState::Detailed, Direction::Down), Some(BarState::Closed));
        assert_eq!(begin_target(BarState::AppGrid, Direction::Down), Some(BarState::Detailed));
    }

    #[test]
    fn begin_target_astro_opens_from_detailed_only() {
        // Astro opens from Detailed exclusively, via the two up-grid zones
        // (apps section + astro sliver bar). No other arm reaches AppGrid.
        assert_eq!(
            begin_target(BarState::Detailed, Direction::UpGrid),
            Some(BarState::AppGrid)
        );
        // The plain-up arm was removed: nothing sends `up` over Detailed any
        // more, so a stray `up` must not open AppGrid from a random section.
        assert_eq!(begin_target(BarState::Detailed, Direction::Up), None);
        assert_eq!(begin_target(BarState::AppGrid, Direction::UpGrid), None);
        assert_eq!(begin_target(BarState::Hidden, Direction::UpGrid), None);
        assert_eq!(begin_target(BarState::Hidden, Direction::Down), None);
    }

    #[test]
    fn begin_target_hidden_up_unhides_to_closed() {
        assert_eq!(begin_target(BarState::Hidden, Direction::Up), Some(BarState::Closed));
    }

    #[test]
    fn input_region_rect_matches_the_bar_for_closed() {
        let g = Geometry::default();
        let (win_w, win_h) = (1280, 746);
        let (x, y, w, h) = input_region_rect(win_w, win_h, &g, 0);
        assert_eq!(w, g.width_px.min(win_w));
        // Closed: the region is just the bar + margin band at the bottom.
        assert_eq!(h, g.margin_bottom_px + g.default_height_px);
        assert_eq!(x, (win_w - w) / 2);
        assert_eq!(y, win_h - h);
    }

    #[test]
    fn input_region_rect_expands_up_for_detailed() {
        let g = Geometry::default();
        let (win_w, win_h) = (1280, 746);
        let (x, y, w, h) = input_region_rect(win_w, win_h, &g, g.detailed_content_height_px);
        assert_eq!(w, g.width_px.min(win_w));
        // Detailed: the region grows by the detailed content height, up from
        // the bar + margin band.
        assert_eq!(
            h,
            g.margin_bottom_px + g.default_height_px + g.detailed_content_height_px
        );
        assert_eq!(y, win_h - h);
        assert_eq!(x, (win_w - w) / 2);
    }

    // ---- slice 003: quick-flick decisiveness ---------------------------

    const FLING: f64 = 0.6; // compiled default of fling_threshold_px_per_ms

    #[test]
    fn quick_flick_forward_commits_at_low_progress() {
        // The locked requirement: a quick flick commits regardless of how
        // far the finger travelled. v_forward above the fling threshold
        // must win the tier even at p ≈ 0.1 (the tall detailed zone's
        // progress at a 10px commit).
        assert!(commits_to_target(0.1, 2.0, FLING));
        assert!(commits_to_target(0.0, 2.0, FLING));
    }

    #[test]
    fn quick_flick_backward_bounces_even_near_target() {
        // Same velocity, wrong direction: the velocity sign decides, so a
        // fast flick away from the target never commits.
        assert!(!commits_to_target(0.95, -2.0, FLING));
    }

    #[test]
    fn sub_fling_forward_bias_commits_from_low_p() {
        // Slow-ish drag still moving toward the target: the ±0.05 bias
        // drops the threshold to 0.2.
        assert!(commits_to_target(0.21, 0.1, FLING));
        assert!(!commits_to_target(0.19, 0.1, FLING));
        // Bias boundary: exactly at the ±0.05 bias is NOT biased — the
        // stationary 0.5 midpoint applies.
        assert!(!commits_to_target(0.5, 0.05, FLING));
        assert!(!commits_to_target(0.5, -0.05, FLING));
        assert!(commits_to_target(0.51, 0.05, FLING));
    }

    #[test]
    fn sub_fling_backward_bias_raises_threshold() {
        // Slow drag moving away from the target must nearly arrive to
        // commit — threshold raised to 0.8.
        assert!(commits_to_target(0.81, -0.1, FLING));
        assert!(!commits_to_target(0.79, -0.1, FLING));
    }

    #[test]
    fn stationary_midpoint_is_neutral() {
        assert!(commits_to_target(0.51, 0.0, FLING));
        assert!(!commits_to_target(0.49, 0.0, FLING));
    }

    #[test]
    fn fling_tier_is_strictly_above_threshold() {
        // Exactly at the threshold is NOT a fling — |v| > threshold, so a
        // flick at exactly 0.6 px/ms falls to the p tiers; a hair past it
        // takes the fling path.
        assert!(!commits_to_target(0.05, 0.6, FLING));
        assert!(commits_to_target(0.05, 0.6001, FLING));
        // The negative mirror, with p above the backward-biased 0.8 tier:
        // exactly at -0.6 the tier commits, a hair past it the fling sign
        // wins outright and bounces.
        assert!(commits_to_target(0.95, -0.6, FLING));
        assert!(!commits_to_target(0.95, -0.6001, FLING));
    }

    #[test]
    fn every_routing_arm_maps_gesture_velocity_to_positive_forward() {
        // Slice step 4: re-verify every (state, direction) arm against the
        // sign mapping. For each arm a velocity in the gesture's own
        // physical direction (down = +1 px/ms, up = -1 px/ms) must map to
        // POSITIVE forward velocity — the fling tier commits to target on
        // positive v_forward, so a sign error here inverts the fling and
        // presents exactly as "inconsistent".
        let arms: [(BarState, Direction, BarState); 6] = [
            (BarState::Closed, Direction::Up, BarState::Detailed),
            (BarState::Closed, Direction::Down, BarState::Hidden),
            (BarState::Detailed, Direction::Down, BarState::Closed),
            (BarState::Detailed, Direction::UpGrid, BarState::AppGrid),
            (BarState::Hidden, Direction::Up, BarState::Closed),
            (BarState::AppGrid, Direction::Down, BarState::Detailed),
        ];
        for (source, dir, target) in arms {
            assert_eq!(begin_target(source, dir), Some(target));
            let gesture_v = match dir {
                Direction::Down => 1.0,
                Direction::Up | Direction::UpGrid => -1.0,
            };
            let forward = forward_velocity(dir, gesture_v);
            assert!(
                forward > 0.0,
                "{source:?} {dir:?}: forward velocity {forward} not positive"
            );
        }
    }

    #[test]
    fn every_routing_arm_commits_on_a_full_forward_fling() {
        // The flip side of the sign sweep: with the velocity sign mapping
        // correct, a fling in the gesture direction must commit from ANY
        // progress on every arm — this is "quick flick commits regardless
        // of distance" stated per arm, including the astro-open arms.
        let arms: [(BarState, Direction, BarState); 6] = [
            (BarState::Closed, Direction::Up, BarState::Detailed),
            (BarState::Closed, Direction::Down, BarState::Hidden),
            (BarState::Detailed, Direction::Down, BarState::Closed),
            (BarState::Detailed, Direction::UpGrid, BarState::AppGrid),
            (BarState::Hidden, Direction::Up, BarState::Closed),
            (BarState::AppGrid, Direction::Down, BarState::Detailed),
        ];
        for (source, dir, _target) in arms {
            let gesture_v = match dir {
                Direction::Down => 2.0,
                Direction::Up | Direction::UpGrid => -2.0,
            };
            let forward = forward_velocity(dir, gesture_v);
            assert!(
                commits_to_target(0.05, forward, FLING),
                "{source:?} {dir:?}: fling did not commit at low p"
            );
        }
    }
}
