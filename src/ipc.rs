//! Line-delimited JSON over a Unix socket between the daemon and the GUI/CLI.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Status,
    /// Re-read the graph from disk (after the GUI saved, or to drop a preview).
    Reload,
    /// Apply the editor's unsaved version of the graph live. Dropped on reload.
    Preview { profile: crate::model::Graph },
    /// Make a switch node use one of its pins (0-based).
    Select { switch: String, pin: u32 },
    /// A keyboard shortcut was pressed: select every switch pin bound to it.
    Key { key: String },
    /// Signal levels of the processing nodes since the last call, by node name.
    Levels,
    /// Replace these virtual devices (made elsewhere, e.g. by your PipeWire config, which the
    /// caller has already moved aside) with our own of the same name.
    TakeOver { names: Vec<String> },
    /// Set the master volume (0..=1): a gain on everything leaving the virtual outputs.
    Master { volume: f32 },
    /// Stop the daemon. Its links and buses disappear with it.
    Stop { restore_paused: bool },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub graph_loaded: bool,
    /// Switch name -> selected pin.
    #[serde(default)]
    pub selected: crate::model::Selections,
    pub pid: u32,
    /// Programs paused for the duration of this session (restored on stop).
    pub paused: Vec<String>,
    pub links_owned: usize,
    pub buses_owned: Vec<String>,
    #[serde(default)]
    pub filters_running: Vec<String>,
    pub last_error: Option<String>,
    /// Master volume (0..=1).
    #[serde(default = "one")]
    pub master: f32,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Status(Status),
    Levels(std::collections::BTreeMap<String, crate::dsp::Reading>),
    Error(String),
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("better-sound-patching.sock")
}

pub fn request(req: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path()).context("daemon is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(&reply)?)
}

pub fn status() -> Option<Status> {
    match request(&Request::Status) {
        Ok(Response::Status(s)) => Some(s),
        _ => None,
    }
}
