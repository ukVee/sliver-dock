//! Inbound IPC over `$XDG_RUNTIME_DIR/sliver-dock.sock`.
//!
//! Same shape as `~/projects/swipe-panel/panel-daemon/src/ipc.rs`. ASCII
//! line-protocol; one connect-write-(read)-close per command. The socket
//! is shared between two callers:
//!   * the CLI (`sliver-dock show|hide|...`) — writes one line, exits.
//!   * the touch-drawer plugin — writes `drag-begin/drag/drag-end` lines on
//!     gesture commits (we register this socket as the reply socket for
//!     our zone in `zones::register`).
//!
//! Reply traffic exists only for `get-state`, which writes JSON back.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime.join("sliver-dock.sock")
}

/// Client: write one command and (optionally) read the response back. The
/// daemon closes the connection after handling each command, so EOF is the
/// terminator.
pub fn send(cmd: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(socket_path())?;
    s.write_all(cmd.as_bytes())?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Incoming request paired with a reply channel. The GTK main thread drains
/// these and writes the reply (an empty string for verbs that don't reply).
pub struct Request {
    pub line: String,
    pub reply: Option<UnixStream>,
}

pub fn serve(tx: async_channel::Sender<Request>) -> std::io::Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut buf = String::new();
            if stream.read_to_string(&mut buf).is_err() {
                continue;
            }
            let line = buf.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let req = Request {
                line,
                reply: Some(stream),
            };
            if tx.send_blocking(req).is_err() {
                break;
            }
        }
    });

    Ok(())
}

/// Best-effort reply write. Failures (closed peer, broken pipe) are logged
/// at most once per call — IPC is fire-and-forget for callers that don't
/// wait for output.
pub fn reply(req: &mut Request, body: &str) {
    let Some(mut stream) = req.reply.take() else { return };
    if let Err(e) = stream.write_all(body.as_bytes()) {
        eprintln!("sliver-dock: ipc reply: {e}");
    }
}
