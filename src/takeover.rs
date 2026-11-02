//! Taking over virtual devices that your own PipeWire config makes (e.g. a null sink in
//! `~/.config/pipewire/pipewire.conf.d/`), so this program makes them instead and can rename
//! them. Config files are moved aside, never edited or deleted, and `bsp revert` puts them back.

use crate::model;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a device comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// A file we can move aside. `also` lists other devices the same file makes.
    File { path: PathBuf, also: Vec<String> },
    /// A file we won't touch (system-wide, or your main pipewire.conf).
    Fixed(PathBuf),
    /// Not found in any PipeWire config (made by some program at runtime).
    Unknown,
}

/// Config directories whose files can be moved aside: each holds drop-in fragments.
fn movable_dirs() -> Vec<PathBuf> {
    let base = dirs::config_dir().unwrap_or_default().join("pipewire");
    vec![base.join("pipewire.conf.d"), base.join("pipewire-pulse.conf.d")]
}

/// Files that can make devices but aren't ours to move.
fn fixed_files() -> Vec<PathBuf> {
    let mut out = vec![];
    let user = dirs::config_dir().unwrap_or_default().join("pipewire");
    for f in ["pipewire.conf", "pipewire-pulse.conf"] {
        out.push(user.join(f));
    }
    for root in ["/etc/pipewire", "/usr/share/pipewire"] {
        for sub in ["pipewire.conf.d", "pipewire-pulse.conf.d"] {
            out.extend(conf_files(&Path::new(root).join(sub)));
        }
    }
    out
}

fn conf_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "conf"))
        .collect();
    v.sort();
    v
}

/// Device names a config file makes: `node.name = "x"` in `context.objects`, and
/// `sink_name=x` / `source_name=x` in pulse `load-module` lines.
pub fn names_in(text: &str) -> Vec<String> {
    let mut out = vec![];
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        for key in ["node.name", "sink_name", "source_name"] {
            let mut rest = line;
            while let Some(i) = rest.find(key) {
                rest = &rest[i + key.len()..];
                let v = rest.trim_start();
                let Some(v) = v.strip_prefix('=').or_else(|| v.strip_prefix(':')) else { continue };
                let v = v.trim_start();
                let name: String = match v.strip_prefix('"') {
                    Some(q) => q.chars().take_while(|&c| c != '"').collect(),
                    None => v.chars().take_while(|c| !c.is_whitespace() && !matches!(c, '}' | ']' | ',' | '"')).collect(),
                };
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// Where each of `names` comes from.
pub fn find(names: &[String]) -> BTreeMap<String, Origin> {
    let movable: Vec<(PathBuf, Vec<String>)> = movable_dirs()
        .iter()
        .flat_map(|d| conf_files(d))
        .filter_map(|p| Some((p.clone(), names_in(&std::fs::read_to_string(&p).ok()?))))
        .collect();
    let fixed: Vec<(PathBuf, Vec<String>)> = fixed_files()
        .into_iter()
        .filter_map(|p| Some((p.clone(), names_in(&std::fs::read_to_string(&p).ok()?))))
        .collect();
    names
        .iter()
        .map(|name| {
            let origin = if let Some((p, defined)) = movable.iter().find(|(_, d)| d.contains(name)) {
                Origin::File { path: p.clone(), also: defined.iter().filter(|d| !names.contains(d)).cloned().collect() }
            } else if let Some((p, _)) = fixed.iter().find(|(_, d)| d.contains(name)) {
                Origin::Fixed(p.clone())
            } else {
                Origin::Unknown
            };
            (name.clone(), origin)
        })
        .collect()
}

/// Moved files: where each one is now -> where it came from.
#[derive(Default, Serialize, Deserialize)]
struct Moved {
    #[serde(default)]
    files: BTreeMap<PathBuf, PathBuf>,
}

fn moved_dir() -> PathBuf {
    model::config_dir().join("moved-pipewire-config")
}

fn moved_record() -> PathBuf {
    moved_dir().join("moved.toml")
}

fn load_moved() -> Moved {
    std::fs::read_to_string(moved_record()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

/// Move config files aside (into our config folder), remembering where they came from.
pub fn move_aside(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut moved = load_moved();
    let mut out = vec![];
    for path in paths {
        let parent = path.parent().and_then(|p| p.file_name()).unwrap_or_default();
        let dir = moved_dir().join(parent);
        std::fs::create_dir_all(&dir)?;
        let mut dest = dir.join(path.file_name().unwrap_or_default());
        // Never overwrite an earlier backup.
        let mut k = 2;
        while dest.exists() {
            dest = dir.join(format!("{}.{k}", path.file_name().unwrap_or_default().to_string_lossy()));
            k += 1;
        }
        std::fs::rename(path, &dest).with_context(|| format!("moving {}", path.display()))?;
        moved.files.insert(dest.clone(), path.clone());
        out.push(dest);
    }
    std::fs::create_dir_all(moved_dir())?;
    model::write_atomic(&moved_record(), &toml::to_string_pretty(&moved)?)?;
    Ok(out)
}

/// Put every moved config file back. Returns the restored paths.
pub fn restore() -> Result<Vec<PathBuf>> {
    let mut moved = load_moved();
    let mut restored = vec![];
    let entries: Vec<(PathBuf, PathBuf)> = moved.files.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
    for (now, orig) in entries {
        if orig.exists() {
            log::warn!("not restoring {}: something is already there", orig.display());
            continue;
        }
        if let Some(p) = orig.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(&now, &orig).with_context(|| format!("restoring {}", orig.display()))?;
        moved.files.remove(&now);
        restored.push(orig);
    }
    model::write_atomic(&moved_record(), &toml::to_string_pretty(&moved)?)?;
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_device_names_in_pipewire_and_pulse_config() {
        let pw = r#"
context.objects = [
    {   factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = "system-in"
            node.description = "system-in"
        }
    }
    { factory = adapter args = { node.name = bare-name, media.class = Audio/Sink } }
    # node.name = "commented-out"
]"#;
        assert_eq!(names_in(pw), ["system-in", "bare-name"]);
        let pulse = r#"pulse.cmd = [ { cmd = "load-module" args = "module-null-sink sink_name=music sink_properties=device.description=Music" } ]"#;
        assert_eq!(names_in(pulse), ["music"]);
    }
}
