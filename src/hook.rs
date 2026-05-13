use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::process::ExitCode;

use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use crate::config::socket_path;
use crate::event::{AgentEvent, AgentType, EventType};
use crate::ipc;

#[derive(Debug, Deserialize)]
struct ClaudeCodeHookInput {
    session_id: String,
    hook_event_name: String,
    cwd: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
    prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeHookInput {
    session_id: String,
    event: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    status: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct CopilotCliHookInput {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "toolName")]
    tool_name: Option<String>,
    #[serde(rename = "toolArgs")]
    tool_args: Option<Value>,
    reason: Option<String>,
    prompt: Option<String>,
    #[serde(rename = "userPrompt")]
    user_prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

pub fn handle_hook(agent: &str, event_name: Option<&str>) -> ExitCode {
    let input = match read_stdin() {
        Some(s) if !s.is_empty() => s,
        _ => return ExitCode::SUCCESS,
    };

    let event = match agent {
        "opencode" => {
            let hook_input: OpenCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_opencode_event(hook_input)
        }
        "copilot-cli" => {
            // Copilot CLI passes the event name as argv to the hook script
            // (not inside the JSON payload).
            let event_name = match event_name {
                Some(e) => e,
                None => return ExitCode::SUCCESS,
            };
            let hook_input: CopilotCliHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_copilot_event(event_name, hook_input)
        }
        _ => {
            let hook_input: ClaudeCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_event(hook_input)
        }
    };

    let event = match event {
        Some(e) => e,
        None => return ExitCode::SUCCESS,
    };

    let json = match serde_json::to_string(&event) {
        Ok(j) => j,
        Err(_) => return ExitCode::SUCCESS,
    };

    let _ = send_to_socket(&json);
    ExitCode::SUCCESS
}

fn read_stdin() -> Option<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn map_event_type(hook_event_name: &str) -> Option<EventType> {
    match hook_event_name {
        "SessionStart" => Some(EventType::SessionStart),
        "SessionEnd" => Some(EventType::SessionEnd),
        "UserPromptSubmit" => Some(EventType::Thinking),
        "PreToolUse" => Some(EventType::ToolStart),
        "PostToolUse" => Some(EventType::ToolEnd),
        "Notification" => Some(EventType::WaitingForInput),
        "PermissionRequest" => Some(EventType::PermissionRequest),
        "Stop" => Some(EventType::Idle),
        "StopFailure" => Some(EventType::Error),
        "PreCompact" => Some(EventType::Compacting),
        "PostCompact" => Some(EventType::Thinking),
        "SubagentStart" => Some(EventType::SubagentStart),
        "SubagentStop" => Some(EventType::SubagentStop),
        _ => None,
    }
}

fn extract_tool_detail(tool_name: Option<&str>, tool_input: Option<&Value>) -> Option<String> {
    let input = tool_input?.as_object()?;
    let detail = match tool_name? {
        "Bash" => {
            let cmd = input.get("command")?.as_str()?;
            let first_line = cmd.lines().next().unwrap_or(cmd);
            truncate(first_line, 120)
        }
        "Read" | "Edit" | "Write" => input.get("file_path")?.as_str()?.to_string(),
        "Grep" | "Glob" => input.get("pattern")?.as_str()?.to_string(),
        "Agent" => input.get("description")?.as_str()?.to_string(),
        _ => {
            // First string-valued key
            let val = input.values().find_map(|v| v.as_str())?;
            truncate(val, 80)
        }
    };
    Some(detail)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn build_event(input: ClaudeCodeHookInput) -> Option<AgentEvent> {
    let ClaudeCodeHookInput {
        session_id,
        hook_event_name,
        cwd,
        tool_name,
        tool_input,
        tool_use_id,
        prompt,
        _extra: _,
    } = input;

    let event_type = map_event_type(&hook_event_name)?;
    let tool_detail = extract_tool_detail(tool_name.as_deref(), tool_input.as_ref());

    let user_prompt = prompt.map(|p| truncate(&p, 200));
    let pane_id = std::env::var("DOT_AGENT_DECK_PANE_ID").ok();

    let mut metadata = HashMap::new();
    if let Some(tool_use_id) = tool_use_id {
        metadata.insert("tool_use_id".to_string(), tool_use_id);
    }

    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && tool_name.as_deref() == Some("Bash")
        && let Some(ref input) = tool_input
        && let Some(cmd) = input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    Some(AgentEvent {
        session_id,
        agent_type: AgentType::ClaudeCode,
        event_type,
        tool_name,
        tool_detail,
        cwd,
        timestamp: Utc::now(),
        user_prompt,
        metadata,
        pane_id,
    })
}

fn map_opencode_event_type(event: &str, status: Option<&str>) -> Option<EventType> {
    match event {
        "session.created" => Some(EventType::SessionStart),
        "session.deleted" => Some(EventType::SessionEnd),
        "session.idle" => Some(EventType::Idle),
        "session.error" => Some(EventType::Error),
        "session.prompt" => Some(EventType::Thinking),
        "session.status" | "session.status.updated" => {
            let norm = status.map(|s| s.to_ascii_lowercase());
            match norm.as_deref() {
                Some("idle") => Some(EventType::Idle),
                Some("error") => Some(EventType::Error),
                Some("waiting") => Some(EventType::WaitingForInput),
                _ => Some(EventType::Thinking),
            }
        }
        "tool.execute.before" => Some(EventType::ToolStart),
        "tool.execute.after" => Some(EventType::ToolEnd),
        "permission.asked" => Some(EventType::PermissionRequest),
        "permission.replied" => Some(EventType::Thinking),
        _ => None,
    }
}

fn build_opencode_event(input: OpenCodeHookInput) -> Option<AgentEvent> {
    let event_type = map_opencode_event_type(&input.event, input.status.as_deref())?;
    let tool_detail = extract_tool_detail(input.tool_name.as_deref(), input.tool_input.as_ref());
    let user_prompt = input.prompt.map(|p| truncate(&p, 200));
    let pane_id = std::env::var("DOT_AGENT_DECK_PANE_ID").ok();

    let mut metadata = HashMap::new();
    if matches!(event_type, EventType::PermissionRequest) {
        metadata.insert("permission_state".to_string(), "pending".to_string());
        metadata.insert(
            "tool_use_id".to_string(),
            format!(
                "perm-{}-{}",
                input.session_id,
                Utc::now().timestamp_millis()
            ),
        );
    }

    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && input.tool_name.as_deref() == Some("Bash")
        && let Some(ref tool_input) = input.tool_input
        && let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    Some(AgentEvent {
        session_id: input.session_id,
        agent_type: AgentType::OpenCode,
        event_type,
        tool_name: input.tool_name,
        tool_detail,
        cwd: input.cwd,
        timestamp: Utc::now(),
        user_prompt,
        metadata,
        pane_id,
    })
}

pub fn send_to_socket(json: &str) -> Option<()> {
    let path = socket_path();
    let mut stream = ipc::connect_sync(&path).ok()?;
    let msg = format!("{json}\n");
    stream.write_all(msg.as_bytes()).ok()?;
    stream.flush().ok()?;
    Some(())
}

// ---------------------------------------------------------------------------
// GitHub Copilot CLI hook integration
// ---------------------------------------------------------------------------
//
// Copilot CLI fires hooks declared in `~/.copilot/hooks/*.json` and passes the
// event name as the *command argument*, not in the JSON payload (cf. the
// constellation integration). The stdin payload uses camelCase field names
// (`sessionId`, `toolName`, `toolArgs`, etc.).
//
// `dot-agent-deck hook --agent copilot-cli --event <eventName>` decodes the
// payload and forwards a normalised `AgentEvent` to the daemon.

fn map_copilot_event_type(hook_event_name: &str, reason: Option<&str>) -> Option<EventType> {
    match hook_event_name {
        "sessionStart" => Some(EventType::SessionStart),
        // Copilot fires `sessionEnd` with `reason: "complete"` at the end of
        // every conversation turn (not actually session end). Treat that as
        // `Idle`; treat other reasons (or absent reason) as a true session end.
        "sessionEnd" => {
            if reason == Some("complete") {
                Some(EventType::Idle)
            } else {
                Some(EventType::SessionEnd)
            }
        }
        "userPromptSubmitted" => Some(EventType::Thinking),
        "preToolUse" => Some(EventType::ToolStart),
        "postToolUse" => Some(EventType::ToolEnd),
        "errorOccurred" => Some(EventType::Error),
        "agentStop" => Some(EventType::Idle),
        "subagentStart" => Some(EventType::SubagentStart),
        "subagentStop" => Some(EventType::SubagentStop),
        "preCompact" => Some(EventType::Compacting),
        _ => None,
    }
}

fn extract_copilot_tool_detail(
    tool_name: Option<&str>,
    tool_args: Option<&Value>,
) -> Option<String> {
    let input = tool_args?.as_object()?;
    let detail = match tool_name? {
        "shell" | "bash" | "Bash" => {
            let cmd = input.get("command")?.as_str()?;
            let first_line = cmd.lines().next().unwrap_or(cmd);
            truncate(first_line, 120)
        }
        "str-replace-editor" | "str_replace_editor" | "edit" | "view" | "create" | "write" => input
            .get("path")
            .or_else(|| input.get("file_path"))?
            .as_str()?
            .to_string(),
        "grep" | "glob" => input.get("pattern")?.as_str()?.to_string(),
        _ => {
            let val = input.values().find_map(|v| v.as_str())?;
            truncate(val, 80)
        }
    };
    Some(detail)
}

fn build_copilot_event(event_name: &str, input: CopilotCliHookInput) -> Option<AgentEvent> {
    let CopilotCliHookInput {
        session_id,
        cwd,
        tool_name,
        tool_args,
        reason,
        prompt,
        user_prompt,
        _extra: _,
    } = input;

    let event_type = map_copilot_event_type(event_name, reason.as_deref())?;

    // Older Copilot CLI versions may omit `sessionId`; synthesise a stable
    // per-pane fallback so the dashboard still has *something* to key on.
    let session_id = session_id.unwrap_or_else(|| {
        std::env::var("DOT_AGENT_DECK_PANE_ID")
            .map(|pid| format!("copilot-pane-{pid}"))
            .unwrap_or_else(|_| format!("copilot-{}", Utc::now().timestamp_millis()))
    });

    let tool_detail = extract_copilot_tool_detail(tool_name.as_deref(), tool_args.as_ref());
    let user_prompt_text = prompt.or(user_prompt).map(|p| truncate(&p, 200));
    let pane_id = std::env::var("DOT_AGENT_DECK_PANE_ID").ok();

    let mut metadata = HashMap::new();
    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && let Some(tname) = tool_name.as_deref()
        && matches!(tname, "shell" | "bash" | "Bash")
        && let Some(ref args) = tool_args
        && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    Some(AgentEvent {
        session_id,
        agent_type: AgentType::CopilotCli,
        event_type,
        tool_name,
        tool_detail,
        cwd,
        timestamp: Utc::now(),
        user_prompt: user_prompt_text,
        metadata,
        pane_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_session_start() {
        assert_eq!(
            map_event_type("SessionStart"),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_pre_tool_use() {
        assert_eq!(map_event_type("PreToolUse"), Some(EventType::ToolStart));
    }

    #[test]
    fn map_post_tool_use() {
        assert_eq!(map_event_type("PostToolUse"), Some(EventType::ToolEnd));
    }

    #[test]
    fn map_notification() {
        assert_eq!(
            map_event_type("Notification"),
            Some(EventType::WaitingForInput)
        );
    }

    #[test]
    fn map_permission_request() {
        assert_eq!(
            map_event_type("PermissionRequest"),
            Some(EventType::PermissionRequest)
        );
    }

    #[test]
    fn map_stop() {
        assert_eq!(map_event_type("Stop"), Some(EventType::Idle));
    }

    #[test]
    fn map_session_end() {
        assert_eq!(map_event_type("SessionEnd"), Some(EventType::SessionEnd));
    }

    #[test]
    fn map_unknown_returns_none() {
        assert_eq!(map_event_type("SomethingElse"), None);
    }

    #[test]
    fn tool_detail_bash_command() {
        let input: Value = serde_json::json!({"command": "ls -la\necho hello"});
        let detail = extract_tool_detail(Some("Bash"), Some(&input));
        assert_eq!(detail.as_deref(), Some("ls -la"));
    }

    #[test]
    fn tool_detail_bash_truncates_long_command() {
        let long_cmd = "x".repeat(200);
        let input: Value = serde_json::json!({"command": long_cmd});
        let detail = extract_tool_detail(Some("Bash"), Some(&input)).unwrap();
        assert!(detail.len() <= 124); // 120 + "…" (3 bytes)
    }

    #[test]
    fn tool_detail_read_file_path() {
        let input: Value = serde_json::json!({"file_path": "/src/main.rs"});
        let detail = extract_tool_detail(Some("Read"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn tool_detail_edit_file_path() {
        let input: Value =
            serde_json::json!({"file_path": "/src/lib.rs", "old_string": "a", "new_string": "b"});
        let detail = extract_tool_detail(Some("Edit"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/lib.rs"));
    }

    #[test]
    fn tool_detail_grep_pattern() {
        let input: Value = serde_json::json!({"pattern": "fn main"});
        let detail = extract_tool_detail(Some("Grep"), Some(&input));
        assert_eq!(detail.as_deref(), Some("fn main"));
    }

    #[test]
    fn tool_detail_glob_pattern() {
        let input: Value = serde_json::json!({"pattern": "**/*.rs"});
        let detail = extract_tool_detail(Some("Glob"), Some(&input));
        assert_eq!(detail.as_deref(), Some("**/*.rs"));
    }

    #[test]
    fn tool_detail_agent_description() {
        let input: Value = serde_json::json!({"description": "explore codebase"});
        let detail = extract_tool_detail(Some("Agent"), Some(&input));
        assert_eq!(detail.as_deref(), Some("explore codebase"));
    }

    #[test]
    fn tool_detail_unknown_tool_uses_first_string() {
        let input: Value = serde_json::json!({"query": "SELECT 1", "timeout": 30});
        let detail = extract_tool_detail(Some("SQL"), Some(&input));
        assert_eq!(detail.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn tool_detail_none_when_no_input() {
        let detail = extract_tool_detail(Some("Bash"), None);
        assert!(detail.is_none());
    }

    #[test]
    fn tool_detail_none_when_no_tool_name() {
        let input: Value = serde_json::json!({"command": "ls"});
        let detail = extract_tool_detail(None, Some(&input));
        assert!(detail.is_none());
    }

    #[test]
    fn build_event_session_start() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "SessionStart".into(),
            cwd: Some("/tmp".into()),
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.session_id, "test-123");
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("/tmp"));
        assert!(event.tool_name.is_none());
        assert!(event.user_prompt.is_none());
    }

    #[test]
    fn build_event_tool_start_with_detail() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "/src/main.rs"})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Read"));
        assert_eq!(event.tool_detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn build_event_unknown_hook_returns_none() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UnknownHook".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        assert!(build_event(input).is_none());
    }

    #[test]
    fn build_event_user_prompt_submit_extracts_prompt() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UserPromptSubmit".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: Some("fix the login bug".into()),
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::Thinking);
        assert_eq!(event.user_prompt.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn build_event_prompt_truncated_to_200() {
        let long_prompt = "x".repeat(300);
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UserPromptSubmit".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: Some(long_prompt),
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        let prompt = event.user_prompt.unwrap();
        assert!(prompt.len() <= 204); // 200 + "…" (3 bytes)
        assert!(prompt.ends_with('…'));
    }

    #[test]
    fn send_to_missing_socket_returns_none() {
        // With no daemon running, send should silently fail
        // SAFETY: This test runs single-threaded; no other thread reads this env var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SOCKET", "/tmp/nonexistent-test-socket.sock");
        }
        let result = send_to_socket(r#"{"test": true}"#);
        assert!(result.is_none());
    }

    #[test]
    fn deserialize_claude_code_hook_input() {
        let json = r#"{
            "session_id": "abc-123",
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "source": "claude_code"
        }"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "abc-123");
        assert_eq!(input.hook_event_name, "PreToolUse");
        assert_eq!(input.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn deserialize_minimal_hook_input() {
        let json = r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionStart"
        }"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "abc-123");
        assert!(input.cwd.is_none());
        assert!(input.tool_name.is_none());
        assert!(input.tool_input.is_none());
    }

    // --- OpenCode tests ---

    #[test]
    fn map_opencode_session_created() {
        assert_eq!(
            map_opencode_event_type("session.created", None),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_opencode_session_deleted() {
        assert_eq!(
            map_opencode_event_type("session.deleted", None),
            Some(EventType::SessionEnd)
        );
    }

    #[test]
    fn map_opencode_session_idle() {
        assert_eq!(
            map_opencode_event_type("session.idle", None),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_opencode_session_error() {
        assert_eq!(
            map_opencode_event_type("session.error", None),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_opencode_session_status_default() {
        assert_eq!(
            map_opencode_event_type("session.status", None),
            Some(EventType::Thinking)
        );
        assert_eq!(
            map_opencode_event_type("session.status", Some("busy")),
            Some(EventType::Thinking)
        );
        assert_eq!(
            map_opencode_event_type("session.status.updated", Some("retry")),
            Some(EventType::Thinking)
        );
    }

    #[test]
    fn map_opencode_session_status_idle() {
        assert_eq!(
            map_opencode_event_type("session.status", Some("idle")),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_opencode_permission_asked() {
        assert_eq!(
            map_opencode_event_type("permission.asked", None),
            Some(EventType::PermissionRequest)
        );
    }

    #[test]
    fn map_opencode_session_status_error() {
        assert_eq!(
            map_opencode_event_type("session.status", Some("error")),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_opencode_tool_before() {
        assert_eq!(
            map_opencode_event_type("tool.execute.before", None),
            Some(EventType::ToolStart)
        );
    }

    #[test]
    fn map_opencode_tool_after() {
        assert_eq!(
            map_opencode_event_type("tool.execute.after", None),
            Some(EventType::ToolEnd)
        );
    }

    #[test]
    fn map_opencode_unknown_returns_none() {
        assert_eq!(map_opencode_event_type("unknown.event", None), None);
    }

    #[test]
    fn build_opencode_event_session_created() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "session.created".into(),
            tool_name: None,
            tool_input: None,
            status: None,
            cwd: Some("/tmp".into()),
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.session_id, "oc-123");
        assert_eq!(event.agent_type, AgentType::OpenCode);
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn build_opencode_event_tool_with_detail() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "tool.execute.before".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo build"})),
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_detail.as_deref(), Some("cargo build"));
    }

    #[test]
    fn build_opencode_event_unknown_returns_none() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "unknown.event".into(),
            tool_name: None,
            tool_input: None,
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        assert!(build_opencode_event(input).is_none());
    }

    #[test]
    fn deserialize_opencode_hook_input() {
        let json = r#"{
            "session_id": "oc-456",
            "event": "tool.execute.before",
            "tool_name": "Read",
            "tool_input": {"file_path": "/src/main.rs"},
            "cwd": "/home/user",
            "extra_field": "ignored"
        }"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "oc-456");
        assert_eq!(input.event, "tool.execute.before");
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
        assert!(input.status.is_none());
    }

    #[test]
    fn deserialize_minimal_opencode_input() {
        let json = r#"{
            "session_id": "oc-456",
            "event": "session.created"
        }"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "oc-456");
        assert!(input.tool_name.is_none());
        assert!(input.status.is_none());
        assert!(input.cwd.is_none());
    }

    /// Serialize env-var-mutating tests to avoid races.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn pane_id_propagated_from_env_claude_code() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let key = "DOT_AGENT_DECK_PANE_ID";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "pane-42") };

        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "SessionStart".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.pane_id.as_deref(), Some("pane-42"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn pane_id_propagated_from_env_opencode() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let key = "DOT_AGENT_DECK_PANE_ID";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "pane-99") };

        let input = OpenCodeHookInput {
            session_id: "oc-1".into(),
            event: "session.created".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            prompt: None,
            status: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.pane_id.as_deref(), Some("pane-99"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn build_event_bash_tool_start_stores_full_command() {
        let full_cmd = "kubectl get pods -n production\nkubectl get svc -n production";
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": full_cmd})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
        // tool_detail should only have the first line (truncated)
        assert_eq!(
            event.tool_detail.as_deref(),
            Some("kubectl get pods -n production"),
        );
    }

    #[test]
    fn build_event_non_bash_tool_start_no_bash_command() {
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "/src/main.rs"})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert!(!event.metadata.contains_key("bash_command"));
    }

    #[test]
    fn build_event_bash_tool_end_no_bash_command() {
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PostToolUse".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            tool_use_id: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        assert!(!event.metadata.contains_key("bash_command"));
    }

    #[test]
    fn build_opencode_event_bash_tool_start_stores_full_command() {
        let full_cmd = "helm status my-release --namespace prod";
        let input = OpenCodeHookInput {
            session_id: "oc-1".into(),
            event: "tool.execute.before".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": full_cmd})),
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
    }

    // -----------------------------------------------------------------------
    // GitHub Copilot CLI hook parsing
    // -----------------------------------------------------------------------

    #[test]
    fn map_copilot_session_start() {
        assert_eq!(
            map_copilot_event_type("sessionStart", None),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_copilot_pre_tool_use() {
        assert_eq!(
            map_copilot_event_type("preToolUse", None),
            Some(EventType::ToolStart)
        );
    }

    #[test]
    fn map_copilot_post_tool_use() {
        assert_eq!(
            map_copilot_event_type("postToolUse", None),
            Some(EventType::ToolEnd)
        );
    }

    #[test]
    fn map_copilot_user_prompt_is_thinking() {
        assert_eq!(
            map_copilot_event_type("userPromptSubmitted", None),
            Some(EventType::Thinking)
        );
    }

    #[test]
    fn map_copilot_agent_stop_is_idle() {
        assert_eq!(
            map_copilot_event_type("agentStop", None),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_copilot_error_event() {
        assert_eq!(
            map_copilot_event_type("errorOccurred", None),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_copilot_subagent_events() {
        assert_eq!(
            map_copilot_event_type("subagentStart", None),
            Some(EventType::SubagentStart)
        );
        assert_eq!(
            map_copilot_event_type("subagentStop", None),
            Some(EventType::SubagentStop)
        );
    }

    #[test]
    fn map_copilot_pre_compact() {
        assert_eq!(
            map_copilot_event_type("preCompact", None),
            Some(EventType::Compacting)
        );
    }

    #[test]
    fn map_copilot_session_end_complete_is_idle() {
        // Copilot fires sessionEnd with reason=complete after each turn;
        // treat that as Idle so the session card doesn't disappear.
        assert_eq!(
            map_copilot_event_type("sessionEnd", Some("complete")),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_copilot_session_end_other_reason_is_real_end() {
        assert_eq!(
            map_copilot_event_type("sessionEnd", Some("logout")),
            Some(EventType::SessionEnd)
        );
        assert_eq!(
            map_copilot_event_type("sessionEnd", None),
            Some(EventType::SessionEnd)
        );
    }

    #[test]
    fn map_copilot_unknown_returns_none() {
        assert_eq!(map_copilot_event_type("somethingElse", None), None);
    }

    #[test]
    fn extract_copilot_tool_detail_shell() {
        let input: Value = serde_json::json!({"command": "git status\necho hi"});
        let detail = extract_copilot_tool_detail(Some("shell"), Some(&input));
        assert_eq!(detail.as_deref(), Some("git status"));
    }

    #[test]
    fn extract_copilot_tool_detail_str_replace_editor() {
        let input: Value = serde_json::json!({"path": "C:\\code\\foo.rs", "old": "a", "new": "b"});
        let detail = extract_copilot_tool_detail(Some("str-replace-editor"), Some(&input));
        assert_eq!(detail.as_deref(), Some("C:\\code\\foo.rs"));
    }

    #[test]
    fn extract_copilot_tool_detail_view() {
        let input: Value = serde_json::json!({"path": "/src/main.rs"});
        let detail = extract_copilot_tool_detail(Some("view"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn extract_copilot_tool_detail_grep() {
        let input: Value = serde_json::json!({"pattern": "TODO"});
        let detail = extract_copilot_tool_detail(Some("grep"), Some(&input));
        assert_eq!(detail.as_deref(), Some("TODO"));
    }

    #[test]
    fn extract_copilot_tool_detail_unknown_first_string() {
        let input: Value = serde_json::json!({"unknown_field": "interesting value"});
        let detail = extract_copilot_tool_detail(Some("mystery-tool"), Some(&input));
        assert_eq!(detail.as_deref(), Some("interesting value"));
    }

    #[test]
    fn build_copilot_event_session_start() {
        let input = CopilotCliHookInput {
            session_id: Some("cp-abc".into()),
            cwd: Some("C:\\proj".into()),
            tool_name: None,
            tool_args: None,
            reason: None,
            prompt: None,
            user_prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_copilot_event("sessionStart", input).unwrap();
        assert_eq!(event.session_id, "cp-abc");
        assert_eq!(event.agent_type, AgentType::CopilotCli);
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("C:\\proj"));
    }

    #[test]
    fn build_copilot_event_pre_tool_use_with_shell_detail_and_full_command() {
        let full_cmd = "cargo test --release --quiet";
        let input = CopilotCliHookInput {
            session_id: Some("cp-1".into()),
            cwd: None,
            tool_name: Some("shell".into()),
            tool_args: Some(serde_json::json!({"command": full_cmd})),
            reason: None,
            prompt: None,
            user_prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_copilot_event("preToolUse", input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("shell"));
        assert_eq!(event.tool_detail.as_deref(), Some(full_cmd));
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd)
        );
    }

    #[test]
    fn build_copilot_event_session_end_complete_is_idle() {
        let input = CopilotCliHookInput {
            session_id: Some("cp-1".into()),
            cwd: None,
            tool_name: None,
            tool_args: None,
            reason: Some("complete".into()),
            prompt: None,
            user_prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_copilot_event("sessionEnd", input).unwrap();
        assert_eq!(event.event_type, EventType::Idle);
    }

    #[test]
    fn build_copilot_event_unknown_returns_none() {
        let input = CopilotCliHookInput {
            session_id: Some("cp-1".into()),
            cwd: None,
            tool_name: None,
            tool_args: None,
            reason: None,
            prompt: None,
            user_prompt: None,
            _extra: HashMap::new(),
        };
        assert!(build_copilot_event("notAnEvent", input).is_none());
    }

    #[test]
    fn deserialize_copilot_hook_input_camel_case() {
        let json = r#"{
            "sessionId": "cp-xyz",
            "cwd": "C:\\Users\\jonovak\\proj",
            "toolName": "view",
            "toolArgs": {"path": "src/main.rs"},
            "transcriptPath": "/tmp/transcript.json"
        }"#;
        let input: CopilotCliHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id.as_deref(), Some("cp-xyz"));
        assert_eq!(input.tool_name.as_deref(), Some("view"));
        assert!(input.tool_args.is_some());
    }

    #[test]
    fn build_copilot_event_fallback_session_id_from_pane() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let key = "DOT_AGENT_DECK_PANE_ID";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "pane-7") };

        let input = CopilotCliHookInput {
            session_id: None, // Older Copilot didn't include sessionId
            cwd: None,
            tool_name: None,
            tool_args: None,
            reason: None,
            prompt: None,
            user_prompt: None,
            _extra: HashMap::new(),
        };
        let event = build_copilot_event("sessionStart", input).unwrap();
        assert_eq!(event.session_id, "copilot-pane-pane-7");
        assert_eq!(event.pane_id.as_deref(), Some("pane-7"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
