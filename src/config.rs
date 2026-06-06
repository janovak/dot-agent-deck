use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::AgentType;
use crate::state::{SessionState, SessionStatus, is_placeholder_session_id};
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
        "Seconds in Working before card flips to Pending (default: 30, set to 0 to disable)",
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
            // causing visible Pending → Working flicker. 30 s comfortably
            // exceeds typical LLM silence between tool calls while still
            // catching true stalls within half a minute. Users can tune
            // back down with `pending.timeout_seconds` or disable the
            // heuristic with `0`.
            timeout_seconds: 30,
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
            let matching = sessions
                .values()
                .filter(|s| {
                    s.pane_id.as_deref() == Some(id.as_str())
                        && s.agent_type != AgentType::None
                        && !is_placeholder_session_id(&s.session_id)
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

/// Returns `true` if `command` is a bare invocation of the binary for
/// `agent`, with no arguments other than an optional `--resume <id>`
/// pair. Path-aware: a fully-qualified `C:\…\copilot.exe` is accepted.
///
/// This is the gate workspace-restore uses before rewriting a command
/// to inject `--resume <session_id>`. Anything more complex (custom
/// flags, wrappers like `npx copilot` or `cmd /c copilot`, quoted args,
/// pipelines) is left untouched so we don't silently corrupt a power
/// user's command line.
fn is_simple_agent_invocation(command: &str, agent: &AgentType) -> bool {
    let stripped = strip_resume_flag(command);
    let trimmed = stripped.trim();
    let mut tokens = trimmed.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    if tokens.next().is_some() {
        // More than one token after --resume stripping → custom args
        // we can't safely round-trip.
        return false;
    }
    let basename = std::path::Path::new(first)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(first);
    let expected: &[&str] = match agent {
        AgentType::CopilotCli => &["copilot"],
        AgentType::ClaudeCode => &["claude"],
        AgentType::OpenCode | AgentType::None => return false,
    };
    expected.iter().any(|n| basename.eq_ignore_ascii_case(n))
}

/// Returns the launch command rewritten to resume the given session,
/// when safe to do so. Falls back to `base` unchanged when:
///   - `session_id` / `agent` are missing (no resume info recorded);
///   - the agent doesn't support `--resume` (OpenCode, None);
///   - `base` isn't a simple agent invocation (see
///     `is_simple_agent_invocation` — quoted args, wrappers, etc.);
///   - `session_id` contains characters outside `[A-Za-z0-9_-]`.
///     Real Copilot/Claude ids are UUID-shaped, so anything else
///     is treated as corrupt and skipped rather than fed into the
///     PTY where it would be interpreted by the user's shell.
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

pub(crate) fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `DOT_AGENT_DECK_WORKSPACES`. Cargo runs
    /// unit tests in parallel and they otherwise race on the shared env var.
    static WORKSPACES_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn pending_timeout_seconds_default_is_30() {
        let cfg = PendingConfig::default();
        assert_eq!(cfg.timeout_seconds, 30);
    }

    #[test]
    fn pending_timeout_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("pending.timeout_seconds").unwrap(), "30");
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
            pending_strikes: 0,
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
    fn is_simple_invocation_rejects_custom_flags() {
        assert!(!is_simple_agent_invocation(
            "copilot --model gpt-5",
            &AgentType::CopilotCli
        ));
        assert!(!is_simple_agent_invocation(
            "claude --print",
            &AgentType::ClaudeCode
        ));
    }

    #[test]
    fn is_simple_invocation_rejects_wrappers() {
        assert!(!is_simple_agent_invocation(
            "npx copilot",
            &AgentType::CopilotCli
        ));
        assert!(!is_simple_agent_invocation(
            "cmd /c copilot",
            &AgentType::CopilotCli
        ));
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
        let out = build_resume_command("copilot", Some("abc-123"), Some(&AgentType::CopilotCli));
        assert_eq!(out, "copilot --resume abc-123");
    }

    #[test]
    fn build_resume_command_happy_path_claude() {
        let out = build_resume_command("claude", Some("def-456"), Some(&AgentType::ClaudeCode));
        assert_eq!(out, "claude --resume def-456");
    }

    #[test]
    fn build_resume_command_round_trip_is_idempotent() {
        // Restore writes `copilot --resume X`; on the next save+restore
        // we must end up with the new id rather than two flags.
        let first =
            build_resume_command("copilot", Some("session-1"), Some(&AgentType::CopilotCli));
        assert_eq!(first, "copilot --resume session-1");
        let second = build_resume_command(&first, Some("session-2"), Some(&AgentType::CopilotCli));
        assert_eq!(second, "copilot --resume session-2");
    }

    #[test]
    fn build_resume_command_passes_through_complex() {
        // Custom flags → don't touch.
        let cmd = "copilot --model gpt-5";
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
        assert_eq!(
            build_resume_command(cmd, Some("abc-123_DEF-456"), Some(&AgentType::CopilotCli)),
            "copilot --resume abc-123_DEF-456"
        );
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
