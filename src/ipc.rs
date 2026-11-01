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
    /// Re-read profiles from disk (after the GUI saved).
    Reload,
    /// Activate a profile.
    Switch { profile: String },
    /// Apply the editor's unsaved version of the active profile live. Dropped on reload/switch.
    Preview { profile: crate::model::Profile },
    /// Stop the daemon. Its links and buses disappear with it.
    Stop { restore_paused: bool },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub active_profile: Option<String>,
    pub pid: u32,
    /// Programs paused for the duration of this session (restored on stop).
    pub paused: Vec<String>,
    pub links_owned: usize,
    pub buses_owned: Vec<String>,
    #[serde(default)]
    pub eqs_running: Vec<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Status(Status),
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
