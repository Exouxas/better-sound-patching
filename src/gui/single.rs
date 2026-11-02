//! One editor at a time. The open editor listens on a socket next to the daemon's; a second
//! editor asks it to come forward and quits, and the tray icon can ask it to close.
//!
//! A minimized editor may not run at all: on Wayland the compositor stops sending it frames,
//! and egui only acts on requests while drawing one. If it doesn't answer quickly, the
//! listener closes it itself (keeping unsaved changes, like a normal close) and says so, so
//! the asker can open a fresh window instead.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// What another program asks the open editor to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ask {
    /// Come to the front.
    Show,
    /// Close (unless there are unsaved changes; then come to the front and say so).
    Close,
}

fn socket_path() -> PathBuf {
    crate::ipc::socket_path().with_file_name("better-sound-patching-editor.sock")
}

/// How long a running editor gets to act on a request before it counts as asleep.
const ANSWER_WITHIN: Duration = Duration::from_millis(500);

/// What came of asking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Answer {
    /// No editor is open.
    Nobody,
    /// The editor did it.
    Done,
    /// The editor was asleep (minimized) and has closed instead.
    Closed,
}

/// Ask the open editor, if there is one.
pub fn ask(what: Ask) -> Answer {
    let Ok(mut s) = UnixStream::connect(socket_path()) else { return Answer::Nobody };
    let _ = s.set_read_timeout(Some(ANSWER_WITHIN + Duration::from_secs(2)));
    let line = match what {
        Ask::Show => "show\n",
        Ask::Close => "close\n",
    };
    if s.write_all(line.as_bytes()).is_err() {
        return Answer::Nobody;
    }
    let mut reply = String::new();
    let _ = BufReader::new(s).read_line(&mut reply);
    match reply.trim() {
        "ok" => Answer::Done,
        "closed" => Answer::Closed,
        _ => Answer::Nobody,
    }
}

/// What a closing editor must not lose, kept up to date by the editor every frame so it can
/// be closed from the listener while it sleeps.
#[derive(Default, PartialEq)]
pub struct Leaving {
    /// The graph with unsaved changes (`None` when everything is saved).
    pub unsaved: Option<crate::model::Graph>,
    /// Unsaved changes are playing live.
    pub previewing: bool,
}

impl Leaving {
    /// What closing does: keep unsaved changes as a draft and stop playing them.
    pub fn close(&self) {
        match &self.unsaved {
            Some(g) => {
                if let Err(e) = crate::model::save_draft(g) {
                    log::warn!("keeping unsaved changes: {e:#}");
                }
            }
            None => crate::model::delete_draft(),
        }
        if self.previewing {
            let _ = crate::ipc::request(&crate::ipc::Request::Reload);
        }
    }
}

/// A request for the editor, and how it says it's done.
pub type Request = (Ask, mpsc::Sender<()>);

/// Become the one editor: listen for requests. `None` if another editor already runs.
pub fn claim() -> Option<UnixListener> {
    let path = socket_path();
    match UnixListener::bind(&path) {
        Ok(l) => Some(l),
        // Taken: by a live editor, or left over from one that crashed.
        Err(_) if UnixStream::connect(&path).is_ok() => None,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path).ok()
        }
    }
}

/// Hand requests to the editor (waking it up for each). If it doesn't act in time, close it
/// from here using `leaving`.
pub fn serve(listener: UnixListener, ctx: egui::Context, leaving: Arc<Mutex<Leaving>>) -> mpsc::Receiver<Request> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).is_err() {
                continue;
            }
            let ask = match line.trim() {
                "show" => Ask::Show,
                "close" => Ask::Close,
                _ => continue,
            };
            let (done_tx, done_rx) = mpsc::channel();
            if tx.send((ask, done_tx)).is_err() {
                return;
            }
            ctx.request_repaint();
            if done_rx.recv_timeout(ANSWER_WITHIN).is_ok() {
                let _ = (&conn).write_all(b"ok\n");
                continue;
            }
            log::info!("the editor is asleep (minimized?); closing it");
            leaving.lock().unwrap().close();
            release();
            let _ = (&conn).write_all(b"closed\n");
            std::process::exit(0);
        }
    });
    rx
}

/// The editor is closing: stop answering.
pub fn release() {
    let _ = std::fs::remove_file(socket_path());
}
