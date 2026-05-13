//! Install / uninstall dot-agent-deck hooks for GitHub Copilot CLI.
//!
//! Copilot CLI reads any `*.json` file under `~/.copilot/hooks/` and merges
//! the declared lifecycle hooks. Each file is owned by a single integration,
//! so dot-agent-deck installs its hooks at `~/.copilot/hooks/dot-agent-deck.json`
//! and never touches files belonging to other integrations (e.g. constellation).
//!
//! Hook command form: `dot-agent-deck hook --agent copilot-cli --event <name>`.
//! Copilot CLI fires the same event-name argument that the hook config
//! declares (e.g. `sessionStart`, `preToolUse`), passing the JSON payload on
//! stdin. The bash form relies on stdin inheritance; the PowerShell form
//! explicitly pipes `$input` so stdin propagates.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// All Copilot CLI lifecycle events dot-agent-deck listens to.
const HOOK_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "userPromptSubmitted",
    "preToolUse",
    "postToolUse",
    "errorOccurred",
    "agentStop",
    "subagentStart",
    "subagentStop",
    "preCompact",
];

/// Default per-hook timeout. `preToolUse` gets a longer one because Copilot
/// CLI waits on its response (the constellation hook uses 120s for the same
/// reason); all others are fire-and-forget on the Copilot side.
const TIMEOUT_DEFAULT_SEC: u32 = 5;
const TIMEOUT_PRE_TOOL_USE_SEC: u32 = 120;

const CONFIG_FILE_NAME: &str = "dot-agent-deck.json";

fn home_dir() -> PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

fn copilot_hooks_dir() -> PathBuf {
    home_dir().join(".copilot").join("hooks")
}

fn config_path() -> PathBuf {
    copilot_hooks_dir().join(CONFIG_FILE_NAME)
}

fn current_binary_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "dot-agent-deck".to_string())
}

fn build_hook_entry(binary_path: &str, event_name: &str) -> Value {
    let timeout = if event_name == "preToolUse" {
        TIMEOUT_PRE_TOOL_USE_SEC
    } else {
        TIMEOUT_DEFAULT_SEC
    };
    // bash form: stdin is inherited automatically.
    // powershell form: `$input |` explicitly forwards the JSON payload that
    // Copilot CLI writes to the hook script's stdin.
    let bash = format!("\"{binary_path}\" hook --agent copilot-cli --event {event_name}");
    let powershell =
        format!("$input | & \"{binary_path}\" hook --agent copilot-cli --event {event_name}");
    json!({
        "type": "command",
        "bash": bash,
        "powershell": powershell,
        "cwd": ".",
        "timeoutSec": timeout,
        "comment": "dot-agent-deck monitoring hook"
    })
}

fn build_config(binary_path: &str) -> Value {
    let mut hooks_map = serde_json::Map::new();
    for event in HOOK_EVENTS {
        hooks_map.insert(
            (*event).to_string(),
            Value::Array(vec![build_hook_entry(binary_path, event)]),
        );
    }
    json!({
        "version": 1,
        "hooks": Value::Object(hooks_map)
    })
}

fn write_config_to(path: &Path, binary_path: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config = build_config(binary_path);
    let contents = serde_json::to_string_pretty(&config)?;
    std::fs::write(path, contents)
}

/// `dot-agent-deck hooks install --agent copilot-cli`: write the hook config,
/// creating `~/.copilot/hooks/` if necessary.
pub fn install() -> std::io::Result<()> {
    let path = config_path();
    write_config_to(&path, &current_binary_path())?;
    println!("Installed Copilot CLI hooks at {}", path.display());
    Ok(())
}

/// `dot-agent-deck hooks uninstall --agent copilot-cli`: remove the hook
/// config file (no-op if missing).
pub fn uninstall() -> std::io::Result<()> {
    let path = config_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("Removed Copilot CLI hooks config at {}", path.display());
    } else {
        println!(
            "No Copilot CLI hooks config to remove at {}",
            path.display()
        );
    }
    Ok(())
}

/// Silent auto-install on dashboard startup. Skips if `~/.copilot/hooks/`
/// doesn't exist (i.e. Copilot CLI not installed) or if the config file is
/// already in place — preserves any other integrations' files untouched.
pub fn auto_install() {
    let hooks_dir = copilot_hooks_dir();
    if !hooks_dir.exists() {
        return;
    }
    let path = config_path();
    if path.exists() {
        return;
    }
    let binary_path = current_binary_path();
    if let Err(e) = write_config_to(&path, &binary_path) {
        tracing::warn!("auto-install: failed to write Copilot CLI hooks: {e}");
        return;
    }
    tracing::info!("auto-installed Copilot CLI hooks at {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn build_config_contains_all_events() {
        let cfg = build_config("/usr/local/bin/dot-agent-deck");
        let hooks = cfg.get("hooks").unwrap().as_object().unwrap();
        for ev in HOOK_EVENTS {
            assert!(hooks.contains_key(*ev), "missing event: {ev}");
            assert!(hooks[*ev].is_array(), "{ev} entry must be an array");
        }
        assert_eq!(cfg["version"], json!(1));
    }

    #[test]
    fn build_hook_entry_carries_binary_path_and_event() {
        let entry = build_hook_entry("/path/to/bin", "preToolUse");
        let bash = entry["bash"].as_str().unwrap();
        assert!(bash.contains("/path/to/bin"));
        assert!(bash.contains("--agent copilot-cli"));
        assert!(bash.contains("--event preToolUse"));
        let ps = entry["powershell"].as_str().unwrap();
        assert!(ps.starts_with("$input |"));
        assert!(ps.contains("--event preToolUse"));
        assert_eq!(entry["type"], "command");
    }

    #[test]
    fn pre_tool_use_uses_long_timeout() {
        let pre = build_hook_entry("/bin/dad", "preToolUse");
        let other = build_hook_entry("/bin/dad", "sessionStart");
        assert_eq!(pre["timeoutSec"], json!(TIMEOUT_PRE_TOOL_USE_SEC));
        assert_eq!(other["timeoutSec"], json!(TIMEOUT_DEFAULT_SEC));
    }

    #[test]
    fn write_config_creates_file_and_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dot-agent-deck.json");
        write_config_to(&path, "/bin/dad").unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["version"], json!(1));
        assert!(
            parsed["hooks"]["sessionStart"][0]["bash"]
                .as_str()
                .unwrap()
                .contains("/bin/dad")
        );
    }

    #[test]
    fn config_path_lives_under_dot_copilot_hooks() {
        let path = config_path();
        let s = path.to_string_lossy();
        assert!(s.contains(".copilot"));
        assert!(s.contains("hooks"));
        assert!(s.ends_with("dot-agent-deck.json"));
    }
}
