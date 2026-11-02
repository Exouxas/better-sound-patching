//! Trial sessions, approval and revert.
//!
//! The rule: nothing persistent changes until you approve. A trial only pauses programs for
//! this login session and keeps a record so they can be relaunched; rebooting always gives you
//! your old setup back. Approval is reversible with `revert`.

use crate::engine::{self, Cmd, EngineConfig};
use crate::ipc::{self, Request, Response};
use crate::model;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Programs that also manage routing and would fight with us.
pub const KNOWN_CONFLICTS: &[(&str, &str)] = &[
    ("qpwgraph", "Its exclusive patchbay re-draws its own links and removes ours."),
    ("easyeffects", "Its effect chains link themselves to devices. Pause it unless your graph routes through it."),
];

/// Which conflicting programs to pause/disable by default: qpwgraph always, EasyEffects
/// unless the graph still routes through its devices.
pub fn default_conflicts() -> Vec<String> {
    let uses_easyeffects = model::load_graph().ok().is_some_and(|p| {
            p.nodes.iter().any(|n| match &n.kind {
                model::NodeKind::Input { device: Some(d) } | model::NodeKind::Output { device: Some(d) } => {
                    d.node_name.starts_with("easyeffects")
                }
                _ => false,
            })
        });
    let mut v = vec!["qpwgraph".to_owned()];
    if !uses_easyeffects {
        v.push("easyeffects".to_owned());
    }
    v
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PausedApp {
    pub name: String,
    /// Command used to bring it back (taken from its autostart entry when there is one).
    pub relaunch: Vec<String>,
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

fn paused_record() -> PathBuf {
    runtime_dir().join("better-sound-patching-paused.json")
}

pub fn pids_of(process: &str) -> Vec<(u32, Vec<String>)> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            if pid == me {
                return None;
            }
            let comm = std::fs::read_to_string(e.path().join("comm")).ok()?;
            if comm.trim() != process {
                return None;
            }
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let args = cmdline
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            Some((pid, args))
        })
        .collect()
}

pub fn is_running(process: &str) -> bool {
    !pids_of(process).is_empty()
}

fn autostart_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("autostart")
}

fn disabled_autostart_dir() -> PathBuf {
    model::config_dir().join("disabled-autostart")
}

fn desktop_exec(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some(exec) = line.strip_prefix("Exec=") {
            return Some(
                exec.split_whitespace()
                    .filter(|a| !(a.len() == 2 && a.starts_with('%')))
                    .map(str::to_owned)
                    .collect(),
            );
        }
    }
    None
}

/// The autostart `.desktop` file that launches `process`, if any.
pub fn autostart_entry(process: &str, dir: &Path) -> Option<(PathBuf, Vec<String>)> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let path = e.path();
        if path.extension()? != "desktop" {
            return None;
        }
        let exec = desktop_exec(&path)?;
        let bin = Path::new(exec.first()?).file_name()?.to_str()?;
        (bin == process).then_some((path, exec))
    })
}

fn stop_process(process: &str) {
    for (pid, _) in pids_of(process) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while is_running(process) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    for (pid, _) in pids_of(process) {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

/// Relaunched programs log next to the daemon, so a failed relaunch leaves a trace.
fn app_log() -> Stdio {
    let path = ipc::socket_path().with_file_name("better-sound-patching-apps.log");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

fn launch_detached(cmd: &[String]) -> Result<()> {
    let Some((bin, args)) = cmd.split_first() else { bail!("empty command") };
    Command::new("setsid")
        .arg("-f")
        .arg(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(app_log())
        .stderr(app_log())
        .status()
        .with_context(|| format!("launching {bin}"))?;
    Ok(())
}

/// Open the editor as its own program. Started through systemd when possible, so it isn't
/// part of the daemon's service and keeps running if the daemon restarts.
/// `activation` is the desktop's token for the click that asked for it (lets the new window
/// take focus on Wayland).
pub fn open_editor(activation: Option<String>) -> Result<()> {
    // After an update replaced the file, Linux reports our own path with " (deleted)" on the
    // end; the new program is at the plain path.
    let exe = std::env::current_exe()?.to_string_lossy().trim_end_matches(" (deleted)").to_owned();
    let via_systemd = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet"])
        .args(activation.iter().flat_map(|t| ["--setenv".to_owned(), format!("XDG_ACTIVATION_TOKEN={t}")]))
        .arg("--unit")
        .arg(format!("better-sound-patching-editor-{}", std::process::id() ^ (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos())))
        .args([exe.as_str(), "gui"])
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !via_systemd {
        launch_detached(&[exe, "gui".into()])?;
    }
    Ok(())
}

pub fn pause_apps(names: &[String]) -> Vec<PausedApp> {
    let mut paused = Vec::new();
    for name in names {
        let running = pids_of(name);
        if running.is_empty() {
            continue;
        }
        let relaunch = autostart_entry(name, &autostart_dir())
            .map(|(_, exec)| exec)
            .unwrap_or_else(|| running[0].1.clone());
        log::info!("pausing {name} for this session (will relaunch with {relaunch:?})");
        stop_process(name);
        paused.push(PausedApp { name: name.clone(), relaunch });
    }
    if !paused.is_empty()
        && let Ok(json) = serde_json::to_string_pretty(&paused)
    {
        let _ = std::fs::write(paused_record(), json);
    }
    paused
}

/// Relaunch everything a trial paused. Also works after a daemon crash.
pub fn restore_paused() -> Result<Vec<String>> {
    let Ok(text) = std::fs::read_to_string(paused_record()) else { return Ok(vec![]) };
    let paused: Vec<PausedApp> = serde_json::from_str(&text).unwrap_or_default();
    let mut restored = vec![];
    let mut failed = vec![];
    for app in paused {
        if is_running(&app.name) || relaunch(&app) {
            restored.push(app.name);
        } else {
            failed.push(app);
        }
    }
    if failed.is_empty() {
        let _ = std::fs::remove_file(paused_record());
        Ok(restored)
    } else {
        // Keep the record so the GUI's restore button and `bsp restore` can try again.
        let names: Vec<_> = failed.iter().map(|a| a.name.clone()).collect();
        let _ = model::write_atomic(&paused_record(), &serde_json::to_string(&failed)?);
        bail!("could not relaunch {}", names.join(", "))
    }
}

/// Launch a paused program and make sure it stays up. Some programs (qpwgraph) leave a stale
/// single-instance lock when killed: the next launch quietly exits after clearing it, and the
/// one after that works. So retry until the process survives a short grace period.
fn relaunch(app: &PausedApp) -> bool {
    for attempt in 1..=3 {
        log::info!("relaunching {} (attempt {attempt})", app.name);
        if let Err(e) = launch_detached(&app.relaunch) {
            log::error!("{e:#}");
            return false;
        }
        let deadline = Instant::now() + Duration::from_millis(2500);
        let mut alive_since = None;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            match (is_running(&app.name), alive_since) {
                (true, None) => alive_since = Some(Instant::now()),
                (true, Some(t)) if t.elapsed() > Duration::from_millis(1500) => return true,
                (false, _) => alive_since = None,
                _ => {}
            }
        }
        if is_running(&app.name) {
            return true;
        }
    }
    false
}

pub fn paused_pending() -> Vec<PausedApp> {
    std::fs::read_to_string(paused_record())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Run the routing daemon in the foreground.
pub fn run_daemon(pause: &[String]) -> Result<()> {
    if ipc::status().is_some() {
        bail!("a daemon is already running");
    }
    let sock = ipc::socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;

    let paused = pause_apps(pause);
    let (tx, rx) = pipewire::channel::channel::<Cmd>();
    // Test daemons never grab global shortcuts.
    let hotkeys = std::env::var_os("BSP_TEST_KEEP_HARDWARE").is_none().then(|| crate::hotkeys::spawn(tx.clone()));
    // Nor a tray icon.
    let tray = (std::env::var_os("BSP_TEST_KEEP_HARDWARE").is_none() || std::env::var_os("BSP_TEST_TRAY").is_some())
        .then(|| crate::tray::spawn(tx.clone()))
        .flatten();

    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut line = String::new();
                let mut reader = BufReader::new(&conn);
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let resp = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => {
                        let (rtx, rrx) = std::sync::mpsc::channel();
                        if tx.send(Cmd::Ipc(req, rtx)).is_err() {
                            return;
                        }
                        rrx.recv_timeout(Duration::from_secs(4))
                            .unwrap_or_else(|_| Response::Error("daemon busy".into()))
                    }
                    Err(e) => Response::Error(format!("bad request: {e}")),
                };
                if let Ok(mut s) = serde_json::to_string(&resp) {
                    s.push('\n');
                    let _ = (&conn).write_all(s.as_bytes());
                }
            });
        }
    });

    let result = engine::run(
        EngineConfig {
            enforce: true,
            on_snapshot: Box::new(|_| {}),
            paused: paused.iter().map(|p| p.name.clone()).collect(),
            hotkeys,
            on_master: tray.map(|h| Box::new(move |m: f32| crate::tray::show_master(&h, m)) as Box<dyn Fn(f32)>),
        },
        rx,
    );
    let _ = std::fs::remove_file(&sock);

    let restore = match &result {
        Ok(info) => info.restore_paused,
        Err(_) => true,
    };
    if restore {
        match restore_paused() {
            Ok(names) if !names.is_empty() => log::info!("restored {}", names.join(", ")),
            Err(e) => log::error!("restoring paused programs: {e:#}"),
            _ => {}
        }
    }
    result.map(|_| ())
}

/// Start a trial daemon in the background, detached from the GUI.
pub fn start_trial(pause: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = runtime_dir().join("better-sound-patching.log");
    let mut cmd = Command::new("setsid");
    cmd.arg("-f").arg(exe).arg("daemon");
    for p in pause {
        cmd.arg("--pause").arg(p);
    }
    let logf = std::fs::File::create(&log)?;
    cmd.stdin(Stdio::null()).stdout(logf.try_clone()?).stderr(logf);
    cmd.status().context("starting daemon")?;
    // Wait for the socket so the caller can talk to it immediately.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if ipc::status().is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("daemon did not start; see {}", log.display())
}

pub fn stop_daemon(restore: bool) -> Result<()> {
    match ipc::request(&Request::Stop { restore_paused: restore })? {
        Response::Error(e) => bail!(e),
        _ => {
            let deadline = Instant::now() + Duration::from_secs(5);
            while ipc::socket_path().exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- approval

const UNIT: &str = "better-sound-patching.service";

fn unit_path() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("systemd/user").join(UNIT)
}

fn installed_bin() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".local/bin/bsp")
}

pub fn is_approved() -> bool {
    unit_path().exists()
}

fn systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl").arg("--user").args(args).status()?;
    if !st.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

/// Make the new system permanent: install the binary and a user service that starts at
/// login, and stop the given programs from autostarting (their entries are moved aside, not
/// deleted, so `revert` can put them back).
pub fn approve(disable: &[String]) -> Result<()> {
    if !model::graph_exists() {
        bail!("no routing graph yet; build and save one in the editor first");
    }
    let bin = installed_bin();
    std::fs::create_dir_all(bin.parent().unwrap())?;
    let tmp = bin.with_extension("new");
    std::fs::copy(std::env::current_exe()?, &tmp)?;
    std::fs::rename(&tmp, &bin)?;

    if ipc::status().is_some() {
        stop_daemon(false)?;
    }
    let _ = std::fs::remove_file(paused_record());

    std::fs::create_dir_all(disabled_autostart_dir())?;
    for name in disable {
        if let Some((path, _)) = autostart_entry(name, &autostart_dir()) {
            let dest = disabled_autostart_dir().join(path.file_name().unwrap());
            std::fs::rename(&path, &dest)?;
            println!("autostart for {name} moved to {}", dest.display());
        }
        stop_process(name);
    }

    let unit = format!(
        "[Unit]\n\
         Description=Better Sound Patching routing daemon\n\
         After=pipewire.service wireplumber.service\n\
         Requires=pipewire.service\n\
         \n\
         [Service]\n\
         ExecStart={bin} daemon\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = bin.display()
    );
    std::fs::create_dir_all(unit_path().parent().unwrap())?;
    model::write_atomic(&unit_path(), &unit)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])?;
    install_desktop_entry(&bin)?;
    Ok(())
}

/// Undo `approve`: remove the service and restore the moved autostart entries.
pub fn revert() -> Result<()> {
    if unit_path().exists() {
        let _ = systemctl(&["disable", "--now", UNIT]);
        std::fs::remove_file(unit_path())?;
        systemctl(&["daemon-reload"])?;
    } else if ipc::status().is_some() {
        stop_daemon(true)?;
    }
    for e in std::fs::read_dir(disabled_autostart_dir()).into_iter().flatten().flatten() {
        let path = e.path();
        let dest = autostart_dir().join(path.file_name().unwrap());
        std::fs::create_dir_all(autostart_dir())?;
        std::fs::rename(&path, &dest)?;
        if let Some(exec) = desktop_exec(&dest) {
            let name = Path::new(&exec[0]).file_name().and_then(|s| s.to_str()).unwrap_or_default();
            if !is_running(name) {
                launch_detached(&exec)?;
            }
        }
        println!("restored autostart {}", dest.display());
    }
    restore_paused()?;
    // Devices taken over from your PipeWire config: put the config back and restart PipeWire
    // so it makes them again.
    let restored = crate::takeover::restore()?;
    if !restored.is_empty() {
        for p in &restored {
            println!("restored {}", p.display());
        }
        systemctl(&["restart", "pipewire", "pipewire-pulse", "wireplumber"])?;
    }
    Ok(())
}

fn install_desktop_entry(bin: &Path) -> Result<()> {
    let icons = dirs::data_dir().unwrap_or_default().join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icons)?;
    model::write_atomic(&icons.join("better-sound-patching.svg"), include_str!("../assets/icon.svg"))?;
    let dir = dirs::data_dir().unwrap_or_default().join("applications");
    std::fs::create_dir_all(&dir)?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Better Sound Patching\n\
         Comment=Audio routing and mixer\nExec={} gui\nIcon=better-sound-patching\n\
         Categories=AudioVideo;Audio;Mixer;\n",
        bin.display()
    );
    model::write_atomic(&dir.join("better-sound-patching.desktop"), &entry)?;
    icons_changed();
    Ok(())
}

/// Tell running KDE programs (the taskbar, the menu) to look for icons again. They scan icon
/// folders once at start, so an icon put in a folder that didn't exist yet stays invisible
/// (a blank page in the taskbar) until the next login otherwise.
fn icons_changed() {
    let sent = zbus::blocking::Connection::session()
        .and_then(|c| c.emit_signal(None::<&str>, "/KIconLoader", "org.kde.KIconLoader", "iconChanged", &(0i32,)));
    if let Err(e) = sent {
        log::debug!("couldn't ask the desktop to reload icons: {e}");
    }
}
