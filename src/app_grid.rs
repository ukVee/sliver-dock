use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;

use gtk4::cairo;
use serde::Deserialize;

use crate::apps::AppEntry;

const ICON_SIZE: f64 = 41.0;
const CENTER_RING_COUNT: usize = 8;
const MID_RING_COUNT: usize = 16;

#[derive(Debug, Clone)]
pub struct Ring {
    pub entries: Vec<(AppEntry, f64)>,
    pub radius: f64,
    pub scale: f64,
    pub alpha: f64,
}

#[derive(Debug, Clone)]
pub struct GridState {
    pub rings: Vec<Ring>,
}

pub fn build_grid(
    entries: &BTreeMap<String, AppEntry>,
    launch_counts: &HashMap<String, u64>,
    grid_w: f64,
    grid_h: f64,
) -> GridState {
    let mut sorted: Vec<&AppEntry> = entries.values().collect();
    sorted.sort_by(|a, b| {
        let ca = launch_counts.get(&a.id).copied().unwrap_or(0);
        let cb = launch_counts.get(&b.id).copied().unwrap_or(0);
        cb.cmp(&ca)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let center_cut = CENTER_RING_COUNT.min(sorted.len());
    let mid_cut = (center_cut + MID_RING_COUNT).min(sorted.len());

    let mut rings = Vec::new();

    let _total_rings = if mid_cut < sorted.len() { 3usize } else if center_cut < sorted.len() { 2 } else { 1 };
    // Leave ~one icon-width of breathing room between the outer ring and the
    // card border so icons don't crowd the frame edge.
    let max_area = grid_w.min(grid_h);
    let r_outer = (max_area - ICON_SIZE * 1.6) / 2.0;
    let r_mid = r_outer * 0.58;
    let r_center = r_mid * 0.5;

    if !sorted[..center_cut].is_empty() {
        rings.push(make_ring(
            &sorted[..center_cut],
            r_center,
            1.0,
            1.0,
        ));
    }
    if center_cut < sorted.len() && !sorted[center_cut..mid_cut].is_empty() {
        rings.push(make_ring(
            &sorted[center_cut..mid_cut],
            r_mid,
            0.75,
            0.95,
        ));
    }
    if mid_cut < sorted.len() {
        rings.push(make_ring(
            &sorted[mid_cut..],
            r_outer,
            0.5,
            0.88,
        ));
    }

    GridState {
        rings,
    }
}

fn make_ring(
    entries: &[&AppEntry],
    radius: f64,
    scale: f64,
    alpha: f64,
) -> Ring {
    use std::f64::consts::TAU;
    let n = entries.len() as f64;
    let angle_step = TAU / n;
    let entries_with_angle: Vec<(AppEntry, f64)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| ((*e).clone(), -std::f64::consts::PI / 2.0 + (i as f64) * angle_step))
        .collect();
    Ring {
        entries: entries_with_angle,
        radius,
        scale,
        alpha,
    }
}

/// Draw the rings at their full, fixed geometry. The grid is centered on
/// `(center_x, center_y)` and never scaled or panned by the animation — the
/// open/close reveal is done by the caller clipping to a panel that grows
/// upward from the launcher (see `draw_app_grid_overlay`). `alpha` is the
/// whole-grid fade (1.0 at rest); each ring's own alpha rides on top of it.
pub fn draw_grid(
    ctx: &cairo::Context,
    state: &GridState,
    center_x: f64,
    center_y: f64,
    alpha: f64,
    icon_loader: &dyn Fn(&str, Option<&str>) -> Option<(cairo::ImageSurface, f64)>,
) {
    if alpha < 0.001 || state.rings.is_empty() {
        return;
    }

    for ring in &state.rings {
        let ring_alpha = alpha * ring.alpha;
        if ring_alpha < 0.01 {
            continue;
        }
        let icon_scale = ring.scale;
        if icon_scale < 0.05 {
            continue;
        }

        let r_drawn = ring.radius;
        if r_drawn < 1.0 {
            continue;
        }

        for (entry, angle) in &ring.entries {
            let (icon_x, icon_y) = projected_icon_pos(center_x, center_y, r_drawn, *angle);
            let half = ICON_SIZE as f64 * icon_scale / 2.0;
            let draw_x = icon_x - half;
            let draw_y = icon_y - half;
            let size = ICON_SIZE * icon_scale;

            let Some((surf, _w)) = icon_loader(&entry.id, entry.icon.as_deref()) else {
                continue;
            };

            ctx.save().ok();

            let blue = (0.478, 0.635, 0.969);
            rounded_rect_path(ctx, draw_x, draw_y, size, size, 5.0);
            ctx.set_source_rgba(blue.0, blue.1, blue.2, 0.65 * ring_alpha);
            ctx.set_line_width(1.5);
            let _ = ctx.stroke();

            ctx.translate(draw_x, draw_y);
            if (icon_scale - 1.0).abs() > 1e-3 {
                ctx.scale(icon_scale, icon_scale);
            }
            let _ = ctx.set_source_surface(&surf, 0.0, 0.0);
            let _ = ctx.paint_with_alpha(ring_alpha);
            ctx.restore().ok();
        }
    }
}

fn projected_icon_pos(cx: f64, cy: f64, radius: f64, angle: f64) -> (f64, f64) {
    let x = cx + radius * angle.cos();
    let y = cy + radius * angle.sin();
    (x, y)
}

pub fn hit_test(
    x: f64,
    y: f64,
    state: &GridState,
    center_x: f64,
    center_y: f64,
    grid_amount: f64,
) -> Option<AppEntry> {
    for ring in state.rings.iter().rev() {
        let icon_scale = ring.scale * grid_amount;
        if icon_scale < 0.05 {
            continue;
        }
        let r_drawn = ring.radius;
        if r_drawn < 1.0 {
            continue;
        }

        for (entry, angle) in &ring.entries {
            let (icon_x, icon_y) = projected_icon_pos(center_x, center_y, r_drawn, *angle);
            let half = ICON_SIZE as f64 * icon_scale / 2.0;

            if x >= icon_x - half
                && x < icon_x + half
                && y >= icon_y - half
                && y < icon_y + half
            {
                return Some(entry.clone());
            }
        }
    }
    None
}

#[derive(Debug, Default, Deserialize)]
struct LaunchFile {
    #[serde(flatten)]
    entries: HashMap<String, LaunchEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct LaunchEntry {
    count: u64,
}

pub fn load_launches() -> HashMap<String, u64> {
    let Some(path) = launches_path() else {
        return HashMap::new();
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            eprintln!(
                "sliver-dock: cannot read {}: {e}",
                path.display()
            );
            return HashMap::new();
        }
    };
    match toml::from_str::<LaunchFile>(&body) {
        Ok(lf) => lf
            .entries
            .into_iter()
            .map(|(id, e)| (id, e.count))
            .collect(),
        Err(e) => {
            eprintln!(
                "sliver-dock: launches parse error in {}: {e}",
                path.display()
            );
            HashMap::new()
        }
    }
}

pub fn save_launches(map: &HashMap<String, u64>) -> io::Result<()> {
    let Some(dir) = launches_dir() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no HOME or XDG_DATA_HOME",
        ));
    };
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("launches.toml");
    let mut out = String::with_capacity(map.len() * 40);
    let mut ids: Vec<&String> = map.keys().collect();
    ids.sort();
    for id in ids {
        let count = map[id];
        out.push_str(&format!("[{id}]\ncount = {count}\n\n"));
    }
    std::fs::write(&path, out)
}

fn launches_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("sliver-dock"))
}

fn launches_path() -> Option<PathBuf> {
    launches_dir().map(|d| d.join("launches.toml"))
}

fn rounded_rect_path(ctx: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0);
    ctx.new_sub_path();
    ctx.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    ctx.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    ctx.arc(x + r, y + h - r, r, PI / 2.0, PI);
    ctx.arc(x + r, y + r, r, PI, 1.5 * PI);
    ctx.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.to_string(),
            name: name.to_string(),
            icon: None,
            exec: Some("/usr/bin/true".to_string()),
            no_display: false,
            hidden: false,
            categories: Vec::new(),
        }
    }

    #[test]
    fn make_ring_single_entry() {
        let e = entry("a", "App A");
        let ring = make_ring(&[&e], 100.0, 0.8, 0.9);
        assert_eq!(ring.entries.len(), 1);
        assert!((ring.radius - 100.0).abs() < 1e-9);
        assert!((ring.scale - 0.8).abs() < 1e-9);
        assert!((ring.alpha - 0.9).abs() < 1e-9);
        assert!((ring.entries[0].1 + std::f64::consts::PI / 2.0).abs() < 1e-9);
    }

    #[test]
    fn make_ring_multiple_entries() {
        let apps: Vec<AppEntry> = (0..4)
            .map(|i| entry(&format!("id{i}"), &format!("App {i}")))
            .collect();
        let refs: Vec<&AppEntry> = apps.iter().collect();
        let ring = make_ring(&refs, 200.0, 1.0, 1.0);
        assert_eq!(ring.entries.len(), 4);
        use std::f64::consts::TAU;
        for (i, (_, angle)) in ring.entries.iter().enumerate() {
            let expected = -std::f64::consts::PI / 2.0 + (i as f64) * TAU / 4.0;
            assert!((angle - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn projected_icon_pos_cardinals() {
        let (x, y) = projected_icon_pos(100.0, 200.0, 50.0, 0.0);
        assert!((x - 150.0).abs() < 1e-9);
        assert!((y - 200.0).abs() < 1e-9);

        let (x, y) = projected_icon_pos(100.0, 200.0, 50.0, std::f64::consts::PI);
        assert!((x - 50.0).abs() < 1e-9);
        assert!((y - 200.0).abs() < 1e-6);
    }

    #[test]
    fn build_grid_empty() {
        let entries = BTreeMap::new();
        let counts = HashMap::new();
        let state = build_grid(&entries, &counts, 400.0, 300.0);
        assert!(state.rings.is_empty());
    }

    #[test]
    fn hit_test_inside_icon() {
        let e = entry("a", "App A");
        let ring = make_ring(&[&e], 1.0, 1.0, 1.0);
        let state = GridState {
            rings: vec![ring],
        };
        let result = hit_test(100.0, 100.0, &state, 100.0, 100.0, 1.0);
        assert_eq!(result.unwrap().id, "a");
    }

    #[test]
    fn hit_test_outside_icon() {
        let e = entry("a", "App A");
        let ring = make_ring(&[&e], 100.0, 1.0, 1.0);
        let state = GridState {
            rings: vec![ring],
        };
        let result = hit_test(100.0, 100.0, &state, 100.0, 100.0, 1.0);
        assert!(result.is_none());
    }

    #[test]
    fn hit_test_scales_with_grid_amount() {
        let e = entry("a", "App A");
        let ring = make_ring(&[&e], 1.0, 1.0, 1.0);
        let state = GridState {
            rings: vec![ring],
        };
        let inside = hit_test(100.0, 100.0, &state, 100.0, 100.0, 1.0);
        assert!(inside.is_some());
        let outside = hit_test(100.0, 100.0, &state, 100.0, 100.0, 0.001);
        assert!(outside.is_none());
    }
}
