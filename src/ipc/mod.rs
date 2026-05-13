//! Cross-platform IPC abstraction for the dot-agent-deck daemon and its hooks.
//!
//! On Unix, this is a thin wrapper around `tokio::net::UnixListener` /
//! `std::os::unix::net::UnixStream`. On Windows, it uses Tokio named pipes
//! (`tokio::net::windows::named_pipe`) for the async daemon side and the
//! standard file API on the named-pipe path for the synchronous hook client.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Listener, Stream, SyncStream, connect_sync};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Listener, Stream, SyncStream, connect_sync};
