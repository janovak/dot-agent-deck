//! Windows implementation of the IPC abstraction (named pipes).
//!
//! Each `Listener::accept` call returns a `NamedPipeServer` instance that has
//! already been connected to by a client. A new server instance is pre-created
//! before returning so that subsequent clients are never refused while we
//! handle the previous one.
//!
//! The synchronous client opens the pipe path through the regular file API,
//! which is the documented way to connect to a named pipe from a non-async
//! context on Windows.

use std::io;
use std::path::Path;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

pub type Stream = NamedPipeServer;

/// Synchronous client stream wrapping a `std::fs::File` opened on the named
/// pipe path. Implements `std::io::Write` so callers can use it identically to
/// the Unix `std::os::unix::net::UnixStream`.
pub struct SyncStream(std::fs::File);

impl io::Write for SyncStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Async listener for the daemon side. Holds the path and a pre-created
/// "next" server instance that is consumed on each `accept` call.
pub struct Listener {
    path: String,
    next: Option<NamedPipeServer>,
}

impl Listener {
    /// Create the first server instance for the given pipe path. The path
    /// must use the Windows named-pipe namespace, e.g.
    /// `\\.\pipe\dot-agent-deck-{user}`.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let path_str = path
            .to_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "pipe path is not valid UTF-8")
            })?
            .to_string();
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path_str)?;
        Ok(Listener {
            path: path_str,
            next: Some(first),
        })
    }

    /// Wait for the next client to connect, return the connected server
    /// instance, and pre-create a fresh server instance for the next caller.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let server = self
            .next
            .take()
            .expect("listener server instance must be present");
        server.connect().await?;
        // Make a new server instance immediately so the pipe is always available
        // for the next incoming client.
        self.next = Some(ServerOptions::new().create(&self.path)?);
        Ok(server)
    }
}

/// Synchronous client used by the hook command-line entry point.
pub fn connect_sync(path: &Path) -> io::Result<SyncStream> {
    let path_str = path.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "pipe path is not valid UTF-8")
    })?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path_str)?;
    Ok(SyncStream(file))
}
