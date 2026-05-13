//! Unix implementation of the IPC abstraction (Unix domain sockets).

use std::io;
use std::path::Path;

pub type Stream = tokio::net::UnixStream;
pub type SyncStream = std::os::unix::net::UnixStream;

/// Async listener for the daemon side. Wraps `tokio::net::UnixListener`.
pub struct Listener(tokio::net::UnixListener);

impl Listener {
    /// Bind to the given socket path, removing any stale socket file first.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        tokio::net::UnixListener::bind(path).map(Listener)
    }

    /// Accept the next incoming connection.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _addr) = self.0.accept().await?;
        Ok(stream)
    }
}

/// Synchronous client used by the hook command-line entry point.
pub fn connect_sync(path: &Path) -> io::Result<SyncStream> {
    SyncStream::connect(path)
}
