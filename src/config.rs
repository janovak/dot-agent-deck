use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::event::AgentType;
use crate::state::{SessionState, SessionStatus, is_placeholder_session_id, is_tool_call_id};
use crate::theme::Theme;

pub const CONFIG_KEYS: &[(&str, &str)] = &[
    ("default_command", "Default shell command for new panes"),
    ("theme", "Color theme: auto, light, dark (default: auto)"),
    (
        "auto_config_prompt",
        "Enable/disable the config generation prompt (default: true)",
    ),
    (
        "bell.enabled",
        "Enable/disable terminal bell (default: true)",
    ),
    (
        "bell.on_waiting_for_input",
        "Bell when agent waits for input (default: true)",
    ),
    (
        "bell.on_idle",
        "Bell when session goes idle (default: false)",
    ),
    ("bell.on_error", "Bell on agent error (default: true)"),
    (
        "bell.on_pending",
        "Bell when card transitions to Pending (default: true)",
    ),
    (
        "pending.timeout_seconds",
        "Seconds in Working before card flips to Pending (default: 10, set to 0 to disable)",
    ),
    (
        "idle_art.enabled",
        "Enable ASCII art in dashboard idle cards (default: false)",
    ),
    (
        "idle_art.provider",
        "LLM provider: anthropic (ANTHROPIC_API_KEY), openai (OPENAI_API_KEY), ollama (no key needed) (default: anthropic)",
    ),
    ("idle_art.model", "LLM model (default: claude-haiku-4-5)"),
    (
        "idle_art.timeout_secs",
        "Seconds idle before triggering art (default: 300)",
    ),
];

pub fn config_keys_help() -> String {
    let mut help = String::from("Available keys:\n");
    for (key, desc) in CONFIG_KEYS {
        help.push_str(&format!("  {key:<30} {desc}\n"));
    }
    help
}

pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_SOCKET") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
        }

        PathBuf::from("/tmp/dot-agent-deck.sock")
    }

    #[cfg(windows)]
    {
        // Windows named pipes live in the `\\.\pipe\` namespace. Include the
        // current username so multiple users on the same machine don't collide.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        PathBuf::from(format!(r"\\.\pipe\dot-agent-deck-{user}"))
    }
}

/// A process-unique daemon socket path for this deck instance.
///
/// Multiple decks must not share one daemon socket. Only the first deck can
/// bind the shared socket; every other deck's daemon then dies with
/// "Access is denied" and shows "No agent" for everything. Worse, all panes'
/// `dot-agent-deck hook` invocations post to that one bound socket, and pane
/// ids collide across decks (each deck numbers panes `1`, `2`, …), so the
/// socket-owning deck applies other decks' events onto its own same-numbered
/// panes — phantom "Working", swapped sessions, and (once auto-save runs)
/// corrupted workspace files.
///
/// Including the PID gives each deck its own socket. `run_dashboard` publishes
/// this via `DOT_AGENT_DECK_SOCKET`, so the in-process daemon binds it and every
/// child pane's hook posts back to *this* deck's daemon.
pub fn unique_socket_path() -> PathBuf {
    let pid = std::process::id();

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join(format!("dot-agent-deck-{pid}.sock"));
        }

        PathBuf::from(format!("/tmp/dot-agent-deck-{pid}.sock"))
    }

    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        PathBuf::from(format!(r"\\.\pipe\dot-agent-deck-{user}-{pid}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BellConfig {
    pub enabled: bool,
    pub on_waiting_for_input: bool,
    pub on_pending: bool,
    pub on_idle: bool,
    pub on_error: bool,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_waiting_for_input: true,
            on_pending: true,
            on_idle: false,
            on_error: true,
        }
    }
}

impl BellConfig {
    pub fn should_bell(&self, status: &SessionStatus) -> bool {
        if !self.enabled {
            return false;
        }
        match status {
            SessionStatus::WaitingForInput => self.on_waiting_for_input,
            SessionStatus::Pending => self.on_pending,
            SessionStatus::Idle => self.on_idle,
            SessionStatus::Error => self.on_error,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IdleArtConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub timeout_secs: u64,
}

const MAX_IDLE_ART_TIMEOUT_SECS: u64 = i64::MAX as u64;

impl Default for IdleArtConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5".to_string(),
            timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingConfig {
    /// Seconds a session may sit in `Working` (without an active tool and
    /// without any new events arriving) before dot-agent-deck flips its
    /// status to `Pending`. The heuristic catches agents that have stalled
    /// at an interactive prompt without firing a corresponding hook event
    /// (e.g., Copilot CLI printing a multiple-choice menu and waiting on
    /// stdin). Set to `0` to disable the transition entirely — the card
    /// will keep displaying `Working` regardless of elapsed time.
    pub timeout_seconds: u64,
}

impl Default for PendingConfig {
    fn default() -> Self {
        Self {
            // 30 s is a deliberate trade-off. The heuristic exists to catch
            // genuine stalls at interactive prompts, but the actual signal
            // (no hook events while in Working/Thinking with no active tool)
            // is also produced by long pure-LLM-thinking gaps — common with
            // reasoning models and slow Copilot CLI responses. A 10 s
            // default false-positived during normal mid-conversation gaps,
            // With PTY-byte activity tracking (commit 8315bd2), the timeout
            // can be more aggressive — streaming response tokens keep the
            // PTY active and suppress false-positive Pending flips. 10 s
            // comfortably catches true user-input waits (e.g. Copilot CLI's
            // ask_user prompts) while avoiding flicker during normal LLM gaps.
            // Users can tune via `pending.timeout_seconds` or disable with `0`.
            timeout_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub default_command: String,
    pub bell: BellConfig,
    pub pending: PendingConfig,
    pub theme: Theme,
    pub idle_art: IdleArtConfig,
    pub auto_config_prompt: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            default_command: String::new(),
            bell: BellConfig::default(),
            pending: PendingConfig::default(),
            theme: Theme::default(),
            idle_art: IdleArtConfig::default(),
            auto_config_prompt: true,
        }
    }
}

impl DashboardConfig {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("Invalid config at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!("Failed to read config at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {e}"))?;
        }
        let contents =
            toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {e}"))?;
        std::fs::write(&path, contents)
            .map_err(|e| format!("Failed to write config at {}: {e}", path.display()))
    }

    pub fn get_field(&self, key: &str) -> Result<String, String> {
        match key {
            "default_command" => Ok(self.default_command.clone()),
            "theme" => Ok(self.theme.to_string()),
            "bell.enabled" => Ok(self.bell.enabled.to_string()),
            "bell.on_waiting_for_input" => Ok(self.bell.on_waiting_for_input.to_string()),
            "bell.on_idle" => Ok(self.bell.on_idle.to_string()),
            "bell.on_error" => Ok(self.bell.on_error.to_string()),
            "bell.on_pending" => Ok(self.bell.on_pending.to_string()),
            "pending.timeout_seconds" => Ok(self.pending.timeout_seconds.to_string()),
            "idle_art.enabled" => Ok(self.idle_art.enabled.to_string()),
            "idle_art.provider" => Ok(self.idle_art.provider.clone()),
            "idle_art.model" => Ok(self.idle_art.model.clone()),
            "idle_art.timeout_secs" => Ok(self.idle_art.timeout_secs.to_string()),
            "auto_config_prompt" => Ok(self.auto_config_prompt.to_string()),
            _ => Err(format!("Unknown config key: {key}\n{}", config_keys_help())),
        }
    }

    pub fn set_field(&mut self, key: &str, value: &str) -> Result<(), String> {
        let parse_bool = |v: &str| -> Result<bool, String> {
            v.parse().map_err(|_| format!("Invalid boolean: {v}"))
        };
        match key {
            "default_command" => {
                self.default_command = value.to_string();
                Ok(())
            }
            "theme" => {
                self.theme = value.parse().map_err(|e: String| e)?;
                Ok(())
            }
            "bell.enabled" => {
                self.bell.enabled = parse_bool(value)?;
                Ok(())
            }
            "bell.on_waiting_for_input" => {
                self.bell.on_waiting_for_input = parse_bool(value)?;
                Ok(())
            }
            "bell.on_idle" => {
                self.bell.on_idle = parse_bool(value)?;
                Ok(())
            }
            "bell.on_error" => {
                self.bell.on_error = parse_bool(value)?;
                Ok(())
            }
            "bell.on_pending" => {
                self.bell.on_pending = parse_bool(value)?;
                Ok(())
            }
            "pending.timeout_seconds" => {
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("Invalid number: {value}"))?;
                self.pending.timeout_seconds = secs;
                Ok(())
            }
            "idle_art.enabled" => {
                self.idle_art.enabled = parse_bool(value)?;
                Ok(())
            }
            "idle_art.provider" => {
                self.idle_art.provider = value.to_string();
                Ok(())
            }
            "idle_art.model" => {
                self.idle_art.model = value.to_string();
                Ok(())
            }
            "idle_art.timeout_secs" => {
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("Invalid number: {value}"))?;
                if secs > MAX_IDLE_ART_TIMEOUT_SECS {
                    return Err(format!(
                        "idle_art.timeout_secs must be <= {MAX_IDLE_ART_TIMEOUT_SECS}"
                    ));
                }
                self.idle_art.timeout_secs = secs;
                Ok(())
            }
            "auto_config_prompt" => {
                self.auto_config_prompt = value
                    .parse()
                    .map_err(|_| "Expected 'true' or 'false'".to_string())?;
                Ok(())
            }
            _ => Err(format!("Unknown config key: {key}\n{}", config_keys_help())),
        }
    }
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_CONFIG") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/config.toml")
}

fn session_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_SESSION") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/session.toml")
}

/// Root directory for named workspaces. Each workspace lives at
/// `<root>/<sanitized_name>.toml`.
pub fn workspaces_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_WORKSPACES") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/workspaces")
}

/// Resolve the on-disk path for a workspace, or the default session file when
/// no workspace name is given.
///
/// Returns an error if the name contains anything other than ASCII letters,
/// digits, dashes, or underscores — that guards against path traversal
/// (`../foo`), shell weirdness (`.`, spaces), and platform-specific reserved
/// names. Reserved Windows device names (`con`, `prn`, `aux`, `nul`, `com1`,
/// `lpt1`, etc.) are also rejected.
pub fn workspace_session_path(name: Option<&str>) -> Result<PathBuf, String> {
    match name {
        None => Ok(session_path()),
        Some(raw) => {
            validate_workspace_name(raw)?;
            Ok(workspaces_dir().join(format!("{raw}.toml")))
        }
    }
}

/// Returns `Ok` if `name` is safe to use as a filename component for a
/// workspace file: 1–64 chars of `[A-Za-z0-9_-]` and not a Windows reserved
/// device name.
pub fn validate_workspace_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Workspace name cannot be empty".to_string());
    }
    if name.len() > 64 {
        return Err(format!(
            "Workspace name '{name}' is too long (max 64 characters)"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Workspace name '{name}' contains invalid characters \
             (only letters, digits, '-' and '_' are allowed)"
        ));
    }
    // Windows reserved device names — case-insensitive, no extension.
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!(
            "Workspace name '{name}' conflicts with a reserved system name"
        ));
    }
    Ok(())
}

/// List all named workspaces present on disk, sorted alphabetically.
/// Returns an empty `Vec` if the workspaces directory does not exist yet
/// (i.e., the user has never saved a named workspace).
pub fn list_workspaces() -> Vec<String> {
    let dir = workspaces_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            // Defence-in-depth: only return names we'd accept back as input.
            if validate_workspace_name(&stem).is_ok() {
                Some(stem)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

/// Delete the named workspace from disk. Returns `Ok(true)` if a file was
/// removed, `Ok(false)` if no such workspace existed.
pub fn delete_workspace(name: &str) -> Result<bool, String> {
    let path = workspace_session_path(Some(name))?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Failed to delete {}: {e}", path.display())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPane {
    pub dir: String,
    pub name: String,
    pub command: String,
    /// When set, this pane was the agent pane of a mode tab.
    /// The value is the mode name from the project config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Live agent session id at the moment this snapshot was taken.
    /// Used together with `agent_type` to reconstruct a
    /// `<agent> --resume <id>` command at restore time so workspaces
    /// reopen the same conversation instead of a fresh one.
    ///
    /// `None` means either (a) the pane never bound to a real agent
    /// session (still on placeholder), or (b) the saved `command`
    /// isn't a simple agent invocation we can safely rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Agent type that produced `session_id`. Needed because the
    /// `--resume` syntax is per-agent (e.g., `copilot --resume <id>`
    /// vs.\ `claude --resume <id>`; OpenCode has no equivalent flag).
    /// Kept in lock-step with `session_id` — either both populated
    /// or both `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<AgentType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SavedSession {
    #[serde(default)]
    pub panes: Vec<SavedPane>,
}

impl SavedSession {
    /// Load the saved session for the given workspace (or the default
    /// unnamed session when `workspace` is `None`). Missing files
    /// produce an empty session, not an error.
    pub fn load(workspace: Option<&str>) -> Self {
        let path = match workspace_session_path(workspace) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("{err}");
                return Self::default();
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(session) => session,
                Err(err) => {
                    eprintln!("Invalid session at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!("Failed to read session at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Write the session to the given workspace's file (or the default
    /// unnamed session when `workspace` is `None`).
    ///
    /// The write is atomic via the standard temp-file-and-rename pattern:
    /// the contents land in `<path>.tmp` first and only then replace the
    /// real file via `std::fs::rename`. Without this, a crash or
    /// disk-full error mid-`std::fs::write` would leave the workspace file
    /// truncated to zero bytes (since `fs::write` opens with `truncate(true)`
    /// before writing). Atomic rename is guaranteed by both Unix and Windows
    /// when source and destination live on the same filesystem — which they
    /// always do here because the temp file is constructed by appending
    /// `.tmp` to the final path.
    pub fn save(&self, workspace: Option<&str>) -> Result<(), String> {
        let path = workspace_session_path(workspace)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create session directory: {e}"))?;
        }
        let contents = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize session: {e}"))?;

        let tmp_path = {
            let mut s = path.clone().into_os_string();
            s.push(".tmp");
            std::path::PathBuf::from(s)
        };
        std::fs::write(&tmp_path, contents).map_err(|e| {
            format!(
                "Failed to write session temp file at {}: {e}",
                tmp_path.display()
            )
        })?;
        std::fs::rename(&tmp_path, &path).map_err(|e| {
            // Best-effort cleanup so a failed rename doesn't leave a stray
            // `.tmp` sibling around. The cleanup error is intentionally
            // ignored — the rename failure is the real story.
            let _ = std::fs::remove_file(&tmp_path);
            format!(
                "Failed to atomically replace session at {}: {e}",
                path.display()
            )
        })
    }

    /// Delete the saved session file for the given workspace.
    pub fn clear(workspace: Option<&str>) -> Result<(), std::io::Error> {
        let path = match workspace_session_path(workspace) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("{err}");
                return Ok(());
            }
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Build a `SavedSession` snapshot from the live UI state.
    ///
    /// Must be called *before* tearing down mode/orchestration tabs — i.e., while
    /// `live_panes` (the authoritative `state.managed_pane_ids`) still contains
    /// every pane, including mode-tab agent panes that carry `mode = Some(...)`.
    /// `retain` here only prunes panes the user externally closed before exit;
    /// running it after teardown would also drop the mode-tab agent pane and lose
    /// the mode field, breaking `--continue` restoration (PRD #69).
    ///
    /// `sessions` is consulted to populate per-pane `session_id` and
    /// `agent_type` so a restored workspace can spawn `<agent> --resume <id>`
    /// instead of a fresh conversation. For each pane we pick the
    /// most-recently-active *real* session — placeholders (which carry
    /// `agent_type == None` and `session_id` shaped as `pane-<n>`, see
    /// `is_placeholder_session_id`) are skipped.
    ///
    /// When no real session matches we *preserve* any previously-stored
    /// resume metadata rather than clearing it. The seeded snapshot
    /// runs immediately after restore, before the freshly-spawned
    /// agent has emitted `SessionStart`; clearing there would wipe the
    /// id the user just restored from disk if they exit the workspace
    /// before the agent boots. This matches the
    /// "bookmark id may go stale" tradeoff the existing bookmark
    /// feature already accepts.
    pub fn snapshot(
        pane_metadata: &mut HashMap<String, SavedPane>,
        pane_display_names: &HashMap<String, String>,
        live_panes: &HashSet<String>,
        sessions: &HashMap<String, SessionState>,
    ) -> Self {
        pane_metadata.retain(|id, _| live_panes.contains(id));
        for (id, meta) in pane_metadata.iter_mut() {
            if let Some(name) = pane_display_names.get(id) {
                meta.name = name.clone();
            }
            // Find the most-recently-active real session bound to this
            // pane. Picking the latest handles real-→-real restarts on
            // the same pane (`/clear`, Claude `/restart`) where
            // `apply_event` updates the SessionState's `session_id`
            // field to the live id while keeping the old map key. We
            // read the field, not the key.
            //
            // The `is_placeholder_session_id` guard is belt-and-
            // suspenders alongside `agent_type != None` — placeholders
            // always have both set, but checking the id shape too
            // catches the (theoretical) case where someone forgets to
            // set agent_type when constructing a placeholder.
            //
            // The `is_tool_call_id` guard is a second safety net: a
            // subagent's tool-call id (`toolu_…`/`call_…`) must never be
            // persisted as a resume target. `apply_event` already keeps
            // it out of the canonical `session_id`; this ensures even a
            // legacy/corrupted snapshot field can't round-trip into a
            // broken `--resume toolu_…` command.
            let matching = sessions
                .values()
                .filter(|s| {
                    s.pane_id.as_deref() == Some(id.as_str())
                        && s.agent_type != AgentType::None
                        && !is_placeholder_session_id(&s.session_id)
                        && !is_tool_call_id(&s.session_id)
                })
                .max_by_key(|s| s.last_activity);
            if let Some(s) = matching {
                meta.session_id = Some(s.session_id.clone());
                meta.agent_type = Some(s.agent_type.clone());
            }
            // else: leave existing meta.session_id / meta.agent_type
            // alone. See doc comment above.
        }
        let mut ids: Vec<&String> = pane_metadata.keys().collect();
        ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        Self {
            panes: ids
                .into_iter()
                .filter_map(|id| pane_metadata.get(id).cloned())
                .collect(),
        }
    }
}

/// Drop any `--resume <id>` token pair from a shell command line.
///
/// Operates on whitespace-separated tokens — adequate for the limited
/// `<agent>` / `<agent> --resume <id>` shapes `is_simple_agent_invocation`
/// gates on. Not safe for arbitrary quoted shell args; callers must
/// confirm the command is simple before relying on the round-trip.
fn strip_resume_flag(command: &str) -> String {
    let mut tokens: Vec<&str> = command.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "--resume" {
            tokens.remove(i);
            if i < tokens.len() {
                tokens.remove(i);
            }
        } else {
            i += 1;
        }
    }
    tokens.join(" ")
}

/// Known launcher / shim wrappers that may precede the agent binary
/// while still being safe to round-trip. Single-token only — anything
/// requiring its own arguments (e.g. `cmd /c …`, `pnpm dlx …`) is
/// rejected because we can't tell which arg belongs to which layer.
///
/// `agency`/`agenchy` are the Microsoft internal Copilot wrappers most
/// commonly used to expose hook events to the deck. The npm-ecosystem
/// runners are included because they're the obvious cross-machine
/// install path for `claude` / `copilot` binaries.
const KNOWN_WRAPPER_NAMES: &[&str] = &["agency", "agenchy", "npx", "pnpx", "bunx"];

/// Conservative shell-metachar denylist for tokens we'd splice back
/// into a PTY command line. Matches the spirit of `is_safe_session_id`
/// but applied at the token level rather than the value level.
fn contains_shell_metachar(tok: &str) -> bool {
    tok.chars().any(|c| {
        matches!(
            c,
            '|' | '&' | ';' | '>' | '<' | '`' | '$' | '"' | '\'' | '(' | ')' | '\n' | '\r'
        )
    })
}

/// Returns `true` if `command` is a safely-rewritable launch shape for
/// `agent`. Specifically, after stripping any existing `--resume <id>`
/// pair, the remaining tokens must match the pattern
///
/// ```text
///   [wrapper]* <agent_binary> [--flag …]*
/// ```
///
/// where:
///   - each `wrapper` is one of `KNOWN_WRAPPER_NAMES` (basename match,
///     case-insensitive, path-aware);
///   - `agent_binary` is the configured binary for `agent`
///     (`copilot`/`claude`, basename match, path-aware so `C:\…\copilot.exe`
///     and `/usr/local/bin/claude` both qualify);
///   - each post-agent token starts with `-` (flag-style only — no
///     positional sub-commands, no space-separated flag values like
///     `--model gpt-5`; use `--model=gpt-5` for those);
///   - no token contains shell metacharacters.
///
/// This is the gate workspace-restore uses before rewriting a command
/// to inject `--resume <session_id>` at the end. Anything more complex
/// (unknown wrapper, quoted args, pipelines, positional subcommands)
/// is left untouched so we don't silently corrupt a power user's
/// command line.
fn is_simple_agent_invocation(command: &str, agent: &AgentType) -> bool {
    let expected: &[&str] = match agent {
        AgentType::CopilotCli => &["copilot"],
        AgentType::ClaudeCode => &["claude"],
        AgentType::OpenCode | AgentType::None => return false,
    };
    let stripped = strip_resume_flag(command);
    let trimmed = stripped.trim();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }

    let mut agent_seen = false;
    for tok in &tokens {
        if contains_shell_metachar(tok) {
            return false;
        }
        if agent_seen {
            // Post-agent: only `--flag` style args (no positional
            // subcommands, no quoted values). Empty token can't happen
            // after `split_whitespace`, but check defensively.
            if !tok.starts_with('-') || tok.len() == 1 {
                return false;
            }
            continue;
        }
        // Pre-agent: this token must be either the agent binary or a
        // known wrapper. Path-aware basename match.
        let basename = std::path::Path::new(tok)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(tok);
        if expected.iter().any(|n| basename.eq_ignore_ascii_case(n)) {
            agent_seen = true;
            continue;
        }
        if !KNOWN_WRAPPER_NAMES
            .iter()
            .any(|w| basename.eq_ignore_ascii_case(w))
        {
            return false;
        }
    }

    agent_seen
}

/// Returns the launch command rewritten to resume the given session,
/// when safe to do so. Falls back to `base` unchanged when:
///   - `session_id` / `agent` are missing (no resume info recorded);
///   - the agent doesn't support `--resume` (OpenCode, None);
///   - `base` isn't a simple agent invocation (see
///     `is_simple_agent_invocation` — quoted args, unknown wrappers,
///     positional subcommands, etc.);
///   - `session_id` isn't a canonical UUID. Real Copilot/Claude ids
///     are UUID-shaped; anything else — a value with characters
///     outside `[A-Za-z0-9_-]`, or a charset-clean non-session
///     identifier that leaked into the `sessionId` hook field (a
///     subagent/MCP id or name) — is treated as unresumable and
///     skipped rather than fed to the agent (which would reject it
///     with "No session or name matched '…'") or the shell.
///
/// `--resume <id>` is always appended at the *end* of the stripped
/// command, after any wrapper / flag tokens — Copilot CLI and Claude
/// Code both accept flag-style args following positional ones, and
/// `agency`-style wrappers pass them through transparently.
///
/// Idempotent across workspace round-trips: a stale `--resume <old>`
/// in `base` is stripped before the new flag is appended.
pub fn build_resume_command(
    base: &str,
    session_id: Option<&str>,
    agent: Option<&AgentType>,
) -> String {
    let (Some(sid), Some(at)) = (session_id, agent) else {
        return base.to_string();
    };
    if !is_simple_agent_invocation(base, at) {
        return base.to_string();
    }
    if !is_safe_session_id(sid) {
        return base.to_string();
    }
    // A tool-call id (`toolu_…`/`call_…`) is shell-safe but is NOT a
    // resumable session — it can leak into a saved snapshot via a
    // subagent hook event (see `state::is_tool_call_id`). Refuse to
    // build `--resume toolu_…`; fall back to a fresh session so a
    // legacy/corrupted snapshot opens cleanly instead of producing a
    // command the agent rejects with "No session or name matched '…'".
    if is_tool_call_id(sid) {
        return base.to_string();
    }
    // Only a canonical UUID is a resumable Copilot/Claude session. A non-UUID
    // value here is a non-session identifier that leaked into the `sessionId`
    // hook field — a subagent/MCP id such as
    // `sidekick-github-context-memory-<ts>`, or a stale name — which the agent
    // rejects with "No session or name matched '…'". Fall back to a fresh
    // session so the pane opens cleanly instead of erroring.
    if !looks_like_session_id(sid) {
        return base.to_string();
    }
    let stripped = strip_resume_flag(base);
    let trimmed = stripped.trim();
    format!("{trimmed} --resume {sid}")
}

/// Whether a session id is safe to splice into a shell command line.
///
/// Real-world Copilot CLI / Claude Code session ids are UUIDs — we
/// require ASCII alphanumerics, `-`, and `_` only. Anything else
/// (spaces, shell metacharacters, quotes, newlines) means the value
/// is corrupted or a malicious agent stdout has injected something
/// unsafe; the restore code skips the resume rewrite in that case.
fn is_safe_session_id(sid: &str) -> bool {
    !sid.is_empty()
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether `sid` has the canonical UUID shape (`8-4-4-4-12` hex) that real
/// Copilot CLI and Claude Code session ids use.
///
/// `is_safe_session_id` only rejects shell-unsafe *characters* — it accepts any
/// `[A-Za-z0-9_-]` string, including a non-session identifier that a subagent or
/// MCP hook event leaked into the `sessionId` field (e.g.
/// `sidekick-github-context-memory-1783703040081`). Requiring the UUID shape
/// before splicing `--resume <id>` keeps those out; the agent would otherwise
/// reject them with "No session or name matched '…'".
pub fn looks_like_session_id(sid: &str) -> bool {
    let bytes = sid.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

const STAR_PROMPT_INTERVAL: u64 = 10;

fn star_prompt_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_STAR_PROMPT") {
        return PathBuf::from(p);
    }
    dirs_home().join(".config/dot-agent-deck/star-prompt-state.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StarPromptState {
    pub launch_count: u64,
    pub permanently_dismissed: bool,
    pub last_prompt_at_launch: u64,
}

impl StarPromptState {
    pub fn load() -> Self {
        let path = star_prompt_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("Invalid star prompt state at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "Failed to read star prompt state at {}: {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = star_prompt_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create star prompt directory: {e}"))?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize star prompt state: {e}"))?;
        std::fs::write(&path, contents).map_err(|e| {
            format!(
                "Failed to write star prompt state at {}: {e}",
                path.display()
            )
        })
    }

    pub fn increment_and_check(&mut self) -> bool {
        self.launch_count += 1;
        let _ = self.save();
        !self.permanently_dismissed
            && self.launch_count - self.last_prompt_at_launch >= STAR_PROMPT_INTERVAL
    }

    pub fn snooze(&mut self) {
        self.last_prompt_at_launch = self.launch_count;
        let _ = self.save();
    }

    pub fn dismiss_permanently(&mut self) {
        self.permanently_dismissed = true;
        let _ = self.save();
    }
}

// ---------------------------------------------------------------------------
// Config generation state — tracks directories where the user chose "Never"
// for the auto-config-prompt modal.
// ---------------------------------------------------------------------------

fn config_gen_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_CONFIG_GEN_STATE") {
        return PathBuf::from(p);
    }
    dirs_home().join(".config/dot-agent-deck/config-gen-state.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigGenState {
    pub suppressed_dirs: Vec<String>,
}

impl ConfigGenState {
    pub fn load() -> Self {
        let path = config_gen_state_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("Invalid config gen state at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "Failed to read config gen state at {}: {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_gen_state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config gen state directory: {e}"))?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config gen state: {e}"))?;
        std::fs::write(&path, contents).map_err(|e| {
            format!(
                "Failed to write config gen state at {}: {e}",
                path.display()
            )
        })
    }

    pub fn is_suppressed(&self, dir: &str) -> bool {
        self.suppressed_dirs.iter().any(|d| d == dir)
    }

    pub fn suppress_dir(&mut self, dir: &str) {
        if !self.is_suppressed(dir) {
            self.suppressed_dirs.push(dir.to_string());
            let _ = self.save();
        }
    }
}

/// Serializes tests that mutate `DOT_AGENT_DECK_CONFIG_GEN_STATE` or call
/// `ConfigGenState::save()` / `load()` (directly or through handlers like
/// `handle_config_gen_prompt_key`). Rust runs unit tests in parallel, so
/// without this lock those tests race on the shared env var and on whatever
/// state file each one points it at.
#[cfg(test)]
pub(crate) static CONFIG_GEN_STATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only RAII guard that sets `DOT_AGENT_DECK_CONFIG_GEN_STATE` and
/// restores its prior value on drop, even if the test panics. Callers must
/// hold `CONFIG_GEN_STATE_ENV_LOCK` for the guard's lifetime.
#[cfg(test)]
pub(crate) struct ConfigGenStateEnvGuard {
    prev: Option<String>,
}

#[cfg(test)]
impl ConfigGenStateEnvGuard {
    pub(crate) fn set(value: &str) -> Self {
        let prev = std::env::var("DOT_AGENT_DECK_CONFIG_GEN_STATE").ok();
        // SAFETY: callers must hold CONFIG_GEN_STATE_ENV_LOCK for the
        // duration of this guard, which serializes env-var access.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", value);
        }
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for ConfigGenStateEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see ConfigGenStateEnvGuard::set.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", v),
                None => std::env::remove_var("DOT_AGENT_DECK_CONFIG_GEN_STATE"),
            }
        }
    }
}

/// Resolve the user's home directory across platforms.
///
/// Fallback chain (first non-empty wins):
///   1. `USERPROFILE` — the canonical Windows answer; matches what
///      Windows-native programs (and the `dirs` crate) use. Putting
///      it first means dot-agent-deck's config lives where Windows
///      power users expect, and stays consistent with the other
///      home-directory lookups in this codebase
///      (`copilot_manage::home_dir`, `ui::open_bookmark` cwd fallback).
///   2. `HOME` — set on Unix; on Windows it's typically set by
///      Git Bash / MSYS2 users, and we accept it as a fallback
///      rather than a primary so behaviour matches a Windows-native
///      app on a mixed setup.
///   3. `HOMEDRIVE` + `HOMEPATH` — the Windows-legacy decomposition,
///      used by some restricted profiles where `USERPROFILE` is
///      unset.
///   4. `/` — last-resort sentinel; will route config writes to the
///      drive root on Windows, which usually fails permission-wise
///      but at least keeps a single consistent path.
///
/// Older builds resolved only `HOME` and fell straight through to
/// `/`, which on a vanilla Windows profile (no `HOME` set) stranded
/// the entire dot-agent-deck config tree under `C:\.config\`. New
/// installs land in `%USERPROFILE%\.config\` instead — see
/// [`migrate_legacy_config_dir`] for the one-time mover.
pub fn dirs_home() -> PathBuf {
    if let Ok(p) = std::env::var("USERPROFILE")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return PathBuf::from(h);
    }
    if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH"))
        && !drive.is_empty()
        && !path.is_empty()
    {
        return PathBuf::from(format!("{drive}{path}"));
    }
    PathBuf::from("/")
}

/// One-time migration: if a previous build wrote
/// `<legacy>/.config/dot-agent-deck/` because `HOME` was unset on
/// Windows, move it under the current [`dirs_home`] location.
///
/// Idempotent: bails out cleanly if (a) legacy and new resolve to the
/// same path, (b) the legacy tree is absent, or (c) the new tree
/// already exists (don't clobber freshly-written state).
///
/// Output is `eprintln!` rather than `tracing!` because this runs
/// before the tracing subscriber is installed in `main()`.
pub fn migrate_legacy_config_dir() {
    let new = dirs_home().join(".config/dot-agent-deck");
    let legacy = PathBuf::from("/.config/dot-agent-deck");
    if let Err(msg) = migrate_legacy_config_dir_impl(&legacy, &new) {
        eprintln!("dot-agent-deck: config migration warning: {msg}");
    }
}

/// Inner migration helper exposed for tests. Returns `Ok(())` when
/// nothing was needed or the move succeeded; `Err(_)` when the legacy
/// directory existed but couldn't be moved (the original is left in
/// place; user can move it manually).
fn migrate_legacy_config_dir_impl(legacy: &Path, new: &Path) -> Result<(), String> {
    if legacy == new {
        return Ok(());
    }
    if !legacy.exists() {
        return Ok(());
    }
    if new.exists() {
        // Don't clobber. User has both — they'll have to merge by hand.
        return Ok(());
    }
    if let Some(parent) = new.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::rename(legacy, new).map_err(|e| {
        format!(
            "could not move {} to {}: {e}. Move it manually to keep your bookmarks and workspaces.",
            legacy.display(),
            new.display()
        )
    })?;
    eprintln!(
        "dot-agent-deck: migrated config from {} to {}",
        legacy.display(),
        new.display(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `DOT_AGENT_DECK_WORKSPACES`. Cargo runs
    /// unit tests in parallel and they otherwise race on the shared env var.
    static WORKSPACES_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn unique_socket_path_is_per_process() {
        let p = unique_socket_path();
        let s = p.to_string_lossy();
        let pid = std::process::id().to_string();
        assert!(
            s.contains(&pid),
            "unique socket path {s} should embed the pid {pid} so decks don't collide"
        );

        #[cfg(windows)]
        assert!(
            s.starts_with(r"\\.\pipe\dot-agent-deck-"),
            "unexpected pipe name: {s}"
        );

        #[cfg(unix)]
        assert!(
            s.contains("dot-agent-deck-") && s.ends_with(".sock"),
            "unexpected socket path: {s}"
        );
    }

    #[test]
    fn bell_config_defaults() {
        let bc = BellConfig::default();
        assert!(bc.enabled);
        assert!(bc.on_waiting_for_input);
        assert!(!bc.on_idle);
        assert!(bc.on_error);
    }

    #[test]
    fn bell_config_deserialize_empty() {
        let bc: BellConfig = toml::from_str("").unwrap();
        assert!(bc.enabled);
        assert!(bc.on_waiting_for_input);
        assert!(!bc.on_idle);
        assert!(bc.on_error);
    }

    #[test]
    fn bell_config_deserialize_partial() {
        let bc: BellConfig = toml::from_str("on_idle = true").unwrap();
        assert!(bc.enabled);
        assert!(bc.on_idle);
    }

    #[test]
    fn dashboard_config_without_bell_section() {
        let dc: DashboardConfig = toml::from_str(r#"default_command = "echo hi""#).unwrap();
        assert_eq!(dc.default_command, "echo hi");
        assert!(dc.bell.enabled);
    }

    #[test]
    fn dashboard_config_with_bell_section() {
        let toml_str = r#"
default_command = "test"

[bell]
enabled = false
on_idle = true
"#;
        let dc: DashboardConfig = toml::from_str(toml_str).unwrap();
        assert!(!dc.bell.enabled);
        assert!(dc.bell.on_idle);
        assert!(dc.bell.on_waiting_for_input);
    }

    #[test]
    fn should_bell_respects_enabled() {
        let bc = BellConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(!bc.should_bell(&SessionStatus::WaitingForInput));
        assert!(!bc.should_bell(&SessionStatus::Error));
    }

    #[test]
    fn theme_defaults_to_auto() {
        let dc: DashboardConfig = toml::from_str("").unwrap();
        assert_eq!(dc.theme, Theme::Auto);
    }

    #[test]
    fn theme_deserialize_light() {
        let dc: DashboardConfig = toml::from_str(r#"theme = "light""#).unwrap();
        assert_eq!(dc.theme, Theme::Light);
    }

    #[test]
    fn theme_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("theme").unwrap(), "auto");
        dc.set_field("theme", "dark").unwrap();
        assert_eq!(dc.theme, Theme::Dark);
        assert!(dc.set_field("theme", "invalid").is_err());
    }

    #[test]
    fn saved_session_round_trip() {
        let session = SavedSession {
            panes: vec![
                SavedPane {
                    dir: "/repo/api".to_string(),
                    name: "api".to_string(),
                    command: "claude".to_string(),
                    mode: None,
                    session_id: None,
                    agent_type: None,
                },
                SavedPane {
                    dir: "/repo/ui".to_string(),
                    name: "ui".to_string(),
                    command: "".to_string(),
                    mode: None,
                    session_id: None,
                    agent_type: None,
                },
            ],
        };
        let toml_str = toml::to_string_pretty(&session).unwrap();
        let loaded: SavedSession = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.panes.len(), 2);
        assert_eq!(loaded.panes[0].dir, "/repo/api");
        assert_eq!(loaded.panes[0].name, "api");
        assert_eq!(loaded.panes[0].command, "claude");
        assert_eq!(loaded.panes[1].command, "");
    }

    #[test]
    fn saved_session_empty_default() {
        let session = SavedSession::default();
        assert!(session.panes.is_empty());
    }

    #[test]
    fn saved_session_deserialize_empty() {
        let session: SavedSession = toml::from_str("").unwrap();
        assert!(session.panes.is_empty());
    }

    #[test]
    fn saved_session_load_save_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let prev = std::env::var("DOT_AGENT_DECK_SESSION").ok();
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SESSION", path.to_str().unwrap());
        }

        // Load returns default when file missing
        let session = SavedSession::load(None);
        assert!(session.panes.is_empty());

        // Save then load round-trips
        let session = SavedSession {
            panes: vec![SavedPane {
                dir: "/tmp/test".to_string(),
                name: "test".to_string(),
                command: "echo hi".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            }],
        };
        session.save(None).unwrap();
        let loaded = SavedSession::load(None);
        assert_eq!(loaded.panes.len(), 1);
        assert_eq!(loaded.panes[0].dir, "/tmp/test");

        // Clear removes the file
        SavedSession::clear(None).unwrap();
        assert!(!path.exists());

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SESSION", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SESSION"),
            }
        }
    }

    #[test]
    fn should_bell_per_status() {
        let bc = BellConfig::default();
        assert!(bc.should_bell(&SessionStatus::WaitingForInput));
        assert!(!bc.should_bell(&SessionStatus::Idle));
        assert!(bc.should_bell(&SessionStatus::Error));
        assert!(!bc.should_bell(&SessionStatus::Thinking));
        assert!(!bc.should_bell(&SessionStatus::Working));
        assert!(!bc.should_bell(&SessionStatus::Compacting));
        // Pending defaults to true — same family as WaitingForInput.
        assert!(bc.should_bell(&SessionStatus::Pending));
    }

    #[test]
    fn bell_on_pending_can_be_disabled() {
        let bc = BellConfig {
            on_pending: false,
            ..Default::default()
        };
        assert!(!bc.should_bell(&SessionStatus::Pending));
        // Other defaults still fire.
        assert!(bc.should_bell(&SessionStatus::WaitingForInput));
    }

    #[test]
    fn pending_timeout_seconds_default_is_10() {
        let cfg = PendingConfig::default();
        assert_eq!(cfg.timeout_seconds, 10);
    }

    #[test]
    fn pending_timeout_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("pending.timeout_seconds").unwrap(), "10");
        dc.set_field("pending.timeout_seconds", "25").unwrap();
        assert_eq!(dc.pending.timeout_seconds, 25);
        // Zero disables the feature.
        dc.set_field("pending.timeout_seconds", "0").unwrap();
        assert_eq!(dc.pending.timeout_seconds, 0);
        // Bad input rejected.
        assert!(dc.set_field("pending.timeout_seconds", "abc").is_err());
    }

    #[test]
    fn bell_on_pending_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("bell.on_pending").unwrap(), "true");
        dc.set_field("bell.on_pending", "false").unwrap();
        assert!(!dc.bell.on_pending);
    }

    // -----------------------------------------------------------------------
    // Named workspaces
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_name_accepts_valid_inputs() {
        assert!(validate_workspace_name("client-x").is_ok());
        assert!(validate_workspace_name("client_X_2026").is_ok());
        assert!(validate_workspace_name("A").is_ok());
        assert!(validate_workspace_name("a-b_c-1").is_ok());
        // 64 chars is the maximum.
        let max = "a".repeat(64);
        assert!(validate_workspace_name(&max).is_ok());
    }

    #[test]
    fn workspace_name_rejects_invalid_inputs() {
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name(".").is_err());
        assert!(validate_workspace_name("..").is_err());
        assert!(validate_workspace_name("../sneaky").is_err());
        assert!(validate_workspace_name("with space").is_err());
        assert!(validate_workspace_name("with/slash").is_err());
        assert!(validate_workspace_name("with\\slash").is_err());
        assert!(validate_workspace_name("with:colon").is_err());
        assert!(validate_workspace_name("foo.toml").is_err());
        assert!(validate_workspace_name("emoji😀").is_err());
        // 65 chars is one past the maximum.
        let too_long = "a".repeat(65);
        assert!(validate_workspace_name(&too_long).is_err());
    }

    #[test]
    fn workspace_name_rejects_windows_reserved() {
        for name in ["con", "CON", "Con", "prn", "aux", "nul", "com1", "lpt9"] {
            assert!(
                validate_workspace_name(name).is_err(),
                "expected '{name}' to be rejected as a reserved name"
            );
        }
    }

    #[test]
    fn workspace_session_path_returns_default_for_none() {
        let prev = std::env::var("DOT_AGENT_DECK_SESSION").ok();
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SESSION", "/tmp/dot-agent-deck-test.toml");
        }
        let path = workspace_session_path(None).unwrap();
        assert_eq!(path.to_string_lossy(), "/tmp/dot-agent-deck-test.toml");
        // SAFETY: restore env var to original value.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SESSION", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SESSION"),
            }
        }
    }

    #[test]
    fn workspace_session_path_resolves_under_workspaces_dir() {
        let _guard = WORKSPACES_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DOT_AGENT_DECK_WORKSPACES").ok();
        // SAFETY: test is single-threaded.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_WORKSPACES", "/tmp/ws-test");
        }
        let path = workspace_session_path(Some("client-x")).unwrap();
        assert!(
            path.to_string_lossy().ends_with("client-x.toml"),
            "path was {}",
            path.display()
        );
        assert!(
            path.to_string_lossy().contains("ws-test"),
            "path was {}",
            path.display()
        );
        // SAFETY: restore env var to original value.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_WORKSPACES", v),
                None => std::env::remove_var("DOT_AGENT_DECK_WORKSPACES"),
            }
        }
    }

    #[test]
    fn workspace_session_path_rejects_invalid_name() {
        assert!(workspace_session_path(Some("../sneaky")).is_err());
        assert!(workspace_session_path(Some("")).is_err());
    }

    #[test]
    fn workspace_save_load_clear_round_trip() {
        let _guard = WORKSPACES_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("DOT_AGENT_DECK_WORKSPACES").ok();
        // SAFETY: test is single-threaded.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_WORKSPACES", dir.path().to_str().unwrap());
        }

        // Missing workspace loads as empty.
        let empty = SavedSession::load(Some("alpha"));
        assert!(empty.panes.is_empty());

        // Save then load round-trips.
        let session = SavedSession {
            panes: vec![SavedPane {
                dir: "/tmp/proj".to_string(),
                name: "proj".to_string(),
                command: "claude".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            }],
        };
        session.save(Some("alpha")).unwrap();
        let loaded = SavedSession::load(Some("alpha"));
        assert_eq!(loaded.panes.len(), 1);
        assert_eq!(loaded.panes[0].name, "proj");

        // Saving to a *different* workspace name produces a separate file.
        let other = SavedSession {
            panes: vec![SavedPane {
                dir: "/tmp/other".to_string(),
                name: "other".to_string(),
                command: "opencode".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            }],
        };
        other.save(Some("beta")).unwrap();
        assert_eq!(SavedSession::load(Some("alpha")).panes[0].command, "claude");
        assert_eq!(
            SavedSession::load(Some("beta")).panes[0].command,
            "opencode"
        );

        // Clear removes only the named workspace.
        SavedSession::clear(Some("alpha")).unwrap();
        assert!(SavedSession::load(Some("alpha")).panes.is_empty());
        assert_eq!(SavedSession::load(Some("beta")).panes.len(), 1);

        // SAFETY: restore env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_WORKSPACES", v),
                None => std::env::remove_var("DOT_AGENT_DECK_WORKSPACES"),
            }
        }
    }

    #[test]
    fn list_workspaces_returns_sorted_stems() {
        let _guard = WORKSPACES_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("DOT_AGENT_DECK_WORKSPACES").ok();
        // SAFETY: test is single-threaded.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_WORKSPACES", dir.path().to_str().unwrap());
        }

        // Empty directory → empty list.
        assert!(list_workspaces().is_empty());

        // Create three workspace files and one non-toml file.
        let session = SavedSession::default();
        session.save(Some("zeta")).unwrap();
        session.save(Some("alpha")).unwrap();
        session.save(Some("middle")).unwrap();
        std::fs::write(dir.path().join("not-a-workspace.txt"), "ignore me").unwrap();
        // Also drop a file with an invalid stem to confirm it's filtered out.
        std::fs::write(dir.path().join("with space.toml"), "panes = []").unwrap();

        let workspaces = list_workspaces();
        assert_eq!(workspaces, vec!["alpha", "middle", "zeta"]);

        // SAFETY: restore env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_WORKSPACES", v),
                None => std::env::remove_var("DOT_AGENT_DECK_WORKSPACES"),
            }
        }
    }

    #[test]
    fn delete_workspace_returns_true_when_removed_false_when_absent() {
        let _guard = WORKSPACES_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("DOT_AGENT_DECK_WORKSPACES").ok();
        // SAFETY: test is single-threaded.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_WORKSPACES", dir.path().to_str().unwrap());
        }

        // Nothing to delete yet.
        assert!(!delete_workspace("nope").unwrap());

        // Create and delete.
        SavedSession::default().save(Some("transient")).unwrap();
        assert!(delete_workspace("transient").unwrap());
        assert!(!delete_workspace("transient").unwrap());

        // SAFETY: restore env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_WORKSPACES", v),
                None => std::env::remove_var("DOT_AGENT_DECK_WORKSPACES"),
            }
        }
    }

    #[test]
    fn save_is_atomic_via_temp_rename() {
        // Regression guard: a successful save() must leave no `.tmp` file
        // behind. Earlier implementations used `fs::write` directly, which
        // is non-atomic — a crash between truncate and write would leave a
        // zero-byte workspace file. The fix writes to `<path>.tmp` then
        // `rename`s on top of the real path; if either step fails the tmp
        // file should be cleaned up.
        let _guard = WORKSPACES_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("DOT_AGENT_DECK_WORKSPACES").ok();
        // SAFETY: test is single-threaded.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_WORKSPACES", dir.path().to_str().unwrap());
        }

        let session = SavedSession {
            panes: vec![SavedPane {
                dir: "/tmp/proj".to_string(),
                name: "proj".to_string(),
                command: "claude".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            }],
        };
        session.save(Some("atomic")).unwrap();

        // The real file should exist with full contents.
        let real_path = dir.path().join("atomic.toml");
        assert!(real_path.is_file(), "real workspace file must exist");
        let loaded = SavedSession::load(Some("atomic"));
        assert_eq!(loaded.panes.len(), 1);

        // No leftover `.tmp` sibling.
        let tmp_path = dir.path().join("atomic.toml.tmp");
        assert!(
            !tmp_path.exists(),
            "temp file should have been renamed away, not left behind"
        );

        // Saving again over an existing file (with new content) still
        // ends up atomic.
        let session2 = SavedSession {
            panes: vec![
                SavedPane {
                    dir: "/a".to_string(),
                    name: "a".to_string(),
                    command: "claude".to_string(),
                    mode: None,
                    session_id: None,
                    agent_type: None,
                },
                SavedPane {
                    dir: "/b".to_string(),
                    name: "b".to_string(),
                    command: "opencode".to_string(),
                    mode: None,
                    session_id: None,
                    agent_type: None,
                },
            ],
        };
        session2.save(Some("atomic")).unwrap();
        let loaded = SavedSession::load(Some("atomic"));
        assert_eq!(loaded.panes.len(), 2);
        assert!(!tmp_path.exists());

        // SAFETY: restore env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_WORKSPACES", v),
                None => std::env::remove_var("DOT_AGENT_DECK_WORKSPACES"),
            }
        }
    }

    #[test]
    fn save_does_not_corrupt_existing_file_when_target_dir_was_just_created() {
        // The save() path creates parent dirs on demand. Verify that the
        // temp-then-rename still works when the parent didn't exist before.
        let _guard = WORKSPACES_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let outer = tempfile::tempdir().unwrap();
        let nested = outer.path().join("does/not/exist/yet");
        let prev = std::env::var("DOT_AGENT_DECK_WORKSPACES").ok();
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_WORKSPACES", nested.to_str().unwrap());
        }

        SavedSession::default().save(Some("freshdir")).unwrap();
        assert!(nested.join("freshdir.toml").is_file());
        assert!(!nested.join("freshdir.toml.tmp").exists());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_WORKSPACES", v),
                None => std::env::remove_var("DOT_AGENT_DECK_WORKSPACES"),
            }
        }
    }

    #[test]
    fn star_prompt_default_values() {
        let state = StarPromptState::default();
        assert_eq!(state.launch_count, 0);
        assert!(!state.permanently_dismissed);
        assert_eq!(state.last_prompt_at_launch, 0);
    }

    #[test]
    fn star_prompt_serde_round_trip() {
        let state = StarPromptState {
            launch_count: 42,
            permanently_dismissed: true,
            last_prompt_at_launch: 30,
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: StarPromptState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.launch_count, 42);
        assert!(loaded.permanently_dismissed);
        assert_eq!(loaded.last_prompt_at_launch, 30);
    }

    #[test]
    fn star_prompt_serde_missing_fields() {
        let loaded: StarPromptState = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded.launch_count, 0);
        assert!(!loaded.permanently_dismissed);
        assert_eq!(loaded.last_prompt_at_launch, 0);
    }

    #[test]
    fn star_prompt_increment_and_check_triggers_at_10() {
        // Test pure logic without file I/O — manually track state
        let mut state = StarPromptState::default();
        for i in 1..=9 {
            state.launch_count = i;
            let should_show = !state.permanently_dismissed
                && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
            assert!(!should_show, "should not trigger at launch {i}");
        }
        state.launch_count = 10;
        let should_show = !state.permanently_dismissed
            && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
        assert!(should_show, "should trigger at launch 10");
    }

    #[test]
    fn star_prompt_snooze_resets_window() {
        let mut state = StarPromptState::default();
        state.launch_count = 10;
        state.last_prompt_at_launch = state.launch_count; // snooze
        for i in 11..=19 {
            state.launch_count = i;
            let should_show = !state.permanently_dismissed
                && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
            assert!(!should_show, "should not trigger at launch {i}");
        }
        state.launch_count = 20;
        let should_show = !state.permanently_dismissed
            && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
        assert!(should_show, "should trigger at launch 20");
    }

    #[test]
    fn star_prompt_dismiss_permanently() {
        let mut state = StarPromptState {
            permanently_dismissed: true,
            ..StarPromptState::default()
        };
        for i in 1..=20 {
            state.launch_count = i;
            let should_show = !state.permanently_dismissed
                && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
            assert!(!should_show, "dismissed state should never trigger");
        }
    }

    #[test]
    fn star_prompt_load_save_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let prev = std::env::var("DOT_AGENT_DECK_STAR_PROMPT").ok();
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_STAR_PROMPT", path.to_str().unwrap());
        }

        let state = StarPromptState {
            launch_count: 15,
            permanently_dismissed: false,
            last_prompt_at_launch: 10,
        };
        state.save().unwrap();

        let loaded = StarPromptState::load();
        assert_eq!(loaded.launch_count, 15);
        assert!(!loaded.permanently_dismissed);
        assert_eq!(loaded.last_prompt_at_launch, 10);

        // Load from corrupted file returns default
        std::fs::write(&path, "not valid json!!!").unwrap();
        let loaded = StarPromptState::load();
        assert_eq!(loaded.launch_count, 0);

        // Load from missing file returns default
        std::fs::remove_file(&path).unwrap();
        let loaded = StarPromptState::load();
        assert_eq!(loaded.launch_count, 0);
        assert!(!loaded.permanently_dismissed);

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STAR_PROMPT", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STAR_PROMPT"),
            }
        }
    }

    #[test]
    fn idle_art_config_defaults() {
        let config = IdleArtConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.model, "claude-haiku-4-5");
        assert_eq!(config.timeout_secs, 300);
    }

    #[test]
    fn dashboard_config_without_idle_art() {
        let dc: DashboardConfig = toml::from_str("").unwrap();
        assert!(!dc.idle_art.enabled);
        assert_eq!(dc.idle_art.provider, "anthropic");
        assert_eq!(dc.idle_art.model, "claude-haiku-4-5");
    }

    #[test]
    fn dashboard_config_with_idle_art() {
        let toml_str = r#"
[idle_art]
enabled = true
provider = "openai"
model = "gpt-4o-mini"
timeout_secs = 600
"#;
        let dc: DashboardConfig = toml::from_str(toml_str).unwrap();
        assert!(dc.idle_art.enabled);
        assert_eq!(dc.idle_art.provider, "openai");
        assert_eq!(dc.idle_art.model, "gpt-4o-mini");
        assert_eq!(dc.idle_art.timeout_secs, 600);
    }

    #[test]
    fn idle_art_get_set_fields() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("idle_art.enabled").unwrap(), "false");
        assert_eq!(dc.get_field("idle_art.provider").unwrap(), "anthropic");
        assert_eq!(dc.get_field("idle_art.model").unwrap(), "claude-haiku-4-5");
        assert_eq!(dc.get_field("idle_art.timeout_secs").unwrap(), "300");

        dc.set_field("idle_art.enabled", "true").unwrap();
        assert!(dc.idle_art.enabled);

        dc.set_field("idle_art.provider", "ollama").unwrap();
        assert_eq!(dc.idle_art.provider, "ollama");

        dc.set_field("idle_art.model", "llama3").unwrap();
        assert_eq!(dc.idle_art.model, "llama3");

        dc.set_field("idle_art.timeout_secs", "120").unwrap();
        assert_eq!(dc.idle_art.timeout_secs, 120);

        assert!(dc.set_field("idle_art.enabled", "notabool").is_err());
        assert!(dc.set_field("idle_art.timeout_secs", "notanumber").is_err());
    }

    #[test]
    fn auto_config_prompt_defaults_to_true() {
        let dc = DashboardConfig::default();
        assert!(dc.auto_config_prompt);
    }

    #[test]
    fn auto_config_prompt_deserialize_missing() {
        let dc: DashboardConfig = toml::from_str("").unwrap();
        assert!(dc.auto_config_prompt);
    }

    #[test]
    fn auto_config_prompt_deserialize_false() {
        let dc: DashboardConfig = toml::from_str("auto_config_prompt = false").unwrap();
        assert!(!dc.auto_config_prompt);
    }

    #[test]
    fn auto_config_prompt_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("auto_config_prompt").unwrap(), "true");
        dc.set_field("auto_config_prompt", "false").unwrap();
        assert!(!dc.auto_config_prompt);
        assert_eq!(dc.get_field("auto_config_prompt").unwrap(), "false");
        assert!(dc.set_field("auto_config_prompt", "notbool").is_err());
    }

    #[test]
    fn config_gen_state_default_empty() {
        let state = ConfigGenState::default();
        assert!(state.suppressed_dirs.is_empty());
    }

    #[test]
    fn config_gen_state_suppress_and_check() {
        let mut state = ConfigGenState::default();
        assert!(!state.is_suppressed("/some/dir"));
        state.suppressed_dirs.push("/some/dir".to_string());
        assert!(state.is_suppressed("/some/dir"));
        assert!(!state.is_suppressed("/other/dir"));
    }

    #[test]
    fn config_gen_state_suppress_dir_deduplicates() {
        // suppress_dir() calls save(), which reads DOT_AGENT_DECK_CONFIG_GEN_STATE.
        // Hold the env-var lock and point at a temp path so we neither race
        // against load_save_cycle nor pollute the real home dir.
        let _guard = CONFIG_GEN_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config-gen-state.json");
        // Drop guard restores the env var even if an assertion below panics.
        let _env_restore = ConfigGenStateEnvGuard::set(path.to_str().unwrap());

        let mut state = ConfigGenState::default();
        state.suppressed_dirs.push("/dup".to_string());
        state.suppressed_dirs.push("/dup".to_string()); // manual dup
        // suppress_dir should not add again
        assert_eq!(state.suppressed_dirs.len(), 2);
        // But the method itself checks before adding
        let mut state2 = ConfigGenState::default();
        state2.suppressed_dirs.push("/dup".to_string());
        state2.suppress_dir("/dup");
        assert_eq!(state2.suppressed_dirs.len(), 1);
    }

    #[test]
    fn config_gen_state_serde_round_trip() {
        let state = ConfigGenState {
            suppressed_dirs: vec!["/a".to_string(), "/b".to_string()],
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: ConfigGenState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.suppressed_dirs.len(), 2);
        assert!(loaded.is_suppressed("/a"));
        assert!(loaded.is_suppressed("/b"));
    }

    #[test]
    fn config_gen_state_load_save_cycle() {
        // Serialize against any other test that touches this env var or calls
        // save()/load() — Rust runs unit tests in parallel.
        let _guard = CONFIG_GEN_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config-gen-state.json");
        let prev = std::env::var("DOT_AGENT_DECK_CONFIG_GEN_STATE").ok();
        // SAFETY: env-var lock held for the duration of this test.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", path.to_str().unwrap());
        }

        // Load returns default when file missing
        let state = ConfigGenState::load();
        assert!(state.suppressed_dirs.is_empty());

        // Save then load round-trips
        let mut state = ConfigGenState::default();
        state.suppressed_dirs.push("/test/dir".to_string());
        state.save().unwrap();
        let loaded = ConfigGenState::load();
        assert_eq!(loaded.suppressed_dirs.len(), 1);
        assert!(loaded.is_suppressed("/test/dir"));

        // Load from corrupted file returns default
        std::fs::write(&path, "not valid json!!!").unwrap();
        let loaded = ConfigGenState::load();
        assert!(loaded.suppressed_dirs.is_empty());

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", v),
                None => std::env::remove_var("DOT_AGENT_DECK_CONFIG_GEN_STATE"),
            }
        }
    }

    // ----- Workspace-resume helpers -----

    fn make_session(
        session_id: &str,
        pane_id: &str,
        agent: AgentType,
        seconds_ago: i64,
    ) -> SessionState {
        SessionState {
            session_id: session_id.to_string(),
            agent_type: agent,
            cwd: None,
            status: SessionStatus::Idle,
            active_tool: None,
            started_at: chrono::Utc::now(),
            last_activity: chrono::Utc::now() - chrono::Duration::seconds(seconds_ago),
            recent_events: Default::default(),
            tool_count: 0,
            last_user_prompt: None,
            first_prompts: vec![],
            pane_id: Some(pane_id.to_string()),
            active_subagent_count: 0,
        }
    }

    #[test]
    fn strip_resume_flag_no_op_when_absent() {
        assert_eq!(strip_resume_flag(""), "");
        assert_eq!(strip_resume_flag("copilot"), "copilot");
        assert_eq!(
            strip_resume_flag("copilot --model foo"),
            "copilot --model foo"
        );
    }

    #[test]
    fn strip_resume_flag_drops_flag_and_value() {
        assert_eq!(strip_resume_flag("copilot --resume abc"), "copilot");
        assert_eq!(
            strip_resume_flag("copilot --resume abc-123 --model x"),
            "copilot --model x"
        );
    }

    #[test]
    fn strip_resume_flag_tolerates_dangling_flag() {
        // No following token — drop just the flag itself.
        assert_eq!(strip_resume_flag("copilot --resume"), "copilot");
    }

    #[test]
    fn strip_resume_flag_removes_multiple_pairs() {
        assert_eq!(
            strip_resume_flag("copilot --resume a --resume b"),
            "copilot"
        );
    }

    #[test]
    fn is_simple_invocation_bare_agent() {
        assert!(is_simple_agent_invocation(
            "copilot",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation("claude", &AgentType::ClaudeCode));
    }

    #[test]
    fn is_simple_invocation_with_resume_only() {
        // Existing --resume <id> is the round-trip case — must still qualify.
        assert!(is_simple_agent_invocation(
            "copilot --resume abc-123",
            &AgentType::CopilotCli
        ));
    }

    #[test]
    fn is_simple_invocation_accepts_flag_args_after_agent() {
        // Boolean flags (and `--flag=value`) after the agent binary
        // round-trip safely. (Previously rejected; users with
        // `copilot --allow-all` should still get resume on restore.)
        assert!(is_simple_agent_invocation(
            "copilot --allow-all",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "copilot --model=gpt-5",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "claude --print",
            &AgentType::ClaudeCode
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_flag_value_pair_after_agent() {
        // We can't tell `--model gpt-5` (flag + value) apart from
        // `--print run` (flag + positional sub-command), so the
        // strict gate rejects any non-flag token after the agent.
        // Users with `--flag value` form lose `--resume` on restore
        // — same as the pre-fix behaviour. They can switch to
        // `--flag=value` to get resume.
        assert!(!is_simple_agent_invocation(
            "copilot --model gpt-5",
            &AgentType::CopilotCli
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_positional_after_agent() {
        // Plain positional tokens (no preceding flag) are sub-commands
        // or quoted args that we can't safely round-trip.
        assert!(!is_simple_agent_invocation(
            "copilot foo",
            &AgentType::CopilotCli
        ));
        assert!(!is_simple_agent_invocation(
            "claude help me",
            &AgentType::ClaudeCode
        ));
    }

    #[test]
    fn is_simple_invocation_accepts_known_wrappers() {
        // Internal Microsoft wrappers (`agency`/`agenchy`) and
        // npm-ecosystem runners (`npx`/`pnpx`/`bunx`) precede the
        // agent token safely and pass flags through to the child.
        assert!(is_simple_agent_invocation(
            "agency copilot",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "agency copilot --allow-all",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "agenchy copilot --allow-all",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "npx copilot",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "pnpx claude --print",
            &AgentType::ClaudeCode
        ));
        assert!(is_simple_agent_invocation(
            "bunx copilot",
            &AgentType::CopilotCli
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_unknown_wrappers() {
        // `cmd /c copilot` has a flag (`/c`) before the agent that
        // belongs to the wrapper layer — we can't safely append
        // `--resume` because the inner shell may re-parse it.
        assert!(!is_simple_agent_invocation(
            "cmd /c copilot",
            &AgentType::CopilotCli
        ));
        // Random preceding word that isn't in the wrapper whitelist.
        assert!(!is_simple_agent_invocation(
            "myshim copilot",
            &AgentType::CopilotCli
        ));
        // Even a known wrapper plus its own flags before the agent
        // is rejected — we can't tell `-p` from `--p` from `--p val`.
        assert!(!is_simple_agent_invocation(
            "npx -p something copilot",
            &AgentType::CopilotCli
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_shell_metachars() {
        // Tokens containing shell metacharacters must never be
        // re-emitted — even past `is_safe_session_id`, the launch
        // command itself could be a re-injection vector.
        for cmd in [
            "copilot --model 'foo'",
            r#"copilot --model "foo""#,
            "copilot --model $(whoami)",
            "copilot --model `whoami`",
            "copilot --model foo|cat",
            "copilot && rm -rf /",
        ] {
            assert!(
                !is_simple_agent_invocation(cmd, &AgentType::CopilotCli),
                "expected reject for: {cmd:?}"
            );
        }
    }

    #[test]
    fn is_simple_invocation_strips_path_and_exe() {
        assert!(is_simple_agent_invocation(
            r"C:\Users\me\bin\copilot.exe",
            &AgentType::CopilotCli
        ));
        assert!(is_simple_agent_invocation(
            "/usr/local/bin/claude",
            &AgentType::ClaudeCode
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_agent_mismatch() {
        // The launch command is `copilot` but the recorded agent type
        // says `ClaudeCode` — user must have hand-edited TOML, so we
        // refuse to rewrite (safer to leave the command alone).
        assert!(!is_simple_agent_invocation(
            "copilot",
            &AgentType::ClaudeCode
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_opencode_and_none() {
        // Neither agent supports `--resume` in this UI, so it can never
        // be a "simple" rewritable invocation regardless of command text.
        assert!(!is_simple_agent_invocation(
            "opencode",
            &AgentType::OpenCode
        ));
        assert!(!is_simple_agent_invocation("copilot", &AgentType::None));
    }

    #[test]
    fn build_resume_command_happy_path_copilot() {
        let sid = "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa";
        let out = build_resume_command("copilot", Some(sid), Some(&AgentType::CopilotCli));
        assert_eq!(out, format!("copilot --resume {sid}"));
    }

    #[test]
    fn build_resume_command_happy_path_claude() {
        let sid = "faa64853-a973-4e84-a4da-84844b63a9cf";
        let out = build_resume_command("claude", Some(sid), Some(&AgentType::ClaudeCode));
        assert_eq!(out, format!("claude --resume {sid}"));
    }

    #[test]
    fn build_resume_command_round_trip_is_idempotent() {
        // Restore writes `copilot --resume X`; on the next save+restore
        // we must end up with the new id rather than two flags.
        let id1 = "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa";
        let id2 = "faa64853-a973-4e84-a4da-84844b63a9cf";
        let first = build_resume_command("copilot", Some(id1), Some(&AgentType::CopilotCli));
        assert_eq!(first, format!("copilot --resume {id1}"));
        let second = build_resume_command(&first, Some(id2), Some(&AgentType::CopilotCli));
        assert_eq!(second, format!("copilot --resume {id2}"));
    }

    #[test]
    fn build_resume_command_appends_to_flag_args() {
        // Boolean flags after the agent stay intact; `--resume` is
        // appended at the end. (Previously this passed through
        // unchanged — we now resume in this shape too.)
        let sid = "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa";
        assert_eq!(
            build_resume_command(
                "copilot --allow-all",
                Some(sid),
                Some(&AgentType::CopilotCli)
            ),
            format!("copilot --allow-all --resume {sid}")
        );
        assert_eq!(
            build_resume_command(
                "copilot --model=gpt-5",
                Some(sid),
                Some(&AgentType::CopilotCli)
            ),
            format!("copilot --model=gpt-5 --resume {sid}")
        );
    }

    #[test]
    fn build_resume_command_passes_through_flag_value_pair() {
        // `--flag value` form is rejected by the gate (see
        // `is_simple_invocation_rejects_flag_value_pair_after_agent`)
        // — command preserved as-is, no resume on restore.
        let cmd = "copilot --model gpt-5";
        assert_eq!(
            build_resume_command(cmd, Some("x"), Some(&AgentType::CopilotCli)),
            cmd
        );
    }

    #[test]
    fn build_resume_command_appends_through_wrapper() {
        // The headline workspace-resume scenario: `agency` is a hook
        // wrapper around Copilot CLI and must transparently pass
        // `--resume <id>` through to the inner agent.
        assert_eq!(
            build_resume_command(
                "agency copilot --allow-all",
                Some("faa64853-a973-4e84-a4da-84844b63a9cf"),
                Some(&AgentType::CopilotCli)
            ),
            "agency copilot --allow-all --resume faa64853-a973-4e84-a4da-84844b63a9cf"
        );
        assert_eq!(
            build_resume_command(
                "npx claude",
                Some("0dc83c83-3bd6-4fb8-83c1-c8d25d820f86"),
                Some(&AgentType::ClaudeCode)
            ),
            "npx claude --resume 0dc83c83-3bd6-4fb8-83c1-c8d25d820f86"
        );
    }

    #[test]
    fn build_resume_command_wrapper_round_trip_is_idempotent() {
        // Restore writes `agency copilot --allow-all --resume X`; on
        // the next save+restore we must end up with the new id rather
        // than two flags appended.
        let id1 = "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa";
        let id2 = "faa64853-a973-4e84-a4da-84844b63a9cf";
        let first = build_resume_command(
            "agency copilot --allow-all",
            Some(id1),
            Some(&AgentType::CopilotCli),
        );
        assert_eq!(first, format!("agency copilot --allow-all --resume {id1}"));
        let second = build_resume_command(&first, Some(id2), Some(&AgentType::CopilotCli));
        assert_eq!(second, format!("agency copilot --allow-all --resume {id2}"));
    }

    #[test]
    fn build_resume_command_passes_through_unknown_wrapper() {
        // `cmd /c <agent>` has a wrapper-level flag we can't safely
        // round-trip — leave it alone.
        let cmd = "cmd /c copilot";
        assert_eq!(
            build_resume_command(cmd, Some("x"), Some(&AgentType::CopilotCli)),
            cmd
        );
    }

    #[test]
    fn build_resume_command_passes_through_opencode() {
        // No --resume support → leave command alone even if a session id
        // was somehow recorded.
        let cmd = "opencode";
        assert_eq!(
            build_resume_command(cmd, Some("x"), Some(&AgentType::OpenCode)),
            cmd
        );
    }

    #[test]
    fn build_resume_command_passes_through_when_missing_metadata() {
        let cmd = "copilot";
        assert_eq!(build_resume_command(cmd, None, None), cmd);
        assert_eq!(build_resume_command(cmd, Some("x"), None), cmd);
        assert_eq!(
            build_resume_command(cmd, None, Some(&AgentType::CopilotCli)),
            cmd
        );
    }

    #[test]
    fn build_resume_command_rejects_unsafe_session_ids() {
        // Corrupted / hostile ids must not be spliced into a shell
        // command line. We refuse the rewrite and the original
        // command is preserved (which means the user gets a fresh
        // session — annoying but safe).
        let cmd = "copilot";
        let bad = [
            "",
            "abc 123",           // space
            "abc;rm -rf /",      // semicolon
            "abc\nrm",           // newline
            "abc$(whoami)",      // command substitution
            "abc`whoami`",       // backticks
            "abc&whoami",        // background
            "abc|cat",           // pipe
            "abc/../etc/passwd", // slash
            r"abc\foo",          // backslash
            "\"abc\"",           // quotes
        ];
        for sid in bad {
            assert_eq!(
                build_resume_command(cmd, Some(sid), Some(&AgentType::CopilotCli)),
                cmd,
                "must refuse unsafe session id: {sid:?}"
            );
        }
        // And the happy UUID-shaped case still works.
        let uuid = "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa";
        assert_eq!(
            build_resume_command(cmd, Some(uuid), Some(&AgentType::CopilotCli)),
            format!("copilot --resume {uuid}")
        );
    }

    #[test]
    fn build_resume_command_rejects_tool_call_ids() {
        // A subagent tool-call id (`toolu_…`/`call_…`) is shell-safe but
        // isn't a resumable session. If one leaked into a legacy/corrupted
        // snapshot, restore must fall back to a fresh session rather than
        // emit `--resume toolu_…`, which the agent rejects with
        // "No session or name matched '…'".
        let cmd = "agency copilot --allow-all";
        for sid in [
            "toolu_018dZ3HtuEnKRQfjGwaGZEFc",
            "call_GGpCiUtRHsZ9gtsmEusrlbHH",
        ] {
            assert_eq!(
                build_resume_command(cmd, Some(sid), Some(&AgentType::CopilotCli)),
                cmd,
                "must refuse to resume a tool-call id: {sid:?}"
            );
        }
    }

    #[test]
    fn build_resume_command_rejects_non_uuid_leaked_ids() {
        // Subagent/MCP hook events can leak a non-session identifier into
        // `sessionId` that is charset-clean but not a resumable session — a real
        // case is `sidekick-github-context-memory-<ts>`. Restore must fall back
        // to a fresh session rather than emit a `--resume` the agent rejects
        // with "No session or name matched '…'".
        let cmd = "agency copilot --allow-all";
        for sid in [
            "sidekick-github-context-memory-1783703040081",
            "my-named-session",
            "1783703040081",
            "7eddc990ea494c5d9e6af0b718aa39aa", // UUID without hyphens
            "7eddc990-ea49-4c5d-9e6a-f0b718aa39a", // one char too short
            "7eddc990-ea49-4c5d-9e6a-f0b718aa39aaa", // one char too long
        ] {
            assert_eq!(
                build_resume_command(cmd, Some(sid), Some(&AgentType::CopilotCli)),
                cmd,
                "must refuse to resume a non-uuid id: {sid:?}"
            );
        }
        // A canonical UUID still resumes.
        let uuid = "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa";
        assert_eq!(
            build_resume_command(cmd, Some(uuid), Some(&AgentType::CopilotCli)),
            format!("{cmd} --resume {uuid}")
        );
    }

    #[test]
    fn looks_like_session_id_accepts_only_canonical_uuids() {
        assert!(looks_like_session_id(
            "7eddc990-ea49-4c5d-9e6a-f0b718aa39aa"
        ));
        // Hex is case-insensitive.
        assert!(looks_like_session_id(
            "FAA64853-A973-4E84-A4DA-84844B63A9CF"
        ));
        assert!(!looks_like_session_id(""));
        assert!(!looks_like_session_id(
            "sidekick-github-context-memory-1783703040081"
        ));
        assert!(!looks_like_session_id("7eddc990ea494c5d9e6af0b718aa39aa")); // no hyphens
        assert!(!looks_like_session_id("toolu_018dZ3HtuEnKRQfjGwaGZEFc"));
        // Right length and hyphen layout, but a non-hex char.
        assert!(!looks_like_session_id(
            "zeddc990-ea49-4c5d-9e6a-f0b718aa39aa"
        ));
    }

    #[test]
    fn saved_pane_round_trip_with_resume_fields() {
        let session = SavedSession {
            panes: vec![SavedPane {
                dir: "/repo".to_string(),
                name: "api".to_string(),
                command: "copilot".to_string(),
                mode: None,
                session_id: Some("abc-123".to_string()),
                agent_type: Some(AgentType::CopilotCli),
            }],
        };
        let s = toml::to_string_pretty(&session).unwrap();
        // Spelling matters: AgentType serializes snake_case, so the
        // workspace file we write must look like `copilot_cli`, not
        // `CopilotCli` or `copilot-cli`. Manual edits in a saved
        // workspace TOML depend on this being stable.
        assert!(s.contains("agent_type = \"copilot_cli\""), "got: {s}");
        assert!(s.contains("session_id = \"abc-123\""), "got: {s}");
        let loaded: SavedSession = toml::from_str(&s).unwrap();
        assert_eq!(loaded.panes[0].session_id.as_deref(), Some("abc-123"));
        assert_eq!(
            loaded.panes[0].agent_type.as_ref(),
            Some(&AgentType::CopilotCli)
        );
    }

    // ----- dirs_home + legacy-dir migration -----

    /// Serializes tests that mutate `HOME` / `USERPROFILE` /
    /// `HOMEDRIVE` / `HOMEPATH`. Same rationale as
    /// `WORKSPACES_ENV_LOCK` — these are global mutable state.
    static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that snapshots the four home-related env vars,
    /// unsets them all, and restores the originals on drop — so the
    /// inner test starts from a clean slate and the rest of the
    /// process keeps its real values when the test finishes.
    struct HomeEnvGuard {
        home: Option<String>,
        profile: Option<String>,
        drive: Option<String>,
        path: Option<String>,
    }

    impl HomeEnvGuard {
        fn new() -> Self {
            let g = HomeEnvGuard {
                home: std::env::var("HOME").ok(),
                profile: std::env::var("USERPROFILE").ok(),
                drive: std::env::var("HOMEDRIVE").ok(),
                path: std::env::var("HOMEPATH").ok(),
            };
            // SAFETY: protected by HOME_ENV_LOCK at the call site.
            unsafe {
                std::env::remove_var("HOME");
                std::env::remove_var("USERPROFILE");
                std::env::remove_var("HOMEDRIVE");
                std::env::remove_var("HOMEPATH");
            }
            g
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            // SAFETY: lock-protected.
            unsafe {
                match &self.home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match &self.profile {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
                match &self.drive {
                    Some(v) => std::env::set_var("HOMEDRIVE", v),
                    None => std::env::remove_var("HOMEDRIVE"),
                }
                match &self.path {
                    Some(v) => std::env::set_var("HOMEPATH", v),
                    None => std::env::remove_var("HOMEPATH"),
                }
            }
        }
    }

    #[test]
    fn dirs_home_prefers_userprofile_first() {
        // USERPROFILE wins over HOME so dot-agent-deck behaves like a
        // Windows-native app (and matches `copilot_manage::home_dir`,
        // `ui::open_bookmark` fallback, and the `dirs` crate).
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let _guard = HomeEnvGuard::new();
        // SAFETY: lock-protected.
        unsafe {
            std::env::set_var("HOME", "/from-home");
            std::env::set_var("USERPROFILE", "/from-profile");
        }
        assert_eq!(dirs_home(), PathBuf::from("/from-profile"));
    }

    #[test]
    fn dirs_home_falls_back_to_home_when_userprofile_unset() {
        // Git Bash / MSYS2 users on Windows often have HOME set but
        // not USERPROFILE (or vice versa). HOME is the fallback.
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let _guard = HomeEnvGuard::new();
        // SAFETY: lock-protected.
        unsafe {
            std::env::set_var("HOME", "/from-home");
        }
        assert_eq!(dirs_home(), PathBuf::from("/from-home"));
    }

    #[test]
    fn dirs_home_falls_back_to_userprofile_when_home_unset() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let _guard = HomeEnvGuard::new();
        // SAFETY: lock-protected.
        unsafe {
            std::env::set_var("USERPROFILE", r"C:\Users\jonovak");
        }
        assert_eq!(dirs_home(), PathBuf::from(r"C:\Users\jonovak"));
    }

    #[test]
    fn dirs_home_skips_empty_userprofile() {
        // Defensive: an empty USERPROFILE (rare, but possible in some
        // service contexts) must not return PathBuf::from("") — fall
        // through to the next fallback.
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let _guard = HomeEnvGuard::new();
        // SAFETY: lock-protected.
        unsafe {
            std::env::set_var("USERPROFILE", "");
            std::env::set_var("HOME", "/from-home");
        }
        assert_eq!(dirs_home(), PathBuf::from("/from-home"));
    }

    #[test]
    fn dirs_home_falls_back_to_homedrive_homepath() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let _guard = HomeEnvGuard::new();
        // SAFETY: lock-protected.
        unsafe {
            std::env::set_var("HOMEDRIVE", "C:");
            std::env::set_var("HOMEPATH", r"\Users\jonovak");
        }
        assert_eq!(dirs_home(), PathBuf::from(r"C:\Users\jonovak"));
    }

    #[test]
    fn dirs_home_falls_back_to_slash_when_nothing_set() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let _guard = HomeEnvGuard::new();
        // All four env vars are now unset (HomeEnvGuard).
        assert_eq!(dirs_home(), PathBuf::from("/"));
    }

    #[test]
    fn migrate_legacy_config_dir_noop_when_legacy_missing() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let new = dir.path().join("new");
        // Legacy doesn't exist → must be a no-op, and must not create `new`.
        assert!(migrate_legacy_config_dir_impl(&legacy, &new).is_ok());
        assert!(!new.exists());
    }

    #[test]
    fn migrate_legacy_config_dir_noop_when_paths_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot-agent-deck");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("config.toml"), "x").unwrap();
        // Same path on both sides — must not delete or recreate.
        assert!(migrate_legacy_config_dir_impl(&path, &path).is_ok());
        assert!(path.join("config.toml").exists());
    }

    #[test]
    fn migrate_legacy_config_dir_moves_when_only_legacy_exists() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let new = dir.path().join("new_parent").join("dot-agent-deck");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.toml"), "hi").unwrap();
        std::fs::write(legacy.join("bookmarked-sessions.json"), "[]").unwrap();

        assert!(migrate_legacy_config_dir_impl(&legacy, &new).is_ok());
        // Legacy gone, new populated, parent created on the fly.
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "hi"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("bookmarked-sessions.json")).unwrap(),
            "[]"
        );
    }

    #[test]
    fn migrate_legacy_config_dir_does_not_clobber_existing_new() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let new = dir.path().join("new");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.toml"), "old").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("config.toml"), "current").unwrap();

        assert!(migrate_legacy_config_dir_impl(&legacy, &new).is_ok());
        // Both dirs preserved; new untouched; user resolves manually.
        assert_eq!(
            std::fs::read_to_string(legacy.join("config.toml")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "current"
        );
    }

    #[test]
    fn migrate_legacy_config_dir_is_idempotent_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let new = dir.path().join("new");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.toml"), "x").unwrap();

        assert!(migrate_legacy_config_dir_impl(&legacy, &new).is_ok());
        assert!(new.exists() && !legacy.exists());
        // Second call: legacy is gone → no-op, no panic.
        assert!(migrate_legacy_config_dir_impl(&legacy, &new).is_ok());
        assert!(new.exists());
    }

    #[test]
    fn saved_pane_loads_old_format_without_resume_fields() {
        // Workspaces saved before this feature shipped don't have the
        // two new fields. Backward-compat is non-negotiable; otherwise
        // every user's existing workspaces would fail to load.
        let old = r#"
[[panes]]
dir = "/repo"
name = "api"
command = "copilot"
"#;
        let loaded: SavedSession = toml::from_str(old).unwrap();
        assert_eq!(loaded.panes.len(), 1);
        assert!(loaded.panes[0].session_id.is_none());
        assert!(loaded.panes[0].agent_type.is_none());
    }

    #[test]
    fn snapshot_populates_resume_metadata_from_live_session() {
        let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
        pane_metadata.insert(
            "1".to_string(),
            SavedPane {
                dir: "/repo".to_string(),
                name: "api".to_string(),
                command: "copilot".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            },
        );
        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        sessions.insert(
            "real-session".to_string(),
            make_session("real-session", "1", AgentType::CopilotCli, 5),
        );
        let live: HashSet<String> = ["1".to_string()].into_iter().collect();
        let display: HashMap<String, String> = HashMap::new();

        let session = SavedSession::snapshot(&mut pane_metadata, &display, &live, &sessions);

        assert_eq!(session.panes.len(), 1);
        assert_eq!(session.panes[0].session_id.as_deref(), Some("real-session"));
        assert_eq!(
            session.panes[0].agent_type.as_ref(),
            Some(&AgentType::CopilotCli)
        );
    }

    #[test]
    fn snapshot_picks_most_recently_active_session_for_pane() {
        // Real-world failure mode: user runs Copilot in a pane, exits
        // (without sessionEnd), then starts Claude in the same pane.
        // Both SessionStates linger in `sessions`, both with the same
        // pane_id. The save MUST record the newer one or restore brings
        // back the wrong agent.
        let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
        pane_metadata.insert(
            "1".to_string(),
            SavedPane {
                dir: "/repo".to_string(),
                name: "p".to_string(),
                command: "claude".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            },
        );
        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        sessions.insert(
            "old-copilot".to_string(),
            make_session("old-copilot", "1", AgentType::CopilotCli, 300),
        );
        sessions.insert(
            "new-claude".to_string(),
            make_session("new-claude", "1", AgentType::ClaudeCode, 10),
        );
        let live: HashSet<String> = ["1".to_string()].into_iter().collect();
        let display: HashMap<String, String> = HashMap::new();

        let session = SavedSession::snapshot(&mut pane_metadata, &display, &live, &sessions);

        assert_eq!(session.panes[0].session_id.as_deref(), Some("new-claude"));
        assert_eq!(
            session.panes[0].agent_type.as_ref(),
            Some(&AgentType::ClaudeCode)
        );
    }

    #[test]
    fn snapshot_preserves_existing_metadata_when_no_live_session() {
        // The seed snapshot runs immediately after restore, before the
        // freshly-spawned agent has emitted SessionStart. If we cleared
        // here, an early exit would wipe the resume id the user just
        // restored from disk. So we preserve unless a live session
        // explicitly overrides — accepting the same "id may go stale"
        // tradeoff bookmarks already have.
        let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
        pane_metadata.insert(
            "1".to_string(),
            SavedPane {
                dir: "/repo".to_string(),
                name: "p".to_string(),
                command: "copilot".to_string(),
                mode: None,
                session_id: Some("preserved".to_string()),
                agent_type: Some(AgentType::CopilotCli),
            },
        );
        let sessions: HashMap<String, SessionState> = HashMap::new();
        let live: HashSet<String> = ["1".to_string()].into_iter().collect();
        let display: HashMap<String, String> = HashMap::new();

        let session = SavedSession::snapshot(&mut pane_metadata, &display, &live, &sessions);

        assert_eq!(
            session.panes[0].session_id.as_deref(),
            Some("preserved"),
            "session_id must survive when no live session is bound"
        );
        assert_eq!(
            session.panes[0].agent_type.as_ref(),
            Some(&AgentType::CopilotCli)
        );
    }

    #[test]
    fn snapshot_ignores_placeholder_sessions() {
        // Placeholders carry agent_type=None AND a session_id shaped
        // like "pane-<id>" (see `placeholder_session_id`). The snapshot
        // must skip them so workspaces don't try to resume `--resume
        // pane-1`, which no agent CLI recognises.
        let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
        pane_metadata.insert(
            "1".to_string(),
            SavedPane {
                dir: "/repo".to_string(),
                name: "p".to_string(),
                command: "copilot".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            },
        );
        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        sessions.insert(
            "pane-1".to_string(),
            make_session("pane-1", "1", AgentType::None, 1),
        );
        let live: HashSet<String> = ["1".to_string()].into_iter().collect();
        let display: HashMap<String, String> = HashMap::new();

        let session = SavedSession::snapshot(&mut pane_metadata, &display, &live, &sessions);

        assert!(session.panes[0].session_id.is_none());
        assert!(session.panes[0].agent_type.is_none());
    }

    #[test]
    fn snapshot_captures_live_id_after_real_to_real_restart() {
        // After `apply_event` collapses a real→real session restart,
        // `sessions[OLD_KEY].session_id == NEW_LIVE_ID`. The snapshot
        // must read the field, not the map key, so the restored
        // command resumes the live conversation rather than the
        // original (now-stale) one.
        let mut pane_metadata: HashMap<String, SavedPane> = HashMap::new();
        pane_metadata.insert(
            "1".to_string(),
            SavedPane {
                dir: "/repo".to_string(),
                name: "p".to_string(),
                command: "copilot".to_string(),
                mode: None,
                session_id: None,
                agent_type: None,
            },
        );
        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        let mut s = make_session("OLD_KEY", "1", AgentType::CopilotCli, 5);
        // Diverged field — simulates the apply_event post-restart state.
        s.session_id = "NEW_LIVE_ID".to_string();
        sessions.insert("OLD_KEY".to_string(), s);
        let live: HashSet<String> = ["1".to_string()].into_iter().collect();
        let display: HashMap<String, String> = HashMap::new();

        let session = SavedSession::snapshot(&mut pane_metadata, &display, &live, &sessions);

        assert_eq!(
            session.panes[0].session_id.as_deref(),
            Some("NEW_LIVE_ID"),
            "snapshot must read SessionState.session_id field, not the map key"
        );
    }
}
