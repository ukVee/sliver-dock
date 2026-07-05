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
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Geometry {
    pub width_px: i32,
    pub margin_bottom_px: i32,
    pub default_height_px: i32,
    pub detailed_content_height_px: i32,
    pub taper_slope: f64,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width_px: 900,
            margin_bottom_px: 35,
            default_height_px: 6,
            detailed_content_height_px: 66,
            taper_slope: 0.35,
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
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            hide_when_fullscreen: true,
            remember_per_workspace: true,
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
