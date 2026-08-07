//! Runtime config loaded from `$XDG_CONFIG_HOME/sliver-dock/config.toml`.
//!
//! Missing file or parse error → defaults. Every section has a Default so
//! users only specify what they override. The shape mirrors the schema
//! frozen in `docs/v1-plan.md` "Configuration".

use std::path::PathBuf;

use serde::Deserialize;

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

/// Astro view — planet sizing, sky layout and globe ring budget.
///
/// Note 010 §Configuration reserves `[astro]` for sizing clamps, ring budget,
/// seed, transparency and pan/pinch thresholds; slice 4 lands the first three.
/// The palette (`[astro.palette]`) belongs to the backdrop and arrives with
/// slice 5.
#[derive(Debug, Clone, Copy, Deserialize)]
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
