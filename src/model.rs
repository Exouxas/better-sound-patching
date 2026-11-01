//! Persistent data model: profiles made of nodes and wires, stored as TOML.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A reference to a PipeWire node by its stable `node.name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRef {
    /// Stable identifier, e.g. `alsa_output.usb-Astro_Gaming_Astro_A50-00.stereo-chat`.
    pub node_name: String,
    /// Human-readable name remembered for display while the device is unplugged.
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BusKind {
    /// A virtual speaker (Audio/Sink). Apps play into it; its output is what they played.
    #[default]
    Playback,
    /// A virtual microphone (Audio/Source). Feed it audio; apps can record from it.
    Microphone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Channels {
    Mono,
    #[default]
    Stereo,
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusSpec {
    /// PipeWire `node.name`. Buses with the same name in different profiles are the same bus,
    /// which is what lets profile switches happen without apps noticing.
    pub name: String,
    #[serde(default)]
    pub kind: BusKind,
    #[serde(default)]
    pub channels: Channels,
}

impl BusSpec {
    pub fn media_class(&self) -> &'static str {
        match self.kind {
            BusKind::Playback => "Audio/Sink",
            BusKind::Microphone => "Audio/Source/Virtual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NodeKind {
    /// A specific capture device (or any audio source).
    Input { device: Option<DeviceRef> },
    /// A specific playback device (or any audio sink).
    Output { device: Option<DeviceRef> },
    /// A virtual device owned by this profile.
    Bus(BusSpec),
    /// Stands in for every hardware capture device that has no Input node.
    FallbackInput,
    /// Stands in for every hardware playback device that has no Output node.
    FallbackOutput,
}

impl NodeKind {
    pub fn has_input(&self) -> bool {
        matches!(self, NodeKind::Output { .. } | NodeKind::Bus(_) | NodeKind::FallbackOutput)
    }
    pub fn has_output(&self) -> bool {
        matches!(self, NodeKind::Input { .. } | NodeKind::Bus(_) | NodeKind::FallbackInput)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: u32,
    pub pos: [f32; 2],
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Audio flows from node `from` to node `to`. Channels are matched automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Wire {
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, rename = "node")]
    pub nodes: Vec<GraphNode>,
    #[serde(default, rename = "wire")]
    pub wires: Vec<Wire>,
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    pub active_profile: Option<String>,
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("better-sound-patching")
}

fn profiles_dir() -> PathBuf {
    config_dir().join("profiles")
}

pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        && !name.starts_with('.')
}

fn profile_path(name: &str) -> Result<PathBuf> {
    if !valid_profile_name(name) {
        bail!("invalid profile name {name:?} (use letters, digits, space, - _ .)");
    }
    Ok(profiles_dir().join(format!("{name}.toml")))
}

pub fn list_profiles() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profiles_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "toml")
                .then(|| p.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

pub fn load_profile(name: &str) -> Result<Profile> {
    let path = profile_path(name)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_profile(name: &str, profile: &Profile) -> Result<()> {
    let path = profile_path(name)?;
    std::fs::create_dir_all(profiles_dir())?;
    write_atomic(&path, &toml::to_string_pretty(profile)?)
}

pub fn delete_profile(name: &str) -> Result<()> {
    Ok(std::fs::remove_file(profile_path(name)?)?)
}

pub fn load_state() -> AppState {
    std::fs::read_to_string(config_dir().join("state.toml"))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(state: &AppState) -> Result<()> {
    std::fs::create_dir_all(config_dir())?;
    write_atomic(&config_dir().join("state.toml"), &toml::to_string_pretty(state)?)
}

/// Your own names for devices, keyed by `node.name`. Shared by all profiles and the mixer.
pub type Labels = std::collections::BTreeMap<String, String>;

pub fn load_labels() -> Labels {
    std::fs::read_to_string(config_dir().join("labels.toml"))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_labels(labels: &Labels) -> Result<()> {
    std::fs::create_dir_all(config_dir())?;
    write_atomic(&config_dir().join("labels.toml"), &toml::to_string_pretty(labels)?)
}

pub fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrips_through_toml() {
        let p = Profile {
            nodes: vec![
                GraphNode {
                    id: 1,
                    pos: [0.0, 0.0],
                    kind: NodeKind::Bus(BusSpec {
                        name: "system-in".into(),
                        kind: BusKind::Playback,
                        channels: Channels::Stereo,
                    }),
                },
                GraphNode {
                    id: 2,
                    pos: [100.0, 0.0],
                    kind: NodeKind::Output {
                        device: Some(DeviceRef {
                            node_name: "alsa_output.x".into(),
                            description: "X".into(),
                        }),
                    },
                },
                GraphNode { id: 3, pos: [0.0, 50.0], kind: NodeKind::FallbackInput },
            ],
            wires: vec![Wire { from: 1, to: 2 }],
        };
        let text = toml::to_string_pretty(&p).unwrap();
        let back: Profile = toml::from_str(&text).unwrap();
        assert_eq!(p, back);
    }
}
