//! Astro view — planet layout and globe render (astro-view slice 4).
//!
//! Consumes the planets `categories.rs` groups the roster into and turns them
//! into the launcher's sky:
//!
//!   * **Free-floating, seeded layout.** No sun, no orbits. Positions come
//!     from a seeded best-candidate ("dart throwing") pass plus a relaxation
//!     sweep, so the same seed and the same roster always give the same sky —
//!     muscle memory survives a restart — while a changed roster re-flows
//!     instead of colliding.
//!   * **Size = `sqrt(app count)`**, clamped, then shrunk uniformly if the
//!     discs would cover more than `fill_fraction` of the rect.
//!   * **Each planet is a globe.** Apps live at real spherical coordinates:
//!     the most-launched app sits at the front pole and the rest radiate out
//!     in rings, exactly the old radial-grid reading, but on a sphere. That
//!     is what makes "back of the globe = the less-used apps, revealed by
//!     rotation" fall out for free: the back *is* the far hemisphere, and
//!     spinning is one rotation of the projection ([`PlanetBody::placed_icons`]
//!     already takes the angle — slices 6/7 only have to animate it).
//!
//! Only the backdrop is ASCII (slice 5); the planets carry real PNG app icons.

use std::cmp::Ordering;
use std::f64::consts::{PI, TAU};

use gtk4::cairo;

use crate::apps::AppEntry;
use crate::categories::Planet;
use crate::config::Astro;

/// Tokyo Night planet accents from note 010 §Configuration (`#7aa2f7`,
/// `#bb9af7`, `#7dcfff`), assigned round-robin in roster order so a planet
/// keeps its colour across restarts. Slice 5 lifts the palette into a
/// config-driven `[astro.palette]` alongside the backdrop colours.
const ACCENTS: [(f64, f64, f64); 3] = [
    (0.478, 0.635, 0.969),
    (0.733, 0.604, 0.969),
    (0.490, 0.812, 1.000),
];

/// Note 010's star colour `#c0caf5`, used for planet labels.
const LABEL_RGB: (f64, f64, f64) = (0.753, 0.792, 0.961);

/// Candidate positions drawn per planet in the seeded placement pass. Each
/// planet takes the candidate that maximises its clearance, so more candidates
/// means a more even sky; 128 is far below the cost of a frame and the whole
/// layout is computed once per surface size, not per frame.
const PLACEMENT_CANDIDATES: usize = 128;

/// Relaxation sweeps run after placement. Dart throwing alone can still seat
/// the last (smallest) planets in a tight spot at a high `fill_fraction`;
/// pushing overlapping pairs apart converges in a handful of passes and keeps
/// the result deterministic.
const RELAX_PASSES: usize = 24;

/// Breathing room kept between two planet rims, and between a rim and the
/// rect edge.
const PLANET_PAD: f64 = 10.0;

/// Gap between a planet's rim and the top of its label.
const LABEL_GAP: f64 = 6.0;

/// Icons sit *on* the sphere, so their projected distance from the centre is
/// pulled in slightly from the true rim — otherwise an icon at the limb hangs
/// half outside the disc.
const SURFACE_INSET: f64 = 0.90;

/// An icon closer to the limb than this is edge-on and not worth drawing.
const LIMB_CUTOFF: f64 = 0.05;

/// Golden angle, in radians. Offsets each ring's first icon so successive
/// rings don't line up in a seam down the front of the globe.
const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;

/// Resolves an app icon at a given pixel size: `(app id, icon spec, size)`.
/// The daemon passes `Drag::app_icon`, which caches per id *and* size — the
/// globes ask for a different size than the dock row does.
pub type IconLoader<'a> = dyn Fn(&str, Option<&str>, i32) -> Option<cairo::ImageSurface> + 'a;

/// The rectangle a sky is laid out in, in **surface** coordinates. Doubles as
/// the layout cache key — a monitor change re-solves the sky.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// One app on a globe, in that globe's own frame: `theta` is the angular
/// distance from the front pole (0 = dead centre of the face the user is
/// looking at), `phi` the azimuth around it.
#[derive(Debug, Clone)]
pub struct GlobeApp {
    pub entry: AppEntry,
    pub theta: f64,
    pub phi: f64,
}

/// A planet: a category, where it sits, and the globe of apps it carries.
#[derive(Debug, Clone)]
pub struct PlanetBody {
    pub key: String,
    pub display_name: String,
    /// Centre, in surface coordinates.
    pub cx: f64,
    pub cy: f64,
    pub radius: f64,
    /// Base icon size before foreshortening — also the size icons are loaded
    /// at, so the cache holds one surface per app.
    pub icon_px: f64,
    /// Rings this globe fits on its front face. The "ring budget" of note 010.
    pub front_rings: usize,
    /// Index into [`ACCENTS`].
    pub accent: usize,
    /// Most-launched first, so `apps[0]` is the front pole.
    pub apps: Vec<GlobeApp>,
}

/// The laid-out sky. Planets stay in roster order (`categories.rs` sorts them,
/// `Other` last) — placement order is largest-first internally, but the output
/// order is the one `get-state` and the renderer read.
#[derive(Debug, Clone)]
pub struct Sky {
    pub rect: SkyRect,
    pub planets: Vec<PlanetBody>,
}

/// One globe icon projected to the screen for a given spin.
pub struct PlacedIcon<'a> {
    pub entry: &'a AppEntry,
    /// Centre, surface coordinates.
    pub x: f64,
    pub y: f64,
    pub size: f64,
    /// Distance towards the viewer: 1.0 dead centre, 0.0 at the limb.
    pub depth: f64,
}

impl PlanetBody {
    /// The front-hemisphere icons at rotation `spin`, back-to-front so the
    /// caller can paint straight through the slice (and reverse it for hit
    /// testing). `spin` rotates the globe about its vertical axis; slice 4
    /// always passes 0.0, slices 6/7 animate it.
    pub fn placed_icons(&self, spin: f64) -> Vec<PlacedIcon<'_>> {
        let (sin_spin, cos_spin) = spin.sin_cos();
        let mut out: Vec<PlacedIcon<'_>> = Vec::with_capacity(self.apps.len());
        for app in &self.apps {
            let (sin_t, cos_t) = app.theta.sin_cos();
            let (sin_p, cos_p) = app.phi.sin_cos();
            // Unit vector on the sphere: +z points at the viewer, +x right,
            // +y down (surface coords).
            let (x, y, z) = (sin_t * cos_p, sin_t * sin_p, cos_t);
            // Spin about the vertical axis. y is unchanged by construction.
            let xr = x * cos_spin + z * sin_spin;
            let zr = z * cos_spin - x * sin_spin;
            if zr <= LIMB_CUTOFF {
                continue; // on the back of the globe
            }
            out.push(PlacedIcon {
                entry: &app.entry,
                x: self.cx + self.radius * SURFACE_INSET * xr,
                y: self.cy + self.radius * SURFACE_INSET * y,
                // Foreshorten with depth, but keep limb icons legible rather
                // than letting them collapse to a dot.
                size: self.icon_px * (0.5 + 0.5 * zr),
                depth: zr,
            });
        }
        out.sort_by(|a, b| a.depth.partial_cmp(&b.depth).unwrap_or(Ordering::Equal));
        out
    }

    /// Apps currently hidden on the far hemisphere. Zero for every planet on
    /// this device's roster (10 apps fit comfortably on a front face); it
    /// grows the moment a category outgrows its globe.
    pub fn back_count(&self, spin: f64) -> usize {
        self.apps.len() - self.placed_icons(spin).len()
    }
}

/// Label size for a planet, so layout and render agree on the space it needs.
fn label_px(radius: f64) -> f64 {
    (radius * 0.24).clamp(9.0, 15.0)
}

/// Vertical space a planet's label occupies below its rim.
fn label_block(radius: f64) -> f64 {
    LABEL_GAP + label_px(radius) + 2.0
}

/// SplitMix64. The crate has no `rand` dependency and doesn't want one for a
/// layout seed; this is a few lines, has good distribution, and is stable
/// across builds and machines — which is exactly what "same sky every open"
/// requires.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Lay the planets out in `rect`. Deterministic in `(cfg.seed, planets, rect)`.
pub fn build_sky(planets: &[Planet], cfg: &Astro, rect: SkyRect) -> Sky {
    if planets.is_empty() || rect.w <= 0.0 || rect.h <= 0.0 {
        return Sky {
            rect,
            planets: Vec::new(),
        };
    }

    let radii = solve_radii(planets, cfg, rect);
    let centres = place(&radii, cfg.seed, rect);

    Sky {
        rect,
        planets: planets
            .iter()
            .zip(radii)
            .zip(centres)
            .enumerate()
            .map(|(i, ((planet, radius), (cx, cy)))| {
                let (apps, icon_px, front_rings) = build_globe(&planet.apps, radius, cfg);
                PlanetBody {
                    key: planet.key.clone(),
                    display_name: planet.display_name.clone(),
                    cx,
                    cy,
                    radius,
                    icon_px,
                    front_rings,
                    accent: i % ACCENTS.len(),
                    apps,
                }
            })
            .collect(),
    }
}

/// `sqrt(app count)` sizing with the configured clamps, then two guards: the
/// whole set shrinks uniformly if it would cover more than `fill_fraction` of
/// the rect, and no single planet may outgrow the rect.
fn solve_radii(planets: &[Planet], cfg: &Astro, rect: SkyRect) -> Vec<f64> {
    let (lo, hi) = if cfg.planet_min_radius_px <= cfg.planet_max_radius_px {
        (cfg.planet_min_radius_px, cfg.planet_max_radius_px)
    } else {
        (cfg.planet_max_radius_px, cfg.planet_min_radius_px)
    };
    let mut radii: Vec<f64> = planets
        .iter()
        .map(|p| (cfg.planet_base_radius_px * (p.apps.len() as f64).sqrt()).clamp(lo, hi))
        .collect();

    let area: f64 = radii.iter().map(|r| PI * r * r).sum();
    let budget = rect.w * rect.h * cfg.fill_fraction.clamp(0.01, 1.0);
    if area > budget {
        let shrink = (budget / area).sqrt();
        for r in &mut radii {
            *r *= shrink;
        }
    }

    // A planet still has to fit between the rect edges, label included.
    let cap = (rect.w.min(rect.h) / 2.0 - PLANET_PAD).max(1.0);
    for r in &mut radii {
        *r = r.min(cap);
    }
    radii
}

/// Seeded placement: largest planet first, each taking the candidate position
/// with the most clearance (Mitchell's best-candidate), then relaxation sweeps
/// to push apart anything still overlapping.
fn place(radii: &[f64], seed: u64, rect: SkyRect) -> Vec<(f64, f64)> {
    let mut order: Vec<usize> = (0..radii.len()).collect();
    order.sort_by(|&a, &b| {
        radii[b]
            .partial_cmp(&radii[a])
            .unwrap_or(Ordering::Equal)
            .then(a.cmp(&b))
    });

    let mut rng = Rng::new(seed);
    let mut centres = vec![(0.0, 0.0); radii.len()];
    let mut placed: Vec<usize> = Vec::with_capacity(radii.len());

    for &i in &order {
        let (lo, hi) = bounds(radii[i], rect);
        let mut best = (f64::NEG_INFINITY, ((lo.0 + hi.0) / 2.0, (lo.1 + hi.1) / 2.0));
        for _ in 0..PLACEMENT_CANDIDATES {
            let cx = lo.0 + rng.next_f64() * (hi.0 - lo.0);
            let cy = lo.1 + rng.next_f64() * (hi.1 - lo.1);
            // Clearance = how much room is left around this candidate, from
            // whichever is tighter: an already-placed neighbour, or the rect
            // edge. Scoring the edge too keeps a sparse sky spread out
            // instead of hugging one corner.
            let mut clearance = edge_clearance(cx, cy, radii[i], rect);
            for &j in &placed {
                let (jx, jy) = centres[j];
                let gap = (cx - jx).hypot(cy - jy) - (radii[i] + radii[j]);
                clearance = clearance.min(gap);
            }
            if clearance > best.0 {
                best = (clearance, (cx, cy));
            }
        }
        centres[i] = best.1;
        placed.push(i);
    }

    relax(&mut centres, radii, rect);
    centres
}

/// The box a planet's centre may sit in — its rim (and label) stay inside the
/// rect. Degenerate rects collapse to a single point rather than inverting.
fn bounds(radius: f64, rect: SkyRect) -> ((f64, f64), (f64, f64)) {
    let pad = radius + PLANET_PAD;
    let x0 = rect.x + pad;
    let x1 = (rect.x + rect.w - pad).max(x0);
    let y0 = rect.y + pad;
    let y1 = (rect.y + rect.h - pad - label_block(radius)).max(y0);
    ((x0, y0), (x1, y1))
}

fn edge_clearance(cx: f64, cy: f64, radius: f64, rect: SkyRect) -> f64 {
    let left = cx - radius - rect.x;
    let right = rect.x + rect.w - (cx + radius);
    let top = cy - radius - rect.y;
    let bottom = rect.y + rect.h - (cy + radius + label_block(radius));
    left.min(right).min(top).min(bottom)
}

/// Push overlapping planets apart, then pull everyone back inside the rect.
/// Deterministic — no randomness, fixed pass count, fixed iteration order.
fn relax(centres: &mut [(f64, f64)], radii: &[f64], rect: SkyRect) {
    for _ in 0..RELAX_PASSES {
        let mut moved = false;
        for i in 0..centres.len() {
            for j in (i + 1)..centres.len() {
                let (ix, iy) = centres[i];
                let (jx, jy) = centres[j];
                let want = radii[i] + radii[j] + PLANET_PAD;
                let (dx, dy) = (jx - ix, jy - iy);
                let dist = dx.hypot(dy);
                if dist >= want {
                    continue;
                }
                // Coincident centres have no separating direction; nudge along
                // x so the next pass has something to work with.
                let (ux, uy) = if dist > 1e-6 {
                    (dx / dist, dy / dist)
                } else {
                    (1.0, 0.0)
                };
                let push = (want - dist) / 2.0;
                centres[i] = (ix - ux * push, iy - uy * push);
                centres[j] = (jx + ux * push, jy + uy * push);
                moved = true;
            }
        }
        for (c, &r) in centres.iter_mut().zip(radii) {
            let (lo, hi) = bounds(r, rect);
            *c = (c.0.clamp(lo.0, hi.0), c.1.clamp(lo.1, hi.1));
        }
        if !moved {
            break;
        }
    }
}

/// Place a planet's apps on its globe.
///
/// The most-launched app takes the front pole; the rest fill rings of
/// increasing angular distance from it. A ring's capacity is its own
/// circumference divided by the icon pitch, and the number of rings that fit
/// before the front face runs out is `radius * front_arc / pitch` — so both
/// scale with planet size, which is note 010's "ring budget scales with
/// planet size". Apps past the front budget keep going onto the back of the
/// globe, where rotation reveals them.
///
/// Returns the placed apps, the base icon size, and the front ring budget.
fn build_globe(apps: &[AppEntry], radius: f64, cfg: &Astro) -> (Vec<GlobeApp>, f64, usize) {
    let (lo, hi) = if cfg.icon_min_px <= cfg.icon_max_px {
        (cfg.icon_min_px, cfg.icon_max_px)
    } else {
        (cfg.icon_max_px, cfg.icon_min_px)
    };
    let icon_px = (radius * cfg.icon_ratio).clamp(lo, hi).max(1.0);
    let pitch = (icon_px * cfg.ring_pitch_ratio).max(1.0);
    let front_arc = cfg.front_arc_deg.to_radians().clamp(0.1, PI);

    let front_rings = ((radius * front_arc / pitch).floor() as usize).max(1);
    let dtheta = front_arc / front_rings as f64;
    // Rings stop growing once they've wrapped the sphere — only reachable
    // with a pathological roster, but it keeps the fill loop finite.
    let last_ring = ((PI / dtheta).floor() as usize).max(1);

    let mut placed = Vec::with_capacity(apps.len());
    let mut next = 0usize;
    if let Some(first) = apps.first() {
        placed.push(GlobeApp {
            entry: first.clone(),
            theta: 0.0,
            phi: 0.0,
        });
        next = 1;
    }

    let mut ring = 1usize;
    while next < apps.len() {
        let theta = (ring.min(last_ring) as f64) * dtheta;
        let capacity = if ring >= last_ring {
            apps.len() - next
        } else {
            ((TAU * radius * theta.sin() / pitch).floor() as usize).max(1)
        };
        let take = capacity.min(apps.len() - next);
        let step = TAU / take as f64;
        let offset = ring as f64 * GOLDEN_ANGLE;
        for (k, entry) in apps[next..next + take].iter().enumerate() {
            placed.push(GlobeApp {
                entry: entry.clone(),
                theta,
                phi: offset + k as f64 * step,
            });
        }
        next += take;
        ring += 1;
    }

    (placed, icon_px, front_rings)
}

/// Paint the sky. `alpha` is the whole-view fade (it rides the open/close
/// animation); `spin` is the globe rotation, 0.0 until slice 6.
pub fn draw_sky(
    ctx: &cairo::Context,
    sky: &Sky,
    alpha: f64,
    spin: f64,
    icon_loader: &IconLoader<'_>,
) {
    if alpha < 0.01 {
        return;
    }
    for body in &sky.planets {
        draw_planet(ctx, body, alpha, spin, icon_loader);
    }
}

fn draw_planet(
    ctx: &cairo::Context,
    body: &PlanetBody,
    alpha: f64,
    spin: f64,
    icon_loader: &IconLoader<'_>,
) {
    if body.radius < 1.0 {
        return;
    }
    let (r, g, b) = ACCENTS[body.accent % ACCENTS.len()];

    // Sphere shading: lit from the upper left, falling away to a dark rim.
    // Cheap (one gradient per planet per frame) and it is what makes a flat
    // disc read as a ball before a single icon lands on it.
    let grad = cairo::RadialGradient::new(
        body.cx - body.radius * 0.35,
        body.cy - body.radius * 0.35,
        body.radius * 0.08,
        body.cx,
        body.cy,
        body.radius,
    );
    grad.add_color_stop_rgba(0.0, lift(r), lift(g), lift(b), 0.97 * alpha);
    grad.add_color_stop_rgba(0.60, r * 0.55, g * 0.55, b * 0.55, 0.94 * alpha);
    grad.add_color_stop_rgba(1.0, r * 0.20, g * 0.20, b * 0.20, 0.92 * alpha);
    ctx.arc(body.cx, body.cy, body.radius, 0.0, TAU);
    let _ = ctx.set_source(&grad);
    let _ = ctx.fill();

    ctx.arc(body.cx, body.cy, body.radius, 0.0, TAU);
    ctx.set_source_rgba(r, g, b, 0.80 * alpha);
    ctx.set_line_width(1.5);
    let _ = ctx.stroke();

    let load_px = body.icon_px.round().max(1.0) as i32;
    for icon in body.placed_icons(spin) {
        let Some(surface) = icon_loader(&icon.entry.id, icon.entry.icon.as_deref(), load_px) else {
            continue;
        };
        let src = surface.width() as f64;
        if src <= 0.0 {
            continue;
        }
        // Icons fade slightly towards the limb along with their size, so the
        // globe curves away instead of ending in a hard ring of icons.
        let icon_alpha = alpha * (0.55 + 0.45 * icon.depth);
        let half = icon.size / 2.0;

        ctx.save().ok();
        rounded_rect_path(
            ctx,
            icon.x - half,
            icon.y - half,
            icon.size,
            icon.size,
            icon.size * 0.22,
        );
        ctx.set_source_rgba(0.02, 0.02, 0.05, 0.45 * icon_alpha);
        let _ = ctx.fill();

        ctx.translate(icon.x - half, icon.y - half);
        let scale = icon.size / src;
        ctx.scale(scale, scale);
        let _ = ctx.set_source_surface(&surface, 0.0, 0.0);
        let _ = ctx.paint_with_alpha(icon_alpha);
        ctx.restore().ok();
    }

    draw_label(ctx, body, alpha);
}

fn draw_label(ctx: &cairo::Context, body: &PlanetBody, alpha: f64) {
    let size = label_px(body.radius);
    ctx.save().ok();
    ctx.select_font_face(
        "Sans",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Bold,
    );
    ctx.set_font_size(size);
    if let Ok(ext) = ctx.text_extents(&body.display_name) {
        let tx = body.cx - ext.width() / 2.0 - ext.x_bearing();
        let ty = body.cy + body.radius + LABEL_GAP - ext.y_bearing();
        ctx.set_source_rgba(LABEL_RGB.0, LABEL_RGB.1, LABEL_RGB.2, 0.92 * alpha);
        ctx.move_to(tx, ty);
        let _ = ctx.show_text(&body.display_name);
    }
    ctx.restore().ok();
}

/// Highlight end of the sphere gradient — the accent pulled towards white.
fn lift(c: f64) -> f64 {
    (c * 0.85 + 0.18).min(1.0)
}

/// Which app a tap landed on, or `None` for the backdrop. Front-most icon
/// wins, mirroring the paint order.
pub fn hit_test(x: f64, y: f64, sky: &Sky, spin: f64) -> Option<AppEntry> {
    for body in &sky.planets {
        // Cheap reject before projecting a whole globe: the tap has to be
        // within the disc plus an icon's overhang at the rim.
        let reach = body.radius + body.icon_px;
        if (x - body.cx).hypot(y - body.cy) > reach {
            continue;
        }
        for icon in body.placed_icons(spin).iter().rev() {
            let half = icon.size / 2.0;
            if x >= icon.x - half && x < icon.x + half && y >= icon.y - half && y < icon.y + half {
                return Some(icon.entry.clone());
            }
        }
    }
    None
}

fn rounded_rect_path(ctx: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
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

    const RECT: SkyRect = SkyRect {
        x: 36.0,
        y: 0.0,
        w: 828.0,
        h: 639.0,
    };

    fn app(id: &str) -> AppEntry {
        AppEntry {
            id: id.to_string(),
            name: id.to_string(),
            icon: None,
            exec: Some("/usr/bin/true".to_string()),
            no_display: false,
            hidden: false,
            categories: Vec::new(),
        }
    }

    fn planet(key: &str, n: usize) -> Planet {
        Planet {
            key: key.to_string(),
            display_name: key.to_string(),
            apps: (0..n).map(|i| app(&format!("{key}-{i}"))).collect(),
        }
    }

    /// The live roster on this device (note 013): 8 planets, 40 apps, 2–10 each.
    fn device_roster() -> Vec<Planet> {
        vec![
            planet("AudioVideo", 3),
            planet("Development", 2),
            planet("Game", 4),
            planet("Network", 10),
            planet("Settings", 2),
            planet("System", 9),
            planet("Utility", 6),
            planet("Other", 4),
        ]
    }

    #[test]
    fn empty_roster_is_an_empty_sky() {
        let sky = build_sky(&[], &Astro::default(), RECT);
        assert!(sky.planets.is_empty());
    }

    #[test]
    fn degenerate_rect_is_an_empty_sky() {
        let rect = SkyRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        let sky = build_sky(&device_roster(), &Astro::default(), rect);
        assert!(sky.planets.is_empty());
    }

    #[test]
    fn planets_keep_roster_order() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        let keys: Vec<&str> = sky.planets.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "AudioVideo",
                "Development",
                "Game",
                "Network",
                "Settings",
                "System",
                "Utility",
                "Other"
            ]
        );
    }

    #[test]
    fn radius_follows_sqrt_of_app_count() {
        let cfg = Astro::default();
        let sky = build_sky(&device_roster(), &cfg, RECT);
        let by_key = |k: &str| sky.planets.iter().find(|p| p.key == k).unwrap().radius;
        // 4 apps is sqrt(2)× the radius of 2 apps (both inside the clamps).
        let ratio = by_key("Game") / by_key("Development");
        assert!(
            (ratio - std::f64::consts::SQRT_2).abs() < 1e-6,
            "expected sqrt(2) growth, got {ratio}"
        );
        // Bigger roster ⇒ bigger planet, monotonically.
        assert!(by_key("Network") > by_key("System"));
        assert!(by_key("System") > by_key("Utility"));
    }

    #[test]
    fn radius_respects_the_clamps() {
        let cfg = Astro::default();
        let sky = build_sky(&[planet("Tiny", 1), planet("Huge", 400)], &cfg, RECT);
        assert!((sky.planets[0].radius - cfg.planet_min_radius_px).abs() < 1e-9);
        assert!((sky.planets[1].radius - cfg.planet_max_radius_px).abs() < 1e-9);
    }

    #[test]
    fn an_overfull_sky_shrinks_uniformly() {
        let cfg = Astro::default();
        let crowded: Vec<Planet> = (0..40).map(|i| planet(&format!("P{i}"), 9)).collect();
        let sky = build_sky(&crowded, &cfg, RECT);
        // Every planet has the same app count, so they must all shrink by the
        // same factor — and below the clamp, since the clamp alone can't fit
        // 40 planets in the rect.
        let first = sky.planets[0].radius;
        assert!(first < cfg.planet_max_radius_px);
        for p in &sky.planets {
            assert!((p.radius - first).abs() < 1e-9);
        }
        let covered: f64 = sky.planets.iter().map(|p| PI * p.radius * p.radius).sum();
        assert!(covered <= RECT.w * RECT.h * cfg.fill_fraction + 1e-6);
    }

    #[test]
    fn planets_stay_inside_the_rect() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        for p in &sky.planets {
            assert!(p.cx - p.radius >= RECT.x - 1e-6, "{} off the left", p.key);
            assert!(
                p.cx + p.radius <= RECT.x + RECT.w + 1e-6,
                "{} off the right",
                p.key
            );
            assert!(p.cy - p.radius >= RECT.y - 1e-6, "{} off the top", p.key);
            assert!(
                p.cy + p.radius + label_block(p.radius) <= RECT.y + RECT.h + 1e-6,
                "{} label off the bottom",
                p.key
            );
        }
    }

    #[test]
    fn planets_do_not_overlap() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        for (i, a) in sky.planets.iter().enumerate() {
            for b in &sky.planets[i + 1..] {
                let dist = (a.cx - b.cx).hypot(a.cy - b.cy);
                assert!(
                    dist >= a.radius + b.radius,
                    "{} and {} overlap ({dist:.1} < {:.1})",
                    a.key,
                    b.key,
                    a.radius + b.radius
                );
            }
        }
    }

    #[test]
    fn a_crowded_sky_still_does_not_overlap() {
        let crowded: Vec<Planet> = (0..24).map(|i| planet(&format!("P{i}"), 4 + i % 7)).collect();
        let sky = build_sky(&crowded, &Astro::default(), RECT);
        for (i, a) in sky.planets.iter().enumerate() {
            for b in &sky.planets[i + 1..] {
                let dist = (a.cx - b.cx).hypot(a.cy - b.cy);
                assert!(dist >= a.radius + b.radius - 1e-6, "{} / {} overlap", a.key, b.key);
            }
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_sky() {
        let cfg = Astro::default();
        let a = build_sky(&device_roster(), &cfg, RECT);
        let b = build_sky(&device_roster(), &cfg, RECT);
        for (p, q) in a.planets.iter().zip(&b.planets) {
            assert!((p.cx - q.cx).abs() < 1e-12 && (p.cy - q.cy).abs() < 1e-12);
        }
    }

    #[test]
    fn a_different_seed_gives_a_different_sky() {
        let roster = device_roster();
        let a = build_sky(&roster, &Astro::default(), RECT);
        let b = build_sky(
            &roster,
            &Astro {
                seed: 99,
                ..Astro::default()
            },
            RECT,
        );
        let moved = a
            .planets
            .iter()
            .zip(&b.planets)
            .filter(|(p, q)| (p.cx - q.cx).abs() > 1.0 || (p.cy - q.cy).abs() > 1.0)
            .count();
        assert!(moved >= a.planets.len() / 2, "only {moved} planets moved");
    }

    #[test]
    fn every_app_lands_on_the_globe_exactly_once() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        for (planet, body) in device_roster().iter().zip(&sky.planets) {
            assert_eq!(body.apps.len(), planet.apps.len(), "{}", body.key);
            let mut ids: Vec<&str> = body.apps.iter().map(|a| a.entry.id.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), planet.apps.len(), "{} has duplicates", body.key);
        }
    }

    #[test]
    fn the_most_launched_app_takes_the_front_pole() {
        // `categories::build_planets` hands apps over most-launched-first.
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        for body in &sky.planets {
            assert_eq!(body.apps[0].theta, 0.0, "{}", body.key);
            assert_eq!(body.apps[0].entry.id, format!("{}-0", body.key));
        }
    }

    #[test]
    fn rings_radiate_outward_from_the_pole() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        let net = sky.planets.iter().find(|p| p.key == "Network").unwrap();
        // Apps are handed out in launch order, so theta must never decrease.
        for pair in net.apps.windows(2) {
            assert!(pair[1].theta >= pair[0].theta - 1e-9);
        }
        assert!(net.apps.last().unwrap().theta > 0.0, "everything piled on the pole");
    }

    #[test]
    fn the_ring_budget_scales_with_planet_size() {
        let cfg = Astro::default();
        let small = build_globe(&[app("a")], 40.0, &cfg).2;
        let large = build_globe(&[app("a")], 200.0, &cfg).2;
        assert!(large > small, "budget {large} not above {small}");
    }

    #[test]
    fn the_whole_device_roster_fits_on_the_front_faces() {
        // 2–10 apps per planet all sit on the visible hemisphere; nothing is
        // stranded on the back where slice 6's spin hasn't shipped yet.
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        for body in &sky.planets {
            assert_eq!(body.back_count(0.0), 0, "{} hides apps", body.key);
        }
    }

    #[test]
    fn an_oversized_roster_spills_onto_the_back() {
        let sky = build_sky(&[planet("Everything", 300)], &Astro::default(), RECT);
        let body = &sky.planets[0];
        assert!(body.back_count(0.0) > 0, "300 apps all fit on one face?");
        // And the overflow is the *least*-launched tail, not a random slice.
        let front: Vec<&str> = body
            .placed_icons(0.0)
            .iter()
            .map(|i| i.entry.id.as_str())
            .collect();
        assert!(front.contains(&"Everything-0"));
        assert!(!front.contains(&"Everything-299"));
    }

    #[test]
    fn spinning_the_globe_swaps_which_hemisphere_shows() {
        let sky = build_sky(&[planet("Everything", 300)], &Astro::default(), RECT);
        let body = &sky.planets[0];
        let front: Vec<&str> = body
            .placed_icons(0.0)
            .iter()
            .map(|i| i.entry.id.as_str())
            .collect();
        let spun: Vec<&str> = body
            .placed_icons(PI)
            .iter()
            .map(|i| i.entry.id.as_str())
            .collect();
        // A half turn shows the far side: the front pole is gone and apps that
        // were hidden are now visible.
        assert!(!spun.contains(&"Everything-0"));
        assert!(spun.iter().any(|id| !front.contains(id)));
    }

    #[test]
    fn icons_stay_inside_their_planet() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        for body in &sky.planets {
            for icon in body.placed_icons(0.0) {
                let d = (icon.x - body.cx).hypot(icon.y - body.cy) + icon.size / 2.0;
                assert!(
                    d <= body.radius + 1.0,
                    "{}: icon reaches {d:.1} beyond r={:.1}",
                    body.key,
                    body.radius
                );
            }
        }
    }

    #[test]
    fn icons_shrink_towards_the_limb() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        let net = sky.planets.iter().find(|p| p.key == "Network").unwrap();
        let icons = net.placed_icons(0.0);
        // Painted back-to-front, so the last one is the closest — and biggest.
        assert!(icons.last().unwrap().size > icons.first().unwrap().size);
        for icon in &icons {
            assert!(icon.size <= net.icon_px + 1e-9);
        }
    }

    #[test]
    fn tapping_an_icon_returns_its_app() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        let body = &sky.planets[0];
        let icon = body.placed_icons(0.0).pop().unwrap();
        let (x, y, want) = (icon.x, icon.y, icon.entry.id.clone());
        assert_eq!(hit_test(x, y, &sky, 0.0).unwrap().id, want);
    }

    #[test]
    fn tapping_the_backdrop_hits_nothing() {
        let sky = build_sky(&device_roster(), &Astro::default(), RECT);
        assert!(hit_test(RECT.x + 0.5, RECT.y + 0.5, &sky, 0.0).is_none());
    }

    #[test]
    fn tapping_a_planet_between_icons_hits_nothing() {
        let sky = build_sky(&[planet("Sparse", 2)], &Astro::default(), RECT);
        let body = &sky.planets[0];
        // The rim of a 2-app globe is bare — the apps are at the pole and one
        // ring in.
        let x = body.cx + body.radius * 0.97;
        assert!(hit_test(x, body.cy, &sky, 0.0).is_none());
    }

    #[test]
    fn rng_is_stable_and_spread() {
        // Guards the "same sky every open" promise at its root: if SplitMix64
        // ever drifts, every recorded layout moves.
        let mut rng = Rng::new(1);
        let first: Vec<f64> = (0..4).map(|_| rng.next_f64()).collect();
        let mut again = Rng::new(1);
        let second: Vec<f64> = (0..4).map(|_| again.next_f64()).collect();
        assert_eq!(first, second);
        for v in first {
            assert!((0.0..1.0).contains(&v));
        }
    }
}
