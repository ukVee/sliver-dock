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

use crate::app_grid;
use crate::apps::{self, AppEntry};
use crate::battery::{self, BatteryInfo, BatteryState};
use crate::config::{self, Animation, Geometry};
use crate::cpu::{self, CpuInfo};
use crate::drag::{self, Direction, Drag};
use crate::ipc;
use crate::state::{BarState, WorkspaceId};
use crate::wayfire::{self, GridSize};
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
        window.clone(),
        area.clone(),
    );
    load_apps_into(&drag);
    drag.load_launches();

    let geom = cfg.geometry;
    let anim = cfg.animation;
    let drag_for_draw = Rc::clone(&drag);
    area.set_draw_func(move |_, ctx, w, h| {
        draw_bar(ctx, w as f64, h as f64, &geom, &anim, &drag_for_draw);
        draw_app_grid_overlay(ctx, w as f64, h as f64, &geom, &drag_for_draw);
    });

    let popovers = build_info_popovers(&area);
    install_section_click_handler(&area, &drag, geom, popovers);

    wayfire::log_status();
    spawn_wayfire_subscriber(&drag);

    register_zones(&drag, &geom);

    let drag_for_map = Rc::clone(&drag);
    let geom_for_map = geom;
    window.connect_map(move |_| {
        update_zone_bounds(&geom_for_map);
        // Re-derive zone enabled flags now that the surface is mapped —
        // `set_visible(true)` in jump_to / ensure_mapped can race the map.
        let s = drag_for_map.state();
        zones::set_enabled(drag::BOTTOM_EDGE_ZONE_ID, matches!(s, BarState::Hidden));
        zones::set_enabled(drag::BAR_ZONE_ID, matches!(s, BarState::Closed));
        zones::set_enabled(drag::DETAILED_ZONE_ID, false);
        zones::set_enabled(
            drag::SLIVER_EDGE_ZONE_ID,
            matches!(s, BarState::Closed | BarState::Detailed | BarState::AppGrid),
        );
        drag_for_map.refresh_input_region();
    });

    watch_monitor_changes(geom);
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
                wayfire::Event::WorkspaceAppMontagesChanged(map) => {
                    drag.set_workspace_app_montages(map);
                }
                wayfire::Event::ViewMapped { id, app_id } => {
                    if app_id == terminal_app_id(drag.terminal()) {
                        if let Some((x, y, w, h)) = drag.pop_terminal_slot() {
                            if let Err(e) = wayfire::configure_view(id, x, y, w, h) {
                                eprintln!("sliver-dock: configure-view({id}): {e}");
                            }
                        }
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
        "drag-begin" => match tokens.next() {
            Some("up") => drag.begin(Direction::Up),
            Some("down") => drag.begin(Direction::Down),
            Some("up-grid") => drag.begin(Direction::UpGrid),
            other => eprintln!("sliver-dock: drag-begin expects up|down|up-grid, got {:?}", other),
        },
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

/// Scan `.desktop` entries + load `apps.toml`, push the merged ordered
/// list onto `Drag` so the apps section can render it. Logs a one-line
/// summary so the user can sanity-check counts at startup.
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

    window.present();
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

    // detailed_amount lifts the top half off the cut, opening a gap that
    // future phases will fill with section content. At amount = 0 the two
    // halves meet exactly at cut_y and reproduce the unsplit silhouette.
    let lift = v.detailed_amount * g.detailed_content_height_px as f64;
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

    top_half_path(
        ctx,
        bar_left,
        bar_w,
        bar_h,
        bar_top - lift,
        cut_y - lift,
        slope,
    );
    let _ = ctx.fill();

    if v.hidden_amount < 0.999 {
        draw_sliver_bar(ctx, bar_left, bar_w, bar_h, bar_top, slope, alpha);
    }
}

fn draw_sliver_bar(ctx: &CairoContext, x: f64, w: f64, bar_h: f64, main_bar_top: f64, slope: f64, alpha: f64) {
    let sliver_top = main_bar_top - bar_h;
    let sliver_bot = main_bar_top;
    let (left_tip, left_inner, right_inner, right_tip, off_x, off_y) = spear_geom(x, w, bar_h, slope);
    let mid_y = (sliver_top + sliver_bot) / 2.0;

    ctx.set_source_rgba(0.05, 0.05, 0.05, alpha);

    ctx.move_to(left_inner, sliver_top);
    ctx.line_to(right_inner, sliver_top);
    ctx.curve_to(right_inner + off_x, sliver_top, right_tip, mid_y - off_y, right_tip, mid_y);
    ctx.curve_to(right_tip, mid_y + off_y, right_inner + off_x, sliver_bot, right_inner, sliver_bot);
    ctx.line_to(left_inner, sliver_bot);
    ctx.curve_to(left_inner - off_x, sliver_bot, left_tip, mid_y + off_y, left_tip, mid_y);
    ctx.curve_to(left_tip, mid_y - off_y, left_inner - off_x, sliver_top, left_inner, sliver_top);
    ctx.close_path();
    let _ = ctx.fill();
}

/// The dock's own vertical footprint from the screen bottom in the Detailed
/// state — `margin + the 6px bar + the detailed content`. This is `D` in the
/// astro spec (note 010): the surface is sized `screen_h - D` tall and the
/// astro rect spans screen `[D, h-D]`, leaving an even `D`-tall band above
/// and below.
fn dock_stack_px(g: &Geometry) -> i32 {
    g.margin_bottom_px + g.default_height_px + g.detailed_content_height_px
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
    let tip_len = g.default_height_px as f64 * 6.0;
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
/// SLICE 1 (astro-view): this is the geometry foundation. The backdrop is the
/// new astro surface; the interim content is the existing radial app grid,
/// re-centered into the rect so app-launch keeps working — planets + ASCII
/// backdrop replace it in slices 4-5. Sliver fusion/travel is slice 2.
fn draw_app_grid_overlay(ctx: &CairoContext, w: f64, h: f64, g: &Geometry, drag: &Drag) {
    let v = drag.visual();
    let a = v.grid_amount;
    if a < 0.001 {
        return;
    }
    let Some(state) = drag.grid_state() else { return };

    let rect = astro_rect(w, h, g);
    if rect.w <= 0.0 || rect.bottom <= rect.top {
        return;
    }
    let corner = 12.0;

    // Extrude the panel up from its bottom edge; the rounded clip does the
    // reveal, so the fill itself stays fully opaque (solid black, zero
    // translucency per note 010).
    let full_h = rect.bottom - rect.top;
    let panel_top = rect.bottom - full_h * a;
    let card_h = (rect.bottom - panel_top).max(0.0);

    ctx.save().ok();
    rounded_rect_path(ctx, rect.x, panel_top, rect.w, card_h, corner);
    ctx.clip();

    ctx.set_source_rgba(0.0, 0.0, 0.0, 1.0);
    ctx.rectangle(rect.x, panel_top, rect.w, card_h);
    let _ = ctx.fill();

    // Interim: the radial grid centered in the rect (replaced by planets in
    // slice 4). The whole-grid fade rides on `a` so it appears with the panel.
    let center_x = rect.x + rect.w / 2.0;
    let center_y = (rect.top + rect.bottom) / 2.0;
    let icon_loader = |id: &str, spec: Option<&str>| -> Option<(cairo::ImageSurface, f64)> {
        let s = drag.app_icon(id, spec, 41)?;
        Some((s, 41.0))
    };
    app_grid::draw_grid(ctx, &state, center_x, center_y, a, &icon_loader);
    ctx.restore().ok();
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
/// Each icon's interior shows a per-workspace MRU app-icon montage
/// (Milestone D); workspaces with no tracked toplevels fall back to the
/// bare placeholder.
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
        let montage = drag.workspace_app_montage(*ws);

        rounded_rect_path(ctx, rect.x, rect.y, rect.w, rect.h, 7.0);
        let f_alpha = if is_current { 0.95 } else { 0.65 };
        ctx.set_source_rgba(bg.0, bg.1, bg.2, f_alpha * alpha);
        let _ = ctx.fill();

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

/// Partition `rect` into 1/2/3/4 cells per the workspace-icon montage
/// rules. Returns rects in MRU draw order:
///   * 1 — full rect.
///   * 2 — vertical split, MRU on the left.
///   * 3 — triangular: top-left = MRU, top-right = 2nd, bottom-full = 3rd.
///   * 4+ — 2×2 row-major; caller draws the `+N` badge separately when `n > 4`.
///
/// `MONTAGE_GAP` (1 px) is the internal partition gap. Cells do not round
/// — the outer rounded square owns the curvature.
fn montage_cells(rect: Rect, n: usize) -> Vec<Rect> {
    let g = MONTAGE_GAP;
    match n {
        0 => Vec::new(),
        1 => vec![rect],
        2 => {
            let half_w = ((rect.w - g) / 2.0).max(1.0);
            vec![
                Rect { x: rect.x, y: rect.y, w: half_w, h: rect.h },
                Rect {
                    x: rect.x + half_w + g,
                    y: rect.y,
                    w: (rect.w - half_w - g).max(1.0),
                    h: rect.h,
                },
            ]
        }
        3 => {
            let half_w = ((rect.w - g) / 2.0).max(1.0);
            let half_h = ((rect.h - g) / 2.0).max(1.0);
            vec![
                Rect { x: rect.x, y: rect.y, w: half_w, h: half_h },
                Rect {
                    x: rect.x + half_w + g,
                    y: rect.y,
                    w: (rect.w - half_w - g).max(1.0),
                    h: half_h,
                },
                Rect {
                    x: rect.x,
                    y: rect.y + half_h + g,
                    w: rect.w,
                    h: (rect.h - half_h - g).max(1.0),
                },
            ]
        }
        _ => {
            let half_w = ((rect.w - g) / 2.0).max(1.0);
            let half_h = ((rect.h - g) / 2.0).max(1.0);
            let right_w = (rect.w - half_w - g).max(1.0);
            let bot_h = (rect.h - half_h - g).max(1.0);
            vec![
                Rect { x: rect.x, y: rect.y, w: half_w, h: half_h },
                Rect { x: rect.x + half_w + g, y: rect.y, w: right_w, h: half_h },
                Rect { x: rect.x, y: rect.y + half_h + g, w: half_w, h: bot_h },
                Rect {
                    x: rect.x + half_w + g,
                    y: rect.y + half_h + g,
                    w: right_w,
                    h: bot_h,
                },
            ]
        }
    }
}

/// Paint the per-workspace app-icon montage inside `rect` (the outer 41×41
/// rounded icon's interior bounds). Truncates `app_ids` to 4 cells; if more
/// than 4 are supplied, a TN-purple `+N` badge is drawn at the bottom-right
/// over the 4th cell.
///
/// Each cell gets a 2-px-inset rounded bg (TN `bg_highlight`) so adjacent
/// icons read as distinct against the outer rounded square's fill, then
/// the cached icon surface scaled to fit (preserving aspect, centered).
/// Icons load at `ICON_SIZE` to match the apps section's cache key — the
/// surface gets reused across both sections without a size collision.
fn draw_montage(
    ctx: &CairoContext,
    rect: Rect,
    app_ids: &[String],
    drag: &Drag,
    alpha: f64,
) {
    let count = app_ids.len();
    if count == 0 {
        return;
    }
    let cells = montage_cells(rect, count.min(4));
    let bg_highlight = (0.161, 0.180, 0.259);
    let inset = 2.0;
    let cell_radius = 3.0;
    for (i, cell) in cells.iter().enumerate() {
        let icell = Rect {
            x: cell.x + inset,
            y: cell.y + inset,
            w: (cell.w - 2.0 * inset).max(1.0),
            h: (cell.h - 2.0 * inset).max(1.0),
        };
        rounded_rect_path(ctx, icell.x, icell.y, icell.w, icell.h, cell_radius);
        ctx.set_source_rgba(bg_highlight.0, bg_highlight.1, bg_highlight.2, 0.95 * alpha);
        let _ = ctx.fill();

        if let Some(surf) = drag.app_icon_by_id(&app_ids[i], ICON_SIZE as i32) {
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
    }
    if count > 4 {
        draw_overflow_badge(ctx, rect, count - 4, alpha);
    }
}

/// `+N` chip at the bottom-right of the outer icon rect. Sits *on top of*
/// the 4th cell's icon — the spec deliberately overlays rather than
/// replacing the 4th icon. TN purple body with TN-fg text + thin border.
fn draw_overflow_badge(ctx: &CairoContext, rect: Rect, extra: usize, alpha: f64) {
    let badge_w = 16.0;
    let badge_h = 11.0;
    let inset = 1.0;
    let badge_x = rect.x + rect.w - badge_w - inset;
    let badge_y = rect.y + rect.h - badge_h - inset;

    let purple = (0.733, 0.604, 0.969);
    let fg = (0.85, 0.88, 0.95);

    rounded_rect_path(ctx, badge_x, badge_y, badge_w, badge_h, 3.0);
    ctx.set_source_rgba(purple.0, purple.1, purple.2, 0.95 * alpha);
    let _ = ctx.fill_preserve();
    ctx.set_source_rgba(fg.0, fg.1, fg.2, 0.4 * alpha);
    ctx.set_line_width(1.0);
    let _ = ctx.stroke();

    let text = format!("+{}", extra);
    ctx.save().ok();
    ctx.select_font_face(
        "Sans",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Bold,
    );
    ctx.set_font_size(8.5);
    if let Ok(ext) = ctx.text_extents(&text) {
        let tx = badge_x + (badge_w - ext.width()) / 2.0 - ext.x_bearing();
        let ty = badge_y + (badge_h - ext.height()) / 2.0 - ext.y_bearing();
        ctx.set_source_rgba(fg.0, fg.1, fg.2, 0.95 * alpha);
        ctx.move_to(tx, ty);
        let _ = ctx.show_text(&text);
    }
    ctx.restore().ok();
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
        11 | 12 | 13 => "th",
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
/// Internal partition gap between montage cells inside a workspace icon.
const MONTAGE_GAP: f64 = 1.0;

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
            if let Some(state) = drag.grid_state() {
                // Mirror draw_app_grid_overlay: the interim radial grid is
                // centered in the astro rect (surface coords), so tap targets
                // key off the same real-surface geometry.
                let rect = astro_rect(surface_w, surface_h, &geom);
                let center_x = rect.x + rect.w / 2.0;
                let center_y = (rect.top + rect.bottom) / 2.0;
                if let Some(entry) =
                    app_grid::hit_test(x, y, &state, center_x, center_y, 1.0)
                {
                    handle_app_tap(&drag, &entry);
                    if drag.auto_close_app_grid() {
                        drag.snap_to_state(BarState::Detailed);
                    }
                    return;
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

    if let (Some(grid), Some(output_id)) = (drag.grid(), drag.wayfire_output_id()) {
        let opened = drag.opened_workspaces();
        if let Some(target) = first_free_workspace(grid, &opened) {
            if target != drag.current_workspace() {
                if let Err(e) = wayfire::set_workspace(target, output_id) {
                    eprintln!(
                        "sliver-dock: set-workspace({},{}): {e}",
                        target.x, target.y
                    );
                }
            }
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

    if let Some(saved) = drag.terminal_workspace() {
        if drag.running_apps().contains(term_id) {
            if let Err(e) = wayfire::set_workspace(saved, output_id) {
                eprintln!(
                    "sliver-dock: terminal re-focus set-workspace({},{}): {e}",
                    saved.x, saved.y
                );
            }
            return;
        }
    }

    let Some(grid) = drag.grid() else { return };
    let opened = drag.opened_workspaces();
    let target = first_free_workspace(grid, &opened).unwrap_or_else(|| drag.current_workspace());
    let Some(output_size) = drag.output_size() else {
        eprintln!("sliver-dock: terminal launch deferred: output size not yet known");
        return;
    };

    let count = drag.terminal().count.max(1);
    drag.enqueue_terminal_slots(terminal_layout_slots(count, output_size));
    drag.set_terminal_workspace(Some(target));

    if target != drag.current_workspace() {
        if let Err(e) = wayfire::set_workspace(target, output_id) {
            eprintln!(
                "sliver-dock: terminal set-workspace({},{}): {e}",
                target.x, target.y
            );
        }
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

/// Top trapezoid half. Long edge at `cut_y`, short edge at `top_y`.
fn top_half_path(
    ctx: &CairoContext,
    x: f64,
    w: f64,
    bar_h: f64,
    top_y: f64,
    cut_y: f64,
    slope: f64,
) {
    let (left_tip, left_inner, right_inner, right_tip, off_x, off_y) =
        spear_geom(x, w, bar_h, slope);

    ctx.move_to(left_inner, top_y);
    ctx.line_to(right_inner, top_y);
    ctx.curve_to(
        right_inner + off_x,
        top_y,
        right_tip,
        cut_y - off_y,
        right_tip,
        cut_y,
    );
    ctx.line_to(left_tip, cut_y);
    ctx.curve_to(
        left_tip,
        cut_y - off_y,
        left_inner - off_x,
        top_y,
        left_inner,
        top_y,
    );
    ctx.close_path();
}

fn spear_geom(x: f64, w: f64, bar_h: f64, slope: f64) -> (f64, f64, f64, f64, f64, f64) {
    let tip_len = bar_h * 6.0;
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
    if let Some(path) = user_css_path() {
        if path.is_file() {
            let provider = CssProvider::new();
            provider.load_from_path(&path);
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_USER,
            );
        }
    }
}

fn user_css_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("sliver-dock").join("style.css"))
}

fn current_screen_size() -> Option<(i32, i32)> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    let item = monitors.item(0)?;
    let monitor = item.downcast::<gdk::Monitor>().ok()?;
    let geom = monitor.geometry();
    Some((geom.width(), geom.height()))
}

fn register_zones(drag: &Rc<Drag>, geom: &Geometry) {
    let Some((sw, sh)) = current_screen_size() else {
        eprintln!("sliver-dock: no monitor 0; touch-drawer zones not registered");
        return;
    };
    let reply = ipc::socket_path();
    let initial = drag.state();

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
        matches!(initial, BarState::Hidden),
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
        matches!(initial, BarState::Closed),
        Some(bh),
    );

    // Detailed-view bidir zone — covers the full visible expanded window
    // (bar + margin + detailed content), centered horizontally on the
    // spear. Down resolves the close gesture (Detailed → Closed); up is
    // a no-op for now, parked for the future workspaces-section grid
    // expand. Enabled only in Detailed state. travel_px = zone height so
    // close-gesture progress maxes out within the visible window.
    let (dx, dy, dw, dh) = detailed_zone_bounds(sw, sh, geom);
    zones::register_bidir(
        drag::DETAILED_ZONE_ID,
        dx,
        dy,
        dw,
        dh,
        &reply,
        "up",
        "down",
        matches!(initial, BarState::Detailed),
        Some(dh),
    );

    // Sliver-edge bidir zone — 15px strip above the sliver bar. Up triggers
    // the app-grid (UpGrid direction); down is a no-op in this zone. Uses
    // travel_px=15 so a short swipe saturates progress. Enabled in Closed,
    // Detailed, and AppGrid states.
    let (sx, sy, sw_z, sh_z) = sliver_edge_zone_bounds(sw, sh, geom);
    zones::register_bidir(
        drag::SLIVER_EDGE_ZONE_ID,
        sx,
        sy,
        sw_z,
        sh_z,
        &reply,
        "up-grid",
        "down",
        matches!(initial, BarState::Closed | BarState::Detailed | BarState::AppGrid),
        Some(sh_z),
    );
}

fn update_zone_bounds(geom: &Geometry) {
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
    let (dx, dy, dw, dh) = detailed_zone_bounds(sw, sh, geom);
    zones::update_bounds(drag::DETAILED_ZONE_ID, dx, dy, dw, dh);
    let (sx, sy, sw_z, sh_z) = sliver_edge_zone_bounds(sw, sh, geom);
    zones::update_bounds(drag::SLIVER_EDGE_ZONE_ID, sx, sy, sw_z, sh_z);
}

/// Bar zone covers the bar plus the margin band beneath it, centered on
/// the same horizontal extent as the visible spear.
fn bar_zone_bounds(sw: i32, sh: i32, geom: &Geometry) -> (i32, i32, i32, i32) {
    let bar_w = geom.width_px.min(sw);
    let h = geom.margin_bottom_px + geom.default_height_px;
    let x = (sw - bar_w) / 2;
    let y = sh - h;
    (x, y, bar_w, h)
}

/// Detailed zone covers the entire visible expanded window — bar +
/// margin + detailed content height — centered horizontally on the
/// spear. Same horizontal extent as the bar zone, taller.
fn detailed_zone_bounds(sw: i32, sh: i32, geom: &Geometry) -> (i32, i32, i32, i32) {
    let bar_w = geom.width_px.min(sw);
    let h = geom.margin_bottom_px + geom.default_height_px + geom.detailed_content_height_px;
    let x = (sw - bar_w) / 2;
    let y = sh - h;
    (x, y, bar_w, h)
}

/// Sliver-edge zone — 15px strip immediately above the sliver bar. Full
/// bar width, centered horizontally. Y = sliver_top - 15.
fn sliver_edge_zone_bounds(sw: i32, sh: i32, geom: &Geometry) -> (i32, i32, i32, i32) {
    let bar_w = geom.width_px.min(sw);
    let x = (sw - bar_w) / 2;
    let y = sh - geom.margin_bottom_px - geom.default_height_px * 2 - 15;
    (x, y, bar_w, 15)
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

fn watch_monitor_changes(geom: Geometry) {
    let Some(display) = gdk::Display::default() else { return };
    let monitors = display.monitors();
    let Some(item) = monitors.item(0) else { return };
    let Ok(monitor) = item.downcast::<gdk::Monitor>() else { return };
    monitor.connect_geometry_notify(move |_| {
        update_zone_bounds(&geom);
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
    use crate::state::WorkspaceId;
    use std::collections::HashSet;

    #[test]
    fn montage_cells_0_empty() {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        assert!(montage_cells(rect, 0).is_empty());
    }

    #[test]
    fn montage_cells_1_full() {
        let rect = Rect { x: 10.0, y: 20.0, w: 100.0, h: 100.0 };
        let cells = montage_cells(rect, 1);
        assert_eq!(cells.len(), 1);
        assert!((cells[0].x - 10.0).abs() < 1e-9);
        assert!((cells[0].y - 20.0).abs() < 1e-9);
        assert!((cells[0].w - 100.0).abs() < 1e-9);
        assert!((cells[0].h - 100.0).abs() < 1e-9);
    }

    #[test]
    fn montage_cells_2_vertical_split() {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let cells = montage_cells(rect, 2);
        assert_eq!(cells.len(), 2);
        assert!(cells[0].x < cells[1].x);
        assert_eq!(cells[0].h, cells[1].h);
    }

    #[test]
    fn montage_cells_3_triangular() {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let cells = montage_cells(rect, 3);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[2].w, rect.w);
    }

    #[test]
    fn montage_cells_4_grid() {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let cells = montage_cells(rect, 4);
        assert_eq!(cells.len(), 4);
    }

    #[test]
    fn montage_cells_5_truncated_to_4() {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let cells = montage_cells(rect, 5);
        assert_eq!(cells.len(), 4);
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
}
