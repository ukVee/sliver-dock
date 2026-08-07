//! Per-app launch counts, persisted to
//! `$XDG_DATA_HOME/sliver-dock/launches.toml`.
//!
//! One number per app, bumped on every tap. Two consumers order themselves by
//! it: the astro view puts a planet's most-launched app at the front pole of
//! its globe (`categories::build_planets`), and the dock row's overrides read
//! the same file.
//!
//! Was part of `app_grid.rs` until astro-view slice 4 retired the radial grid;
//! the counts outlived it, so they moved here rather than dying with it.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct LaunchFile {
    #[serde(flatten)]
    entries: HashMap<String, LaunchEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct LaunchEntry {
    count: u64,
}

pub fn load() -> HashMap<String, u64> {
    let Some(path) = launches_path() else {
        return HashMap::new();
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            eprintln!("sliver-dock: cannot read {}: {e}", path.display());
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

pub fn save(map: &HashMap<String, u64>) -> io::Result<()> {
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
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        })?;
    Some(base.join("sliver-dock"))
}

fn launches_path() -> Option<PathBuf> {
    launches_dir().map(|d| d.join("launches.toml"))
}
