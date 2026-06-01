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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Generate a unique pipe path per test invocation. Tests in the same
    /// process may run concurrently, and reusing the same pipe name causes
    /// `first_pipe_instance(true)` to fail in the second-to-run test.
    fn unique_pipe_path(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        PathBuf::from(format!(r"\\.\pipe\dot-agent-deck-test-{tag}-{pid}-{nanos}"))
    }

    #[test]
    fn bind_succeeds_and_creates_pipe() {
        let path = unique_pipe_path("bind");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let listener = Listener::bind(&path).expect("first bind must succeed");
            // Holding the listener keeps the server instance alive. Drop it
            // explicitly to release the kernel handle before the test ends.
            drop(listener);
        });
    }

    #[test]
    fn bind_twice_with_same_path_fails() {
        // `first_pipe_instance(true)` is documented to fail if any other
        // instance is already listening on the name — that protects against
        // two daemons silently colliding on the same socket.
        let path = unique_pipe_path("dup");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let _first = Listener::bind(&path).expect("first bind must succeed");
            let second = Listener::bind(&path);
            assert!(
                second.is_err(),
                "second bind on the same pipe must fail while the first is alive"
            );
        });
    }

    #[test]
    fn connect_sync_to_nonexistent_pipe_fails() {
        // Connecting to a pipe that no listener is serving must surface an
        // io::Error rather than hang. The path is unique to avoid colliding
        // with any real local listener.
        let path = unique_pipe_path("missing");
        let result = connect_sync(&path);
        assert!(
            result.is_err(),
            "connect_sync to a missing pipe must error, got Ok"
        );
    }

    #[test]
    fn sync_client_can_write_to_async_listener() {
        // End-to-end smoke test: bind, connect from a sync client, write,
        // and verify the async side gets the bytes. Uses the same code path
        // the hook binary uses to talk to the daemon.
        use std::io::Write as _;
        use tokio::io::AsyncReadExt as _;

        let path = unique_pipe_path("roundtrip");
        let path_for_writer = path.clone();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async move {
            // bind() creates a NamedPipeServer, which requires a Tokio
            // runtime context — so it has to live inside block_on.
            let mut listener = Listener::bind(&path).expect("bind");

            // Connect from a blocking thread to avoid deadlocking the
            // single-threaded runtime on the synchronous OpenOptions::open
            // call.
            let writer_handle = std::thread::spawn(move || -> io::Result<()> {
                // Brief sleep to let the runtime poll `accept()` first so
                // the server-side instance is awaiting a client.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let mut stream = connect_sync(&path_for_writer)?;
                stream.write_all(b"hello\n")?;
                stream.flush()?;
                Ok(())
            });

            let mut server = listener.accept().await.expect("accept");
            let mut buf = [0u8; 16];
            let n = server.read(&mut buf).await.expect("read");
            assert_eq!(&buf[..n], b"hello\n");

            writer_handle.join().expect("writer thread").expect("write");
        });
    }
}
