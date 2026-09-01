//! Layer-shell window + spear-tapered render + main loop wiring.
//!
//! Phase 1: bar drawn at the right geometry.
//! Phase 2: drag/IPC/zones layered on top — bar animates between Closed,
//! Hidden, and Detailed steady states. The Detailed render here is a
//! placeholder hairline (Phase 3 owns the real split + sections).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::rc::Rc;

use gtk4::cairo::Context as CairoContext;
use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{
    glib, Application, ApplicationWindow, Calendar, CssProvider, DrawingArea, GestureClick,
    Label, Popover,
};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

use crate::apps::{self, AppEntry};
use crate::astro;
use crate::battery::{self, BatteryInfo, BatteryState};
use crate::categories;
use crate::config::{self, Animation, Geometry};
use crate::cpu::{self, CpuInfo};
use crate::drag::{self, Direction, Drag, Visual};
use crate::ipc;
use crate::state::{BarState, WorkspaceId};
use crate::wayfire::{self, GridSize, MontageView, ViewRect};
use crate::zones;

const APP_ID: &str = "dev.unkownvee.SliverDock";

const BOTTOM_EDGE_BAND_HEIGHT: i32 = 30;

/// Width of the rightmost (info) section. Hardcoded both in `draw_sections`
/// (for paint) and `section_bounds` (for click hit-test); centralize so a
/// future bump only changes one number.
const INFO_SECTION_W: f64 = 72.0;

/// Radius and edge-pad of the bottom-row round icons (battery left, CPU right)
/// inside the info section. Shared by the draw routines and the click
/// hit-rect helpers so a future tweak only changes one number.
const INFO_DISC_RADIUS: f64 = 11.0;
const INFO_DISC_PAD: f64 = 6.0;

pub fn run_daemon() -> ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(on_activate);

    let exit_status = app.run_with_args::<&str>(&[]);
    if exit_status.value() != 0 {
        ExitCode::from(exit_status.value() as u8)
    } else {
        ExitCode::SUCCESS
    }
}

fn on_activate(app: &Application) {
    load_css();
    let cfg = Rc::new(config::load());
    let (window, area) = build_window(app, &cfg);

    let drag = Drag::new(
        cfg.geometry,
        cfg.animation,
        cfg.terminal.clone(),
        cfg.behaviour,
        cfg.astro.clone(),
        window.clone(),
        area.clone(),
    );
    // Launch counts first — `load_apps_into` orders each planet's apps by
    // launch frequency, so the counts have to be in place before it runs.
    drag.load_launches();
    load_apps_into(&drag);

    let geom = cfg.geometry;
    let anim = cfg.animation;
    let drag_for_draw = Rc::clone(&drag);
    area.set_draw_func(move |_, ctx, w, h| {
        draw_bar(ctx, w as f64, h as f64, &geom, &anim, &drag_for_draw);
        draw_astro_overlay(ctx, w as f64, h as f64, &geom, &drag_for_draw);
        // The astro sliver is the astro view's top border and paints last, on
        // top of the black backdrop it just pulled out from behind the dock.
        draw_astro_sliver(ctx, w as f64, h as f64, &geom, &drag_for_draw);
    });

    let popovers = build_info_popovers(&area);
    install_section_click_handler(&area, &drag, geom, popovers);

    wayfire::log_status();
    spawn_wayfire_subscriber(&drag);

    register_zones(&drag, &geom);

    let drag_for_map = Rc::clone(&drag);
    let geom_for_map = geom;
    window.connect_map(move |_| {
        let s = drag_for_map.state();
        update_zone_bounds(&geom_for_map, s);
        // Re-derive zone enabled flags now that the surface is mapped —
        // `set_visible(true)` in jump_to / ensure_mapped can race the map.
        // One shared source of truth (`drag::update_zone_states`): the map
        // callback and the settle path must never hand-roll their own copies
        // of the enabled-set again — the drift between two copies is exactly
        // what killed the Detailed swipe-down zone.
        drag::update_zone_states(s, &geom_for_map);
        drag_for_map.refresh_input_region();
    });

    // The input region is measured up from the surface's bottom edge, so it is
    // only valid for the surface height it was computed against. The astro
    // surface is `screen_h - D` tall — an unconstrained one swallows every
    // touch on the desktop below — and its real height arrives with the
    // compositor's configure, after this function returns. Re-derive on every
    // allocation change, the same real-height-is-the-source-of-truth rule the
    // draw path follows.
    let drag_for_resize = Rc::clone(&drag);
    area.connect_resize(move |_, _, _| {
        drag_for_resize.refresh_input_region();
    });

    watch_monitor_changes(Rc::clone(&drag), geom);
    spawn_clock_tick(&area);
    battery::spawn(Rc::clone(&drag));
    cpu::spawn(Rc::clone(&drag));

    let (tx, rx) = async_channel::unbounded::<ipc::Request>();
    if let Err(e) = ipc::serve(tx) {
        eprintln!("sliver-dock: ipc bind failed: {e}");
    }

    let app_quit = app.clone();
    glib::spawn_future_local(async move {
        while let Ok(mut req) = rx.recv().await {
            apply_command(&mut req, &drag, &app_quit);
        }
    });

    // Last, so the map / resize handlers above are live for the first map.
    window.present();
}

/// Spawn the wayfire subscriber thread + a GTK-side consumer that pipes
/// `Event::WorkspaceChanged` into `Drag::set_workspace`. The subscriber is
/// resilient — on disconnect it backs off and reconnects, emitting fresh
/// `Connected` events that re-sync the focused workspace.
fn spawn_wayfire_subscriber(drag: &Rc<Drag>) {
    let (tx, rx) = async_channel::unbounded::<wayfire::Event>();
    wayfire::spawn_subscriber(tx);

    let drag = Rc::clone(drag);
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                wayfire::Event::Connected {
                    workspace,
                    grid,
                    output_id,
                    output_size,
                } => {
                    eprintln!(
                        "sliver-dock: wayfire connected — workspace=({},{}) grid={}x{} output={} size={}x{}",
                        workspace.x, workspace.y, grid.width, grid.height, output_id,
                        output_size.0, output_size.1,
                    );
                    drag.set_grid(grid);
                    drag.set_wayfire_output_id(output_id);
                    drag.set_output_size(output_size);
                    drag.set_workspace(workspace);
                }
                wayfire::Event::WorkspaceChanged {
                    workspace,
                    output_id,
                } => {
                    drag.set_wayfire_output_id(output_id);
                    drag.set_workspace(workspace);
                }
                wayfire::Event::OpenedWorkspacesChanged(set) => {
                    drag.set_opened_workspaces(set);
                }
                wayfire::Event::RunningAppsChanged(set) => {
                    drag.set_running_apps(set);
                }
                wayfire::Event::FullscreenWorkspacesChanged(set) => {
                    drag.set_fullscreen_workspaces(set);
                }
                wayfire::Event::WorkspaceViewsChanged(map) => {
                    drag.set_workspace_views(map);
                }
                wayfire::Event::ViewMapped { id, app_id } => {
                    if app_id == terminal_app_id(drag.terminal())
                        && let Some((x, y, w, h)) = drag.pop_terminal_slot()
                            && let Err(e) = wayfire::configure_view(id, x, y, w, h) {
                                eprintln!("sliver-dock: configure-view({id}): {e}");
                            }
                }
                wayfire::Event::Disconnected => {
                    eprintln!("sliver-dock: wayfire disconnected; reconnecting");
                }
            }
        }
    });
}

fn apply_command(req: &mut ipc::Request, drag: &Rc<Drag>, app: &Application) {
    let line = req.line.clone();
    let mut tokens = line.split_whitespace();
    let Some(verb) = tokens.next() else { return };
    match verb {
        "show" => drag.snap_to_state(BarState::Closed),
        "hide" => drag.snap_to_state(BarState::Hidden),
        "toggle-detailed" => drag.toggle_detailed(),
        "quit" => app.quit(),
        "get-state" => ipc::reply(req, &format!("{}\n", drag.dump_state())),
        "drag-begin" => {
            // Optional `zone=<id>` token after the direction (touch-drawer
            // protocol extension, slice 003): names the zone that matched
            // for the gesture-sample ring. Absent on pre-token plugin
            // binaries — the gesture still routes, only the ring entry's
            // zone stays unknown.
            let direction = tokens.next();
            let zone = tokens.find_map(|t| t.strip_prefix("zone=").map(str::to_owned));
            match direction {
                Some("up") => drag.begin(Direction::Up, zone),
                Some("down") => drag.begin(Direction::Down, zone),
                Some("up-grid") => drag.begin(Direction::UpGrid, zone),
                other => {
                    eprintln!("sliver-dock: drag-begin expects up|down|up-grid, got {other:?}")
                }
            }
        }
        "drag" => {
            if let Some(p) = tokens.next().and_then(|s| s.parse::<f64>().ok()) {
                drag.update(p);
            } else {
                eprintln!("sliver-dock: drag expects one float argument");
            }
        }
        "drag-end" => {
            let p = tokens.next().and_then(|s| s.parse::<f64>().ok());
            let v = tokens.next().and_then(|s| s.parse::<f64>().ok());
            match (p, v) {
                (Some(p), Some(v)) => drag.end(p, v),
                _ => eprintln!("sliver-dock: drag-end expects <progress> <velocity_px_per_ms>"),
            }
        }
        other => eprintln!("sliver-dock: unknown command: {other}"),
    }
}

/// Scan `.desktop` entries + load `apps.toml`, push the merged ordered list
/// onto `Drag` so the apps section can render it, then group the *launchable*
/// roster into astro-view planets. Logs a one-line summary of each so the
/// user can sanity-check counts at startup.
///
/// Requires `drag.load_launches()` to have run — planet contents are ordered
/// most-launched-first.
fn load_apps_into(drag: &Rc<Drag>) {
    let entries = apps::scan();
    let cfg = apps::load_config();
    let visible = apps::build_visible(&entries, &cfg);
    eprintln!(
        "sliver-dock: apps: scanned {} .desktop entries; visible {} (pinned {}, hidden {}, overrides {})",
        entries.len(),
        visible.len(),
        cfg.pinned.len(),
        cfg.hidden.len(),
        cfg.overrides.len(),
    );

    // Planets come off the full launchable roster, not the dock row's
    // curated `visible` — see `apps::build_launchable`.
    let launchable = apps::build_launchable(&entries, &cfg);
    let cat_cfg = categories::load_config();
    let planets = categories::build_planets(&launchable, &cat_cfg, &drag.launch_counts());
    let breakdown: Vec<String> = planets
        .iter()
        .map(|p| format!("{}:{}", p.display_name, p.apps.len()))
        .collect();
    eprintln!(
        "sliver-dock: categories: {} planets from {} launchable apps (overrides {}) — {}",
        planets.len(),
        launchable.len(),
        cat_cfg.overrides.len(),
        breakdown.join(" "),
    );
    drag.set_planets(planets);

    drag.set_apps(visible);
    // Registry covers the full scan so the workspace montage can resolve
    // icons for hidden / NoDisplay apps that wouldn't show in the apps
    // row but might still be running.
    let registry: HashMap<String, AppEntry> = entries.into_iter().collect();
    drag.set_app_registry(registry);
}

fn build_window(app: &Application, cfg: &config::Config) -> (ApplicationWindow, DrawingArea) {
    let g = cfg.geometry;
    // Astro view: the surface fills the screen minus a `D`-tall band at top
    // and bottom (`D` = the dock's own detailed stack height), so the astro
    // rect can span screen `[D, h-D]`. Sized off the real monitor height;
    // falls back to the old grid-inflated formula only if monitor 0 can't be
    // read at startup. Bottom-anchored, so the extra height grows *upward*.
    let dock_stack = dock_stack_px(&g);
    let window_height = match current_screen_size() {
        Some((_, sh)) => (sh - dock_stack).max(dock_stack + g.grid_height_px),
        None => dock_stack + g.grid_height_px,
    };

    let window = ApplicationWindow::builder()
        .application(app)
        .default_height(window_height)
        .build();
    window.add_css_class("sliver-dock");

    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_anchor(Edge::Bottom, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_exclusive_zone(0);

    let area = DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    window.set_child(Some(&area));

    // Deliberately NOT presented here. `present()` realizes and maps the
    // window synchronously, so any handler connected after it — `map`,
    // `resize` — misses the first one. `on_activate` presents once everything
    // is wired; the input region depends on that map/resize pair firing
    // (`incident-20260811-astro-surface-swallows-touch`).
    (window, area)
}

fn draw_bar(ctx: &CairoContext, w: f64, h: f64, g: &Geometry, a: &Animation, drag: &Drag) {
    let v = drag.visual();
    if v.hidden_amount >= 0.999 {
        return;
    }

    let bar_w = (g.width_px as f64).min(w);
    let bar_h = g.default_height_px as f64;
    let bar_left = (w - bar_w) / 2.0;
    let slope = g.taper_slope;

    // Slide the resting position down by max_offset when fully hidden.
    let max_offset = (g.margin_bottom_px + g.default_height_px) as f64 + 4.0;
    let y_offset = v.hidden_amount * max_offset;
    let resting_bot = h - g.margin_bottom_px as f64;
    let bar_bot = resting_bot + y_offset;
    let bar_top = bar_bot - bar_h;
    let cut_y = (bar_top + bar_bot) / 2.0;

    // detailed_amount lifts the top half off the cut, opening a gap filled
    // with the section content. At amount = 0 the two halves meet exactly at
    // cut_y and reproduce the unsplit silhouette. The lift itself lives in
    // `sliver_stack`, which owns everything above the cut.
    let alpha = 0.92 * (1.0 - v.hidden_amount);

    // Sections render *before* the bar halves so the bottom-half edge
    // covers their lower pixel — they appear to extrude from behind it.
    // The gap between the lifted top half and the resting bottom half is
    // a full-width rectangle (`bar_w`) — both halves' cut edges run from
    // tip-to-tip — so workspaces + info can extend all the way to the
    // tip points without exceeding the spear silhouette.
    if v.detailed_amount > 0.001 {
        draw_sections(
            ctx,
            bar_left,
            bar_w,
            cut_y,
            g,
            a,
            drag,
            v.detailed_amount,
            alpha,
        );
    }

    ctx.set_source_rgba(0.05, 0.05, 0.05, alpha);
    bottom_half_path(ctx, bar_left, bar_w, bar_h, cut_y, bar_bot, slope);
    let _ = ctx.fill();

    if v.hidden_amount < 0.999 {
        // Everything above the section cut is ONE shape. The dock sliver rides
        // up with the detailed lift so it caps the *top* of the detailed view
        // (screen `h - D`) rather than floating at the resting bar position; at
        // detailed_amount = 1 it lands exactly on the astro-view bottom border
        // — note 010 §Frame spec, amended 2026-08-05. The bar's top half sits
        // directly under it and the astro sliver directly above, so all three
        // are drawn as a single spear while they touch: three separately tipped
        // spears stacked showed three tapers where the silhouette should read
        // as one blade.
        let stack = sliver_stack(w, h, g, &v);
        ctx.set_source_rgba(0.05, 0.05, 0.05, alpha);
        // Tip on the bottom edge: the cut line runs tip-to-tip so the sections
        // extruding from it stay capped at their corners.
        let piece = stack.assembly().unwrap_or(stack.lower);
        fill_sliver(ctx, bar_left, bar_w, &piece, slope);
    }
}

/// The astro sliver — the astro view's moving handle and top border. Painted
/// after the backdrop so it caps the panel it pulled out. Closed, it sits
/// directly on the dock sliver and the two read as one fused trapezoid; see
/// [`sliver_stack`].
fn draw_astro_sliver(ctx: &CairoContext, w: f64, h: f64, g: &Geometry, drag: &Drag) {
    let v = drag.visual();
    if v.hidden_amount >= 0.999 {
        return;
    }
    let bar_w = (g.width_px as f64).min(w);
    let bar_left = (w - bar_w) / 2.0;
    let stack = sliver_stack(w, h, g, &v);
    if stack.fused {
        // Still resting on the dock: `draw_bar` already painted it as part of
        // the one fused spear.
        return;
    }

    ctx.set_source_rgba(0.05, 0.05, 0.05, 0.92 * (1.0 - v.hidden_amount));
    fill_sliver(ctx, bar_left, bar_w, &stack.astro, g.taper_slope);
}

/// Separation (px) over which the fused pair resolves into two independent
/// spears. Short enough that the split reads as instant on a flick, long
/// enough that the tips morph instead of popping.
const SLIVER_SPLIT_PX: f64 = 24.0;

/// Vertical geometry of everything above the section cut for the current
/// frame, plus the tip shape that fuses or splits it.
///
/// **Fused (`grid_amount == 0`).** The astro sliver, the dock sliver and the
/// main bar's lifted top half are all adjacent, and are drawn as ONE spear
/// spanning `astro_top..lower_bot` — 2.5 slivers tall, `tip_len` that height at
/// the 6x ratio, tipping on its bottom edge. That bottom edge is the section
/// cut line, which `draw_sections` lays sections out along tip-to-tip, so it
/// has to stay full width. The result is a single monotonic taper: exactly one
/// blade. Drawing the three as separately tipped spears instead stacks three
/// tapers, which is what the user reported on 2026-08-07 ("an additional taper
/// upwards") — and, at a shared junction, is the misaligned bowtie note 010
/// §Frame spec calls out.
///
/// **Splitting (`grid_amount` opening).** The astro sliver travels up to the
/// astro rect's top border. Over the first [`SLIVER_SPLIT_PX`] of separation
/// each piece morphs from *the master spear clipped to its own extent* into its
/// own standalone spear — the astro sliver to a lone 6px spear tipping at its
/// mid-line, the lower assembly to a 9px spear still tipping at the cut. At
/// separation 0 both pieces evaluate to exactly the master, so the fused spear
/// *becomes* two spears with no pop; a piece drawn as its own spear from the
/// start would snap back to body width at its cut edge. The dock sliver stays
/// put as the astro view's bottom border.
struct SliverStack {
    /// Top edge of the astro sliver — the backdrop is pulled out behind it, so
    /// `draw_astro_overlay` keys the panel's top edge off this. Every other
    /// edge lives on the pieces: `astro.clip_*` are the astro sliver's own
    /// extent and `lower.clip_top` is the dock sliver's top (its bottom edge,
    /// `+ default_height_px`, is the astro view's bottom border).
    astro_top: f64,
    /// Bottom of the **lower assembly**: the dock sliver *plus* the main bar's
    /// lifted top half, drawn as one shape. They are permanently adjacent — the
    /// top half rides the same lift the dock sliver does.
    lower_bot: f64,
    /// True while the astro sliver still rests on the lower assembly, when the
    /// whole stack draws as one path — see [`SliverStack::assembly`].
    fused: bool,
    astro: SliverPiece,
    lower: SliverPiece,
}

/// One drawn piece: the spear to fill, and the slab of it to keep. While the
/// pieces are separating, `spear` is wider than `clip` — each piece is showing
/// its share of a taller shape, which is what makes the split continuous.
#[derive(Clone, Copy)]
struct SliverPiece {
    top: f64,
    bot: f64,
    tip_len: f64,
    tip_y: f64,
    clip_top: f64,
    clip_bot: f64,
}

impl SliverStack {
    /// The fused resting silhouette as a single spear, or `None` once the astro
    /// sliver has left and the two pieces draw themselves.
    fn assembly(&self) -> Option<SliverPiece> {
        self.fused.then_some(SliverPiece {
            top: self.astro_top,
            bot: self.lower_bot,
            tip_len: tip_len_for(self.lower_bot - self.astro_top),
            tip_y: self.lower_bot,
            clip_top: self.astro_top,
            clip_bot: self.lower_bot,
        })
    }
}

/// Fill `piece`'s spear, clipped to the slab it owns.
fn fill_sliver(ctx: &CairoContext, x: f64, w: f64, piece: &SliverPiece, slope: f64) {
    ctx.save().ok();
    ctx.rectangle(x, piece.clip_top, w, (piece.clip_bot - piece.clip_top).max(0.0));
    ctx.clip();
    spear_path(ctx, x, w, piece.top, piece.bot, piece.tip_len, piece.tip_y, slope);
    let _ = ctx.fill();
    ctx.restore().ok();
}

/// A spear's tip length is [`SPEAR_TIP_RATIO`]x its height — the one rule the
/// whole silhouette keys off, so a fused stack tapers at the same angle as any
/// single piece of it.
fn tip_len_for(height: f64) -> f64 {
    height * SPEAR_TIP_RATIO
}

fn sliver_stack(surface_w: f64, surface_h: f64, g: &Geometry, v: &Visual) -> SliverStack {
    let bar_h = g.default_height_px as f64;
    // Same resting/hidden/lift math as `draw_bar`, so the dock sliver lands on
    // the main bar's top edge in every state.
    let max_offset = (g.margin_bottom_px + g.default_height_px) as f64 + 4.0;
    let bar_bot = surface_h - g.margin_bottom_px as f64 + v.hidden_amount * max_offset;
    let lift = v.detailed_amount * g.detailed_content_height_px as f64;

    let dock_bot = bar_bot - bar_h - lift;
    let dock_top = dock_bot - bar_h;
    let fused_top = dock_top - bar_h;
    // The bar's top half ends at the section cut line, which rides the lift.
    let lower_bot = dock_bot + bar_h / 2.0;

    // Travel: the astro sliver's top edge runs from the fused position to the
    // astro rect's top border, so at full open it *is* the top border (and the
    // backdrop's top edge follows it — see `draw_astro_overlay`).
    let astro_top = lerp(fused_top, astro_rect(surface_w, surface_h, g).top, v.grid_amount);
    let astro_bot = astro_top + bar_h;

    let split = ((dock_top - astro_bot) / SLIVER_SPLIT_PX).clamp(0.0, 1.0);
    let fused_tip = tip_len_for(lower_bot - fused_top);

    SliverStack {
        astro_top,
        lower_bot,
        fused: split <= 0.0,
        // Each piece starts as the master spear (bottom + tip at the cut,
        // fused tip length) and relaxes into its own.
        astro: SliverPiece {
            top: astro_top,
            bot: lerp(lower_bot, astro_bot, split),
            tip_len: lerp(fused_tip, tip_len_for(bar_h), split),
            tip_y: lerp(lower_bot, astro_bot - bar_h / 2.0, split),
            clip_top: astro_top,
            clip_bot: astro_bot,
        },
        lower: SliverPiece {
            top: lerp(astro_top, dock_top, split),
            bot: lower_bot,
            tip_len: lerp(fused_tip, tip_len_for(lower_bot - dock_top), split),
            tip_y: lower_bot,
            clip_top: dock_top,
            clip_bot: lower_bot,
        },
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// The dock's own vertical footprint from the screen bottom in the Detailed
/// state — `margin + the 6px bar + the detailed content`. This is `D` in the
/// astro spec (note 010): the surface is sized `screen_h - D` tall and the
/// astro rect spans screen `[D, h-D]`, leaving an even `D`-tall band above
/// and below.
fn dock_stack_px(g: &Geometry) -> i32 {
    g.margin_bottom_px + g.default_height_px + g.detailed_content_height_px
}

/// The layer-shell window's height for a given screen height — the exact
/// formula `build_window` sizes the surface with (primary path), so zone
/// geometry and the real surface agree by construction instead of by
/// coincidence. `grid_height_px` is the pre-grid fallback floor.
fn window_height_px(sh: i32, g: &Geometry) -> i32 {
    let d = dock_stack_px(g);
    (sh - d).max(d + g.grid_height_px)
}

/// Convert a rectangle from **surface** coordinates (y = 0 at the surface
/// top) to **screen** coordinates. The window is [`window_height_px`] tall
/// and bottom-anchored, so `screen_y = sh - win_h + surface_y`; x is
/// unchanged (the surface is full screen width).
///
/// This is the ONE conversion point for all zone geometry. Nothing
/// re-derives screen offsets from `margin + default + detailed` constants —
/// that is the exact bug fixed in
/// `incident-20260801-app-grid-invisible-stale-surface-height`, where a
/// constant-derived height pinned a region to the top of a
/// taller-than-expected surface. Zone bounds derive from the same
/// surface-anchored math as the draw path, then route through here.
///
/// Rounds outward (floor the origin, ceil the far edge) so a zone is
/// always a superset of the painted rect it mirrors, never a sub-rect that
/// strands touches in a sliver of unpainted-but-live surface.
fn surface_rect_to_screen(sh: i32, g: &Geometry, x: f64, y: f64, w: f64, h: f64) -> (i32, i32, i32, i32) {
    let win_h = window_height_px(sh, g);
    let y_screen = y + (sh - win_h) as f64;
    let sx = x.floor() as i32;
    let sy = y_screen.floor() as i32;
    let sw_z = (x + w).ceil() as i32 - sx;
    let sh_z = (y_screen + h).ceil() as i32 - sy;
    (sx, sy, sw_z, sh_z)
}

/// The fused sliver stack's top edge in **surface** coordinates for the
/// given state (0 lift in Closed/Hidden, the detailed lift in Detailed —
/// shared by the sliver-edge zone bounds and the draw path's
/// `sliver_stack`). AppGrid never uses this: there the astro sliver has
/// travelled to the astro rect's top border, handled separately.
fn sliver_stack_top_surface(win_h: i32, g: &Geometry, state: BarState) -> f64 {
    let lift = match state {
        BarState::Detailed => g.detailed_content_height_px,
        _ => 0,
    };
    win_h as f64 - g.margin_bottom_px as f64 - g.default_height_px as f64 * 3.0 - lift as f64
}

/// The astro-view rectangle in **surface** coordinates (`y = 0` at the surface
/// top). It fills the whole surface above the dock's detailed stack, indented
/// on each side to the sliver-tip x-extent (`tip_len = default_height · 6`).
/// In screen space this is `[D, h-D]`; here — because the surface is
/// bottom-anchored and `screen_h - D` tall — it is `[0, surface_h - D]`.
///
/// All astro geometry keys off the **real** surface height passed to the draw
/// callback / hit-test, never a `margin+default+detailed(+grid)` constant, per
/// `incident-20260801-app-grid-invisible-stale-surface-height`.
struct AstroRect {
    x: f64,
    top: f64,
    w: f64,
    bottom: f64,
}

fn astro_rect(surface_w: f64, surface_h: f64, g: &Geometry) -> AstroRect {
    let bar_w = (g.width_px as f64).min(surface_w);
    let bar_left = (surface_w - bar_w) / 2.0;
    let tip_len = g.default_height_px as f64 * SPEAR_TIP_RATIO;
    AstroRect {
        x: bar_left + tip_len,
        top: 0.0,
        w: (bar_w - 2.0 * tip_len).max(0.0),
        bottom: (surface_h - dock_stack_px(g) as f64).max(0.0),
    }
}

/// The astro view (`BarState::AppGrid`): a full-height, solid-black backdrop
/// panel spanning the astro rect `[D, h-D]`, revealed by a clip that extrudes
/// it *up* from its own bottom edge as `grid_amount` goes 0 → 1. Because it
/// runs after `draw_bar`, it composites on top of the dock sections.
///
/// Contents as of slice 5, painted back to front: the ASCII backdrop (its own
/// background fill, the baked starfield + nebulas, and any shooting star in
/// flight), then the planets — globes carrying real app icons, laid out by
/// `astro::build_sky`. The interim re-centered radial grid slice 1 parked
/// here is retired.
fn draw_astro_overlay(ctx: &CairoContext, w: f64, h: f64, g: &Geometry, drag: &Drag) {
    let v = drag.visual();
    let a = v.grid_amount;
    if a < 0.001 {
        return;
    }

    let rect = astro_rect(w, h, g);
    if rect.w <= 0.0 || rect.bottom <= rect.top {
        return;
    }
    let corner = 12.0;

    // The panel is *pulled out* by the astro sliver: its top edge is the
    // sliver's top edge, so the backdrop extrudes from behind the travelling
    // sliver rather than animating on its own clock (note 010 §Transition).
    // Clamped to the rect bottom for the Closed → AppGrid path, where the
    // sliver starts below the rect (the dock is still un-lifted). The rounded
    // clip does the reveal, so the fill stays fully opaque (solid black, zero
    // translucency per note 010).
    let panel_top = sliver_stack(w, h, g, &v).astro_top.min(rect.bottom);
    let card_h = (rect.bottom - panel_top).max(0.0);

    ctx.save().ok();
    rounded_rect_path(ctx, rect.x, panel_top, rect.w, card_h, corner);
    ctx.clip();

    // Backdrop, then planets. Both are laid out for the whole rect and fade
    // in on `a`; neither moves with the panel — the clip does the reveal, so
    // the sky is already in place behind the extruding panel rather than
    // sliding up into it. The backdrop paints its own background over the
    // full rect (the clip trims it), which is why there is no separate fill
    // here any more.
    let sky = sky_rect(&rect);
    drag.backdrop(sky).draw(ctx, a, drag.astro_clock());

    let icon_loader = |id: &str, spec: Option<&str>, size: i32| -> Option<cairo::ImageSurface> {
        drag.app_icon(id, spec, size)
    };
    astro::draw_sky(ctx, &drag.sky(sky), a, 0.0, &icon_loader);
    ctx.restore().ok();
}

/// The astro rect as `astro.rs` wants it. One conversion point, so the
/// renderer, the tap handler and `get-state` can never disagree about which
/// rectangle the sky was solved for.
fn sky_rect(rect: &AstroRect) -> astro::SkyRect {
    astro::SkyRect {
        x: rect.x,
        y: rect.top,
        w: rect.w,
        h: (rect.bottom - rect.top).max(0.0),
    }
}

/// Three placeholder section blocks staggered in detailed_amount space.
/// Each section's clip-mask grows from 0 to `detailed_content_height_px`
/// upward from `cut_y`. Layout (left → right):
///   * Workspaces — extends to the left tip point.
///   * Apps       — middle, fills whatever's between ws and info minus gaps.
///   * Info       — 60×`max_h` rectangle anchored at the right tip point.
///
/// The gap between the trapezoid halves spans the full `bar_w` at both
/// ends (each half's cut edge runs tip-to-tip), so the edge sections can
/// extend into the tip regions without overflowing the silhouette.
///
/// Tokyo Night palette: section bg uses `bg_highlight` (#292e42). Live
/// content (workspaces / apps / info) renders over the bg, gated by the
/// same per-section clip so the reveal stays in lockstep with extrusion.
#[allow(clippy::too_many_arguments)]
fn draw_sections(
    ctx: &CairoContext,
    bar_left: f64,
    bar_w: f64,
    cut_y: f64,
    g: &Geometry,
    a: &Animation,
    drag: &Drag,
    detailed_amount: f64,
    alpha: f64,
) {
    let max_h = g.detailed_content_height_px as f64;
    let mid_gap = 6.0;
    let main_w = ((bar_w - INFO_SECTION_W - 2.0 * mid_gap) / 2.0).max(0.0);

    let ws_x = bar_left;
    let apps_x = bar_left + main_w + mid_gap;
    let info_x = bar_left + bar_w - INFO_SECTION_W;

    let layout: [(f64, f64); 3] = [
        (ws_x, main_w),
        (apps_x, main_w),
        (info_x, INFO_SECTION_W),
    ];

    // Each section lags the previous by stagger_ms / snap_duration_ms in
    // detailed_amount space. detailed_amount itself is ease-out-cubic over
    // snap_duration_ms, so the lag is approximate but visually correct.
    let lag_step = (a.section_stagger_ms as f64 / a.snap_duration_ms.max(1) as f64).min(0.3);
    let denom = (1.0 - 2.0 * lag_step).max(0.001);

    // Tokyo Night bg_highlight #292e42.
    let bg_rgb = (0.161, 0.180, 0.259);

    for (i, (x, w)) in layout.iter().enumerate() {
        if *w <= 0.0 {
            continue;
        }
        let lag = i as f64 * lag_step;
        let s_amount = ((detailed_amount - lag) / denom).clamp(0.0, 1.0);
        let s_h = s_amount * max_h;
        if s_h < 0.5 {
            continue;
        }

        let y_top = cut_y - s_h;

        ctx.save().ok();
        ctx.rectangle(*x, y_top, *w, s_h);
        ctx.clip();

        ctx.set_source_rgba(bg_rgb.0, bg_rgb.1, bg_rgb.2, 0.92 * alpha);
        ctx.rectangle(*x, cut_y - max_h, *w, max_h);
        let _ = ctx.fill();

        match i {
            0 => draw_workspace_icons(ctx, drag, *x, *w, cut_y, max_h, alpha),
            1 => draw_app_icons(ctx, drag, *x, *w, cut_y, max_h, alpha),
            2 => draw_info_section(ctx, drag, *x, *w, cut_y, max_h, alpha),
            _ => {}
        }

        ctx.restore().ok();
    }
}

/// Workspaces section content: row of rounded icons (one per rendered
/// workspace), with the current workspace's icon brighter / heavier-bordered.
/// Each icon's interior shows a per-window **spatial** montage (slice 006) —
/// one record per open window, no app-id dedupe, cells projected from the
/// windows' workspace-local rects so the miniature mirrors the screen;
/// workspaces with no tracked toplevels fall back to the bare placeholder.
fn draw_workspace_icons(
    ctx: &CairoContext,
    drag: &Drag,
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    alpha: f64,
) {
    let workspaces = drag.rendered_workspaces();
    if workspaces.is_empty() {
        return;
    }
    let layout = workspace_icons_at(section_x, section_w, cut_y, max_h, &workspaces);
    if layout.is_empty() {
        return;
    }

    let current = drag.current_workspace();

    // Tokyo Night colors used here:
    //   bg of icon       — #1a1b26 (TN bg)
    //   border / accent  — #7aa2f7 (TN blue)
    let blue = (0.478, 0.635, 0.969);
    let bg = (0.10, 0.106, 0.149);

    for (ws, rect) in &layout {
        let is_current = *ws == current;
        let montage = drag.workspace_views(*ws);

        rounded_rect_path(ctx, rect.x, rect.y, rect.w, rect.h, 7.0);
        let f_alpha = if is_current { 0.95 } else { 0.65 };
        ctx.set_source_rgba(bg.0, bg.1, bg.2, f_alpha * alpha);
        let _ = ctx.fill();

        // Empty montage = the NORMAL empty-workspace look (slice 004: every
        // grid cell renders an icon, empty or not) — the bare rounded slot
        // below is the intentional placeholder, not a failed icon load. The
        // current-workspace emphasis (fill/border alpha + border width)
        // stays exactly as-is.
        if !montage.is_empty() {
            draw_montage(ctx, *rect, &montage, drag, alpha);
        }

        rounded_rect_path(ctx, rect.x, rect.y, rect.w, rect.h, 7.0);
        let b_alpha = if is_current { 0.95 } else { 0.5 };
        let b_w = if is_current { 1.5 } else { 0.7 };
        ctx.set_source_rgba(blue.0, blue.1, blue.2, b_alpha * alpha);
        ctx.set_line_width(b_w);
        let _ = ctx.stroke();
    }
}

/// The letterboxed content box inside the 41×41 workspace icon: the largest
/// rect with the **live screen's** aspect ratio, centred inside `icon`.
/// With the 1280×853 output the box is ~41×27 centred vertically, so a
/// half-width full-height window projects to a true vertical half (the
/// locked user decision). The aspect comes from `output_size`, never a
/// baked constant — this device rotates (`autorotate-iio`), so portrait
/// must yield a tall narrow box, not a broken layout.
///
/// `None` when the output size is unusable (≤ 0 on either axis) — the
/// renderer then skips the montage rather than guessing a frame.
fn content_box(icon: Rect, output_size: (i32, i32)) -> Option<Rect> {
    let (ow, oh) = output_size;
    if ow <= 0 || oh <= 0 {
        return None;
    }
    let aspect = ow as f64 / oh as f64;
    let icon_aspect = icon.w / icon.h;
    if aspect > icon_aspect {
        // Screen wider than the icon: fit to width, letterbox vertically.
        let h = icon.w / aspect;
        Some(Rect {
            x: icon.x,
            y: icon.y + (icon.h - h) / 2.0,
            w: icon.w,
            h,
        })
    } else {
        // Screen taller than (or as wide as) the icon: fit to height,
        // letterbox horizontally.
        let w = icon.h * aspect;
        Some(Rect {
            x: icon.x + (icon.w - w) / 2.0,
            y: icon.y,
            w,
            h: icon.h,
        })
    }
}

/// Project one window's workspace-local rect into the letterboxed content
/// box at the output's scale: `unit = local / output_size` per axis, then
/// `content.origin + unit * content.size`. This is the heart of slice 006 —
/// the miniature mirrors the screen, so it is pure and unit-tested over the
/// real geometry values.
///
/// Clamps each axis to `[0,1]`: a view larger than the workspace
/// (fullscreen with margins, or geometry briefly exceeding the output)
/// stays inside the content box instead of overpainting its neighbours.
/// Returns `None` (the cell is dropped, never NaN) for a zero-area or
/// not-yet-sized rect, and for a rect fully off the output on either axis
/// after clamping.
fn project_view_cell(view: ViewRect, frame: (i32, i32), content: Rect) -> Option<Rect> {
    let (fw, fh) = frame;
    if fw <= 0 || fh <= 0 || view.w <= 0 || view.h <= 0 {
        return None;
    }
    // f64 math throughout: coords arrive as i32 but `x + w` is computed in
    // f64, so an extreme rect can never overflow.
    let fw = fw as f64;
    let fh = fh as f64;
    let x1 = (view.x as f64 / fw).clamp(0.0, 1.0);
    let y1 = (view.y as f64 / fh).clamp(0.0, 1.0);
    let x2 = ((view.x as f64 + view.w as f64) / fw).clamp(0.0, 1.0);
    let y2 = ((view.y as f64 + view.h as f64) / fh).clamp(0.0, 1.0);
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    Some(Rect {
        x: content.x + x1 * content.w,
        y: content.y + y1 * content.h,
        w: (x2 - x1) * content.w,
        h: (y2 - y1) * content.h,
    })
}

/// Shrink a projected cell by `seam` on every side so adjacent cells keep a
/// visible gutter: their true projection edges touch at the shared boundary,
/// and two adjacent identical icons (the common case — terminals) would
/// otherwise read as one. Never shrinks below 1×1.
fn seam_cell(cell: Rect, seam: f64) -> Rect {
    Rect {
        x: cell.x + seam,
        y: cell.y + seam,
        w: (cell.w - 2.0 * seam).max(1.0),
        h: (cell.h - 2.0 * seam).max(1.0),
    }
}

/// Paint the per-workspace montage inside `rect` (the outer 41×41 rounded
/// icon's interior bounds) as a **spatial miniature** (slice 006): each
/// window's cell is its workspace-local rect projected into the letterboxed
/// content box, so icon positions mirror on-screen positions — two
/// side-by-side windows render side by side, a floating dialog lands on top
/// of the window it overlaps. Records arrive in stacking order bottom →
/// top and are painted in that order (topmost last), so an overlapping
/// window's icon paints over the one below.
///
/// Degenerate cases: no records → the caller already shows the bare
/// placeholder; a record whose rect is zero-area, not-yet-sized, or fully
/// off-screen is skipped (never NaN); a rect larger than the output is
/// clamped to the content box; no `output_size` yet (pre-`Connected` or
/// degraded mode — views can't arrive without it, so this is a defensive
/// early-out, not a real mode) → the montage falls back to the bare
/// placeholder too, rather than guessing a frame.
///
/// Each cell is shrunk by `CELL_SEAM` so adjacent identical icons keep a
/// visible seam, then drawn exactly as before: a rounded bg (`bg_highlight`)
/// inset 2 px, the cached icon scaled to fit (preserving aspect, centered),
/// or the generic window glyph when the app-id can't be resolved (a cell is
/// never empty). Icons still load at `ICON_SIZE` for the shared cache key —
/// no second size.
fn draw_montage(
    ctx: &CairoContext,
    rect: Rect,
    views: &[MontageView],
    drag: &Drag,
    alpha: f64,
) {
    if views.is_empty() {
        return;
    }
    let Some(frame) = drag.output_size() else {
        return;
    };
    let Some(content) = content_box(rect, frame) else {
        return;
    };
    let bg_highlight = (0.161, 0.180, 0.259);
    let inset = 2.0;
    let cell_radius = 3.0;
    for mv in views {
        let Some(cell) = project_view_cell(mv.rect, frame, content) else {
            continue;
        };
        let cell = seam_cell(cell, CELL_SEAM);
        let icell = Rect {
            x: cell.x + inset,
            y: cell.y + inset,
            w: (cell.w - 2.0 * inset).max(1.0),
            h: (cell.h - 2.0 * inset).max(1.0),
        };
        rounded_rect_path(ctx, icell.x, icell.y, icell.w, icell.h, cell_radius);
        ctx.set_source_rgba(bg_highlight.0, bg_highlight.1, bg_highlight.2, 0.95 * alpha);
        let _ = ctx.fill();

        match drag.app_icon_by_id(&mv.app_id, ICON_SIZE as i32) {
            Some(surf) => {
                let sw = surf.width() as f64;
                let sh = surf.height() as f64;
                if sw > 0.0 && sh > 0.0 {
                    let scale = (icell.w / sw).min(icell.h / sh);
                    let dx = icell.x + (icell.w - sw * scale) / 2.0;
                    let dy = icell.y + (icell.h - sh * scale) / 2.0;
                    ctx.save().ok();
                    ctx.translate(dx, dy);
                    if (scale - 1.0).abs() > 1e-3 {
                        ctx.scale(scale, scale);
                    }
                    let _ = ctx.set_source_surface(&surf, 0.0, 0.0);
                    let _ = ctx.paint_with_alpha(alpha);
                    ctx.restore().ok();
                }
            }
            // A cell must never render empty (slice 005): unresolvable or
            // empty app-id → generic window glyph.
            None => draw_fallback_glyph(ctx, icell, alpha),
        }
    }
}

/// Generic window glyph for a montage cell whose app-id can't be resolved
/// to an icon — registry miss (an xwayland client with no `.desktop`
/// entry) or an empty app-id. A rounded rect in Tokyo-Night fg at low
/// alpha, with a hair-brighter top edge so it reads as a window rather
/// than a failed background fill. No new asset, no text.
fn draw_fallback_glyph(ctx: &CairoContext, cell: Rect, alpha: f64) {
    let fg = (0.85, 0.88, 0.95);
    let inset_x = (cell.w * 0.30).max(2.0);
    let inset_y = (cell.h * 0.22).max(2.0);
    let inner = Rect {
        x: cell.x + inset_x,
        y: cell.y + inset_y,
        w: (cell.w - 2.0 * inset_x).max(2.0),
        h: (cell.h - 2.0 * inset_y).max(2.0),
    };
    rounded_rect_path(ctx, inner.x, inner.y, inner.w, inner.h, 2.0);
    ctx.set_source_rgba(fg.0, fg.1, fg.2, 0.28 * alpha);
    let _ = ctx.fill();
    ctx.set_source_rgba(fg.0, fg.1, fg.2, 0.5 * alpha);
    ctx.set_line_width(1.0);
    ctx.move_to(inner.x + 1.5, inner.y + 1.0);
    ctx.line_to(inner.x + inner.w - 1.5, inner.y + 1.0);
    let _ = ctx.stroke();
}

/// Info section content. Top half: clock (`HH:MM` then `Day Nth`). Bottom
/// half: battery icon left, CPU icon right (CPU lands in 6c). The clock 1Hz
/// `queue_draw` from `on_activate` covers redraws without battery events;
/// battery PropertiesChanged events also call `queue_draw` via
/// `Drag::set_battery`.
fn draw_info_section(
    ctx: &CairoContext,
    drag: &Drag,
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    alpha: f64,
) {
    let now = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let hh_mm = format!("{:02}:{:02}", now.hour(), now.minute());
    let date_line = format!(
        "{} {}{}",
        weekday_abbrev(now.weekday()),
        now.day(),
        ordinal_suffix(now.day())
    );

    let top_y = cut_y - max_h;
    let pad_top = 4.0;
    let line1_size = 12.0;
    let line2_size = 9.0;
    let line_gap = 2.0;

    ctx.save().ok();
    ctx.select_font_face(
        "Sans",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Normal,
    );

    // Line 1 — HH:MM, Tokyo Night fg.
    ctx.set_font_size(line1_size);
    if let Ok(ext) = ctx.text_extents(&hh_mm) {
        let x = section_x + (section_w - ext.width()) / 2.0 - ext.x_bearing();
        let y = top_y + pad_top + line1_size;
        ctx.set_source_rgba(0.85, 0.88, 0.95, 0.95 * alpha);
        ctx.move_to(x, y);
        let _ = ctx.show_text(&hh_mm);
    }

    // Line 2 — Day Nth, dimmer comment-grey.
    ctx.set_font_size(line2_size);
    if let Ok(ext) = ctx.text_extents(&date_line) {
        let x = section_x + (section_w - ext.width()) / 2.0 - ext.x_bearing();
        let y = top_y + pad_top + line1_size + line_gap + line2_size;
        ctx.set_source_rgba(0.65, 0.72, 0.85, 0.9 * alpha);
        ctx.move_to(x, y);
        let _ = ctx.show_text(&date_line);
    }

    ctx.restore().ok();

    if let Some(info) = drag.battery() {
        draw_battery_icon(ctx, &info, section_x, section_w, cut_y, max_h, alpha);
    }
    if let Some(info) = drag.cpu() {
        draw_cpu_icon(ctx, &info, section_x, section_w, cut_y, max_h, alpha);
    }
}

/// Bottom-left battery pie. Faint full-circle background ring + a foreground
/// arc that sweeps clockwise from the top by `pct/100 × τ`. Arc color tracks
/// charge state (green when charging / full, blue at normal levels, red ≤ 20%).
fn draw_battery_icon(
    ctx: &CairoContext,
    info: &BatteryInfo,
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    alpha: f64,
) {
    let _ = (section_w, max_h);

    let radius = INFO_DISC_RADIUS;
    let cx = section_x + INFO_DISC_PAD + radius;
    let cy = cut_y - INFO_DISC_PAD - radius;
    let pct = battery::rounded_to_ten(info.percentage);
    let (r, g, b) = battery_fill_color(info.state, pct);
    draw_pie_arc(ctx, cx, cy, radius, pct, (r, g, b), alpha);
}

fn battery_fill_color(state: BatteryState, rounded_pct: u32) -> (f64, f64, f64) {
    // Tokyo Night palette:
    //   green  #9ece6a — charging / full
    //   blue   #7aa2f7 — normal discharge
    //   red    #f7768e — low / critical
    let green = (0.620, 0.808, 0.416);
    let blue = (0.478, 0.635, 0.969);
    let red = (0.969, 0.463, 0.557);
    if state.is_charging() || matches!(state, BatteryState::FullyCharged) {
        green
    } else if rounded_pct <= 20 {
        red
    } else {
        blue
    }
}

/// Bottom-right CPU pie. Mirrors `draw_battery_icon`'s geometry. Ramp is
/// green ≤ 50%, amber 50–80%, red > 80%.
fn draw_cpu_icon(
    ctx: &CairoContext,
    info: &CpuInfo,
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    alpha: f64,
) {
    let _ = max_h;

    let radius = INFO_DISC_RADIUS;
    let cx = section_x + section_w - INFO_DISC_PAD - radius;
    let cy = cut_y - INFO_DISC_PAD - radius;
    let pct = info.percent.min(100);
    let (r, g, b) = cpu_fill_color(pct);
    draw_pie_arc(ctx, cx, cy, radius, pct, (r, g, b), alpha);
}

/// Background ring + percentage-sweep arc starting at top (`-π/2`) and
/// going clockwise. Shared by battery and CPU.
fn draw_pie_arc(
    ctx: &CairoContext,
    cx: f64,
    cy: f64,
    radius: f64,
    pct: u32,
    color: (f64, f64, f64),
    alpha: f64,
) {
    use std::f64::consts::{PI, TAU};

    ctx.save().ok();

    ctx.set_line_cap(gtk4::cairo::LineCap::Butt);
    ctx.set_line_width(2.5);
    ctx.arc(cx, cy, radius, 0.0, TAU);
    ctx.set_source_rgba(0.42, 0.46, 0.55, 0.35 * alpha);
    let _ = ctx.stroke();

    if pct > 0 {
        let (r, g, b) = color;
        let start = -PI / 2.0;
        let end = start + (pct as f64 / 100.0) * TAU;
        ctx.set_line_width(3.0);
        ctx.arc(cx, cy, radius, start, end);
        ctx.set_source_rgba(r, g, b, 0.95 * alpha);
        let _ = ctx.stroke();
    }

    ctx.restore().ok();
}

fn cpu_fill_color(percent: u32) -> (f64, f64, f64) {
    // Same Tokyo Night triple as battery, plus amber #e0af68 for the warm
    // mid-band. Idle reads green, working reads amber, hot reads red.
    let green = (0.620, 0.808, 0.416);
    let amber = (0.878, 0.686, 0.408);
    let red = (0.969, 0.463, 0.557);
    if percent > 80 {
        red
    } else if percent > 50 {
        amber
    } else {
        green
    }
}

fn weekday_abbrev(w: time::Weekday) -> &'static str {
    use time::Weekday::*;
    match w {
        Monday => "Mon",
        Tuesday => "Tue",
        Wednesday => "Wed",
        Thursday => "Thu",
        Friday => "Fri",
        Saturday => "Sat",
        Sunday => "Sun",
    }
}

/// English ordinal suffix for a day-of-month (1..=31). 11/12/13 always
/// take "th" — they look like first/second/third by trailing digit but
/// sound wrong out loud. Otherwise it's the trailing digit.
fn ordinal_suffix(day: u8) -> &'static str {
    match day {
        11..=13 => "th",
        n => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

/// Apps section content: row of icons rendered from cached cairo surfaces
/// (lazy-loaded on first frame; see `Drag::app_icon`). Rounded background
/// square sits behind each icon so icons that fail to load still leave a
/// visible slot, and icon color/alpha doesn't depend on the section bg
/// stripe behind it. Layout truncates to fit; overflow handling is a
/// later sub-phase.
fn draw_app_icons(
    ctx: &CairoContext,
    drag: &Drag,
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    alpha: f64,
) {
    let apps = drag.visible_apps();
    if apps.is_empty() {
        return;
    }
    let layout = app_icons_at(section_x, section_w, cut_y, max_h, &apps);

    let launching = drag.launching_ids();
    let spinner_angle = drag.launching_angle();

    let bg = (0.10, 0.106, 0.149);
    for (entry, rect) in &layout {
        let is_launching = launching.contains(&entry.id);
        // Dim the icon (bg + image alike) so the spinner reads against it.
        // The bg fades less than the image so the slot still looks filled.
        let icon_alpha = if is_launching { 0.40 * alpha } else { alpha };
        let bg_alpha = if is_launching { 0.45 * alpha } else { 0.65 * alpha };

        rounded_rect_path(ctx, rect.x, rect.y, rect.w, rect.h, 7.0);
        ctx.set_source_rgba(bg.0, bg.1, bg.2, bg_alpha);
        let _ = ctx.fill();

        if let Some(surf) = drag.app_icon(&entry.id, entry.icon.as_deref(), rect.w as i32) {
            let sw = surf.width() as f64;
            let sh = surf.height() as f64;
            if sw > 0.0 && sh > 0.0 {
                let scale = (rect.w / sw).min(rect.h / sh);
                let dx = rect.x + (rect.w - sw * scale) / 2.0;
                let dy = rect.y + (rect.h - sh * scale) / 2.0;
                ctx.save().ok();
                ctx.translate(dx, dy);
                if (scale - 1.0).abs() > 1e-3 {
                    ctx.scale(scale, scale);
                }
                let _ = ctx.set_source_surface(&surf, 0.0, 0.0);
                let _ = ctx.paint_with_alpha(icon_alpha);
                ctx.restore().ok();
            }
        }

        if is_launching {
            let cx = rect.x + rect.w / 2.0;
            let cy = rect.y + rect.h / 2.0;
            let radius = (rect.w.min(rect.h) * 0.32).max(8.0);
            draw_launch_spinner(ctx, cx, cy, radius, spinner_angle, alpha);
        }
    }
}

/// Tokyo-Night-blue ¾-circle arc that rotates clockwise. Tail end uses
/// a transparent stroke gradient via two-pass draw — the leading head is
/// fully opaque, the tail fades to 0 — so the spinning feels directional
/// rather than just a chasing donut.
fn draw_launch_spinner(
    ctx: &CairoContext,
    cx: f64,
    cy: f64,
    radius: f64,
    angle: f64,
    alpha: f64,
) {
    use std::f64::consts::TAU;
    let blue = (0.478, 0.635, 0.969);

    ctx.save().ok();
    ctx.set_line_cap(gtk4::cairo::LineCap::Round);
    ctx.set_line_width(2.5);

    // Dim background ring so the rotation reads against partial alpha
    // pixels too — without it, the eye loses the arc on dark icons.
    ctx.arc(cx, cy, radius, 0.0, TAU);
    ctx.set_source_rgba(blue.0, blue.1, blue.2, 0.18 * alpha);
    let _ = ctx.stroke();

    // ¾-circle arc rotates clockwise (`angle` is monotonically increasing
    // from `launching_angle`). cairo's `arc` is clockwise in screen coords
    // (y grows downward), so `start → start + 0.75 * TAU` walks forward.
    let head = angle;
    let tail = head - 0.75 * TAU;
    ctx.arc(cx, cy, radius, tail, head);
    ctx.set_source_rgba(blue.0, blue.1, blue.2, 0.95 * alpha);
    let _ = ctx.stroke();

    ctx.restore().ok();
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

const ICON_SIZE: f64 = 41.0;
const ICON_GAP: f64 = 8.0;
/// Per-cell seam in the spatial montage (slice 006). The partition-era
/// `MONTAGE_GAP` is gone with `montage_cells`; with real geometry the
/// windows' own gutters are sub-pixel at 1/31 scale, so each projected
/// cell shrinks by this fixed amount instead — two adjacent identical
/// icons (terminals) keep a visible seam, and an overlapping window's
/// edge stays readable.
const CELL_SEAM: f64 = 1.0;

/// Place up to `count` icons in a row inside the section. Centers the row
/// horizontally and centers the `ICON_SIZE + extra_below` block vertically
/// within `max_h` so the icons (with any reserved space below them) sit
/// balanced top-to-bottom in the section. Truncates if `count` doesn't fit.
fn row_layout(
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    count: usize,
    extra_below: f64,
) -> Vec<Rect> {
    if count == 0 || section_w < ICON_SIZE {
        return Vec::new();
    }
    let max_fit = (((section_w - 4.0) + ICON_GAP) / (ICON_SIZE + ICON_GAP))
        .max(1.0)
        .floor() as usize;
    let visible = count.min(max_fit);
    let total_w = visible as f64 * ICON_SIZE + (visible as f64 - 1.0).max(0.0) * ICON_GAP;
    let icons_left = section_x + ((section_w - total_w).max(0.0) / 2.0).max(2.0);
    let block_h = ICON_SIZE + extra_below.max(0.0);
    let icon_y_top = cut_y - max_h + ((max_h - block_h) / 2.0).max(0.0);
    (0..visible)
        .map(|i| Rect {
            x: icons_left + (i as f64) * (ICON_SIZE + ICON_GAP),
            y: icon_y_top,
            w: ICON_SIZE,
            h: ICON_SIZE,
        })
        .collect()
}

/// Compute icon rectangles for the workspaces section. Shared between the
/// draw callback and the click hit-tester so they can't drift apart.
/// Workspaces are rendered in the order supplied — `Drag::rendered_workspaces`
/// already sorts them row-major.
fn workspace_icons_at(
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    workspaces: &[WorkspaceId],
) -> Vec<(WorkspaceId, Rect)> {
    let rects = row_layout(section_x, section_w, cut_y, max_h, workspaces.len(), 0.0);
    workspaces.iter().copied().zip(rects).collect()
}

/// Compute icon rectangles for the apps section. The row truncates to fit
/// the section width — overflow handling is a later sub-phase. The result
/// is the order in which icons render (pinned first, then alphabetical).
fn app_icons_at(
    section_x: f64,
    section_w: f64,
    cut_y: f64,
    max_h: f64,
    apps: &[AppEntry],
) -> Vec<(AppEntry, Rect)> {
    let rects = row_layout(section_x, section_w, cut_y, max_h, apps.len(), 0.0);
    apps.iter().take(rects.len()).cloned().zip(rects).collect()
}

#[derive(Debug, Clone, Copy)]
struct SectionLayout {
    /// (x, w) of the workspaces section.
    ws: (f64, f64),
    /// (x, w) of the apps section.
    apps: (f64, f64),
    /// (x, w) of the info section (clock + battery + CPU).
    info: (f64, f64),
    /// y-coordinate of the bar's cut line; section content extends *up*
    /// from here by `detailed_content_height_px`.
    cut_y: f64,
}

/// Re-derive the section slot geometry inside the bar. Mirrors the layout
/// math in `draw_sections` so click hit-testing matches what was painted
/// without coupling to whether the draw callback has run.
fn section_bounds(surface_w: f64, surface_h: f64, g: &Geometry) -> SectionLayout {
    let bar_w = (g.width_px as f64).min(surface_w);
    let bar_left = (surface_w - bar_w) / 2.0;
    let mid_gap = 6.0;
    let main_w = ((bar_w - INFO_SECTION_W - 2.0 * mid_gap) / 2.0).max(0.0);
    // `surface_h` is the real (grid-inflated) surface height; the bar is
    // bottom-anchored within it, so the cut line is measured up from the
    // bottom edge — never from a `margin+default+detailed` constant.
    let bar_bot_y = surface_h - g.margin_bottom_px as f64;
    let bar_top_y = bar_bot_y - g.default_height_px as f64;
    let cut_y = (bar_top_y + bar_bot_y) / 2.0;
    let info_x = bar_left + bar_w - INFO_SECTION_W;
    SectionLayout {
        ws: (bar_left, main_w),
        apps: (bar_left + main_w + mid_gap, main_w),
        info: (info_x, INFO_SECTION_W),
        cut_y,
    }
}

/// Hit-rect for the clock affordance — top half of the info section. The
/// clock visually occupies ~27px from the top; using exactly half the
/// section gives a forgiving touch target without overlapping the discs.
fn info_clock_rect(section_x: f64, section_w: f64, cut_y: f64, max_h: f64) -> Rect {
    Rect {
        x: section_x,
        y: cut_y - max_h,
        w: section_w,
        h: max_h / 2.0,
    }
}

/// Hit-rect for the battery disc — bottom-left of the info section.
/// Bounding box around the circle drawn by `draw_battery_icon`.
fn info_battery_rect(section_x: f64, cut_y: f64) -> Rect {
    let r = INFO_DISC_RADIUS;
    let cx = section_x + INFO_DISC_PAD + r;
    let cy = cut_y - INFO_DISC_PAD - r;
    Rect {
        x: cx - r,
        y: cy - r,
        w: 2.0 * r,
        h: 2.0 * r,
    }
}

/// Hit-rect for the CPU disc — bottom-right of the info section. No popover
/// for now, but we still consume the click so it doesn't fall through to
/// whatever is under the bar.
fn info_cpu_rect(section_x: f64, section_w: f64, cut_y: f64) -> Rect {
    let r = INFO_DISC_RADIUS;
    let cx = section_x + section_w - INFO_DISC_PAD - r;
    let cy = cut_y - INFO_DISC_PAD - r;
    Rect {
        x: cx - r,
        y: cy - r,
        w: 2.0 * r,
        h: 2.0 * r,
    }
}

fn rect_contains(r: &Rect, x: f64, y: f64) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// Phase 6d — popovers parented to the DrawingArea. Built once at startup;
/// each tap re-aims the relevant popover via `set_pointing_to(rect)` and
/// calls `popup()`. `autohide = true` (the gtk4 default) closes them on
/// any click outside, so we don't need an explicit dismiss path.
#[derive(Clone)]
struct InfoPopovers {
    clock: Popover,
    clock_calendar: Calendar,
    battery: Popover,
    battery_label: Label,
}

fn build_info_popovers(area: &DrawingArea) -> InfoPopovers {
    let clock_calendar = Calendar::new();
    clock_calendar.add_css_class("sliver-dock-calendar");
    let clock = Popover::builder()
        .child(&clock_calendar)
        .autohide(true)
        .build();
    clock.add_css_class("sliver-dock-popover");
    clock.set_parent(area);

    let battery_label = Label::new(None);
    battery_label.add_css_class("sliver-dock-battery-label");
    battery_label.set_margin_start(10);
    battery_label.set_margin_end(10);
    battery_label.set_margin_top(8);
    battery_label.set_margin_bottom(8);
    let battery = Popover::builder()
        .child(&battery_label)
        .autohide(true)
        .build();
    battery.add_css_class("sliver-dock-popover");
    battery.set_parent(area);

    InfoPopovers {
        clock,
        clock_calendar,
        battery,
        battery_label,
    }
}

fn rect_to_gdk(r: &Rect) -> gdk::Rectangle {
    gdk::Rectangle::new(r.x as i32, r.y as i32, r.w as i32, r.h as i32)
}

fn popup_clock(popovers: &InfoPopovers, rect: &Rect) {
    if let Ok(now) = glib::DateTime::now_local() {
        popovers.clock_calendar.select_day(&now);
    }
    let gr = rect_to_gdk(rect);
    popovers.clock.set_pointing_to(Some(&gr));
    popovers.clock.popup();
}

fn popup_battery(popovers: &InfoPopovers, drag: &Drag, rect: &Rect) {
    let text = match drag.battery() {
        Some(info) => battery::format_battery_status(&info),
        None => "No battery data".to_string(),
    };
    popovers.battery_label.set_text(&text);
    let gr = rect_to_gdk(rect);
    popovers.battery.set_pointing_to(Some(&gr));
    popovers.battery.popup();
}

/// Add a `GestureClick` to the DrawingArea that handles workspace, apps,
/// and info-section taps. Active only when state is `Detailed`. Apps and
/// workspaces sections don't overlap, but the order of hit-tests is kept
/// stable: apps → workspaces → info.
///
/// Tap-only — no motion tracking; this stays within "basic clicks are
/// allowed" and does not handle gestures (those belong in touch-drawer).
fn install_section_click_handler(
    area: &DrawingArea,
    drag: &Rc<Drag>,
    geom: Geometry,
    popovers: InfoPopovers,
) {
    let gesture = GestureClick::new();
    let drag = Rc::clone(drag);
    let area_for_size = area.clone();
    gesture.connect_pressed(move |_, _n_press, x, y| {
        let bar_state = drag.state();
        if bar_state != BarState::Detailed && bar_state != BarState::AppGrid {
            return;
        }
        let surface_w = area_for_size.width() as f64;
        // Real surface height (== the layer-shell surface, `grid_height_px`
        // taller than the pre-grid window). All vertical hit-test geometry
        // must key off this, not a `margin+default+detailed` constant, or the
        // hit targets land `grid_height_px` above the drawn content.
        let surface_h = area_for_size.height() as f64;

        if bar_state == BarState::AppGrid {
            // Same rect and the same cached sky the renderer drew, so a tap
            // lands on the icon the user is looking at.
            let rect = astro_rect(surface_w, surface_h, &geom);
            let sky = drag.sky(sky_rect(&rect));
            if let Some(entry) = astro::hit_test(x, y, &sky, 0.0) {
                handle_app_tap(&drag, &entry);
                if drag.auto_close_app_grid() {
                    drag.snap_to_state(BarState::Detailed);
                }
            }
            return;
        }

        if surface_w <= 0.0 {
            return;
        }
        let layout = section_bounds(surface_w, surface_h, &geom);
        let max_h = geom.detailed_content_height_px as f64;

        let apps_snapshot = drag.visible_apps();
        let apps_layout =
            app_icons_at(layout.apps.0, layout.apps.1, layout.cut_y, max_h, &apps_snapshot);
        for (entry, rect) in &apps_layout {
            if rect_contains(rect, x, y) {
                handle_app_tap(&drag, entry);
                return;
            }
        }

        // Info section: clock → calendar popover, battery disc → time-remaining
        // popover, CPU disc → consume click (no popover yet — Phase 6d spec).
        // Tested before the workspaces block because popovers don't depend on
        // wayfire being connected, and the workspaces block early-returns when
        // it isn't.
        let (info_x, info_w) = layout.info;
        let clock_rect = info_clock_rect(info_x, info_w, layout.cut_y, max_h);
        if rect_contains(&clock_rect, x, y) {
            popup_clock(&popovers, &clock_rect);
            return;
        }
        let battery_rect = info_battery_rect(info_x, layout.cut_y);
        if rect_contains(&battery_rect, x, y) {
            popup_battery(&popovers, &drag, &battery_rect);
            return;
        }
        let cpu_rect = info_cpu_rect(info_x, info_w, layout.cut_y);
        if rect_contains(&cpu_rect, x, y) {
            return;
        }

        let Some(output_id) = drag.wayfire_output_id() else { return };
        let workspaces = drag.rendered_workspaces();
        if workspaces.is_empty() {
            return;
        }
        let ws_layout =
            workspace_icons_at(layout.ws.0, layout.ws.1, layout.cut_y, max_h, &workspaces);
        for (ws, rect) in ws_layout {
            if rect_contains(&rect, x, y) {
                if let Err(e) = wayfire::set_workspace(ws, output_id) {
                    eprintln!("sliver-dock: set-workspace({},{}): {e}", ws.x, ws.y);
                }
                return;
            }
        }
    });
    area.add_controller(gesture);
}

/// Apps-section tap: switch to the first row-major-empty workspace (so the
/// new toplevel maps there), then spawn the entry's `Exec` line via
/// `glib::spawn_command_line_async` (which handles shell-quoting and reaps
/// the child via the GLib mainloop child-watch). If no workspace is empty,
/// stay put — Wayfire will map the new view onto the focused workspace.
///
/// Terminal entry (id == basename of `cfg.terminal.command`) is special:
/// instead of one process, we spawn `cfg.terminal.count`, queueing a
/// per-process slot rectangle so each `view-mapped` event consumes one
/// slot and lays the view out via `configure-view`. Re-tap re-focuses
/// the saved terminal workspace if the terminal is still running.
fn handle_app_tap(drag: &Rc<Drag>, entry: &AppEntry) {
    if entry.id == terminal_app_id(drag.terminal()) {
        handle_terminal_tap(drag);
        return;
    }

    let Some(exec) = entry.exec.as_deref() else {
        eprintln!("sliver-dock: app {:?} has no Exec; nothing to launch", entry.id);
        return;
    };

    // `grid` is the full wayfire grid (1×1 in degraded mode); placement
    // logic only runs with a real output id, so the no-compositor path is
    // untouched. Still keyed off `opened_workspaces` — NOT the rendered
    // row — per slice 004 (the render set is the whole grid, which would
    // make `first_free_workspace` always return None).
    if let Some(output_id) = drag.wayfire_output_id() {
        let opened = drag.opened_workspaces();
        if let Some(target) = first_free_workspace(drag.grid(), &opened)
            && target != drag.current_workspace()
                && let Err(e) = wayfire::set_workspace(target, output_id) {
                    eprintln!(
                        "sliver-dock: set-workspace({},{}): {e}",
                        target.x, target.y
                    );
                }
    }

    let cmdline = apps::strip_exec_field_codes(exec);
    match glib::spawn_command_line_async(&cmdline) {
            Ok(()) => {
                drag.mark_launching(&entry.id);
                drag.increment_launch_count(&entry.id);
            }
        Err(e) => eprintln!("sliver-dock: spawn {cmdline:?}: {e}"),
    }
}

/// Terminal app-id resolution: prefer the explicit `terminal.app_id`
/// config override; otherwise derive the basename of the command (strips
/// `/usr/bin/` prefix and trailing args). The override exists for
/// terminals whose advertised xdg-toplevel app-id doesn't match their
/// binary name (e.g. some wezterm builds report
/// `org.wezfurlong.wezterm`).
fn terminal_app_id(term: &crate::config::Terminal) -> &str {
    if let Some(ref id) = term.app_id {
        return id.as_str();
    }
    let head = term
        .command
        .split_whitespace()
        .next()
        .unwrap_or(term.command.as_str());
    head.rsplit('/').next().unwrap_or(head)
}

/// Re-focus the existing terminal workspace if it's still alive, otherwise
/// pick a free workspace, queue per-view slot geometries, switch focus, and
/// spawn N terminal processes in succession.
fn handle_terminal_tap(drag: &Rc<Drag>) {
    let Some(output_id) = drag.wayfire_output_id() else { return };
    let term_id = terminal_app_id(drag.terminal());

    if let Some(saved) = drag.terminal_workspace()
        && drag.running_apps().contains(term_id) {
            if let Err(e) = wayfire::set_workspace(saved, output_id) {
                eprintln!(
                    "sliver-dock: terminal re-focus set-workspace({},{}): {e}",
                    saved.x, saved.y
                );
            }
            return;
        }

    let grid = drag.grid();
    let opened = drag.opened_workspaces();
    let target = first_free_workspace(grid, &opened).unwrap_or_else(|| drag.current_workspace());
    let Some(output_size) = drag.output_size() else {
        eprintln!("sliver-dock: terminal launch deferred: output size not yet known");
        return;
    };

    let count = drag.terminal().count.max(1);
    drag.enqueue_terminal_slots(terminal_layout_slots(count, output_size));
    drag.set_terminal_workspace(Some(target));

    if target != drag.current_workspace()
        && let Err(e) = wayfire::set_workspace(target, output_id) {
            eprintln!(
                "sliver-dock: terminal set-workspace({},{}): {e}",
                target.x, target.y
            );
        }

    let cmdline = drag.terminal().command.clone();
    let mut any_spawn = false;
    for _ in 0..count {
        match glib::spawn_command_line_async(&cmdline) {
            Ok(()) => any_spawn = true,
            Err(e) => eprintln!("sliver-dock: terminal spawn {cmdline:?}: {e}"),
        }
    }
    if any_spawn {
        drag.mark_launching(term_id);
        drag.increment_launch_count(term_id);
    }
}

/// Per-view slot rectangles for the terminal layout. Coords are output-
/// relative (0..output_w, 0..output_h). Layouts:
///   * 1 — single fullscreen pane.
///   * 2 — left half / right half.
///   * 3 — left full-height, top-right, bottom-right (matches v1-plan).
///   * else — uniform horizontal split fallback.
fn terminal_layout_slots(count: u32, (ow, oh): (i32, i32)) -> Vec<(i32, i32, i32, i32)> {
    let count = count.max(1) as i32;
    if ow <= 0 || oh <= 0 {
        return Vec::new();
    }
    match count {
        1 => vec![(0, 0, ow, oh)],
        2 => {
            let half = ow / 2;
            vec![(0, 0, half, oh), (half, 0, ow - half, oh)]
        }
        3 => {
            let half_w = ow / 2;
            let half_h = oh / 2;
            vec![
                (0, 0, half_w, oh),
                (half_w, 0, ow - half_w, half_h),
                (half_w, half_h, ow - half_w, oh - half_h),
            ]
        }
        n => {
            let slot_w = ow / n;
            (0..n)
                .map(|i| {
                    let x = i * slot_w;
                    let w = if i == n - 1 { ow - x } else { slot_w };
                    (x, 0, w, oh)
                })
                .collect()
        }
    }
}

/// Lowest row-major workspace not in the opened set. `None` if every cell
/// in the grid has at least one mapped toplevel.
fn first_free_workspace(grid: GridSize, opened: &HashSet<WorkspaceId>) -> Option<WorkspaceId> {
    for y in 0..grid.height {
        for x in 0..grid.width {
            let w = WorkspaceId { x, y };
            if !opened.contains(&w) {
                return Some(w);
            }
        }
    }
    None
}


fn rounded_rect_path(ctx: &CairoContext, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0);
    ctx.new_sub_path();
    ctx.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    ctx.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    ctx.arc(x + r, y + h - r, r, PI / 2.0, PI);
    ctx.arc(x + r, y + r, r, PI, 1.5 * PI);
    ctx.close_path();
}

/// Bottom trapezoid half of the spear. Long edge at `cut_y` (extends to the
/// tip points), short edge at `bot_y` (rectangle-body span). The lower-half
/// tip curves come straight from the original spear path; we just walk them
/// in reverse to close back at the cut edge.
fn bottom_half_path(
    ctx: &CairoContext,
    x: f64,
    w: f64,
    bar_h: f64,
    cut_y: f64,
    bot_y: f64,
    slope: f64,
) {
    let (left_tip, left_inner, right_inner, right_tip, off_x, off_y) =
        spear_geom(x, w, bar_h, slope);

    ctx.move_to(left_tip, cut_y);
    ctx.curve_to(
        left_tip,
        cut_y + off_y,
        left_inner - off_x,
        bot_y,
        left_inner,
        bot_y,
    );
    ctx.line_to(right_inner, bot_y);
    ctx.curve_to(
        right_inner + off_x,
        bot_y,
        right_tip,
        cut_y + off_y,
        right_tip,
        cut_y,
    );
    ctx.line_to(left_tip, cut_y);
    ctx.close_path();
}

/// Tip length per unit of spear height — the "6×" ratio the whole silhouette
/// keys off. A fused 12px sliver pair therefore tips at 72px, a lone 6px
/// sliver at 36px (which is also the astro rect's side indent).
const SPEAR_TIP_RATIO: f64 = 6.0;

/// A closed spear outline with an explicitly placed tip.
///
/// Generalizes [`spear_geom`]'s symmetric shape: `tip_y` need not be the
/// vertical middle, so one call draws a lone sliver (tip centred), a fused
/// half (tip on the shared edge, degenerating that side's curve into the
/// straight junction line), or anything between. The bezier handles scale with
/// each side's own distance to the tip, so a degenerate side stays flat
/// instead of bulging. That continuity is what lets the fused pair split
/// without popping — see [`SliverStack`].
#[allow(clippy::too_many_arguments)]
fn spear_path(
    ctx: &CairoContext,
    x: f64,
    w: f64,
    top: f64,
    bot: f64,
    tip_len: f64,
    tip_y: f64,
    slope: f64,
) {
    let (left_tip, right_tip) = (x, x + w);
    let (left_inner, right_inner) = (x + tip_len, x + w - tip_len);
    let off_x = slope * tip_len;
    let off_top = slope * (tip_y - top);
    let off_bot = slope * (bot - tip_y);

    ctx.move_to(left_inner, top);
    ctx.line_to(right_inner, top);
    ctx.curve_to(right_inner + off_x, top, right_tip, tip_y - off_top, right_tip, tip_y);
    ctx.curve_to(right_tip, tip_y + off_bot, right_inner + off_x, bot, right_inner, bot);
    ctx.line_to(left_inner, bot);
    ctx.curve_to(left_inner - off_x, bot, left_tip, tip_y + off_bot, left_tip, tip_y);
    ctx.curve_to(left_tip, tip_y - off_top, left_inner - off_x, top, left_inner, top);
    ctx.close_path();
}

fn spear_geom(x: f64, w: f64, bar_h: f64, slope: f64) -> (f64, f64, f64, f64, f64, f64) {
    let tip_len = bar_h * SPEAR_TIP_RATIO;
    let off_x = slope * tip_len;
    let off_y = slope * (bar_h / 2.0);
    (x, x + tip_len, x + w - tip_len, x + w, off_x, off_y)
}

fn load_css() {
    let Some(display) = gdk::Display::default() else { return };

    // Baseline rule — keep the layer-shell window transparent so only the
    // Cairo-drawn spear is visible. Applied unconditionally; user CSS layers
    // on top.
    let baseline = CssProvider::new();
    baseline.load_from_string("window.sliver-dock { background: transparent; }");
    gtk4::style_context_add_provider_for_display(
        &display,
        &baseline,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // User stylesheet rendered by toml-bombadil at $XDG_CONFIG_HOME/
    // sliver-dock/style.css (source: dotfiles/src/sliver-dock/style.css).
    // ENOENT-tolerant — popovers and friends fall back to default GTK
    // theme without it.
    if let Some(path) = user_css_path()
        && path.is_file() {
            let provider = CssProvider::new();
            provider.load_from_path(&path);
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_USER,
            );
        }
}

fn user_css_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("sliver-dock").join("style.css"))
}

pub(crate) fn current_screen_size() -> Option<(i32, i32)> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    let item = monitors.item(0)?;
    let monitor = item.downcast::<gdk::Monitor>().ok()?;
    let geom = monitor.geometry();
    Some((geom.width(), geom.height()))
}

/// The sliver-edge zone's gesture arms per state — the zone is registered
/// with different directions because its role changes with the state:
///
/// * `Closed` — **unidir down** (`down` → Hidden). The up-grid arm is
///   dropped here: that was the `Closed → AppGrid` coin-flip the user
///   locked out on 2026-09-01, and a unidir zone means an upward wiggle on
///   the strip never even commits (touch-drawer's unidir commit test is
///   signed, `dy >= need` for down), so the touch falls through instead of
///   committing to a no-op and swallowing the gesture. The down arm must
///   stay: "a downward swipe anywhere on any dock-owned surface steps one
///   state back" includes the sliver strip in Closed, so disabling the
///   zone outright would regress Hidden. No shadowing risk vs `BAR_ZONE`'s
///   down arm — the two rects are disjoint in Closed (6px apart) and, even
///   if they ever overlapped, both down arms map to the same
///   `(Closed, Down) → Hidden` transition and `BAR_ZONE` registers first,
///   so classify() would hand it the touch either way.
/// * `Detailed` — **bidir** (`up-grid` → AppGrid from the astro bar,
///   `down` → Closed). The only state that carries an up-grid arm.
/// * `AppGrid` — **unidir down** (`down` → Detailed). The sliver is the
///   astro view's close handle up at the top border; the up-grid arm is a
///   no-op from AppGrid, so dropping it stops an upward wiggle being
///   grabbed-and-swallowed for nothing.
///
/// `Hidden` is excluded from the table — the zone is disabled there, and
/// its arms are irrelevant; a settle in Hidden leaves whatever arms were
/// last registered in place, which the next Closed/Detailed/AppGrid settle
/// corrects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SliverArms {
    /// `register … down … "down"` — the down-only grab handle.
    UnidirDown,
    /// `register … bidir … "up-grid" "down"` — astro bar in Detailed.
    BidirUpGridDown,
}

fn sliver_arms_for(state: BarState) -> SliverArms {
    match state {
        BarState::Detailed => SliverArms::BidirUpGridDown,
        BarState::Closed | BarState::AppGrid | BarState::Hidden => SliverArms::UnidirDown,
    }
}

thread_local! {
    /// Last sliver-edge arm signature actually registered. Deliberately
    /// failure-aware like `zones.rs`'s LAST_ENABLED/LAST_BOUNDS: recorded
    /// only when the register write reached the plugin, so a plugin that
    /// was down mid-flight gets re-registered on the next settle and
    /// self-heals. Not seeded at boot — `register_zones` goes through the
    /// same helper and records the seed on success.
    static LAST_SLIVER_ARMS: std::cell::Cell<Option<SliverArms>> =
        const { std::cell::Cell::new(None) };
}

/// Register (or re-register) the sliver-edge zone with the arms its state
/// demands — see [`SliverArms`]. Re-registration replaces the zone in
/// place on the plugin side (touch-drawer's `upsert_zone`), so the
/// registration-order position is preserved. Called from `register_zones`
/// at boot and from `drag::update_zone_states` on every state settle;
/// deduped on the arm signature so a settle that keeps the state's arms
/// costs nothing.
pub(crate) fn sync_sliver_edge_zone(sw: i32, sh: i32, geom: &Geometry, state: BarState) {
    let arms = sliver_arms_for(state);
    if LAST_SLIVER_ARMS.with(|c| c.get()) == Some(arms) {
        return;
    }
    let (x, y, w, h) = sliver_edge_zone_bounds(sw, sh, geom, state);
    let reply = ipc::socket_path();
    let enabled = drag::zone_enabled(state, drag::SLIVER_EDGE_ZONE_ID);
    let sent = match arms {
        SliverArms::BidirUpGridDown => zones::register_bidir(
            drag::SLIVER_EDGE_ZONE_ID,
            x,
            y,
            w,
            h,
            &reply,
            "up-grid",
            "down",
            enabled,
            Some(h),
        ),
        SliverArms::UnidirDown => zones::register(
            drag::SLIVER_EDGE_ZONE_ID,
            x,
            y,
            w,
            h,
            zones::Direction::Down,
            &reply,
            "down",
            enabled,
            Some(h),
        ),
    };
    if sent {
        LAST_SLIVER_ARMS.with(|c| c.set(Some(arms)));
    }
}

fn register_zones(drag: &Rc<Drag>, geom: &Geometry) {
    // Registration-order contract: touch-drawer's `classify()` linear-scans
    // the registry and returns the FIRST enabled zone whose rect contains
    // the touch-down point (touch-drawer/src/main.cpp:273). Overlapping
    // zones are therefore resolved by registration order — narrow/specific
    // zones must be registered BEFORE broad ones, or the specific gesture
    // silently stops firing. Current shape, narrow → broad:
    //   BOTTOM_EDGE → BAR_ZONE → SLIVER_EDGE → APPS → DETAILED.
    // The apps zone (slice 002) sits INSIDE the broad detailed zone in
    // Detailed, so it must precede it or up-swipes over the apps section
    // would resolve to the broad down-only zone and never commit. A future
    // re-order changes gesture routing without a compile error, so edit
    // this order deliberately.
    let Some((sw, sh)) = current_screen_size() else {
        eprintln!("sliver-dock: no monitor 0; touch-drawer zones not registered");
        return;
    };
    let reply = ipc::socket_path();
    let initial = drag.state();

    // travel_px policy — decided deliberately in slice 003, kept from
    // slices 001–002: every bounded zone keeps `travel_px = zone height`
    // (only the full-screen bottom-edge band uses touch-drawer's default
    // `screen_height / 3`). Two reasons, one negative, one positive:
    //
    //   * The midpoint must stay REACHABLE. A denominator larger than the
    //     zone caps p at `zone / denominator`: a uniform denominator (e.g.
    //     screen_height/3 ≈ 284 on the 107px detailed zone) would cap p at
    //     ~0.38, below the 0.5 stationary tier — a slow drag could never
    //     commit and close-by-slow-drag would be impossible.
    //   * Progress must TRACK the finger 1:1 inside the zone's own extent.
    //     The live drag visual lerps on p; zone-relative progress is the
    //     only denominator that feels like the finger is pushing the dock.
    //
    // The asymmetry this leaves — a 10px commit is p≈0.48 on the 21px
    // sliver edge but p≈0.09 on the detailed zone — only affects the
    // sub-fling tier. Quick-flick decisiveness is the fling tier's job,
    // and the fling test is zone-independent (velocity only). On a 21px
    // handle a committed drag IS a near-full traversal, so near-saturated
    // progress is correct there, not a defect.

    // Wide bottom-edge band — used to unhide from Hidden state. No
    // travel_px override; the unhide gesture has the full screen to play
    // with, so the default screen_height/3 denominator is appropriate.
    zones::register(
        drag::BOTTOM_EDGE_ZONE_ID,
        0,
        sh - BOTTOM_EDGE_BAND_HEIGHT,
        sw,
        BOTTOM_EDGE_BAND_HEIGHT,
        zones::Direction::Up,
        &reply,
        "up",
        drag::zone_enabled(initial, drag::BOTTOM_EDGE_ZONE_ID),
        None,
    );

    // Bar-overlay zone — covers the bar plus the margin beneath it. Used in
    // Closed state; up triggers Closed → Detailed, down triggers Closed →
    // Hidden. Bidir; commit decision happens once touch-drawer sees motion
    // past commit_threshold in either direction. travel_px = zone height
    // so a swipe across the bar's vertical extent saturates progress.
    let (bx, by, bw, bh) = bar_zone_bounds(sw, sh, geom);
    zones::register_bidir(
        drag::BAR_ZONE_ID,
        bx,
        by,
        bw,
        bh,
        &reply,
        "up",
        "down",
        drag::zone_enabled(initial, drag::BAR_ZONE_ID),
        Some(bh),
    );

    // Sliver-edge zone — the astro sliver's grab target. Arms are
    // state-dependent (see `sync_sliver_edge_zone` / `SliverArms`):
    // bidir (up-grid → AppGrid / down → close) in Detailed, unidir down
    // everywhere else. travel_px = zone height so a short swipe saturates
    // progress. Registered through the same helper the settle path uses,
    // so the dedupe seed and the re-register can never disagree.
    //
    // Registered BEFORE the detailed zone: in AppGrid the sliver-edge strip
    // sits inside the astro body covered by the (extended) detailed zone, so
    // narrow-first ordering keeps the strip's own routing authoritative.
    sync_sliver_edge_zone(sw, sh, geom, initial);

    // Apps-section zone (slice 002) — the quick-launcher apps section's
    // rect in Detailed. Up-swipe over it opens the app-grid; down closes
    // like anywhere else. Bidir; travel_px = zone height. Registered
    // BEFORE the broad detailed zone so the up-grid arm wins inside the
    // section (registration-order contract above).
    let (ax, ay, aw, ah) = apps_zone_bounds(sw, sh, geom);
    zones::register_bidir(
        drag::APPS_ZONE_ID,
        ax,
        ay,
        aw,
        ah,
        &reply,
        "up-grid",
        "down",
        drag::zone_enabled(initial, drag::APPS_ZONE_ID),
        Some(ah),
    );

    // Detailed-view zone — covers the full visible expanded window
    // (bar + margin + detailed content) in Detailed, and the whole astro
    // body in AppGrid (state-dependent bounds, re-registered on every
    // settle by `drag::update_zone_states`). **Unidir down** (slice 002):
    // the close gesture (Detailed → Closed, AppGrid → Detailed) is the
    // only arm; up anywhere in the zone never commits at all, which is
    // the locked no-op for up-swipes over the workspaces/info sections
    // and the bar body. travel_px = zone height so the close-gesture
    // progress maxes out within the visible window.
    let (dx, dy, dw, dh) = detailed_zone_bounds(sw, sh, geom, initial);
    zones::register(
        drag::DETAILED_ZONE_ID,
        dx,
        dy,
        dw,
        dh,
        zones::Direction::Down,
        &reply,
        "down",
        drag::zone_enabled(initial, drag::DETAILED_ZONE_ID),
        Some(dh),
    );
}

fn update_zone_bounds(geom: &Geometry, state: BarState) {
    let Some((sw, sh)) = current_screen_size() else { return };
    zones::update_bounds(
        drag::BOTTOM_EDGE_ZONE_ID,
        0,
        sh - BOTTOM_EDGE_BAND_HEIGHT,
        sw,
        BOTTOM_EDGE_BAND_HEIGHT,
    );
    let (bx, by, bw, bh) = bar_zone_bounds(sw, sh, geom);
    zones::update_bounds(drag::BAR_ZONE_ID, bx, by, bw, bh);
    // Detailed + sliver-edge zones are state-dependent (the detailed zone
    // extends over the astro body in AppGrid; the sliver rides the detailed
    // lift) — mirror the state passed in. The apps zone's bounds are
    // state-independent (the window is sized once) but re-derived here too,
    // so a monitor change re-anchors everything in one place.
    let (dx, dy, dw, dh) = detailed_zone_bounds(sw, sh, geom, state);
    zones::update_bounds(drag::DETAILED_ZONE_ID, dx, dy, dw, dh);
    let (sx, sy, sw_z, sh_z) = sliver_edge_zone_bounds(sw, sh, geom, state);
    zones::update_bounds(drag::SLIVER_EDGE_ZONE_ID, sx, sy, sw_z, sh_z);
    let (ax, ay, aw, ah) = apps_zone_bounds(sw, sh, geom);
    zones::update_bounds(drag::APPS_ZONE_ID, ax, ay, aw, ah);
}

/// Bar zone covers the bar plus the margin band beneath it, centered on
/// the same horizontal extent as the visible spear. Derived in surface
/// coordinates (the bar is bottom-anchored at `win_h - margin`) and
/// converted through [`surface_rect_to_screen`] — same anchor as the draw
/// path, never a constant-derived screen offset.
fn bar_zone_bounds(sw: i32, sh: i32, geom: &Geometry) -> (i32, i32, i32, i32) {
    let bar_w = geom.width_px.min(sw);
    let x = ((sw - bar_w) / 2) as f64;
    let win_h = window_height_px(sh, geom);
    let y_top = win_h as f64 - geom.margin_bottom_px as f64 - geom.default_height_px as f64;
    let h = (geom.margin_bottom_px + geom.default_height_px) as f64;
    surface_rect_to_screen(sh, geom, x, y_top, bar_w as f64, h)
}

/// Detailed zone covers the visible expanded window — bar + margin +
/// detailed content height — centered horizontally on the spear. Same
/// horizontal extent as the bar zone, taller.
///
/// **State-dependent** (mirroring [`sliver_edge_zone_bounds`]): in
/// `AppGrid` the zone covers the whole astro body — that is the
/// down-close gesture's target surface in AppGrid, and the
/// bottom-anchored dock-stack bounds would not reach a single pixel of
/// it. Derived in surface coordinates (the astro rect spans the surface
/// top to `win_h - D`) and converted through [`surface_rect_to_screen`].
pub(crate) fn detailed_zone_bounds(
    sw: i32,
    sh: i32,
    geom: &Geometry,
    state: BarState,
) -> (i32, i32, i32, i32) {
    let bar_w = geom.width_px.min(sw);
    let x = ((sw - bar_w) / 2) as f64;

    if matches!(state, BarState::AppGrid) {
        // Astro body: surface `[0, win_h - D]` → screen `[D, h-D]`.
        let win_h = window_height_px(sh, geom);
        let d = dock_stack_px(geom);
        let h = (win_h - d).max(0) as f64;
        return surface_rect_to_screen(sh, geom, x, 0.0, bar_w as f64, h);
    }

    let win_h = window_height_px(sh, geom);
    let h = (geom.margin_bottom_px + geom.default_height_px + geom.detailed_content_height_px)
        as f64;
    let y_top = win_h as f64 - h;
    surface_rect_to_screen(sh, geom, x, y_top, bar_w as f64, h)
}

/// Sliver-edge zone — the astro sliver's grab target. Full bar width,
/// centered horizontally, and **state-dependent**, because the astro sliver
/// moves in every state:
///
/// * `Closed` — fused on the dock sliver; zone covers it plus a 15px
///   run-up band above.
/// * `Detailed` — the fused pair has ridden up by `detailed_content_height_px`
///   to cap the detailed view; the zone lifts with it.
/// * `AppGrid` — the astro sliver has travelled to the top border at screen
///   `y = D`; the close gesture (swipe-down) lives up there on it, not on the
///   dock sliver left behind at the bottom (note 010 §Transition).
///
/// Must be re-registered on every state settle (see `drag::update_zone_states`)
/// — `update_zone_bounds` alone only fires on map + monitor-change. The
/// zone's *arms* are state-dependent as well; see [`sync_sliver_edge_zone`].
pub(crate) fn sliver_edge_zone_bounds(
    sw: i32,
    sh: i32,
    geom: &Geometry,
    state: BarState,
) -> (i32, i32, i32, i32) {
    let bar_w = geom.width_px.min(sw);
    let x = ((sw - bar_w) / 2) as f64;
    const RUN_UP_PX: i32 = 15;

    if matches!(state, BarState::AppGrid) {
        // Astro rect top in screen coords = D. The sliver occupies its first
        // `default_height_px`; the run-up band hangs below it, inside the
        // view. Surface-anchored: the sliver sits at the surface top.
        return surface_rect_to_screen(
            sh,
            geom,
            x,
            0.0,
            bar_w as f64,
            (geom.default_height_px + RUN_UP_PX) as f64,
        );
    }

    // Top of the fused pair = above the main bar, the dock sliver, and the
    // astro sliver stacked on it (surface coords; the lift only applies in
    // Detailed — see `sliver_stack_top_surface`).
    let win_h = window_height_px(sh, geom);
    let fused_top = sliver_stack_top_surface(win_h, geom, state);
    surface_rect_to_screen(
        sh,
        geom,
        x,
        fused_top - RUN_UP_PX as f64,
        bar_w as f64,
        (RUN_UP_PX + geom.default_height_px) as f64,
    )
}

/// Apps-section zone — the narrow rect that routes an up-swipe to the
/// app-grid from inside the quick-launcher apps section in `Detailed`.
/// Derived from [`section_bounds`] (surface coords): the section content
/// extends **up** from `cut_y` by `detailed_content_height_px`, so the
/// zone can never drift from what is painted. Converted to screen coords
/// through [`surface_rect_to_screen`].
///
/// State-independent bounds: the window is sized once at startup and does
/// not change with the bar state, and the zone is only *enabled* in
/// `Detailed` (`drag::intended_zones`).
pub(crate) fn apps_zone_bounds(sw: i32, sh: i32, geom: &Geometry) -> (i32, i32, i32, i32) {
    let win_h = window_height_px(sh, geom);
    let layout = section_bounds(sw as f64, win_h as f64, geom);
    let y_top = layout.cut_y - geom.detailed_content_height_px as f64;
    surface_rect_to_screen(
        sh,
        geom,
        layout.apps.0,
        y_top,
        layout.apps.1,
        geom.detailed_content_height_px as f64,
    )
}

/// 1Hz redraw loop for the clock. Always running — `queue_draw` is cheap
/// even when the section is hidden (Closed state's draw early-returns
/// without painting the info content). Saves us from coupling the clock
/// to state transitions.
fn spawn_clock_tick(area: &DrawingArea) {
    let area = area.clone();
    glib::timeout_add_seconds_local(1, move || {
        area.queue_draw();
        glib::ControlFlow::Continue
    });
}

fn watch_monitor_changes(drag: Rc<Drag>, geom: Geometry) {
    let Some(display) = gdk::Display::default() else { return };
    let monitors = display.monitors();
    let Some(item) = monitors.item(0) else { return };
    let Ok(monitor) = item.downcast::<gdk::Monitor>() else { return };
    monitor.connect_geometry_notify(move |_| {
        update_zone_bounds(&geom, drag.state());
    });
}

// Compile-time guard: visibility flag tracked elsewhere should match
// rendering. The `Cell<bool>` is unused for now but kept here to mirror
// swipe-panel's pattern in case Phase 3 needs cross-handler state.
#[allow(dead_code)]
fn _visibility_guard() -> Rc<Cell<bool>> {
    Rc::new(Cell::new(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backdrop;
    use crate::state::WorkspaceId;
    use std::collections::HashSet;

    // ---- slice 006: spatial projection (content box + cell projection) ----

    /// The real device geometry: icon 41×41, output 1280×853.
    fn icon() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 41.0, h: 41.0 }
    }

    fn landscape() -> (i32, i32) {
        (1280, 853)
    }

    fn portrait() -> (i32, i32) {
        (853, 1280)
    }

    fn view(x: i32, y: i32, w: i32, h: i32) -> ViewRect {
        ViewRect { x, y, w, h }
    }

    #[test]
    fn content_box_landscape_letterboxes_vertically() {
        // 1280×853 ≈ 1.5:1 inside the 41×41 icon → ~41×27.3 centred
        // vertically: full width, bars top and bottom, no horizontal bars.
        let boxed = content_box(icon(), landscape()).unwrap();
        assert!((boxed.w - 41.0).abs() < 1e-9);
        assert!((boxed.h - 27.32265625).abs() < 1e-6, "got h={}", boxed.h);
        assert!((boxed.x - 0.0).abs() < 1e-9);
        assert!(boxed.y > 0.0 && boxed.y < 41.0 - boxed.h);
        // Centred vertically: equal bars above and below.
        assert!((boxed.y - (41.0 - boxed.h - boxed.y)).abs() < 1e-9);
    }

    #[test]
    fn content_box_portrait_tall_narrow() {
        // 853×1280 (rotated) → the box flips: full height, narrow width,
        // centred horizontally — not clipped, not squashed to landscape.
        let boxed = content_box(icon(), portrait()).unwrap();
        assert!((boxed.h - 41.0).abs() < 1e-9);
        assert!((boxed.w - 27.32265625).abs() < 1e-6, "got w={}", boxed.w);
        assert!((boxed.y - 0.0).abs() < 1e-9);
        assert!((boxed.x - (41.0 - boxed.w - boxed.x)).abs() < 1e-9);
    }

    #[test]
    fn content_box_square_output_fills_icon() {
        let boxed = content_box(icon(), (1000, 1000)).unwrap();
        assert_eq!(boxed.w, 41.0);
        assert_eq!(boxed.h, 41.0);
    }

    #[test]
    fn content_box_bad_output_none() {
        assert!(content_box(icon(), (0, 853)).is_none());
        assert!(content_box(icon(), (1280, 0)).is_none());
        assert!(content_box(icon(), (-1, 853)).is_none());
    }

    #[test]
    fn project_side_by_side_halves() {
        // The live reference values (2026-09-01): two foot windows side by
        // side, `(5,5,629,843)` and `(644,5,631,843)`.
        let content = content_box(icon(), landscape()).unwrap();
        let left = project_view_cell(view(5, 5, 629, 843), landscape(), content).unwrap();
        let right = project_view_cell(view(644, 5, 631, 843), landscape(), content).unwrap();

        // Each ≈ half the content-box width, (almost) full height.
        let half = content.w / 2.0;
        assert!((left.w - half).abs() < 1.0, "left.w={}", left.w);
        assert!((right.w - half).abs() < 1.0, "right.w={}", right.w);
        assert!(left.h > content.h * 0.98);
        assert!(right.h > content.h * 0.98);
        // Left cell strictly left of the right cell; both inside the box.
        assert!(left.x + left.w <= right.x + 1e-9);
        assert!(left.x >= content.x - 1e-9);
        assert!(right.x + right.w <= content.x + content.w + 1e-9);
    }

    #[test]
    fn project_three_terminal_figure() {
        // The user's canonical figure: full-height half-width left; two
        // stacked half-height half-width right. The vertical seam must sit
        // at the content box's horizontal midpoint and the horizontal seam
        // at its vertical midpoint, on the right half only.
        let content = content_box(icon(), landscape()).unwrap();
        let left = project_view_cell(view(0, 0, 640, 853), landscape(), content).unwrap();
        let top = project_view_cell(view(640, 0, 640, 426), landscape(), content).unwrap();
        let bottom = project_view_cell(view(640, 427, 640, 426), landscape(), content).unwrap();

        let mid_x = content.x + content.w / 2.0;
        let mid_y = content.y + content.h / 2.0;

        // Left: full height at half width, ending exactly at the midline.
        assert!((left.w - content.w / 2.0).abs() < 1e-9);
        assert!((left.h - content.h).abs() < 1e-9);
        assert!((left.x + left.w - mid_x).abs() < 1e-9);

        // Right cells: each half width, half height, stacked.
        for cell in [top, bottom] {
            assert!((cell.w - content.w / 2.0).abs() < 1e-9);
            assert!((cell.h - content.h / 2.0).abs() < 0.05, "cell.h={}", cell.h);
            assert!((cell.x - mid_x).abs() < 1e-9);
        }
        // Top: from the box top down to the midline. Bottom: midline to box
        // bottom. 427/853 is not exactly ½, so allow one px of slack.
        assert!((top.y - content.y).abs() < 1e-9);
        assert!((top.y + top.h - mid_y).abs() < 0.1);
        assert!((bottom.y - mid_y).abs() < 0.1);
        assert!((bottom.y + bottom.h - (content.y + content.h)).abs() < 1e-9);
    }

    #[test]
    fn project_single_window_fills_content() {
        let content = content_box(icon(), landscape()).unwrap();
        let cell = project_view_cell(view(0, 0, 1280, 853), landscape(), content).unwrap();
        assert!((cell.x - content.x).abs() < 1e-9);
        assert!((cell.y - content.y).abs() < 1e-9);
        assert!((cell.w - content.w).abs() < 1e-9);
        assert!((cell.h - content.h).abs() < 1e-9);
    }

    #[test]
    fn project_portrait_fullscreen_fills_tall_box() {
        // Rotated output: a fullscreen window fills the tall narrow box.
        let content = content_box(icon(), portrait()).unwrap();
        let cell = project_view_cell(view(0, 0, 853, 1280), portrait(), content).unwrap();
        assert!((cell.x - content.x).abs() < 1e-9);
        assert!((cell.y - content.y).abs() < 1e-9);
        assert!((cell.w - content.w).abs() < 1e-9);
        assert!((cell.h - content.h).abs() < 1e-9);
    }

    #[test]
    fn project_clamps_oversized_view() {
        // Fullscreen-with-margins (or a view briefly exceeding the output):
        // the unit rect clamps to [0,1] — the cell is the whole content
        // box, never larger.
        let content = content_box(icon(), landscape()).unwrap();
        let cell = project_view_cell(view(-100, -50, 1400, 1000), landscape(), content).unwrap();
        assert!((cell.x - content.x).abs() < 1e-9);
        assert!((cell.y - content.y).abs() < 1e-9);
        assert!((cell.w - content.w).abs() < 1e-9);
        assert!((cell.h - content.h).abs() < 1e-9);
    }

    #[test]
    fn project_skips_zero_area_rects() {
        let content = content_box(icon(), landscape()).unwrap();
        assert!(project_view_cell(view(5, 5, 0, 100), landscape(), content).is_none());
        assert!(project_view_cell(view(5, 5, 100, 0), landscape(), content).is_none());
        // Negative w/h (malformed geometry) also skip — never NaN.
        assert!(project_view_cell(view(5, 5, -3, 100), landscape(), content).is_none());
    }

    #[test]
    fn project_skips_fully_off_frame_rects() {
        let content = content_box(icon(), landscape()).unwrap();
        // Fully to the right of the output.
        assert!(project_view_cell(view(2000, 5, 100, 100), landscape(), content).is_none());
        // Fully above the output (clamped to zero height).
        assert!(project_view_cell(view(5, -200, 100, 100), landscape(), content).is_none());
    }

    #[test]
    fn project_bad_frame_none() {
        let content = content_box(icon(), landscape()).unwrap();
        assert!(project_view_cell(view(0, 0, 100, 100), (0, 853), content).is_none());
        assert!(project_view_cell(view(0, 0, 100, 100), (1280, 0), content).is_none());
    }

    #[test]
    fn seam_cell_keeps_gutter_and_min_size() {
        let cell = Rect { x: 10.0, y: 20.0, w: 20.0, h: 27.0 };
        let seamed = seam_cell(cell, 1.0);
        assert!((seamed.x - 11.0).abs() < 1e-9);
        assert!((seamed.y - 21.0).abs() < 1e-9);
        assert!((seamed.w - 18.0).abs() < 1e-9);
        assert!((seamed.h - 25.0).abs() < 1e-9);
        // A sub-seam cell shrinks to 1×1, never negative.
        let tiny = seam_cell(Rect { x: 0.0, y: 0.0, w: 0.4, h: 3.0 }, 1.0);
        assert!(tiny.w >= 1.0 && tiny.h >= 1.0);
    }

    // ---- slice 005: fallback glyph (kept — the cell render still uses it)

    #[test]
    fn fallback_glyph_paints_pixels() {
        // The "never a blank cell" contract: even a failed icon lookup must
        // leave visible pixels in the cell.
        let mut surface = cairo::ImageSurface::create(cairo::Format::A8, 40, 40).unwrap();
        {
            let ctx = CairoContext::new(&surface).unwrap();
            draw_fallback_glyph(&ctx, Rect { x: 0.0, y: 0.0, w: 40.0, h: 40.0 }, 1.0);
        }
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let painted = (0..40).any(|y| (0..40).any(|x| data[y * stride + x] > 0));
        assert!(painted, "fallback glyph must paint pixels");
    }

    #[test]
    fn first_free_workspace_2x2_empty() {
        let grid = GridSize { width: 2, height: 2 };
        let opened = HashSet::new();
        let free = first_free_workspace(grid, &opened);
        assert_eq!(free, Some(WorkspaceId { x: 0, y: 0 }));
    }

    #[test]
    fn first_free_workspace_skips_opened() {
        let grid = GridSize { width: 2, height: 2 };
        let mut opened = HashSet::new();
        opened.insert(WorkspaceId { x: 0, y: 0 });
        let free = first_free_workspace(grid, &opened);
        assert_eq!(free, Some(WorkspaceId { x: 1, y: 0 }));
    }

    #[test]
    fn first_free_workspace_all_full() {
        let grid = GridSize { width: 2, height: 2 };
        let mut opened = HashSet::new();
        for x in 0..2 {
            for y in 0..2 {
                opened.insert(WorkspaceId { x, y });
            }
        }
        assert_eq!(first_free_workspace(grid, &opened), None);
    }

    #[test]
    fn terminal_layout_slots_counts() {
        assert_eq!(terminal_layout_slots(1, (800, 480)).len(), 1);
        assert_eq!(terminal_layout_slots(2, (800, 480)).len(), 2);
        assert_eq!(terminal_layout_slots(3, (800, 480)).len(), 3);
        assert_eq!(terminal_layout_slots(4, (800, 480)).len(), 4);
    }

    #[test]
    fn terminal_layout_slots_single_fullscreen() {
        let slots = terminal_layout_slots(1, (800, 480));
        assert_eq!(slots[0], (0, 0, 800, 480));
    }

    #[test]
    fn terminal_layout_slots_bad_output() {
        assert!(terminal_layout_slots(1, (0, 0)).is_empty());
        assert!(terminal_layout_slots(1, (-1, 480)).is_empty());
    }

    #[test]
    fn ordinal_suffix_basics() {
        assert_eq!(ordinal_suffix(1), "st");
        assert_eq!(ordinal_suffix(2), "nd");
        assert_eq!(ordinal_suffix(3), "rd");
        assert_eq!(ordinal_suffix(4), "th");
        assert_eq!(ordinal_suffix(21), "st");
        assert_eq!(ordinal_suffix(22), "nd");
        assert_eq!(ordinal_suffix(23), "rd");
    }

    #[test]
    fn ordinal_suffix_teens() {
        assert_eq!(ordinal_suffix(11), "th");
        assert_eq!(ordinal_suffix(12), "th");
        assert_eq!(ordinal_suffix(13), "th");
    }

    #[test]
    fn weekday_abbrevs() {
        assert_eq!(weekday_abbrev(time::Weekday::Monday), "Mon");
        assert_eq!(weekday_abbrev(time::Weekday::Friday), "Fri");
        assert_eq!(weekday_abbrev(time::Weekday::Sunday), "Sun");
    }

    #[test]
    fn spear_geom_values() {
        let (left_tip, left_inner, right_inner, right_tip, off_x, off_y) =
            spear_geom(0.0, 900.0, 6.0, 0.35);
        assert!((left_tip - 0.0).abs() < 1e-9);
        assert!((left_inner - 36.0).abs() < 1e-9);
        assert!((right_inner - 864.0).abs() < 1e-9);
        assert!((right_tip - 900.0).abs() < 1e-9);
        assert!((off_x - 12.6).abs() < 1e-9);
        assert!((off_y - 1.05).abs() < 1e-9);
    }

    #[test]
    fn rect_contains_inside() {
        let r = Rect { x: 10.0, y: 20.0, w: 30.0, h: 40.0 };
        assert!(rect_contains(&r, 10.0, 20.0));
        assert!(rect_contains(&r, 39.9, 59.9));
        assert!(rect_contains(&r, 25.0, 40.0));
    }

    #[test]
    fn rect_contains_outside() {
        let r = Rect { x: 10.0, y: 20.0, w: 30.0, h: 40.0 };
        assert!(!rect_contains(&r, 9.9, 20.0));
        assert!(!rect_contains(&r, 10.0, 19.9));
        assert!(!rect_contains(&r, 40.0, 20.0));
        assert!(!rect_contains(&r, 10.0, 60.0));
    }

    #[test]
    fn cpu_fill_color_ranges() {
        let (_r, g1, _b) = cpu_fill_color(30);
        assert!(g1 > 0.6);

        let (r60, _g, _b) = cpu_fill_color(60);
        assert!(r60 > 0.6);

        let (r90, _g, _b) = cpu_fill_color(90);
        assert!(r90 > 0.9);
    }

    #[test]
    fn battery_fill_color_charging() {
        let (_r, g, _b) = battery_fill_color(BatteryState::Charging, 80);
        assert!(g > 0.7);
    }

    #[test]
    fn battery_fill_color_low() {
        let (r, _g, _b) = battery_fill_color(BatteryState::Discharging, 10);
        assert!(r > 0.9);
    }

    #[test]
    fn row_layout_fits_all() {
        let rects = row_layout(0.0, 200.0, 200.0, 66.0, 3, 0.0);
        assert_eq!(rects.len(), 3);
    }

    #[test]
    fn row_layout_empty() {
        let rects = row_layout(0.0, 200.0, 200.0, 66.0, 0, 0.0);
        assert!(rects.is_empty());
    }

    // ---- slice 004: the workspaces row fits the full grid --------------

    #[test]
    fn workspaces_section_fits_the_full_grid_at_device_width() {
        // Device: width_px = 900, INFO_SECTION_W = 72, mid_gap 6 → the
        // workspaces section is (900 − 72 − 12)/2 = 408px wide. All four
        // 2×2 grid cells (4×41 + 3×8 = 188px) must fit with room to spare —
        // a future width_px / INFO_SECTION_W / ICON_SIZE change that would
        // start truncating workspaces fails loudly here.
        let g = Geometry::default();
        let main_w = section_bounds(1280.0, 746.0, &g).ws.1;
        assert_eq!(main_w, 408.0, "device workspaces section width drifted");
        let rects = row_layout(0.0, main_w, 200.0, 66.0, 4, 0.0);
        assert_eq!(rects.len(), 4, "all four 2×2 workspaces must fit");
        assert!(
            rects[3].x + rects[3].w <= main_w + 1e-9,
            "last icon must not overrun the section"
        );
        // max_fit pin: exactly 8 icons fit at 408px; a 9th truncates (the
        // 3×3 grid's ninth workspace would be clipped, not crash).
        assert_eq!(row_layout(0.0, main_w, 200.0, 66.0, 8, 0.0).len(), 8);
        assert_eq!(row_layout(0.0, main_w, 200.0, 66.0, 9, 0.0).len(), 8);
    }

    // ---- astro slice 2: sliver fusion + travel -------------------------

    /// Surface as slice 1 sizes it: `screen_h - D` for a 1280x853 panel.
    const SURFACE_W: f64 = 1280.0;
    const SURFACE_H: f64 = 746.0;

    fn visual(detailed: f64, grid: f64) -> Visual {
        Visual { hidden_amount: 0.0, detailed_amount: detailed, grid_amount: grid }
    }

    #[test]
    fn sliver_stack_fused_when_astro_closed() {
        let g = Geometry::default();
        let bar_h = g.default_height_px as f64;
        for detailed in [0.0, 1.0] {
            let s = sliver_stack(SURFACE_W, SURFACE_H, &g, &visual(detailed, 0.0));
            assert!(s.fused, "detailed={detailed}");
            // Astro sliver, dock sliver and the bar's top half are adjacent —
            // no gap to reveal the backdrop through.
            assert!((s.astro.clip_bot - s.lower.clip_top).abs() < 1e-9, "detailed={detailed}");
            assert!((s.lower_bot - s.lower.clip_top - 1.5 * bar_h).abs() < 1e-9);
            // One shape: `assembly` spans all three, 2.5 slivers tall, and its
            // tip length is that whole height at the 6x ratio.
            let a = s.assembly().unwrap();
            assert!((a.top - s.astro_top).abs() < 1e-9);
            assert!((a.bot - s.lower_bot).abs() < 1e-9);
            assert!((a.bot - a.top - 2.5 * bar_h).abs() < 1e-9);
            assert!((a.tip_len - 2.5 * bar_h * SPEAR_TIP_RATIO).abs() < 1e-9);
            assert!((a.tip_y - a.bot).abs() < 1e-9, "tip must sit on the cut line");
            // Both pieces evaluate to that same master spear while fused, so
            // nothing pops the instant the astro sliver lifts off.
            for piece in [s.astro, s.lower] {
                assert!((piece.top - a.top).abs() < 1e-9);
                assert!((piece.bot - a.bot).abs() < 1e-9);
                assert!((piece.tip_len - a.tip_len).abs() < 1e-9);
                assert!((piece.tip_y - a.tip_y).abs() < 1e-9);
            }
            // ...and their clips tile the master exactly, no gap, no overlap.
            assert!((s.astro.clip_top - a.top).abs() < 1e-9);
            assert!((s.astro.clip_bot - s.lower.clip_top).abs() < 1e-9);
            assert!((s.lower.clip_bot - a.bot).abs() < 1e-9);
        }
    }

    #[test]
    fn sliver_stack_fused_rides_the_detailed_lift() {
        let g = Geometry::default();
        let bar_h = g.default_height_px as f64;
        let closed = sliver_stack(SURFACE_W, SURFACE_H, &g, &visual(0.0, 0.0));
        let detailed = sliver_stack(SURFACE_W, SURFACE_H, &g, &visual(1.0, 0.0));
        let lift = g.detailed_content_height_px as f64;
        assert!((closed.lower.clip_top - detailed.lower.clip_top - lift).abs() < 1e-9);
        // Detailed: the dock sliver's bottom edge is the astro rect's bottom
        // border — one line, per the amended frame spec.
        let rect = astro_rect(SURFACE_W, SURFACE_H, &g);
        assert!((detailed.lower.clip_top + bar_h - rect.bottom).abs() < 1e-9);
    }

    #[test]
    fn sliver_stack_open_puts_astro_on_the_top_border() {
        let g = Geometry::default();
        let bar_h = g.default_height_px as f64;
        let s = sliver_stack(SURFACE_W, SURFACE_H, &g, &visual(1.0, 1.0));
        let rect = astro_rect(SURFACE_W, SURFACE_H, &g);
        // Astro sliver = top border; dock sliver stayed as the bottom border.
        assert!(!s.fused);
        assert!(s.assembly().is_none());
        assert!((s.astro_top - rect.top).abs() < 1e-9);
        assert!((s.lower.clip_top + bar_h - rect.bottom).abs() < 1e-9);
        // Fully split: the astro sliver owns a lone sliver's tip, centred; the
        // lower assembly keeps ONE tip for its 1.5 slivers of height.
        assert!((s.astro.tip_len - bar_h * SPEAR_TIP_RATIO).abs() < 1e-9);
        assert!((s.astro.tip_y - (s.astro_top + s.astro.clip_bot) / 2.0).abs() < 1e-9);
        assert!((s.astro.bot - s.astro.clip_bot).abs() < 1e-9, "piece must be its own height");
        assert!((s.lower.tip_len - 1.5 * bar_h * SPEAR_TIP_RATIO).abs() < 1e-9);
        assert!((s.lower.top - s.lower.clip_top).abs() < 1e-9);
        // The open sliver's tips reach exactly the rect's side indent.
        let bar_left = (SURFACE_W - (g.width_px as f64).min(SURFACE_W)) / 2.0;
        assert!((bar_left + s.astro.tip_len - rect.x).abs() < 1e-9);
    }

    #[test]
    fn sliver_stack_travel_is_monotonic_and_tips_relax_once() {
        let g = Geometry::default();
        let mut prev_top = f64::MAX;
        let mut prev_tip = f64::MAX;
        for i in 0..=20 {
            let t = i as f64 / 20.0;
            let s = sliver_stack(SURFACE_W, SURFACE_H, &g, &visual(1.0, t));
            assert!(s.astro_top < prev_top, "astro sliver must keep travelling up at t={t}");
            assert!(s.astro.tip_len <= prev_tip + 1e-9, "tip length must not grow back at t={t}");
            assert!(s.astro.clip_bot <= s.lower.clip_top + 1e-9, "halves must never overlap at t={t}");
            prev_top = s.astro_top;
            prev_tip = s.astro.tip_len;
        }
        // The tip morph completes inside the first `SLIVER_SPLIT_PX` of travel.
        let travel = SURFACE_H - g.margin_bottom_px as f64
            - g.default_height_px as f64 * 3.0
            - g.detailed_content_height_px as f64;
        let s = sliver_stack(SURFACE_W, SURFACE_H, &g, &visual(1.0, SLIVER_SPLIT_PX / travel));
        assert!((s.astro.tip_len - g.default_height_px as f64 * SPEAR_TIP_RATIO).abs() < 1e-6);
    }

    /// The whole point of the fused assembly, checked in pixels: above the
    /// section cut the silhouette must widen **monotonically** downward — one
    /// taper, not two. Stacking separately tipped spears produced a second tip
    /// line 6px below the first (user report 2026-08-07); a regression here
    /// means that is back.
    #[test]
    fn the_resting_silhouette_has_exactly_one_taper() {
        let g = Geometry::default();
        let bar_h = g.default_height_px as f64;
        let (w, h) = (240.0, 40.0);
        let top = 6.0;
        let bot = top + 2.5 * bar_h;

        let mut surface =
            cairo::ImageSurface::create(cairo::Format::A8, w as i32, h as i32).unwrap();
        {
            let ctx = CairoContext::new(&surface).unwrap();
            ctx.set_source_rgba(0.0, 0.0, 0.0, 1.0);
            spear_path(&ctx, 0.0, w, top, bot, tip_len_for(bot - top), bot, g.taper_slope);
            ctx.fill().unwrap();
        }
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let width_at = |y: usize| (0..w as usize).filter(|x| data[y * stride + x] > 128).count();

        let mut prev = 0;
        for y in (top.ceil() as usize)..(bot.floor() as usize) {
            let wide = width_at(y);
            assert!(
                wide >= prev,
                "row {y} narrows to {wide} after {prev} — a second taper is back"
            );
            prev = wide;
        }
        // And it really does reach full width at the cut, so the sections
        // extruding from that line stay capped at their corners.
        assert_eq!(width_at(bot.floor() as usize - 1), w as usize);
    }

    #[test]
    fn sliver_zone_tracks_the_sliver_in_every_state() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let d = dock_stack_px(&g);

        let (_, closed_y, _, closed_h) = sliver_edge_zone_bounds(sw, sh, &g, BarState::Closed);
        // Covers the astro half of the fused pair plus the run-up band above.
        assert_eq!(closed_y, sh - g.margin_bottom_px - g.default_height_px * 3 - 15);
        assert_eq!(closed_h, 15 + g.default_height_px);

        let (_, detailed_y, _, _) = sliver_edge_zone_bounds(sw, sh, &g, BarState::Detailed);
        assert_eq!(detailed_y, closed_y - g.detailed_content_height_px);

        // Astro open: the zone has travelled to the top border at y = D.
        let (_, grid_y, _, grid_h) = sliver_edge_zone_bounds(sw, sh, &g, BarState::AppGrid);
        assert_eq!(grid_y, d);
        assert_eq!(grid_h, g.default_height_px + 15);
        assert!(grid_y < detailed_y, "close handle must sit above the dock sliver");
    }

    /// This device's live planet roster (note 013), so the dumped frames show
    /// the sky the user actually gets rather than a toy one.
    fn dump_roster() -> Vec<crate::categories::Planet> {
        [
            ("AudioVideo", 3),
            ("Development", 2),
            ("Game", 4),
            ("Network", 10),
            ("Settings", 2),
            ("System", 9),
            ("Utility", 6),
            ("Other", 4),
        ]
        .into_iter()
        .map(|(key, n)| crate::categories::Planet {
            key: key.to_string(),
            display_name: key.to_string(),
            apps: (0..n)
                .map(|i| AppEntry {
                    id: format!("{key}-{i}"),
                    name: format!("{key}-{i}"),
                    icon: None,
                    exec: None,
                    no_display: false,
                    hidden: false,
                    categories: Vec::new(),
                })
                .collect(),
        })
        .collect()
    }

    /// Visual aid, not an assertion: dumps the sliver assembly + backdrop at
    /// real device geometry to `$SLIVER_FRAME_DIR` (default `/tmp`), one
    /// binary PPM per travel frame (PPM keeps this dep-free — cairo's PNG
    /// writer is behind a feature we don't otherwise need; `ffmpeg -i x.ppm
    /// x.png` to view). Screencopy is broken in this compositor session
    /// (`grim` / `wf-recorder` both fail on eDP-1), so this is how the shape
    /// gets reviewed without a screenshot. Run:
    /// `cargo test dump_astro_frames -- --ignored --nocapture`
    ///
    /// `SLIVER_FRAME_GRID=0,0.002,0.01` adds a frame per `grid_amount`, which
    /// is how the sliver split is checked for continuity across the moment of
    /// separation (measure the tip's x-extent outside the backdrop's
    /// `astro_rect` x-range — inside it, the black backdrop reads as sliver).
    #[test]
    #[ignore = "writes PNGs for human review"]
    fn dump_astro_frames() {
        let g = Geometry::default();
        let dir = std::env::var("SLIVER_FRAME_DIR").unwrap_or_else(|_| "/tmp".into());
        let extra: Vec<(String, Visual)> = std::env::var("SLIVER_FRAME_GRID")
            .ok()
            .into_iter()
            .flat_map(|spec| {
                spec.split(',')
                    .filter_map(|t| t.trim().parse::<f64>().ok())
                    .map(|t| (format!("grid-{t}"), visual(1.0, t)))
                    .collect::<Vec<_>>()
            })
            .collect();
        // Ambient clock the frames are rendered at. `SLIVER_FRAME_T=<secs>`
        // moves it, which is how the shooting star gets dumped: it flies for
        // `shooting_star_duration_s` at the top of every
        // `shooting_star_period_s`, so `SLIVER_FRAME_T=0.45` catches one
        // mid-crossing and the default 0.0 catches it at launch.
        let clock: f64 = std::env::var("SLIVER_FRAME_T")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0.0);
        for (name, v) in [
            ("fused-closed".to_string(), visual(0.0, 0.0)),
            ("fused-detailed".to_string(), visual(1.0, 0.0)),
            ("split-early".to_string(), visual(1.0, 0.06)),
            ("travelling".to_string(), visual(1.0, 0.5)),
            ("open".to_string(), visual(1.0, 1.0)),
        ]
        .into_iter()
        .chain(extra)
        {
            let mut surface = cairo::ImageSurface::create(
                cairo::Format::Rgb24,
                SURFACE_W as i32,
                SURFACE_H as i32,
            )
            .unwrap();
            {
                let ctx = CairoContext::new(&surface).unwrap();
                ctx.set_source_rgb(0.35, 0.38, 0.45); // wallpaper stand-in
                ctx.paint().unwrap();

                let rect = astro_rect(SURFACE_W, SURFACE_H, &g);
                let s = sliver_stack(SURFACE_W, SURFACE_H, &g, &v);
                if v.grid_amount > 0.0 {
                    let cfg = config::Astro::default();
                    let top = s.astro_top.min(rect.bottom);
                    ctx.save().unwrap();
                    rounded_rect_path(&ctx, rect.x, top, rect.w, rect.bottom - top, 12.0);
                    ctx.clip();
                    // Backdrop then planets, the live draw order. `clock` is
                    // the ambient time the frame is rendered at, so a shooting
                    // star can be dumped by asking for one mid-flight.
                    backdrop::Backdrop::build(&cfg, sky_rect(&rect))
                        .draw(&ctx, v.grid_amount, clock);
                    // The sky at real device geometry. Icons come out blank —
                    // resolving them needs a live GTK IconTheme — but the
                    // planet layout (positions, sqrt sizing, spread, labels)
                    // is exactly what needs eyeballing and it renders here.
                    let sky = astro::build_sky(&dump_roster(), &cfg, sky_rect(&rect));
                    astro::draw_sky(&ctx, &sky, v.grid_amount, 0.0, &|_, _, _| None);
                    ctx.restore().unwrap();
                }

                let bar_w = (g.width_px as f64).min(SURFACE_W);
                let bar_left = (SURFACE_W - bar_w) / 2.0;
                let bar_h = g.default_height_px as f64;
                ctx.set_source_rgba(0.05, 0.05, 0.05, 0.92);
                // Main bar halves first, then the slivers — same order as the
                // live draw, so the junctions read the same.
                let bar_bot = SURFACE_H - g.margin_bottom_px as f64;
                let cut_y = bar_bot - bar_h / 2.0;
                bottom_half_path(&ctx, bar_left, bar_w, bar_h, cut_y, bar_bot, g.taper_slope);
                ctx.fill().unwrap();
                // Mirrors `draw_bar`: one fused spear while the astro sliver
                // rests, the two clipped pieces once it has left.
                fill_sliver(&ctx, bar_left, bar_w, &s.assembly().unwrap_or(s.lower), g.taper_slope);
                if !s.fused {
                    fill_sliver(&ctx, bar_left, bar_w, &s.astro, g.taper_slope);
                }
            }
            // Rgb24 is 32-bit native-endian xRGB — little-endian byte order
            // B,G,R,x — with a padded stride.
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            let mut ppm = format!("P6\n{} {}\n255\n", SURFACE_W as i32, SURFACE_H as i32)
                .into_bytes();
            for row in 0..SURFACE_H as usize {
                for col in 0..SURFACE_W as usize {
                    let p = row * stride + col * 4;
                    ppm.extend_from_slice(&[data[p + 2], data[p + 1], data[p]]);
                }
            }
            let path = format!("{dir}/astro-{name}.ppm");
            std::fs::write(&path, &ppm).unwrap();
            println!("wrote {path}");
        }
    }

    #[test]
    fn sliver_zone_x_is_centred_on_the_bar() {
        let g = Geometry::default();
        for state in [BarState::Closed, BarState::Detailed, BarState::AppGrid] {
            let (x, _, w, _) = sliver_edge_zone_bounds(1280, 853, &g, state);
            assert_eq!(w, g.width_px);
            assert_eq!(x, (1280 - g.width_px) / 2);
        }
    }

    // ---- slice 001: state-dependent detailed zone + input agreement ----

    #[test]
    fn detailed_zone_covers_the_dock_stack_when_closed_or_detailed() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let want_h = g.margin_bottom_px + g.default_height_px + g.detailed_content_height_px;
        for state in [BarState::Closed, BarState::Detailed] {
            let (x, y, w, h) = detailed_zone_bounds(sw, sh, &g, state);
            assert_eq!(h, want_h, "{state:?}");
            assert_eq!(y, sh - want_h, "{state:?}");
            assert_eq!(w, g.width_px);
            assert_eq!(x, (sw - g.width_px) / 2);
        }
    }

    #[test]
    fn detailed_zone_extends_over_the_astro_body_in_app_grid() {
        // In AppGrid the surface is `screen_h - D` tall and the astro rect
        // spans screen `[D, h-D]`; the bottom-anchored dock-stack bounds
        // would not reach a single pixel of it. The zone must cover the
        // astro body — the down-close gesture's target surface.
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let d = dock_stack_px(&g);
        let (x, y, w, h) = detailed_zone_bounds(sw, sh, &g, BarState::AppGrid);
        assert_eq!(y, d, "starts at the top of the astro rect (screen y = D)");
        assert_eq!(h, sh - 2 * d, "spans exactly the astro body [D, h-D]");
        assert_eq!(w, g.width_px);
        assert_eq!(x, (sw - g.width_px) / 2);
    }

    #[test]
    fn detailed_zone_reaches_the_astro_body_but_closed_does_not() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        // The pre-slice bug: `detailed_zone_bounds` had no state parameter,
        // so AppGrid used the Closed/Detailed bounds — entirely below the
        // astro body, leaving the astro view with no down-close zone.
        let d = dock_stack_px(&g);
        let (_, grid_y, _, grid_h) = detailed_zone_bounds(sw, sh, &g, BarState::AppGrid);
        let (_, closed_y, _, closed_h) = detailed_zone_bounds(sw, sh, &g, BarState::Closed);
        assert!(grid_y < closed_y, "AppGrid zone starts above the Closed one");
        assert!(grid_h > closed_h, "AppGrid zone is taller than the Closed one");
        assert!(grid_y < d + g.detailed_content_height_px, "reaches the astro body");
    }

    /// Input-region derivation for the same surface, in screen coords —
    /// mirrors `Drag::refresh_input_region` via the shared pure
    /// `drag::input_region_rect`, so this test asserts the two derivations
    /// (touch zones vs client input region) agree.
    fn input_region_screen(sw: i32, sh: i32, g: &Geometry, state: BarState) -> (i32, i32, i32, i32) {
        // The window is `screen_h - D` tall and bottom-anchored, so its top
        // edge sits at screen y = D.
        let win_h = sh - dock_stack_px(g);
        let extend_up = if state == BarState::AppGrid {
            // grid_expand_needed: cover the whole surface above the bar.
            (win_h - g.margin_bottom_px - g.default_height_px).max(0)
        } else if state == BarState::Detailed {
            g.detailed_content_height_px
        } else {
            0
        };
        let (x, y, w, h) = drag::input_region_rect(sw, win_h, g, extend_up);
        (x, y + dock_stack_px(g), w, h)
    }

    #[test]
    fn detailed_zone_agrees_with_the_input_region() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let zone = detailed_zone_bounds(sw, sh, &g, BarState::Detailed);
        let input = input_region_screen(sw, sh, &g, BarState::Detailed);
        assert_eq!(zone, input, "Detailed zone must match the client input region");
    }

    #[test]
    fn app_grid_zone_sits_inside_the_input_region() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let (zx, zy, zw, zh) = detailed_zone_bounds(sw, sh, &g, BarState::AppGrid);
        let (ix, iy, iw, ih) = input_region_screen(sw, sh, &g, BarState::AppGrid);
        assert!(zx >= ix && zy >= iy, "zone must start inside the input region");
        assert!(zx + zw <= ix + iw, "zone must end inside the input region");
        assert!(zy + zh <= iy + ih, "zone must end inside the input region");
    }

    #[test]
    fn bar_zone_agrees_with_the_input_region_when_closed() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let zone = bar_zone_bounds(sw, sh, &g);
        let input = input_region_screen(sw, sh, &g, BarState::Closed);
        assert_eq!(zone, input, "Closed bar zone must match the client input region");
    }

    // ---- slice 002: apps-section zone + surface→screen conversion -----

    #[test]
    fn surface_rect_to_screen_anchors_to_the_real_window_height() {
        let g = Geometry::default();
        let d = dock_stack_px(&g);
        // Normal path: the window is `screen_h - D` tall, so surface y = 0
        // lands at screen y = D. x is unchanged.
        let (x, y, w, h) = surface_rect_to_screen(853, &g, 190.0, 0.0, 900.0, 746.0);
        assert_eq!((x, y, w, h), (190, d, 900, 746));
        assert_eq!(window_height_px(853, &g), 853 - d);

        // Grid-inflated fallback path (`build_window`'s `max`): a 600px
        // screen keeps the pre-grid floor, so the window is TALLER than
        // `screen_h - D`. The conversion must follow the real window
        // height — the constant-derived alternative would pin surface y=0
        // to screen y=D and strand every zone above the astro body, which
        // is exactly the stale-surface-height incident.
        let inflated = window_height_px(600, &g);
        assert_eq!(inflated, d + g.grid_height_px, "floor path takes over");
        assert!(inflated > 600 - d);
        let (_, y, _, h) = surface_rect_to_screen(600, &g, 0.0, 0.0, 1280.0, inflated as f64);
        assert_eq!(y, 600 - inflated, "top edge maps to the real window top");
        assert_eq!(h, inflated, "full surface maps to full window height");
    }

    #[test]
    fn apps_zone_matches_section_bounds_at_device_geometry() {
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        // Derive the expected rect independently: section_bounds in surface
        // coords, converted by the same explicit arithmetic the helper
        // implements — so the test mirrors the contract, not the fn.
        let win_h = window_height_px(sh, &g);
        let layout = section_bounds(sw as f64, win_h as f64, &g);
        let y_top = layout.cut_y - g.detailed_content_height_px as f64;
        let want = (
            layout.apps.0 as i32,
            (y_top + (sh - win_h) as f64) as i32,
            layout.apps.1 as i32,
            g.detailed_content_height_px,
        );
        assert_eq!(apps_zone_bounds(sw, sh, &g), want);
        // Exact device values — a drift here (e.g. the section width
        // formula changing without the zone following) is loud.
        assert_eq!(apps_zone_bounds(sw, sh, &g), (604, 749, 408, 66));
    }

    #[test]
    fn apps_zone_lands_inside_the_detailed_zone_and_outside_the_sliver() {
        // Registration-order contract relies on the apps rect being strictly
        // inside the broad detailed zone in Detailed, and disjoint from the
        // sliver-edge strip (which is registered before both and must not
        // shadow the apps section).
        let g = Geometry::default();
        let (sw, sh) = (1280, 853);
        let (ax, ay, aw, ah) = apps_zone_bounds(sw, sh, &g);
        let (dx, dy, dw, dh) = detailed_zone_bounds(sw, sh, &g, BarState::Detailed);
        assert!(ax >= dx && ay >= dy && ax + aw <= dx + dw && ay + ah <= dy + dh,
            "apps zone must sit inside the broad detailed zone");
        let (sx, sy, sw_z, sh_z) = sliver_edge_zone_bounds(sw, sh, &g, BarState::Detailed);
        let disjoint = ay >= sy + sh_z || ay + ah <= sy || ax >= sx + sw_z || ax + aw <= sx;
        assert!(disjoint, "apps zone must not overlap the sliver-edge strip");
    }

    #[test]
    fn apps_zone_tracks_a_grid_inflated_window() {
        // On a grid-inflated window the section content sits lower than the
        // constant-derived position would put it; the zone must follow the
        // real window height.
        let g = Geometry::default();
        let (sw, sh) = (1280, 600);
        let win_h = window_height_px(sh, &g);
        assert_eq!(win_h, dock_stack_px(&g) + g.grid_height_px);
        let layout = section_bounds(sw as f64, win_h as f64, &g);
        let (_, ay, _, _) = apps_zone_bounds(sw, sh, &g);
        let want_y = layout.cut_y - g.detailed_content_height_px as f64 + (sh - win_h) as f64;
        assert!((ay as f64 - want_y).abs() < 1e-9);
    }

    #[test]
    fn detailed_zone_app_grid_tracks_the_window_height() {
        // AppGrid arm: the astro body is the surface above the dock stack,
        // so at a grid-inflated screen the zone must still end exactly at
        // the dock stack's top edge, not at a constant-derived line.
        let g = Geometry::default();
        let (sw, sh) = (1280, 600);
        let win_h = window_height_px(sh, &g);
        let d = dock_stack_px(&g);
        let (x, y, w, h) = detailed_zone_bounds(sw, sh, &g, BarState::AppGrid);
        assert_eq!(y, sh - win_h, "zone top = real window top");
        assert_eq!(h, win_h - d, "astro body = window minus dock stack");
        assert_eq!(y + h, sh - d, "zone bottom = dock stack top in screen coords");
        assert_eq!(w, g.width_px);
        assert_eq!(x, (sw - g.width_px) / 2);
    }

    #[test]
    fn sliver_arms_follow_the_state() {
        // Astro opens from Detailed only: the up-grid arm exists on the
        // astro bar in Detailed, and nowhere else.
        assert_eq!(sliver_arms_for(BarState::Detailed), SliverArms::BidirUpGridDown);
        assert_eq!(sliver_arms_for(BarState::Closed), SliverArms::UnidirDown);
        assert_eq!(sliver_arms_for(BarState::AppGrid), SliverArms::UnidirDown);
        assert_eq!(sliver_arms_for(BarState::Hidden), SliverArms::UnidirDown);
    }
}

