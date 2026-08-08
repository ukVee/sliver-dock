//! Runtime config loaded from `$XDG_CONFIG_HOME/sliver-dock/config.toml`.
//!
//! Missing file or parse error → defaults. Every section has a Default so
//! users only specify what they override. The shape mirrors the schema
//! frozen in `docs/v1-plan.md` "Configuration".

use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub geometry: Geometry,
    pub animation: Animation,
    pub terminal: Terminal,
    pub behaviour: Behaviour,
    pub astro: Astro,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Geometry {
    pub width_px: i32,
    pub margin_bottom_px: i32,
    pub default_height_px: i32,
    pub detailed_content_height_px: i32,
    pub taper_slope: f64,
    pub grid_height_px: i32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width_px: 900,
            margin_bottom_px: 35,
            default_height_px: 6,
            detailed_content_height_px: 66,
            taper_slope: 0.35,
            grid_height_px: 400,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Animation {
    pub snap_duration_ms: u64,
    pub fling_threshold_px_per_ms: f64,
    pub section_stagger_ms: u64,
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            snap_duration_ms: 250,
            fling_threshold_px_per_ms: 0.6,
            section_stagger_ms: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub command: String,
    pub count: u32,
    /// Override for the Wayfire `app-id` the daemon expects when a freshly
    /// spawned terminal toplevel maps. Defaults to `None`, which falls back
    /// to the basename of `command` (correct for foot / kitty / alacritty).
    /// Set this for terminals whose advertised xdg-toplevel app-id doesn't
    /// match their binary name (some wezterm builds use
    /// `org.wezfurlong.wezterm`).
    pub app_id: Option<String>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            command: "foot".to_owned(),
            count: 3,
            app_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Behaviour {
    pub hide_when_fullscreen: bool,
    pub remember_per_workspace: bool,
    pub auto_close_app_grid: bool,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            hide_when_fullscreen: true,
            remember_per_workspace: true,
            auto_close_app_grid: true,
        }
    }
}

/// An RGB colour, written in config as a hex string (`"#c0caf5"`, `"c0caf5"`
/// or the short `"#abc"`) and carried as unit floats so the render path never
/// re-parses. Anything else is a config error, which — like every other parse
/// error here — falls the whole file back to defaults with a message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl Rgb {
    /// Compile-time constructor for the palette defaults, taking the same
    /// 0–255 components the hex strings in note 010 spell out.
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
        }
    }

    /// `(r, g, b)` for cairo's `set_source_rgba`.
    pub fn tuple(self) -> (f64, f64, f64) {
        (self.r, self.g, self.b)
    }

    /// This colour mixed `t` of the way towards `other`. Used for the nebula
    /// tint ramp, so a cloud shades between two palette entries instead of
    /// needing one entry per density step.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        Rgb {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        let byte = |i: usize| -> Result<u8, String> {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| format!("{s}: {e}"))
        };
        match hex.len() {
            6 => Ok(Self::new(byte(0)?, byte(2)?, byte(4)?)),
            // `#abc` is `#aabbcc`, the usual CSS shorthand.
            3 => {
                let nib = |i: usize| -> Result<u8, String> {
                    u8::from_str_radix(&hex[i..i + 1], 16)
                        .map(|v| v * 17)
                        .map_err(|e| format!("{s}: {e}"))
                };
                Ok(Self::new(nib(0)?, nib(1)?, nib(2)?))
            }
            n => Err(format!("{s}: expected 3 or 6 hex digits, got {n}")),
        }
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Rgb::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// `[astro.palette]` — every colour the astro view paints, defaulting to
/// Tokyo Night per note 010 §Configuration. Lifted here in slice 5: the
/// planet accents and the label colour were `const`s at the top of `astro.rs`
/// through slice 4, and the backdrop needs a palette anyway — one palette,
/// not two.
///
/// The two list-valued entries take any number of colours; they are indexed
/// modulo their length, so a single-entry list is a flat scheme and an empty
/// one falls back to the defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Palette {
    /// The astro rect's fill. Opaque black per note 010 ("solid black, zero
    /// translucency"); the transparency flag that would soften it is a
    /// documented later option.
    pub background: Rgb,
    /// Brightest stars. Note 010's `#c0caf5`.
    pub star: Rgb,
    /// Faintest stars. The starfield ramps between this and `star` by
    /// brightness, which is what gives the field depth.
    pub star_dim: Rgb,
    /// Nebula tints, dim by design — a cloud shades from the first towards
    /// the second as its density rises, so two entries read as one cloud.
    pub nebula: Vec<Rgb>,
    /// The shooting star's head; its tail fades this out along the streak.
    pub shooting_star: Rgb,
    /// Planet globes, assigned round-robin in roster order so a planet keeps
    /// its colour across restarts.
    pub planet_accents: Vec<Rgb>,
    /// Planet name under each globe.
    pub label: Rgb,
}

/// Fallbacks for the list-valued palette entries when a config empties them.
const DEFAULT_NEBULA: [Rgb; 2] = [Rgb::new(0x3d, 0x59, 0xa1), Rgb::new(0xbb, 0x9a, 0xf7)];
const DEFAULT_ACCENTS: [Rgb; 3] = [
    Rgb::new(0x7a, 0xa2, 0xf7),
    Rgb::new(0xbb, 0x9a, 0xf7),
    Rgb::new(0x7d, 0xcf, 0xff),
];

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: Rgb::new(0x00, 0x00, 0x00),
            star: Rgb::new(0xc0, 0xca, 0xf5),
            star_dim: Rgb::new(0x56, 0x5f, 0x89),
            nebula: DEFAULT_NEBULA.to_vec(),
            shooting_star: Rgb::new(0xe0, 0xaf, 0x68),
            planet_accents: DEFAULT_ACCENTS.to_vec(),
            label: Rgb::new(0xc0, 0xca, 0xf5),
        }
    }
}

impl Palette {
    /// Accent `i`, wrapping. Never panics on an empty list — a palette with
    /// no accents falls back to the Tokyo Night defaults rather than taking
    /// the view down.
    pub fn accent(&self, i: usize) -> Rgb {
        Self::pick(&self.planet_accents, &DEFAULT_ACCENTS, i)
    }

    /// Nebula tint `i`, wrapping, same fallback rule as [`Self::accent`].
    pub fn nebula_tint(&self, i: usize) -> Rgb {
        Self::pick(&self.nebula, &DEFAULT_NEBULA, i)
    }

    /// How many accents the roster cycles through.
    pub fn accent_count(&self) -> usize {
        self.planet_accents.len().max(DEFAULT_ACCENTS.len())
    }

    fn pick(list: &[Rgb], fallback: &[Rgb], i: usize) -> Rgb {
        let list = if list.is_empty() { fallback } else { list };
        list[i % list.len()]
    }
}

/// Astro view — planet sizing, sky layout, globe ring budget, and the
/// backdrop's density/timing knobs.
///
/// Note 010 §Configuration reserves `[astro]` for sizing clamps, ring budget,
/// seed, transparency and pan/pinch thresholds; slice 4 landed the first
/// three, slice 5 adds the backdrop's and the `[astro.palette]` subsection.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Astro {
    /// Layout seed. The sky is deterministic — the same seed and the same
    /// planet roster always produce the same positions, so the launcher's
    /// muscle memory survives a restart. Change it to reshuffle.
    pub seed: u64,
    /// `radius = planet_base_radius_px * sqrt(app_count)`, then clamped to
    /// `[planet_min_radius_px, planet_max_radius_px]`. On this device the
    /// per-planet spread is 2–10 apps, i.e. sqrt 1.41–3.16.
    pub planet_base_radius_px: f64,
    pub planet_min_radius_px: f64,
    pub planet_max_radius_px: f64,
    /// Share of the astro rect the planet discs may cover. Above this every
    /// radius shrinks by the same factor, so a big roster stays laid out
    /// rather than overlapping.
    pub fill_fraction: f64,
    /// Globe icon size as a fraction of the planet radius, clamped. Icons are
    /// foreshortened on top of this by their position on the sphere.
    pub icon_ratio: f64,
    pub icon_min_px: f64,
    pub icon_max_px: f64,
    /// Centre-to-centre icon spacing as a multiple of the icon size. Drives
    /// both the ring budget and each ring's capacity.
    pub ring_pitch_ratio: f64,
    /// How far from the front pole (degrees) the rings may spread before
    /// spilling onto the back of the globe. With `ring_pitch_ratio` this is
    /// the "ring budget scales with planet size" rule of note 010: a globe
    /// fits `radius * front_arc / pitch` rings on its front face.
    pub front_arc_deg: f64,

    // ---- backdrop (slice 5) ------------------------------------------
    /// Stars per 10 000 px² of the astro rect, so the field keeps its
    /// density on a different monitor instead of thinning out.
    pub star_density: f64,
    /// Glyph pitch of the starfield and the nebula clouds. The backdrop is
    /// ASCII (note 010), so this is effectively its character cell.
    pub glyph_px: f64,
    /// Nebula clouds in the rect. Each is an ellipse of glyphs whose density
    /// falls off from its centre.
    pub nebula_count: usize,
    /// Nebula radius as a fraction of the rect's smaller side.
    pub nebula_radius_fraction: f64,
    /// Seconds between shooting stars. One is in flight at a time; the
    /// schedule is derived from the clock, so it needs no state.
    pub shooting_star_period_s: f64,
    /// How long one shooting star takes to cross, in seconds.
    pub shooting_star_duration_s: f64,
    /// Shooting-star tail length, as a fraction of the rect's diagonal.
    pub shooting_star_tail_fraction: f64,

    /// Colours (`[astro.palette]`), Tokyo Night by default.
    pub palette: Palette,
}

impl Default for Astro {
    fn default() -> Self {
        Self {
            seed: 1,
            planet_base_radius_px: 34.0,
            planet_min_radius_px: 40.0,
            planet_max_radius_px: 104.0,
            fill_fraction: 0.28,
            icon_ratio: 0.34,
            icon_min_px: 14.0,
            icon_max_px: 30.0,
            ring_pitch_ratio: 1.3,
            front_arc_deg: 78.0,
            star_density: 6.0,
            glyph_px: 9.0,
            nebula_count: 3,
            nebula_radius_fraction: 0.42,
            shooting_star_period_s: 7.0,
            shooting_star_duration_s: 0.9,
            shooting_star_tail_fraction: 0.16,
            palette: Palette::default(),
        }
    }
}

pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            eprintln!("sliver-dock: cannot read {}: {e}", path.display());
            return Config::default();
        }
    };
    match toml::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "sliver-dock: config parse error in {}: {e}",
                path.display()
            );
            Config::default()
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sliver-dock").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn hex_colours_parse_in_every_accepted_spelling() {
        let want = Rgb::new(0xc0, 0xca, 0xf5);
        assert_eq!(Rgb::parse("#c0caf5").unwrap(), want);
        assert_eq!(Rgb::parse("c0caf5").unwrap(), want);
        assert_eq!(Rgb::parse("#C0CAF5").unwrap(), want);
        // CSS shorthand expands each nibble, so #abc is #aabbcc.
        assert_eq!(Rgb::parse("#abc").unwrap(), Rgb::parse("#aabbcc").unwrap());
    }

    #[test]
    fn hex_components_land_in_unit_range() {
        let white = Rgb::parse("#ffffff").unwrap();
        assert!(close(white.r, 1.0) && close(white.g, 1.0) && close(white.b, 1.0));
        let black = Rgb::parse("#000000").unwrap();
        assert!(close(black.r, 0.0) && close(black.g, 0.0) && close(black.b, 0.0));
        let blue = Rgb::parse("#7aa2f7").unwrap();
        assert!(close(blue.r, 0x7a as f64 / 255.0));
        assert!(close(blue.b, 0xf7 as f64 / 255.0));
    }

    #[test]
    fn a_bad_colour_is_an_error_not_a_guess() {
        // It has to fail, so `load` reports it and falls back to defaults —
        // silently substituting a colour would be a debugging trap.
        for bad in ["#12345", "nope", "#gggggg", "", "#1234567"] {
            assert!(Rgb::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn the_default_palette_is_the_tokyo_night_one_note_010_specifies() {
        let p = Palette::default();
        assert_eq!(p.star, Rgb::parse("#c0caf5").unwrap());
        assert_eq!(p.label, Rgb::parse("#c0caf5").unwrap());
        assert_eq!(
            p.planet_accents,
            vec![
                Rgb::parse("#7aa2f7").unwrap(),
                Rgb::parse("#bb9af7").unwrap(),
                Rgb::parse("#7dcfff").unwrap(),
            ]
        );
        assert_eq!(p.background, Rgb::parse("#000000").unwrap());
    }

    #[test]
    fn palette_lists_wrap_and_survive_being_emptied() {
        let p = Palette::default();
        assert_eq!(p.accent(0), p.accent(3));
        assert_eq!(p.accent(4), p.accent(1));
        assert_eq!(p.accent_count(), 3);
        // An empty list falls back to the defaults rather than panicking on
        // the modulo — a config typo must not take the launcher down.
        let empty = Palette {
            planet_accents: Vec::new(),
            nebula: Vec::new(),
            ..Palette::default()
        };
        assert_eq!(empty.accent(7), p.accent(7));
        assert_eq!(empty.nebula_tint(2), p.nebula_tint(2));
        assert_eq!(empty.accent_count(), 3);
    }

    #[test]
    fn a_single_accent_is_a_flat_scheme() {
        let one = Palette {
            planet_accents: vec![Rgb::parse("#ff0000").unwrap()],
            ..Palette::default()
        };
        assert_eq!(one.accent(0), one.accent(5));
    }

    #[test]
    fn mix_interpolates_between_two_colours() {
        let a = Rgb::parse("#000000").unwrap();
        let b = Rgb::parse("#ffffff").unwrap();
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert!(close(a.mix(b, 0.5).r, 0.5));
        // Out-of-range t clamps rather than extrapolating past the endpoints.
        assert_eq!(a.mix(b, 2.0), b);
        assert_eq!(a.mix(b, -1.0), a);
    }

    #[test]
    fn a_partial_astro_section_keeps_the_other_defaults() {
        // `r##` because the hex colours contain `"#`, which would close an
        // `r#` raw string.
        let cfg: Config = toml::from_str(
            r##"
            [astro]
            seed = 42
            [astro.palette]
            star = "#ff0000"
            "##,
        )
        .unwrap();
        let d = Astro::default();
        assert_eq!(cfg.astro.seed, 42);
        assert!(close(cfg.astro.star_density, d.star_density));
        assert!(close(cfg.astro.fill_fraction, d.fill_fraction));
        assert_eq!(cfg.astro.palette.star, Rgb::parse("#ff0000").unwrap());
        // Overriding one colour must not blank the rest of the palette.
        assert_eq!(cfg.astro.palette.planet_accents, d.palette.planet_accents);
        assert_eq!(cfg.astro.palette.label, d.palette.label);
    }

    #[test]
    fn a_bad_colour_in_the_palette_fails_the_parse() {
        // `load` turns this into the message-plus-defaults path.
        let bad: Result<Config, _> = toml::from_str(
            r##"
            [astro.palette]
            star = "#zzzzzz"
            "##,
        );
        assert!(bad.is_err());
    }
}
