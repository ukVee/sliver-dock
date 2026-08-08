//! Astro backdrop — the ASCII sky behind the planets (astro-view slice 5).
//!
//! Note 010 §Astro contents: *"space backdrop (ASCII): static seeded
//! starfield, ASCII nebulas, periodic shooting stars"*, and only the backdrop
//! is ASCII — the planets on top of it carry real PNG icons.
//!
//! Three deliberate calls shape this module:
//!
//!   * **The static layer is baked once into an [`cairo::ImageSurface`]**, not
//!     re-drawn per frame. The astro view redraws on a 16ms ticker over an
//!     828×639 rect; ~210 star glyphs plus ~700 nebula glyphs of `show_text`
//!     every frame is real money on a Surface Go 3, and all of it is
//!     unchanging. Baked, an ambient frame costs one blit.
//!   * **The starfield is genuinely static**, as specced. That is what lets
//!     the ambient ticker skip `queue_draw` entirely unless a shooting star is
//!     in flight — the idle astro view redraws zero times per second.
//!   * **A shooting star is derived from the clock, not stored.** The event
//!     index is `floor(t / period)` and its parameters are hashed from that
//!     index, so there is no spawn/expire state to keep, nothing to leak, and
//!     the same second of the same session always looks the same.
//!
//! Determinism comes from the same seed and the same SplitMix64 [`Rng`] the
//! planet layout uses (`[astro] seed`) — one PRNG in the crate, no `rand`
//! dependency.

use std::cell::RefCell;
use std::collections::HashMap;

use gtk4::cairo;

use crate::astro::{Rng, SkyRect};
use crate::config::{Astro, Palette, Rgb};

/// Star glyphs, faintest first. Pure ASCII — the backdrop's whole conceit.
const STAR_GLYPHS: [char; 6] = ['.', '.', '\'', ':', '+', '*'];

/// Nebula glyphs by density, thinnest first.
const NEBULA_GLYPHS: [char; 6] = ['.', ':', ';', '+', '*', '%'];

/// Nebula cells below this density stay empty, which is what gives a cloud a
/// ragged edge instead of an ellipse outline.
const NEBULA_THRESHOLD: f64 = 0.34;

/// Nebula alpha at the threshold and at full density. Note 010 asks for
/// "nebula dims" — they sit behind the stars, never competing with them. The
/// floor still has to clear the "is that dirt on the screen?" line: the dim
/// end of the tint ramp is a dark blue on black and disappears below ~0.15.
const NEBULA_ALPHA: (f64, f64) = (0.18, 0.62);

/// Width of a monospace cell as a fraction of its height. The nebula glyphs
/// are laid on a character grid, and a *square* grid reads as columns with
/// alleys between them, because the glyphs nearly touch vertically while
/// leaving a third of the pitch empty horizontally.
const GLYPH_ASPECT: f64 = 0.6;

/// Star alpha at brightness 0 and 1.
const STAR_ALPHA: (f64, f64) = (0.30, 1.0);

/// Nebula noise wavelength, in glyph cells. Bigger = smoother clouds. Kept
/// small enough that a cloud spans a dozen-odd lattice cells: sample too
/// coarsely and thresholding a near-flat field carves lattice-aligned
/// rectangles, which is exactly what a nebula must not look like.
const NOISE_CELLS: f64 = 2.6;

/// One star: a glyph, a place, and how bright it burns.
#[derive(Debug, Clone, Copy)]
pub struct Star {
    pub x: f64,
    pub y: f64,
    pub glyph: char,
    /// 0 = faintest, 1 = brightest. Drives colour *and* glyph choice.
    pub brightness: f64,
}

/// One nebula glyph — a cell of a cloud that survived the density threshold.
#[derive(Debug, Clone, Copy)]
pub struct NebulaGlyph {
    pub x: f64,
    pub y: f64,
    pub glyph: char,
    pub tint: Rgb,
    pub alpha: f64,
}

/// A shooting star, projected for one instant of its flight.
#[derive(Debug, Clone, Copy)]
pub struct Streak {
    /// Leading point.
    pub head: (f64, f64),
    /// Trailing point — the tail is short at launch and reaches full length
    /// once the head has travelled further than the tail is long.
    pub tail: (f64, f64),
    /// Whole-streak fade, so it dims out at the end of its flight instead of
    /// blinking off.
    pub alpha: f64,
}

/// The generated backdrop for one rect. Deterministic in
/// `(seed, rect, density knobs)`; the render-side bake is memoised behind a
/// `RefCell` because it can only be built once a cairo context exists.
pub struct Backdrop {
    pub rect: SkyRect,
    pub stars: Vec<Star>,
    pub nebula: Vec<NebulaGlyph>,
    glyph_px: f64,
    seed: u64,
    period_s: f64,
    duration_s: f64,
    tail_px: f64,
    palette: Palette,
    /// The static layer (stars + nebulas), drawn in rect-local coordinates.
    baked: RefCell<Option<cairo::ImageSurface>>,
}

impl Backdrop {
    /// Generate the sky for `rect`. Pure and deterministic — no cairo, no
    /// clock, so the whole model is unit-testable.
    pub fn build(cfg: &Astro, rect: SkyRect) -> Self {
        let glyph_px = cfg.glyph_px.max(2.0);
        let diag = rect.w.hypot(rect.h);
        Self {
            rect,
            stars: build_stars(cfg, rect, glyph_px),
            nebula: build_nebulas(cfg, rect, glyph_px),
            glyph_px,
            seed: cfg.seed,
            period_s: cfg.shooting_star_period_s,
            duration_s: cfg.shooting_star_duration_s,
            tail_px: (cfg.shooting_star_tail_fraction * diag).max(glyph_px),
            palette: cfg.palette.clone(),
            baked: RefCell::new(None),
        }
    }

    /// The shooting star in flight at `t` seconds, or `None` in the gap
    /// between two. `t` is the astro view's own monotonic clock.
    ///
    /// Stateless by construction: the event index is `floor(t / period)` and
    /// every parameter is hashed from it, so any `t` can be asked about in
    /// any order — which is what makes it testable and what keeps a paused
    /// or backgrounded view from accumulating a queue of missed spawns.
    pub fn shooting_star(&self, t: f64) -> Option<Streak> {
        if self.period_s <= 0.0 || self.duration_s <= 0.0 || t < 0.0 {
            return None;
        }
        let index = (t / self.period_s).floor();
        let phase = t - index * self.period_s;
        if phase >= self.duration_s {
            return None;
        }
        let p = phase / self.duration_s;

        let mut rng = Rng::new(self.seed ^ event_key(index));
        // Mostly-downward diagonals, mirrored half the time, so successive
        // meteors don't all rake the same way.
        let angle = (20.0 + rng.next_f64() * 35.0f64).to_radians();
        let sign = if rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
        let dir = (angle.cos() * sign, angle.sin());
        // Launch from the upper half, leaning into the direction of travel so
        // the streak crosses the rect rather than leaving it immediately.
        let x0 = if sign > 0.0 {
            self.rect.x + (rng.next_f64() * 0.55 - 0.10) * self.rect.w
        } else {
            self.rect.x + (0.55 + rng.next_f64() * 0.55) * self.rect.w
        };
        let y0 = self.rect.y + (rng.next_f64() * 0.5 - 0.05) * self.rect.h;
        let travel = self.rect.w.hypot(self.rect.h) * (0.55 + rng.next_f64() * 0.40);

        let lead = travel * p;
        let trail = (lead - self.tail_px).max(0.0);
        Some(Streak {
            head: (x0 + dir.0 * lead, y0 + dir.1 * lead),
            tail: (x0 + dir.0 * trail, y0 + dir.1 * trail),
            // Full brightness for most of the flight, fading over the last
            // third — a meteor burns out, it doesn't switch off.
            alpha: ((1.0 - p) * 3.0).min(1.0),
        })
    }

    /// True when the view has something moving to show at `t`. The ambient
    /// ticker asks this before spending a `queue_draw`: the starfield and the
    /// nebulas never change, so an idle astro view costs no frames at all.
    pub fn is_animating(&self, t: f64) -> bool {
        self.shooting_star(t).is_some()
    }

    /// Seconds until the backdrop next has something to show — `0.0` while a
    /// shooting star is already crossing, and `None` when none will ever come
    /// (shooting stars configured off), which lets the caller stop its timer
    /// outright.
    ///
    /// This is what keeps the ambient animation off the CPU. Polling
    /// [`Self::is_animating`] at frame rate would wake the main loop 60×/s to
    /// answer "no" for six seconds out of every seven — measurably ~7% of a
    /// core on a Surface Go 3, spent entirely on asking. Sleeping the gap
    /// instead costs one wakeup.
    pub fn seconds_until_next(&self, t: f64) -> Option<f64> {
        if self.period_s <= 0.0 || self.duration_s <= 0.0 {
            return None;
        }
        let t = t.max(0.0);
        let phase = t - (t / self.period_s).floor() * self.period_s;
        if phase < self.duration_s {
            return Some(0.0);
        }
        Some(self.period_s - phase)
    }

    /// Paint the backdrop into the current clip. `alpha` is the whole-view
    /// fade (it rides the open/close animation); `t` is the ambient clock.
    pub fn draw(&self, ctx: &cairo::Context, alpha: f64, t: f64) {
        if alpha < 0.01 || self.rect.w <= 0.0 || self.rect.h <= 0.0 {
            return;
        }
        // Solid background first — note 010: "background solid black, zero
        // translucency". It is a palette entry rather than a literal so the
        // later `[astro]` transparency option has one place to land.
        let (r, g, b) = self.palette.background.tuple();
        ctx.set_source_rgba(r, g, b, alpha);
        ctx.rectangle(self.rect.x, self.rect.y, self.rect.w, self.rect.h);
        let _ = ctx.fill();

        if let Some(layer) = self.static_layer() {
            let _ = ctx.set_source_surface(&layer, self.rect.x, self.rect.y);
            let _ = ctx.paint_with_alpha(alpha);
        }

        if let Some(streak) = self.shooting_star(t) {
            self.draw_streak(ctx, &streak, alpha);
        }
    }

    /// The baked star + nebula layer, drawn once and kept. Returns `None`
    /// only if cairo refuses the surface, in which case the backdrop simply
    /// renders as its background — degraded, never a panic.
    fn static_layer(&self) -> Option<cairo::ImageSurface> {
        if let Some(surface) = self.baked.borrow().clone() {
            return Some(surface);
        }
        let w = self.rect.w.ceil() as i32;
        let h = self.rect.h.ceil() as i32;
        if w <= 0 || h <= 0 {
            return None;
        }
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
        {
            let ctx = cairo::Context::new(&surface).ok()?;
            let mut pen = GlyphPen::new(&ctx, self.glyph_px);
            // Nebulas first: they are the dim wash the stars sit in front of.
            for cell in &self.nebula {
                let (r, g, b) = cell.tint.tuple();
                ctx.set_source_rgba(r, g, b, cell.alpha);
                pen.draw(&ctx, cell.glyph, cell.x - self.rect.x, cell.y - self.rect.y);
            }
            for star in &self.stars {
                let (r, g, b) = self
                    .palette
                    .star_dim
                    .mix(self.palette.star, star.brightness)
                    .tuple();
                let a = lerp(STAR_ALPHA.0, STAR_ALPHA.1, star.brightness);
                ctx.set_source_rgba(r, g, b, a);
                pen.draw(&ctx, star.glyph, star.x - self.rect.x, star.y - self.rect.y);
            }
        }
        *self.baked.borrow_mut() = Some(surface.clone());
        Some(surface)
    }

    /// The meteor: a run of ASCII glyphs from tail to head, brightening
    /// along the way. Drawn live — it is the only thing that moves.
    fn draw_streak(&self, ctx: &cairo::Context, streak: &Streak, alpha: f64) {
        let (dx, dy) = (
            streak.head.0 - streak.tail.0,
            streak.head.1 - streak.tail.1,
        );
        let len = dx.hypot(dy);
        if len < 1e-6 {
            return;
        }
        let (ux, uy) = (dx / len, dy / len);
        // Slope picks the glyph, so the trail reads as a line rather than a
        // dotted string: `-` when flat, `\` or `/` on the diagonals.
        let body = if uy.abs() < 0.35 * ux.abs() {
            '-'
        } else if ux * uy > 0.0 {
            '\\'
        } else {
            '/'
        };
        let (r, g, b) = self.palette.shooting_star.tuple();
        let steps = (len / self.glyph_px).floor() as i32;

        let ctx_ok = ctx.save().is_ok();
        let mut pen = GlyphPen::new(ctx, self.glyph_px);
        for i in 0..=steps {
            let d = i as f64 * self.glyph_px;
            // 0 at the tail, 1 at the head.
            let along = (d / len).clamp(0.0, 1.0);
            let head = i == steps;
            // Linear along the tail, not squared: a squared ramp puts the
            // first two thirds of the streak below the visible floor on
            // black, so a 167px meteor reads as a 40px tick.
            ctx.set_source_rgba(r, g, b, alpha * streak.alpha * (0.10 + 0.90 * along));
            pen.draw(
                ctx,
                if head { '*' } else { body },
                streak.tail.0 + ux * d,
                streak.tail.1 + uy * d,
            );
        }
        if ctx_ok {
            let _ = ctx.restore();
        }
    }
}

/// Draws single glyphs centred on a point, caching each glyph's centring
/// offset. The offsets come from `text_extents`, which is the only way `.`
/// (baseline-hugging) and `*` (raised) land on the same cell centre; caching
/// them keeps the bake to one extents lookup per distinct glyph.
struct GlyphPen {
    size: f64,
    offsets: HashMap<char, (f64, f64)>,
}

impl GlyphPen {
    fn new(ctx: &cairo::Context, size: f64) -> Self {
        ctx.select_font_face(
            "monospace",
            cairo::FontSlant::Normal,
            cairo::FontWeight::Normal,
        );
        ctx.set_font_size(size);
        Self {
            size,
            offsets: HashMap::new(),
        }
    }

    fn draw(&mut self, ctx: &cairo::Context, glyph: char, x: f64, y: f64) {
        let mut buf = [0u8; 4];
        let s: &str = glyph.encode_utf8(&mut buf);
        let size = self.size;
        let (ox, oy) = *self.offsets.entry(glyph).or_insert_with(|| {
            match ctx.text_extents(s) {
                Ok(e) => (-e.width() / 2.0 - e.x_bearing(), -e.height() / 2.0 - e.y_bearing()),
                Err(_) => (-size / 4.0, size / 4.0),
            }
        });
        ctx.move_to(x + ox, y + oy);
        let _ = ctx.show_text(s);
    }
}

/// Scatter stars over the rect. Count follows `star_density` per 10 000 px²,
/// so the field keeps its look on a different-sized surface.
fn build_stars(cfg: &Astro, rect: SkyRect, glyph_px: f64) -> Vec<Star> {
    let count = ((rect.w * rect.h / 10_000.0) * cfg.star_density.max(0.0)).round() as usize;
    if count == 0 {
        return Vec::new();
    }
    // A separate stream from the planet layout: the same `[astro] seed`
    // reshuffles both, but changing the roster must not move the stars.
    let mut rng = Rng::new(cfg.seed ^ 0x5A17_F1E1_D000_0001);
    let inset = glyph_px / 2.0;
    (0..count)
        .map(|_| {
            let x = rect.x + inset + rng.next_f64() * (rect.w - 2.0 * inset).max(0.0);
            let y = rect.y + inset + rng.next_f64() * (rect.h - 2.0 * inset).max(0.0);
            // Cubed: most stars are faint pinpricks and a handful are bright,
            // which is what a real field looks like. A uniform draw gives a
            // flat, noisy wash.
            let u = rng.next_f64();
            let brightness = u * u * u;
            let idx = ((brightness * STAR_GLYPHS.len() as f64) as usize).min(STAR_GLYPHS.len() - 1);
            Star {
                x,
                y,
                glyph: STAR_GLYPHS[idx],
                brightness,
            }
        })
        .collect()
}

/// Lay out the nebula clouds as glyph cells. Each cloud is a rotated ellipse
/// whose radial falloff is modulated by two octaves of value noise; cells
/// below [`NEBULA_THRESHOLD`] stay empty, which is what makes the edge ragged
/// instead of elliptical.
fn build_nebulas(cfg: &Astro, rect: SkyRect, glyph_px: f64) -> Vec<NebulaGlyph> {
    let mut out = Vec::new();
    if cfg.nebula_count == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
        return out;
    }
    let mut rng = Rng::new(cfg.seed ^ 0x5A17_F1E1_D000_0002);
    let base = rect.w.min(rect.h) * cfg.nebula_radius_fraction.clamp(0.02, 1.0);

    for n in 0..cfg.nebula_count {
        let cx = rect.x + (0.12 + rng.next_f64() * 0.76) * rect.w;
        let cy = rect.y + (0.12 + rng.next_f64() * 0.76) * rect.h;
        let rx = base * (0.65 + rng.next_f64() * 0.70);
        let ry = rx * (0.55 + rng.next_f64() * 0.55);
        let rot = rng.next_f64() * std::f64::consts::PI;
        let noise_seed = rng.next_u64();
        let (from, to) = (cfg.palette.nebula_tint(n), cfg.palette.nebula_tint(n + 1));
        let (sin_r, cos_r) = rot.sin_cos();

        // Only the cloud's own bounding box is walked, not the whole rect —
        // the bake is one-off, but three full-rect sweeps is still waste.
        let cell_w = glyph_px * GLYPH_ASPECT;
        let reach = rx.max(ry);
        let (x0, x1) = (cx - reach, cx + reach);
        let (y0, y1) = (cy - reach, cy + reach);
        let cols = ((x1 - x0) / cell_w).ceil() as i64;
        let rows = ((y1 - y0) / glyph_px).ceil() as i64;

        for row in 0..rows {
            for col in 0..cols {
                let x = x0 + col as f64 * cell_w;
                let y = y0 + row as f64 * glyph_px;
                if x < rect.x || x > rect.x + rect.w || y < rect.y || y > rect.y + rect.h {
                    continue;
                }
                // Into the ellipse's own frame.
                let (dx, dy) = (x - cx, y - cy);
                let u = (dx * cos_r + dy * sin_r) / rx;
                let v = (dy * cos_r - dx * sin_r) / ry;
                let d = u.hypot(v);
                if d >= 1.0 {
                    continue;
                }
                let falloff = smoothstep(1.0 - d);
                // Sample the noise in the *cloud's* frame, not the screen's.
                // The value-noise lattice is axis-aligned, so sampling in
                // world coordinates makes every cloud break along the same
                // horizontal and vertical seams; rotating with the ellipse
                // gives each one its own grain.
                let scale = glyph_px * NOISE_CELLS;
                let (nx, ny) = (
                    (dx * cos_r + dy * sin_r) / scale,
                    (dy * cos_r - dx * sin_r) / scale,
                );
                let density = falloff * fbm(nx, ny, noise_seed);
                if density <= NEBULA_THRESHOLD {
                    continue;
                }
                let t = ((density - NEBULA_THRESHOLD) / (1.0 - NEBULA_THRESHOLD)).clamp(0.0, 1.0);
                let idx =
                    ((t * NEBULA_GLYPHS.len() as f64) as usize).min(NEBULA_GLYPHS.len() - 1);
                out.push(NebulaGlyph {
                    x,
                    y,
                    glyph: NEBULA_GLYPHS[idx],
                    tint: from.mix(to, t),
                    alpha: lerp(NEBULA_ALPHA.0, NEBULA_ALPHA.1, t),
                });
            }
        }
    }
    out
}

/// Three octaves of value noise in `[0, 1)`. Two is enough to be non-uniform
/// but still reads as smooth blobs once thresholded; the third octave is what
/// gives a cloud its frayed, wispy edge. It is all baked, so the cost never
/// reaches a frame.
fn fbm(x: f64, y: f64, seed: u64) -> f64 {
    0.53 * value_noise(x, y, seed)
        + 0.30 * value_noise(x * 2.13, y * 2.13, seed ^ 0x9E37_79B9)
        + 0.17 * value_noise(x * 4.71, y * 4.71, seed ^ 0x85EB_CA6B)
}

fn value_noise(x: f64, y: f64, seed: u64) -> f64 {
    let (fx, fy) = (x.floor(), y.floor());
    let (i, j) = (fx as i64, fy as i64);
    let (sx, sy) = (smoothstep(x - fx), smoothstep(y - fy));
    let top = lerp(lattice(i, j, seed), lattice(i + 1, j, seed), sx);
    let bot = lerp(lattice(i, j + 1, seed), lattice(i + 1, j + 1, seed), sx);
    lerp(top, bot, sy)
}

/// Hash a lattice point to `[0, 1)`. Reuses the crate's SplitMix64 rather
/// than introducing a second mixer: seeding it with the mixed coordinates and
/// taking one draw is exactly a hash, and it keeps "one PRNG, no `rand`".
fn lattice(x: i64, y: i64, seed: u64) -> f64 {
    let key = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    Rng::new(seed ^ key).next_f64()
}

/// Hash a shooting-star event index into a seed offset.
fn event_key(index: f64) -> u64 {
    (index as i64 as u64).wrapping_mul(0xD1B5_4A32_D192_ED03)
}

fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live astro rect on this device (note 014).
    const RECT: SkyRect = SkyRect {
        x: 226.0,
        y: 0.0,
        w: 828.0,
        h: 639.0,
    };

    fn cfg() -> Astro {
        Astro::default()
    }

    #[test]
    fn the_starfield_is_seeded_and_stable() {
        let a = Backdrop::build(&cfg(), RECT);
        let b = Backdrop::build(&cfg(), RECT);
        assert_eq!(a.stars.len(), b.stars.len());
        for (p, q) in a.stars.iter().zip(&b.stars) {
            assert!((p.x - q.x).abs() < 1e-12 && (p.y - q.y).abs() < 1e-12);
            assert_eq!(p.glyph, q.glyph);
        }
    }

    #[test]
    fn a_different_seed_gives_a_different_starfield() {
        let a = Backdrop::build(&cfg(), RECT);
        let b = Backdrop::build(
            &Astro {
                seed: 99,
                ..cfg()
            },
            RECT,
        );
        let moved = a
            .stars
            .iter()
            .zip(&b.stars)
            .filter(|(p, q)| (p.x - q.x).abs() > 1.0 || (p.y - q.y).abs() > 1.0)
            .count();
        assert!(moved > a.stars.len() / 2, "only {moved} stars moved");
    }

    #[test]
    fn star_count_follows_density_and_area() {
        let c = cfg();
        let stars = Backdrop::build(&c, RECT).stars.len();
        let expect = (RECT.w * RECT.h / 10_000.0 * c.star_density).round() as usize;
        assert_eq!(stars, expect);
        // Half the rect, half the stars — density is per unit area, so the
        // field doesn't thin out on another monitor.
        let half = SkyRect {
            w: RECT.w / 2.0,
            ..RECT
        };
        let halved = Backdrop::build(&c, half).stars.len();
        assert!((halved as f64 - stars as f64 / 2.0).abs() <= 1.0);
    }

    #[test]
    fn zero_density_is_an_empty_field() {
        let sky = Backdrop::build(
            &Astro {
                star_density: 0.0,
                nebula_count: 0,
                ..cfg()
            },
            RECT,
        );
        assert!(sky.stars.is_empty() && sky.nebula.is_empty());
    }

    #[test]
    fn everything_generated_stays_inside_the_rect() {
        let sky = Backdrop::build(&cfg(), RECT);
        for s in &sky.stars {
            assert!(s.x >= RECT.x && s.x <= RECT.x + RECT.w, "star off in x");
            assert!(s.y >= RECT.y && s.y <= RECT.y + RECT.h, "star off in y");
        }
        for n in &sky.nebula {
            assert!(n.x >= RECT.x && n.x <= RECT.x + RECT.w, "nebula off in x");
            assert!(n.y >= RECT.y && n.y <= RECT.y + RECT.h, "nebula off in y");
        }
    }

    #[test]
    fn the_backdrop_is_ascii() {
        // The whole point of note 010's backdrop. A non-ASCII glyph would
        // also depend on the monospace font having it.
        let sky = Backdrop::build(&cfg(), RECT);
        for g in sky
            .stars
            .iter()
            .map(|s| s.glyph)
            .chain(sky.nebula.iter().map(|n| n.glyph))
        {
            assert!(g.is_ascii_graphic(), "{g:?} is not ASCII");
        }
    }

    #[test]
    fn most_stars_are_faint_and_a_few_are_bright() {
        // The cubed brightness curve is what stops the field reading as an
        // even wash of noise: a majority of dim pinpricks with a scattering
        // of bright ones. Both halves of that are the point, so assert both.
        let sky = Backdrop::build(&cfg(), RECT);
        let n = sky.stars.len();
        let faint = sky.stars.iter().filter(|s| s.brightness < 0.25).count();
        let bright = sky.stars.iter().filter(|s| s.brightness > 0.75).count();
        assert!(faint * 2 > n, "only {faint} of {n} stars are faint");
        assert!(bright * 6 < n, "{bright} of {n} stars are bright");
        assert!(bright > 0, "no bright stars at all");
    }

    #[test]
    fn nebulas_are_clouds_not_solid_ellipses() {
        let sky = Backdrop::build(&cfg(), RECT);
        assert!(!sky.nebula.is_empty(), "no nebula glyphs at all");
        // A filled ellipse of the configured size would be far denser; the
        // noise threshold has to be carving holes in it.
        let c = cfg();
        let r = RECT.w.min(RECT.h) * c.nebula_radius_fraction;
        let solid = std::f64::consts::PI * r * r / (c.glyph_px * c.glyph_px);
        assert!(
            (sky.nebula.len() as f64) < solid * c.nebula_count as f64 * 0.75,
            "{} glyphs is nearly solid",
            sky.nebula.len()
        );
    }

    #[test]
    fn nebulas_are_dim() {
        // They sit behind the stars; "nebula dims" in note 010.
        let sky = Backdrop::build(&cfg(), RECT);
        for n in &sky.nebula {
            assert!(n.alpha <= NEBULA_ALPHA.1 + 1e-9, "nebula too opaque");
        }
    }

    #[test]
    fn a_shooting_star_flies_then_stops() {
        let sky = Backdrop::build(&cfg(), RECT);
        let c = cfg();
        // Present through its duration, absent for the rest of the period.
        assert!(sky.shooting_star(0.0).is_some());
        assert!(sky.shooting_star(c.shooting_star_duration_s * 0.5).is_some());
        assert!(sky.shooting_star(c.shooting_star_duration_s + 0.01).is_none());
        assert!(sky.shooting_star(c.shooting_star_period_s * 0.9).is_none());
        // And the next period brings a new one.
        assert!(sky.shooting_star(c.shooting_star_period_s + 0.01).is_some());
    }

    #[test]
    fn a_shooting_star_travels_forward_and_fades_out() {
        let sky = Backdrop::build(&cfg(), RECT);
        let d = cfg().shooting_star_duration_s;
        let early = sky.shooting_star(d * 0.1).unwrap();
        let late = sky.shooting_star(d * 0.9).unwrap();
        let start = early.tail;
        let moved = (late.head.0 - start.0).hypot(late.head.1 - start.1);
        let first = (early.head.0 - start.0).hypot(early.head.1 - start.1);
        assert!(moved > first, "the head goes backwards");
        assert!(late.alpha < early.alpha, "it never dims");
        assert!((early.alpha - 1.0).abs() < 1e-9, "it launches dim");
    }

    #[test]
    fn the_tail_grows_to_full_length_then_holds() {
        let sky = Backdrop::build(&cfg(), RECT);
        let d = cfg().shooting_star_duration_s;
        let len = |t: f64| {
            let s = sky.shooting_star(t).unwrap();
            (s.head.0 - s.tail.0).hypot(s.head.1 - s.tail.1)
        };
        // At launch head and tail are together; by mid-flight the tail is at
        // its configured length and stays there.
        assert!(len(0.0) < 1.0);
        let mid = len(d * 0.5);
        let late = len(d * 0.95);
        assert!(mid > 1.0);
        assert!((mid - late).abs() < 1e-6, "tail {mid} then {late}");
        assert!((mid - sky.tail_px).abs() < 1e-6);
    }

    #[test]
    fn successive_shooting_stars_differ() {
        let sky = Backdrop::build(&cfg(), RECT);
        let p = cfg().shooting_star_period_s;
        let a = sky.shooting_star(0.1).unwrap();
        let b = sky.shooting_star(p + 0.1).unwrap();
        let c = sky.shooting_star(2.0 * p + 0.1).unwrap();
        assert!((a.head.0 - b.head.0).abs() > 1.0 || (a.head.1 - b.head.1).abs() > 1.0);
        assert!((b.head.0 - c.head.0).abs() > 1.0 || (b.head.1 - c.head.1).abs() > 1.0);
    }

    #[test]
    fn the_shooting_star_schedule_is_stateless() {
        // Asking out of order must give the same answer as asking in order —
        // the property that lets the ambient tick skip frames freely.
        let sky = Backdrop::build(&cfg(), RECT);
        let p = cfg().shooting_star_period_s;
        let forward: Vec<_> = [0.1, p + 0.2, 2.0 * p + 0.3]
            .iter()
            .map(|&t| sky.shooting_star(t).unwrap().head)
            .collect();
        let backward: Vec<_> = [2.0 * p + 0.3, p + 0.2, 0.1]
            .iter()
            .map(|&t| sky.shooting_star(t).unwrap().head)
            .collect();
        assert_eq!(forward, backward.into_iter().rev().collect::<Vec<_>>());
    }

    #[test]
    fn an_idle_backdrop_reports_nothing_to_animate() {
        // This is what buys the zero-frame idle: between meteors the ticker
        // has nothing to redraw.
        let sky = Backdrop::build(&cfg(), RECT);
        let c = cfg();
        assert!(sky.is_animating(0.05));
        assert!(!sky.is_animating(c.shooting_star_period_s * 0.5));
    }

    #[test]
    fn disabling_shooting_stars_is_allowed() {
        let sky = Backdrop::build(
            &Astro {
                shooting_star_period_s: 0.0,
                ..cfg()
            },
            RECT,
        );
        assert!(sky.shooting_star(1.0).is_none());
        assert!(!sky.is_animating(1.0));
        // And `None` — nothing will ever move, so the caller can stop its
        // timer rather than waking forever to be told "still nothing".
        assert_eq!(sky.seconds_until_next(1.0), None);
    }

    #[test]
    fn the_idle_gap_is_reported_so_the_ticker_can_sleep_through_it() {
        let sky = Backdrop::build(&cfg(), RECT);
        let c = cfg();
        // Mid-flight: nothing to wait for, the caller should be at frame rate.
        assert_eq!(sky.seconds_until_next(c.shooting_star_duration_s * 0.5), Some(0.0));
        // Just after one lands: essentially the whole period is dead air.
        let after = sky.seconds_until_next(c.shooting_star_duration_s + 0.1).unwrap();
        assert!(
            (after - (c.shooting_star_period_s - c.shooting_star_duration_s - 0.1)).abs() < 1e-9,
            "reported {after}"
        );
        // The wait always lands exactly on the next launch, in any period.
        for &t in &[0.95, 3.0, 6.99, 7.95, 20.0] {
            let wait = sky.seconds_until_next(t).unwrap();
            assert!(wait >= 0.0);
            assert!(
                sky.is_animating(t + wait + 1e-6),
                "waiting {wait} from {t} misses the next one"
            );
        }
    }

    #[test]
    fn a_negative_clock_does_not_wedge_the_schedule() {
        // `Instant::elapsed` can't go backwards, but the arithmetic must not
        // produce a negative sleep if it ever did.
        let sky = Backdrop::build(&cfg(), RECT);
        assert!(sky.seconds_until_next(-5.0).unwrap() >= 0.0);
        assert!(sky.shooting_star(-5.0).is_none());
    }

    #[test]
    fn a_degenerate_rect_generates_nothing() {
        let rect = SkyRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        let sky = Backdrop::build(&cfg(), rect);
        assert!(sky.stars.is_empty() && sky.nebula.is_empty());
    }

    #[test]
    fn value_noise_is_smooth_and_bounded() {
        // Neighbouring samples must not jump, or the clouds turn to static.
        let mut prev = fbm(0.0, 0.0, 7);
        for i in 1..200 {
            let v = fbm(i as f64 * 0.05, 0.0, 7);
            assert!((0.0..=1.0).contains(&v), "noise out of range: {v}");
            assert!((v - prev).abs() < 0.35, "noise jumped {prev} -> {v}");
            prev = v;
        }
    }
}
