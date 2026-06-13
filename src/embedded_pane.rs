use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use std::any::Any;

use crate::hyperlink::{HyperlinkMap, Osc8Filter, Osc8Segment};
use crate::pane::{PaneController, PaneDirection, PaneError, PaneInfo};

/// State for a single embedded terminal pane.
struct Pane {
    /// Writer to send input to the PTY.
    writer: Box<dyn std::io::Write + Send>,
    /// Parsed terminal screen (vt100).
    screen: Arc<Mutex<vt100::Parser>>,
    /// The child process handle.
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Master PTY handle (kept alive for resize).
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Display name for this pane.
    name: String,
    /// Whether this pane is currently focused.
    is_focused: bool,
    /// The command that was used to create this pane.
    command: Option<String>,
    /// Whether the child app has enabled mouse reporting (e.g., TUI apps like opencode).
    mouse_mode: Arc<AtomicBool>,
    /// Hyperlink URLs extracted from OSC 8 escape sequences, keyed by screen row.
    hyperlinks: Arc<Mutex<HyperlinkMap>>,
    /// Unix-millisecond timestamp of the most recent non-empty PTY read from the
    /// child process. Updated by the reader thread on every `Ok(n)` with `n > 0`
    /// using `Relaxed` ordering — we only need monotonic-ish "agent emitted bytes
    /// recently" semantics for the Pending-flicker heuristic, not strict ordering.
    last_pty_byte_unix_ms: Arc<AtomicI64>,
}

/// Thread-safe pane registry.
type PaneRegistry = Arc<Mutex<HashMap<String, Pane>>>;

/// Encode the payload portion of a pane input (content + bracketed paste markers if
/// multi-line) without the trailing submit byte. Trailing whitespace is stripped.
fn encode_pane_payload(text: &str) -> Vec<u8> {
    let trimmed = text.trim_end_matches(['\n', '\r', ' ', '\t']);
    let mut out = Vec::with_capacity(trimmed.len() + 16);
    if trimmed.contains('\n') {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(trimmed.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(trimmed.as_bytes());
    }
    out
}

/// Delay between writing input bytes and the submit CR. Agent TUIs like claude
/// treat a CR that arrives fused to the preceding text as newline-in-input; only
/// a CR that arrives as a separate event after a pause is honored as Enter. The
/// same applies after a bracketed-paste close marker. 150ms tuned empirically.
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Embedded terminal pane controller using portable-pty + vt100.
///
/// Replaces `ZellijController` by spawning PTY processes directly and parsing
/// their output with a VT100 terminal emulator.
pub struct EmbeddedPaneController {
    panes: PaneRegistry,
    next_id: Arc<Mutex<u64>>,
}

impl Default for EmbeddedPaneController {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddedPaneController {
    pub fn new() -> Self {
        Self {
            panes: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Access the vt100 screen for a pane (used by the terminal widget for rendering).
    pub fn get_screen(&self, pane_id: &str) -> Option<Arc<Mutex<vt100::Parser>>> {
        let panes = self.panes.lock().unwrap();
        panes.get(pane_id).map(|p| Arc::clone(&p.screen))
    }

    /// Access the hyperlink map for a pane (used for click-to-open).
    pub fn get_hyperlinks(&self, pane_id: &str) -> Option<Arc<Mutex<HyperlinkMap>>> {
        let panes = self.panes.lock().unwrap();
        panes.get(pane_id).map(|p| Arc::clone(&p.hyperlinks))
    }

    /// Return all pane IDs in insertion order (by numeric ID).
    pub fn pane_ids(&self) -> Vec<String> {
        let panes = self.panes.lock().unwrap();
        let mut ids: Vec<String> = panes.keys().cloned().collect();
        ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        ids
    }

    /// Get the currently focused pane ID, if any.
    pub fn focused_pane_id(&self) -> Option<String> {
        let panes = self.panes.lock().unwrap();
        panes
            .iter()
            .find(|(_, p)| p.is_focused)
            .map(|(id, _)| id.clone())
    }

    /// Write raw bytes directly to a pane's PTY stdin without appending CR.
    /// Used for interactive keyboard input forwarding.
    pub fn write_raw_bytes(&self, pane_id: &str, bytes: &[u8]) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.writer.write_all(bytes).map_err(PaneError::Io)?;
            pane.writer.flush().map_err(PaneError::Io)?;
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    /// Scroll a pane's view by `delta` lines (positive = scroll up into history).
    /// vt100 0.16 clamps the offset to the actual scrollback buffer size.
    pub fn scroll_pane(&self, pane_id: &str, delta: isize) {
        let panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get(pane_id)
            && let Ok(mut parser) = pane.screen.lock()
        {
            let current = parser.screen().scrollback();
            let new_offset = if delta > 0 {
                current.saturating_add(delta as usize)
            } else {
                current.saturating_sub((-delta) as usize)
            };
            parser.screen_mut().set_scrollback(new_offset);
        }
    }

    /// Reset a pane's scrollback offset to 0 (show latest output).
    pub fn reset_scrollback(&self, pane_id: &str) {
        let panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get(pane_id)
            && let Ok(mut parser) = pane.screen.lock()
        {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// Resize a pane's PTY and VT100 parser to the given dimensions.
    ///
    /// On Windows, when the requested size differs from the current vt100 size,
    /// the master PTY is resized twice: first to a 1-row jiggle, then to the
    /// target. ConPTY does not always reliably deliver a size change to the
    /// child process — without the jiggle the agent can keep drawing at its
    /// previous size, causing rows to drift into vt100's scrollback (visible
    /// as a stacked prompt / status line). The jiggle forces ConPTY to
    /// re-signal the child.
    pub fn resize_pane_pty(&self, pane_id: &str, rows: u16, cols: u16) -> Result<(), PaneError> {
        let panes = self.panes.lock().unwrap();
        let pane = panes
            .get(pane_id)
            .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
        if rows == 0 || cols == 0 {
            return Ok(());
        }

        // Determine whether the size is actually changing. If it isn't, we
        // skip both the jiggle and the PTY resize call to avoid unnecessary
        // redraw churn on every focus change / tab switch.
        let size_changed = match pane.screen.lock() {
            Ok(parser) => parser.screen().size() != (rows, cols),
            Err(_) => true,
        };

        if !size_changed {
            return Ok(());
        }

        #[cfg(windows)]
        {
            // Pick a jiggle row that is guaranteed to differ from the target.
            let jiggle_rows = if rows > 1 { rows - 1 } else { rows + 1 };
            let _ = pane.master.resize(PtySize {
                rows: jiggle_rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        pane.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PaneError::CommandFailed(format!("PTY resize failed: {e}")))?;
        if let Ok(mut parser) = pane.screen.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    /// Check if a pane's child app has enabled mouse reporting.
    pub fn mouse_mode_enabled(&self, pane_id: &str) -> bool {
        let panes = self.panes.lock().unwrap();
        panes
            .get(pane_id)
            .is_some_and(|p| p.mouse_mode.load(Ordering::Relaxed))
    }

    /// Timestamp of the most recent non-empty PTY read from the child process.
    /// Returns `None` if the pane is unknown or if the stored unix-ms can't be
    /// converted (shouldn't happen for any timestamp we actually write). Used by
    /// the UI loop to bridge PTY-byte activity into `AppState::bump_pty_activity`
    /// before each `apply_pending_timeout` so streaming-tokens sessions don't
    /// false-positive into Pending during long LLM gaps.
    pub fn last_pty_byte_at(&self, pane_id: &str) -> Option<DateTime<Utc>> {
        let panes = self.panes.lock().unwrap();
        let pane = panes.get(pane_id)?;
        let ms = pane.last_pty_byte_unix_ms.load(Ordering::Relaxed);
        DateTime::<Utc>::from_timestamp_millis(ms)
    }

    /// Forward a mouse scroll event to the child app via SGR extended mouse encoding.
    /// Coordinates are pane-relative (0-indexed) and converted to 1-indexed for the protocol.
    /// Also resets vt100 scrollback to 0 so the terminal widget shows live output.
    pub fn forward_mouse_scroll(
        &self,
        pane_id: &str,
        up: bool,
        col: u16,
        row: u16,
    ) -> Result<(), PaneError> {
        // Ensure we're showing live output, not a stale scrollback position.
        self.reset_scrollback(pane_id);
        let button = if up { 64 } else { 65 };
        let seq = format!("\x1b[<{};{};{}M", button, col + 1, row + 1);
        self.write_raw_bytes(pane_id, seq.as_bytes())
    }

    /// Create a new pane with explicit initial PTY and vt100 dimensions.
    ///
    /// Matching the actual rendered dimensions at spawn-time avoids the
    /// startup race where the agent emits its first redraw against a 24×80
    /// grid (the prior default), only for vt100 to shrink to the real layout
    /// size moments later. With the default 24×80 path that race lets early
    /// output (banners, prompts, status lines) accumulate in scrollback and
    /// reappear as stacked duplicates as the agent re-renders.
    pub fn create_pane_with_size(
        &self,
        command: Option<&str>,
        cwd: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<String, PaneError> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let pty_system = NativePtySystem::default();

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PaneError::CommandFailed(format!("Failed to open PTY: {e}")))?;

        let default_shell = default_shell();

        let mut cmd = match command {
            Some(c) if c.contains(' ') => {
                let mut cmd = CommandBuilder::new(&default_shell);
                cmd.arg(shell_command_flag());
                cmd.arg(c);
                cmd
            }
            Some(c) => CommandBuilder::new(c),
            None => CommandBuilder::new(&default_shell),
        };

        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }

        let pane_id = self.allocate_id();
        // Tag the spawned process so hooks can identify which pane it belongs to.
        cmd.env("DOT_AGENT_DECK_PANE_ID", &pane_id);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PaneError::CommandFailed(format!("Failed to spawn command: {e}")))?;

        // Drop the slave — we interact through the master side only.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PaneError::CommandFailed(format!("Failed to get PTY writer: {e}")))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PaneError::CommandFailed(format!("Failed to get PTY reader: {e}")))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let mouse_mode = Arc::new(AtomicBool::new(false));
        let hyperlinks = Arc::new(Mutex::new(HyperlinkMap::new()));
        let last_pty_byte_unix_ms = Arc::new(AtomicI64::new(Utc::now().timestamp_millis()));

        // Spawn a background thread to read PTY output and feed it to the vt100 parser.
        // Strips OSC 8 hyperlink sequences and records row → URL associations.
        let parser_clone = Arc::clone(&parser);
        let mouse_mode_clone = Arc::clone(&mouse_mode);
        let hyperlinks_clone = Arc::clone(&hyperlinks);
        let last_pty_byte_unix_ms_clone = Arc::clone(&last_pty_byte_unix_ms);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut osc8 = Osc8Filter::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &buf[..n];
                        // Record that the agent emitted bytes right now. Cheap, lock-free.
                        // Consumed by AppState::apply_pending_timeout via the UI-loop bridge
                        // to keep streaming-tokens sessions from being flagged Pending.
                        last_pty_byte_unix_ms_clone
                            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
                        scan_mouse_mode(data, &mouse_mode_clone);

                        let segments = osc8.process(data);
                        let mut new_links: Vec<(u16, String)> = Vec::new();
                        let mut scroll_amount: u16 = 0;

                        if let Ok(mut p) = parser_clone.lock() {
                            let max_row = p.screen().size().0.saturating_sub(1);
                            for segment in &segments {
                                match segment {
                                    Osc8Segment::Text(bytes) => {
                                        let rb = p.screen().cursor_position().0;
                                        p.process(bytes);
                                        let ra = p.screen().cursor_position().0;
                                        if rb >= max_row && ra >= max_row {
                                            let nl = bytes.iter().filter(|&&b| b == b'\n').count()
                                                as u16;
                                            scroll_amount += nl;
                                        }
                                    }
                                    Osc8Segment::LinkedText { url, bytes } => {
                                        // cursor_before is the row where link text starts
                                        let row = p.screen().cursor_position().0;
                                        let rb = row;
                                        p.process(bytes);
                                        let ra = p.screen().cursor_position().0;
                                        new_links.push((row, url.clone()));
                                        if rb >= max_row && ra >= max_row {
                                            let nl = bytes.iter().filter(|&&b| b == b'\n').count()
                                                as u16;
                                            scroll_amount += nl;
                                        }
                                    }
                                }
                            }
                        }
                        // parser lock released

                        if (!new_links.is_empty() || scroll_amount > 0)
                            && let Ok(mut hmap) = hyperlinks_clone.lock()
                        {
                            if scroll_amount > 0 {
                                hmap.shift_up(scroll_amount);
                            }
                            for (row, url) in &new_links {
                                hmap.set_row(*row, url);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let pane = Pane {
            writer,
            screen: parser,
            child,
            master: pair.master,
            name: command.unwrap_or("shell").to_string(),
            is_focused: false,
            command: command.map(|c| c.to_string()),
            mouse_mode,
            hyperlinks,
            last_pty_byte_unix_ms,
        };

        self.panes.lock().unwrap().insert(pane_id.clone(), pane);

        Ok(pane_id)
    }

    fn allocate_id(&self) -> String {
        let mut id = self.next_id.lock().unwrap();
        let current = *id;
        *id += 1;
        current.to_string()
    }
}

/// Scan PTY output bytes for mouse mode enable/disable escape sequences.
/// Sets the atomic flag when the child app requests mouse reporting.
fn scan_mouse_mode(data: &[u8], flag: &AtomicBool) {
    // Mouse mode sequences: \x1b[?{mode}h (enable) or \x1b[?{mode}l (disable)
    // Modes: 1000 (basic), 1002 (button-motion), 1003 (any-motion), 1006 (SGR extended)
    let enable_patterns: &[&[u8]] = &[
        b"\x1b[?1000h",
        b"\x1b[?1002h",
        b"\x1b[?1003h",
        b"\x1b[?1006h",
    ];
    let disable_patterns: &[&[u8]] = &[
        b"\x1b[?1000l",
        b"\x1b[?1002l",
        b"\x1b[?1003l",
        b"\x1b[?1006l",
    ];
    for pat in enable_patterns {
        if contains_bytes(data, pat) {
            flag.store(true, Ordering::Relaxed);
            return;
        }
    }
    for pat in disable_patterns {
        if contains_bytes(data, pat) {
            flag.store(false, Ordering::Relaxed);
            return;
        }
    }
}

/// Simple byte pattern search.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Cross-platform default-shell selection used when no explicit command is
/// supplied for a new pane. Prefers `$SHELL` if set on either platform; on
/// Windows falls back to `%ComSpec%` and then `cmd.exe`; on Unix falls back
/// to `/bin/sh`.
fn default_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        return shell;
    }
    #[cfg(windows)]
    {
        if let Ok(com_spec) = std::env::var("ComSpec")
            && !com_spec.is_empty()
        {
            return com_spec;
        }
        "cmd.exe".to_string()
    }
    #[cfg(unix)]
    {
        "/bin/sh".to_string()
    }
}

/// The flag that the platform default shell uses to run a single command
/// string and exit. `cmd.exe /C "..."` on Windows; `<sh> -c "..."` on Unix.
fn shell_command_flag() -> &'static str {
    if cfg!(windows) { "/C" } else { "-c" }
}

impl PaneController for EmbeddedPaneController {
    fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if !panes.contains_key(pane_id) {
            return Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )));
        }
        for (id, pane) in panes.iter_mut() {
            pane.is_focused = id == pane_id;
        }
        Ok(())
    }

    fn create_pane(&self, command: Option<&str>, cwd: Option<&str>) -> Result<String, PaneError> {
        // Default dimensions are a fallback for callers that don't know the
        // target layout yet (tests, mocks, restore paths). UI call sites that
        // know the layout should call `create_pane_with_size` directly so the
        // child process spawns at the correct dimensions — otherwise the
        // agent's early output is rendered against a 24×80 vt100 grid and
        // rows can drift into scrollback before the post-spawn resize lands.
        self.create_pane_with_size(command, cwd, 24, 80)
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let mut pane = {
            let mut panes = self.panes.lock().unwrap();
            match panes.remove(pane_id) {
                Some(p) => p,
                None => {
                    return Err(PaneError::CommandFailed(format!(
                        "Pane {pane_id} not found"
                    )));
                }
            }
        };
        // Kill the child process and wait for it to exit after releasing the
        // lock so we don't hold the mutex during blocking I/O.
        let _ = pane.child.kill();
        let _ = pane.child.wait();
        Ok(())
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
        let panes = self.panes.lock().unwrap();
        let mut list: Vec<(u64, PaneInfo)> = panes
            .iter()
            .map(|(id, p)| {
                (
                    id.parse::<u64>().unwrap_or(0),
                    PaneInfo {
                        pane_id: id.clone(),
                        title: p.name.clone(),
                        is_focused: p.is_focused,
                        command: p.command.clone(),
                    },
                )
            })
            .collect();
        list.sort_by_key(|(num, _)| *num);
        Ok(list.into_iter().map(|(_, info)| info).collect())
    }

    fn resize_pane(
        &self,
        _pane_id: &str,
        _direction: PaneDirection,
        _amount: u16,
    ) -> Result<(), PaneError> {
        // Resize is handled by the layout engine in future milestones.
        // For now, this is a no-op.
        Ok(())
    }

    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<(), PaneError> {
        let mut panes = self.panes.lock().unwrap();
        if let Some(pane) = panes.get_mut(pane_id) {
            pane.name = name.to_string();
            Ok(())
        } else {
            Err(PaneError::CommandFailed(format!(
                "Pane {pane_id} not found"
            )))
        }
    }

    fn toggle_layout(&self) -> Result<(), PaneError> {
        // Layout toggling will be implemented in the layout engine milestone.
        Ok(())
    }

    /// Concurrency contract: callers must not invoke `write_to_pane` concurrently
    /// for the same `pane_id`. The pane lock is released around `SUBMIT_DELAY` so
    /// other panes can be drawn — but interleaved writes for the *same* pane would
    /// produce `payload_A + payload_B + CR + CR`, fusing two prompts. The current
    /// architecture is single-threaded for pane I/O, so this is a latent constraint
    /// rather than an active hazard; a per-pane submit mutex would enforce it if
    /// concurrent callers are ever introduced.
    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError> {
        let payload = encode_pane_payload(text);
        // Write the payload (content, optionally bracketed-paste-wrapped), flush, then
        // pause briefly before sending the submit CR. Agent TUIs like claude treat a
        // CR that arrives fused to the preceding text as newline-in-input; only a CR
        // that arrives as a separate event after a pause is honored as Enter. The
        // pane lock is released during the sleep so the UI thread can keep drawing.
        {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes
                .get_mut(pane_id)
                .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
            pane.writer.write_all(&payload).map_err(PaneError::Io)?;
            pane.writer.flush().map_err(PaneError::Io)?;
        }
        std::thread::sleep(SUBMIT_DELAY);
        {
            let mut panes = self.panes.lock().unwrap();
            let pane = panes
                .get_mut(pane_id)
                .ok_or_else(|| PaneError::CommandFailed(format!("Pane {pane_id} not found")))?;
            pane.writer.write_all(b"\r").map_err(PaneError::Io)?;
            pane.writer.flush().map_err(PaneError::Io)?;
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "embedded"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_list_panes() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.list_panes().unwrap().is_empty());

        let id = ctrl.create_pane(None, None).unwrap();
        assert!(!id.is_empty());

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, id);

        ctrl.close_pane(&id).unwrap();
        assert!(ctrl.list_panes().unwrap().is_empty());
    }

    #[test]
    fn focus_pane_updates_state() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();

        ctrl.focus_pane(&id1).unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert!(panes.iter().find(|p| p.pane_id == id1).unwrap().is_focused);
        assert!(!panes.iter().find(|p| p.pane_id == id2).unwrap().is_focused);

        ctrl.focus_pane(&id2).unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert!(!panes.iter().find(|p| p.pane_id == id1).unwrap().is_focused);
        assert!(panes.iter().find(|p| p.pane_id == id2).unwrap().is_focused);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
    }

    #[test]
    fn rename_pane_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        ctrl.rename_pane(&id, "my-agent").unwrap();
        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "my-agent");

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn close_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.close_pane("999").is_err());
    }

    #[test]
    fn focus_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.focus_pane("999").is_err());
    }

    #[test]
    fn write_to_pane_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        // Should not error — just sends bytes to PTY stdin
        ctrl.write_to_pane(&id, "echo hello").unwrap();

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn encode_pane_payload_single_line() {
        assert_eq!(encode_pane_payload("ls -la"), b"ls -la");
    }

    #[test]
    fn encode_pane_payload_strips_trailing_whitespace() {
        assert_eq!(encode_pane_payload("ls -la\n"), b"ls -la");
        assert_eq!(encode_pane_payload("ls -la  \n\n"), b"ls -la");
    }

    #[test]
    fn encode_pane_payload_wraps_multiline() {
        assert_eq!(
            encode_pane_payload("line1\nline2\nline3"),
            b"\x1b[200~line1\nline2\nline3\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_multiline_with_trailing_newline() {
        // Trailing newline is stripped, but embedded newlines still trigger paste wrapping.
        assert_eq!(
            encode_pane_payload("line1\nline2\n"),
            b"\x1b[200~line1\nline2\x1b[201~"
        );
    }

    #[test]
    fn encode_pane_payload_empty() {
        assert_eq!(encode_pane_payload(""), b"");
        // Edge case: trailing whitespace stripped to empty → no embedded newline → no markers.
        assert_eq!(encode_pane_payload("\n\n"), b"");
    }

    #[test]
    fn controller_metadata() {
        let ctrl = EmbeddedPaneController::new();
        assert_eq!(ctrl.name(), "embedded");
        assert!(ctrl.is_available());
    }

    #[test]
    fn screen_access_works() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(Some("echo hello"), None).unwrap();

        // Give the PTY a moment to produce output
        std::thread::sleep(std::time::Duration::from_millis(200));

        let screen = ctrl.get_screen(&id).expect("screen should exist");
        let parser = screen.lock().unwrap();
        let contents = parser.screen().contents();
        // The screen should have some content (at minimum the echoed text or shell prompt)
        assert!(!contents.trim().is_empty());

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn pane_ids_are_sequential() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();
        let id3 = ctrl.create_pane(None, None).unwrap();

        let n1: u64 = id1.parse().unwrap();
        let n2: u64 = id2.parse().unwrap();
        let n3: u64 = id3.parse().unwrap();
        assert_eq!(n2, n1 + 1);
        assert_eq!(n3, n2 + 1);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
        ctrl.close_pane(&id3).unwrap();
    }

    #[test]
    fn pane_ids_sorted_in_list() {
        let ctrl = EmbeddedPaneController::new();
        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();
        let id3 = ctrl.create_pane(None, None).unwrap();

        let ids = ctrl.pane_ids();
        assert_eq!(ids, vec![id1.clone(), id2.clone(), id3.clone()]);

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
        ctrl.close_pane(&id3).unwrap();
    }

    #[test]
    fn focused_pane_id_tracks_focus() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.focused_pane_id().is_none());

        let id1 = ctrl.create_pane(None, None).unwrap();
        let id2 = ctrl.create_pane(None, None).unwrap();

        ctrl.focus_pane(&id1).unwrap();
        assert_eq!(ctrl.focused_pane_id().as_deref(), Some(id1.as_str()));

        ctrl.focus_pane(&id2).unwrap();
        assert_eq!(ctrl.focused_pane_id().as_deref(), Some(id2.as_str()));

        ctrl.close_pane(&id1).unwrap();
        ctrl.close_pane(&id2).unwrap();
    }

    #[test]
    fn write_raw_bytes_no_cr_appended() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        // write_raw_bytes should succeed without error
        ctrl.write_raw_bytes(&id, b"hello").unwrap();

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn write_raw_bytes_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.write_raw_bytes("999", b"hello").is_err());
    }

    #[test]
    fn rename_nonexistent_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.rename_pane("999", "name").is_err());
    }

    #[test]
    fn create_pane_with_command() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(Some("echo test"), None).unwrap();

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "echo test");
        assert_eq!(panes[0].command.as_deref(), Some("echo test"));

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn create_pane_default_name_is_shell() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();

        let panes = ctrl.list_panes().unwrap();
        assert_eq!(panes[0].title, "shell");
        assert!(panes[0].command.is_none());

        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn create_pane_with_size_initializes_vt100_at_target() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane_with_size(None, None, 40, 120).unwrap();
        let screen = ctrl.get_screen(&id).unwrap();
        let size = screen.lock().unwrap().screen().size();
        assert_eq!(
            size,
            (40, 120),
            "vt100 must spawn at requested dimensions to avoid startup scrollback drift"
        );
        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn create_pane_with_size_does_not_panic_on_zero_dims() {
        let ctrl = EmbeddedPaneController::new();
        // Should either succeed (with clamped dims) or return an error — but
        // must not panic. We don't assert on the exact dim because OS PTYs
        // may refuse very small sizes.
        let result = ctrl.create_pane_with_size(None, None, 0, 0);
        if let Ok(id) = result {
            let _ = ctrl.close_pane(&id);
        }
    }

    #[test]
    fn resize_pane_pty_updates_vt100_size() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();
        ctrl.resize_pane_pty(&id, 30, 100).unwrap();
        let screen = ctrl.get_screen(&id).unwrap();
        assert_eq!(screen.lock().unwrap().screen().size(), (30, 100));
        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn resize_pane_pty_is_noop_when_size_unchanged() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane_with_size(None, None, 20, 60).unwrap();
        // No-op resize — should still succeed and keep size.
        ctrl.resize_pane_pty(&id, 20, 60).unwrap();
        let screen = ctrl.get_screen(&id).unwrap();
        assert_eq!(screen.lock().unwrap().screen().size(), (20, 60));
        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn resize_pane_pty_zero_dim_is_safe_noop_on_known_pane() {
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane(None, None).unwrap();
        // 0 dims should not mutate vt100 and should not error for a known pane.
        ctrl.resize_pane_pty(&id, 0, 80).unwrap();
        ctrl.resize_pane_pty(&id, 24, 0).unwrap();
        let screen = ctrl.get_screen(&id).unwrap();
        assert_eq!(screen.lock().unwrap().screen().size(), (24, 80));
        ctrl.close_pane(&id).unwrap();
    }

    #[test]
    fn resize_pane_pty_unknown_pane_errors() {
        let ctrl = EmbeddedPaneController::new();
        assert!(ctrl.resize_pane_pty("999", 30, 80).is_err());
    }

    #[test]
    fn resize_pane_pty_handles_repeated_size_changes() {
        // Exercises the Windows jiggle path: every change is a real change, so
        // the jiggle fires each time. Should land at the final target.
        let ctrl = EmbeddedPaneController::new();
        let id = ctrl.create_pane_with_size(None, None, 20, 60).unwrap();
        ctrl.resize_pane_pty(&id, 30, 80).unwrap();
        ctrl.resize_pane_pty(&id, 25, 70).unwrap();
        ctrl.resize_pane_pty(&id, 40, 100).unwrap();
        let screen = ctrl.get_screen(&id).unwrap();
        assert_eq!(screen.lock().unwrap().screen().size(), (40, 100));
        ctrl.close_pane(&id).unwrap();
    }
}
