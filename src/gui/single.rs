//! One editor at a time. The open editor listens on a socket next to the daemon's; a second
//! editor asks it to come forward and quits, and the tray icon can ask it to close.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
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

/// Ask the open editor, if there is one. Returns whether an editor answered.
pub fn ask(what: Ask) -> bool {
    let Ok(mut s) = UnixStream::connect(socket_path()) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let line = match what {
        Ask::Show => "show\n",
        Ask::Close => "close\n",
    };
    if s.write_all(line.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).is_ok() && !reply.is_empty()
}

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

/// Hand requests to the editor (waking it up for each).
pub fn serve(listener: UnixListener, ctx: egui::Context) -> mpsc::Receiver<Ask> {
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
            let _ = (&conn).write_all(b"ok\n");
            if tx.send(ask).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
    rx
}

/// The editor is closing: stop answering.
pub fn release() {
    let _ = std::fs::remove_file(socket_path());
}
