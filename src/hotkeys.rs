//! Global keyboard shortcuts for switch pins, through the desktop's GlobalShortcuts portal
//! (KDE asks you once to confirm them, and lists them under System Settings → Shortcuts).
//!
//! Every distinct key used by a switch pin becomes one shortcut. Pressing it is handled exactly
//! like `bsp key <key>`.

use crate::engine::Cmd;
use crate::ipc::Request;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
pub const APP_ID: &str = "better-sound-patching";

/// Shortcut id for a key, e.g. `Ctrl+Alt+1` -> `select-ctrl-alt-1`.
fn shortcut_id(key: &str) -> String {
    let mut id = String::from("select-");
    for c in key.chars() {
        id.push(if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' });
    }
    id
}

/// Our key text (`Ctrl+Alt+A`, as egui names keys) in the XDG shortcut format (`CTRL+ALT+a`).
fn trigger(key: &str) -> String {
    let parts: Vec<&str> = key.split('+').collect();
    let (mods, last) = parts.split_at(parts.len() - 1);
    let mut out: Vec<String> = mods
        .iter()
        .map(|m| match m.to_ascii_lowercase().as_str() {
            "ctrl" => "CTRL".into(),
            "alt" => "ALT".into(),
            "shift" => "SHIFT".into(),
            "meta" | "super" => "LOGO".into(),
            other => other.to_uppercase(),
        })
        .collect();
    let k = last[0];
    out.push(match k {
        k if k.len() == 1 => k.to_ascii_lowercase(),
        "Space" => "space".into(),
        "Enter" => "Return".into(),
        "Escape" => "Escape".into(),
        k => k.to_owned(),
    });
    out.join("+")
}

/// Wait for the portal's answer to a request we're about to make.
struct Pending {
    signals: zbus::blocking::proxy::SignalIterator<'static>,
}

impl Pending {
    fn new(conn: &Connection, token: &str) -> Result<Pending> {
        let sender = conn.unique_name().ok_or_else(|| anyhow!("no bus name"))?.trim_start_matches(':').replace('.', "_");
        let path = format!("{PATH}/request/{sender}/{token}");
        let req = Proxy::new(conn, DEST, path, "org.freedesktop.portal.Request")?;
        Ok(Pending { signals: req.receive_signal("Response")? })
    }

    fn wait(mut self) -> Result<HashMap<String, OwnedValue>> {
        let msg = self.signals.next().ok_or_else(|| anyhow!("portal went away"))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
        match code {
            0 => Ok(results),
            1 => bail!("cancelled"),
            _ => bail!("portal refused (code {code})"),
        }
    }
}

struct Portal {
    conn: Connection,
    shortcuts: Proxy<'static>,
    counter: u32,
}

impl Portal {
    fn connect() -> Result<Portal> {
        let conn = Connection::session()?;
        // Tell the portal who we are; required for programs that aren't sandboxed, and must come
        // before any other portal call on this connection.
        let registry = Proxy::new(&conn, DEST, PATH, "org.freedesktop.host.portal.Registry")?;
        registry
            .call_method("Register", &(APP_ID, HashMap::<&str, Value>::new()))
            .context("registering with the desktop portal")?;
        let shortcuts = Proxy::new(&conn, DEST, PATH, "org.freedesktop.portal.GlobalShortcuts")?;
        Ok(Portal { conn, shortcuts, counter: 0 })
    }

    fn token(&mut self) -> String {
        self.counter += 1;
        format!("bsp{}_{}", std::process::id(), self.counter)
    }

    fn create_session(&mut self) -> Result<OwnedObjectPath> {
        let (token, session_token) = (self.token(), self.token());
        let pending = Pending::new(&self.conn, &token)?;
        let opts = HashMap::from([("handle_token", Value::from(token.as_str())), ("session_handle_token", Value::from(session_token.as_str()))]);
        self.shortcuts.call_method("CreateSession", &(opts,))?;
        let results = pending.wait().context("creating a shortcut session")?;
        let handle = results.get("session_handle").ok_or_else(|| anyhow!("no session handle"))?;
        // Older portals send the handle as a string, newer ones as an object path.
        let path = match &**handle {
            Value::Str(s) => OwnedObjectPath::try_from(s.as_str())?,
            Value::ObjectPath(p) => p.clone().into(),
            v => bail!("unexpected session handle {v:?}"),
        };
        Ok(path)
    }

    fn bind(&mut self, session: &OwnedObjectPath, keys: &BTreeSet<String>) -> Result<()> {
        let token = self.token();
        let pending = Pending::new(&self.conn, &token)?;
        let list: Vec<(String, HashMap<&str, Value>)> = keys
            .iter()
            .map(|k| {
                let props = HashMap::from([
                    ("description", Value::from(format!("Switch audio: {k}"))),
                    ("preferred_trigger", Value::from(trigger(k))),
                ]);
                (shortcut_id(k), props)
            })
            .collect();
        let opts = HashMap::from([("handle_token", Value::from(token.as_str()))]);
        let session: ObjectPath = session.as_ref();
        self.shortcuts.call_method("BindShortcuts", &(session, list, "", opts))?;
        pending.wait().context("registering shortcuts")?;
        Ok(())
    }

    fn close(&self, session: &OwnedObjectPath) {
        if let Ok(p) = Proxy::new(&self.conn, DEST, session.as_str().to_owned(), "org.freedesktop.portal.Session") {
            let _ = p.call_method("Close", &());
        }
    }
}

/// Start the shortcut service. Send it the current set of keys whenever it may have changed;
/// presses come back to the engine as key requests.
pub fn spawn(engine: pipewire::channel::Sender<Cmd>) -> mpsc::Sender<BTreeSet<String>> {
    let (tx, rx) = mpsc::channel::<BTreeSet<String>>();
    std::thread::spawn(move || {
        if let Err(e) = serve(engine, rx) {
            log::warn!("keyboard shortcuts unavailable: {e:#}");
        }
    });
    tx
}

fn serve(engine: pipewire::channel::Sender<Cmd>, rx: mpsc::Receiver<BTreeSet<String>>) -> Result<()> {
    let mut portal = Portal::connect()?;
    // Shortcut id -> key, for the session currently bound.
    let active: Arc<Mutex<(Option<OwnedObjectPath>, HashMap<String, String>)>> = Arc::default();

    {
        let mut activated = portal.shortcuts.receive_signal("Activated")?;
        let active = active.clone();
        std::thread::spawn(move || {
            for msg in &mut activated {
                let Ok((session, id, _ts, _opts)) = msg.body().deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>() else {
                    continue;
                };
                let key = {
                    let a = active.lock().unwrap();
                    if a.0.as_ref() != Some(&session) {
                        continue;
                    }
                    a.1.get(&id).cloned()
                };
                if let Some(key) = key {
                    log::info!("shortcut {key} pressed");
                    let (reply, _) = mpsc::channel();
                    if engine.send(Cmd::Ipc(Request::Key { key }, reply)).is_err() {
                        return;
                    }
                }
            }
        });
    }

    let mut bound: Option<BTreeSet<String>> = None;
    while let Ok(mut keys) = rx.recv() {
        // Only the latest set matters.
        while let Ok(newer) = rx.try_recv() {
            keys = newer;
        }
        if bound.as_ref() == Some(&keys) {
            continue;
        }
        // A session's shortcuts can only be bound once, so changes get a fresh session.
        let old = active.lock().unwrap().0.take();
        if let Some(old) = old {
            portal.close(&old);
        }
        bound = Some(keys.clone());
        if keys.is_empty() {
            continue;
        }
        let session = portal.create_session()?;
        match portal.bind(&session, &keys) {
            Ok(()) => {
                log::info!("keyboard shortcuts: {}", keys.iter().cloned().collect::<Vec<_>>().join(", "));
                *active.lock().unwrap() = (Some(session), keys.iter().map(|k| (shortcut_id(k), k.clone())).collect());
            }
            Err(e) => {
                log::warn!("keyboard shortcuts not set up: {e:#}");
                portal.close(&session);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_translate_to_portal_triggers() {
        assert_eq!(trigger("Ctrl+Alt+1"), "CTRL+ALT+1");
        assert_eq!(trigger("Ctrl+Shift+A"), "CTRL+SHIFT+a");
        assert_eq!(trigger("Meta+F9"), "LOGO+F9");
        assert_eq!(shortcut_id("Ctrl+Alt+1"), "select-ctrl-alt-1");
    }
}
