use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::warn;

use crate::config_validation::sanitize_role_name;
use crate::event::{AgentEvent, AgentType, DelegateSignal, EventType, WorkDoneSignal};

const MAX_RECENT_EVENTS: usize = 50;
const MAX_FIRST_PROMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Thinking,
    Working,
    /// Agent is waiting for non-permission user input — specifically, an
    /// interactive-prompt tool (Copilot CLI's `ask_user`, per
    /// `INTERACTIVE_PROMPT_TOOL_NAMES`) is the active tool. Set the moment
    /// that tool starts (see `apply_event`'s `ToolStart` arm), since such a
    /// tool blocks on the user by definition. Distinct from `WaitingForInput`,
    /// which is the explicit permission-prompt state hooked from
    /// `PermissionRequest` events.
    Pending,
    Compacting,
    WaitingForInput,
    Idle,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardStats {
    pub active: usize,
    pub working: usize,
    pub pending: usize,
    pub thinking: usize,
    pub waiting: usize,
    pub errors: usize,
    pub idle: usize,
    pub compacting: usize,
    pub total_tools: u64,
}

#[derive(Debug, Clone)]
pub struct ActiveTool {
    pub name: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: String,
    pub agent_type: AgentType,
    pub cwd: Option<String>,
    pub status: SessionStatus,
    pub active_tool: Option<ActiveTool>,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub recent_events: VecDeque<AgentEvent>,
    pub tool_count: u32,
    pub last_user_prompt: Option<String>,
    pub first_prompts: Vec<String>,
    pub pane_id: Option<String>,
    /// Net count of subagents the parent has spawned that have not yet
    /// reported `SubagentStop`. Used to keep the card showing `Working`
    /// while background subagents are running, instead of letting the
    /// parent's own `sessionEnd` (reason=complete) flip the card to
    /// `Idle` prematurely. Saturating: never goes negative if events
    /// arrive in an unexpected order.
    pub active_subagent_count: u32,
}

#[derive(Debug, Default, Clone)]
pub struct AppState {
    pub sessions: HashMap<String, SessionState>,
    /// Remembers started_at per pane so a `/clear` restart keeps its position.
    pane_started_at: HashMap<String, DateTime<Utc>>,
    /// Set by the background version-check task when a newer release exists.
    pub update_available: Option<String>,
    /// Pane IDs created by our app — events from unknown panes are rejected.
    pub managed_pane_ids: HashSet<String>,
    /// Maps pane_id → orchestration role name (set when orchestration tab opens).
    pub pane_role_map: HashMap<String, String>,
    /// Maps pane_id → working directory for orchestration panes.
    pub pane_cwd_map: HashMap<String, String>,
    /// Pane IDs that are orchestrator (start=true) roles — only these can delegate.
    pub orchestrator_pane_ids: HashSet<String>,
    /// Delegate signals from the orchestrator, consumed by dispatch (M5).
    pub delegate_events: Vec<DelegateSignal>,
    /// Work-done signals from workers (or orchestrator --done), consumed by feedback (M5b).
    pub work_done_events: Vec<WorkDoneSignal>,
}

pub type SharedState = Arc<RwLock<AppState>>;

impl AppState {
    pub fn aggregate_stats(&self) -> DashboardStats {
        let mut stats = DashboardStats::default();
        for session in self.sessions.values() {
            if session.agent_type == AgentType::None {
                continue;
            }
            stats.active += 1;
            match session.status {
                SessionStatus::Working => stats.working += 1,
                SessionStatus::Pending => stats.pending += 1,
                SessionStatus::Thinking => stats.thinking += 1,
                SessionStatus::WaitingForInput => stats.waiting += 1,
                SessionStatus::Error => stats.errors += 1,
                SessionStatus::Idle => stats.idle += 1,
                SessionStatus::Compacting => stats.compacting += 1,
            }
            stats.total_tools += session.tool_count as u64;
        }
        stats
    }

    /// Register a pane ID as managed by our app.
    pub fn register_pane(&mut self, pane_id: String) {
        self.managed_pane_ids.insert(pane_id);
    }

    /// Create a placeholder session for a newly created pane so it always has a dashboard card.
    ///
    /// `agent_type` lets the caller seed the agent identity up front — from the
    /// pane's launch command on creation, or from the ending session on a
    /// `SessionEnd` restore — so the card reads e.g. "Copilot · Idle" instead
    /// of "No agent" during the (often long) wait for the agent's first hook
    /// event. Pass `AgentType::None` when the agent is genuinely unknown (a
    /// plain shell pane).
    pub fn insert_placeholder_session(
        &mut self,
        pane_id: String,
        cwd: Option<String>,
        agent_type: AgentType,
    ) {
        let session_id = placeholder_session_id(&pane_id);
        let now = Utc::now();
        let started_at = self.pane_started_at.get(&pane_id).copied().unwrap_or(now);
        self.sessions.insert(
            session_id.clone(),
            SessionState {
                session_id,
                agent_type,
                cwd,
                status: SessionStatus::Idle,
                active_tool: None,
                started_at,
                last_activity: now,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                first_prompts: Vec::new(),
                active_subagent_count: 0,
                pane_id: Some(pane_id),
            },
        );
    }

    /// Unregister a pane ID (e.g., when closing a pane).
    pub fn unregister_pane(&mut self, pane_id: &str) {
        self.managed_pane_ids.remove(pane_id);
        self.pane_role_map.remove(pane_id);
        self.pane_cwd_map.remove(pane_id);
        self.orchestrator_pane_ids.remove(pane_id);
    }

    /// Handle a delegate signal from the orchestrator.
    /// Validates that the sender is an orchestrator (start=true) role before enqueuing.
    pub fn handle_delegate(&mut self, signal: DelegateSignal) {
        if !self.pane_role_map.contains_key(&signal.pane_id) {
            warn!(pane_id = %signal.pane_id, "delegate from unknown pane");
            return;
        }
        if !self.orchestrator_pane_ids.contains(&signal.pane_id) {
            let role = self
                .pane_role_map
                .get(&signal.pane_id)
                .cloned()
                .unwrap_or_default();
            warn!(pane_id = %signal.pane_id, role = %role, "delegate from non-orchestrator pane");
            return;
        }
        self.delegate_events.push(signal);
    }

    /// Handle a work-done signal from a worker (or orchestrator --done).
    /// Resolves pane_id → role name, writes a per-role summary file, and
    /// stores the signal for feedback to the orchestrator (M5b).
    pub fn handle_work_done(&mut self, signal: WorkDoneSignal) {
        let role_name = match self.pane_role_map.get(&signal.pane_id) {
            Some(name) => name.clone(),
            None => {
                warn!(pane_id = %signal.pane_id, "work-done from unknown pane");
                return;
            }
        };

        // Write summary to .dot-agent-deck/work-done-{role}.md
        if let Some(cwd) = self.pane_cwd_map.get(&signal.pane_id) {
            let safe_name = sanitize_role_name(&role_name);
            let dir = std::path::Path::new(cwd).join(".dot-agent-deck");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                warn!(dir = %dir.display(), role = %role_name, error = %e, "failed to create work-done directory");
            }
            let file_path = dir.join(format!("work-done-{safe_name}.md"));
            if let Err(e) = std::fs::write(&file_path, &signal.task) {
                warn!(path = %file_path.display(), role = %role_name, error = %e, "failed to write work-done summary");
            }
        }

        self.work_done_events.push(signal);
    }

    pub fn apply_event(&mut self, mut event: AgentEvent) {
        // Only accept events from panes managed by our app.
        //
        // Events *with* a pane_id are accepted when we recognise that pane,
        // or auto-registered on SessionStart (to absorb the startup race
        // where the first hook can fire before `register_pane` lands).
        //
        // Events *without* a pane_id are external — they come from
        // Copilot/Claude/etc. processes the user runs outside our managed
        // panes (e.g., a separate terminal). We reject them
        // unconditionally rather than gating on
        // `!managed_pane_ids.is_empty()`, because the empty-pane-set
        // gate let those externals slip through during the brief window
        // between daemon startup and the first pane spawn, creating a
        // ghost session card that the user could never focus, dismiss,
        // or even map back to a real process.
        if let Some(ref pane_id) = event.pane_id {
            if !self.managed_pane_ids.contains(pane_id) {
                if event.event_type == EventType::SessionStart {
                    // Auto-register the pane to handle the startup race where
                    // the hook fires before register_pane is called.
                    self.managed_pane_ids.insert(pane_id.clone());
                } else {
                    return;
                }
            }
        } else {
            return;
        }
        if let Some(ref pane_id) = event.pane_id
            && let Some(existing_id) = self.sessions.iter().find_map(|(id, session)| {
                (session.pane_id.as_ref().is_some_and(|p| p == pane_id) && id != &event.session_id)
                    .then(|| id.clone())
            })
        {
            // Two cases:
            //
            //  A) `existing_id` is a placeholder (`"pane-<n>"`) we
            //     inserted via `insert_placeholder_session` before the
            //     agent's first hook arrived, and `event.session_id`
            //     is the agent's real session ID (e.g., a Copilot CLI
            //     UUID). PROMOTE: re-key the session under the real
            //     ID so downstream features like Session Bookmarks
            //     see the right ID. Without this, the placeholder
            //     "pane-3" persists forever and `copilot --resume
            //     pane-3` later fails because Copilot doesn't know
            //     that string.
            //
            //  B) Both IDs are "real" — a session restart on the same
            //     pane (e.g., Claude Code `/restart`). Keep the
            //     existing key by rewriting the event so the card
            //     stays in place across the restart. Same behavior
            //     as before.
            if is_placeholder_session_id(&existing_id)
                && !is_placeholder_session_id(&event.session_id)
                && !is_tool_call_id(&event.session_id)
            {
                if let Some(mut moved) = self.sessions.remove(&existing_id) {
                    moved.session_id = event.session_id.clone();
                    self.sessions.insert(event.session_id.clone(), moved);
                }
            } else {
                // Real → real restart (e.g., Claude Code `/restart`, or
                // user `/clear`-ing then continuing in the same pane).
                // Keep the existing map key so the card stays in place
                // and the user-set display name is preserved — but
                // update the SessionState's own `session_id` field so
                // workspace resume targets the *current* conversation,
                // not the stale one. (Map key and field intentionally
                // diverge from here on; callers that need the live id
                // — like `SavedSession::snapshot` — must read the
                // field, not the key.)
                //
                // EXCEPTION: subagent hook events run in the parent's
                // pane but carry the spawning *tool-call id*
                // (`toolu_…`/`call_…`) as their sessionId, not a
                // resumable session GUID. They must still update the
                // parent card (status, subagent counters) via the event
                // rewrite below, but must NOT overwrite the canonical
                // `session_id` — otherwise workspace/bookmark resume
                // saves `copilot --resume toolu_…`, which the agent
                // rejects with "No session or name matched '…'".
                if !is_tool_call_id(&event.session_id)
                    && let Some(s) = self.sessions.get_mut(&existing_id)
                {
                    s.session_id = event.session_id.clone();
                }
                let old_id = std::mem::replace(&mut event.session_id, existing_id);
                if old_id != event.session_id {
                    self.sessions.remove(&old_id);
                }
            }
        }

        if event.event_type == EventType::SessionEnd {
            // Preserve started_at for the pane so a restarted session keeps its position.
            let pane_restore = self.sessions.get(&event.session_id).and_then(|session| {
                session.pane_id.as_ref().map(|pid| {
                    self.pane_started_at.insert(pid.clone(), session.started_at);
                    // Carry the agent identity onto the restored placeholder so
                    // an agent that fires SessionEnd mid-life (Copilot does this
                    // at turn/session boundaries) shows "Copilot · Idle" rather
                    // than reverting the card to "No agent".
                    (pid.clone(), session.cwd.clone(), session.agent_type.clone())
                })
            });
            self.sessions.remove(&event.session_id);
            // Restore a placeholder card so the pane remains visible on the dashboard.
            if let Some((pane_id, cwd, agent_type)) = pane_restore
                && self.managed_pane_ids.contains(&pane_id)
            {
                self.insert_placeholder_session(pane_id, cwd, agent_type);
            }
            return;
        }

        let pane_started = event
            .pane_id
            .as_ref()
            .and_then(|pid| self.pane_started_at.get(pid))
            .copied();

        let session = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionState {
                session_id: event.session_id.clone(),
                agent_type: event.agent_type.clone(),
                cwd: event.cwd.clone(),
                status: SessionStatus::Idle,
                active_tool: None,
                started_at: pane_started.unwrap_or(event.timestamp),
                last_activity: event.timestamp,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                first_prompts: Vec::new(),
                active_subagent_count: 0,
                pane_id: event.pane_id.clone(),
            });

        session.last_activity = event.timestamp;

        if session.agent_type == AgentType::None && event.agent_type != AgentType::None {
            session.agent_type = event.agent_type.clone();
        }

        if event.cwd.is_some() {
            session.cwd.clone_from(&event.cwd);
        }

        if let Some(ref prompt) = event.user_prompt {
            session.last_user_prompt = Some(prompt.clone());
            if session.first_prompts.len() < MAX_FIRST_PROMPTS {
                session.first_prompts.push(prompt.clone());
            }
        }

        if event.pane_id.is_some() {
            session.pane_id.clone_from(&event.pane_id);
        }

        match event.event_type {
            EventType::SessionStart => {
                session.status = SessionStatus::Idle;
                session.active_tool = None;
            }
            EventType::Thinking => {
                session.status = SessionStatus::Thinking;
                session.active_tool = None;
            }
            EventType::ToolStart => {
                if session.status != SessionStatus::WaitingForInput {
                    // An interactive-prompt tool (Copilot's `ask_user`) is,
                    // by definition, blocking on the user — so go straight to
                    // Pending instead of Working. We detect it deterministically,
                    // so there's nothing to wait for: the moment the prompt
                    // appears, the card signals "needs you".
                    let is_prompt = event
                        .tool_name
                        .as_deref()
                        .is_some_and(is_interactive_prompt_tool);
                    session.status = if is_prompt {
                        SessionStatus::Pending
                    } else {
                        SessionStatus::Working
                    };
                }
                session.active_tool = Some(ActiveTool {
                    name: event.tool_name.clone().unwrap_or_default(),
                    detail: event.tool_detail.clone(),
                });
            }
            EventType::ToolEnd => {
                session.active_tool = None;
                session.tool_count += 1;
                // The prompt/permission was answered: the agent is processing
                // again, so clear the attention state to Thinking rather than
                // lingering on WaitingForInput / Pending until the next hook.
                if matches!(
                    session.status,
                    SessionStatus::WaitingForInput | SessionStatus::Pending
                ) {
                    session.status = SessionStatus::Thinking;
                }
            }
            EventType::WaitingForInput | EventType::PermissionRequest => {
                session.status = SessionStatus::WaitingForInput;
            }
            EventType::Idle => {
                // Don't flip the card to Idle while background subagents are
                // still running. The parent agent's `sessionEnd` (reason=
                // "complete") fires at the end of every conversation turn,
                // including the turn where the parent just dispatched
                // subagents and is now waiting on them — without this guard
                // the card would mislead the user into thinking nothing is
                // happening.
                //
                // `WaitingForInput` and `Error` are "sticky" — they reflect
                // attention the user still needs to give. An Idle event
                // ending the parent turn must not silently clobber either
                // state (which would hide a permission prompt or an error).
                // The next genuine transition (ToolStart for WaitingForInput,
                // a new Thinking for Error) is responsible for clearing it.
                if matches!(
                    session.status,
                    SessionStatus::WaitingForInput | SessionStatus::Error
                ) {
                    // Don't touch status; still clear active_tool below.
                } else if session.active_subagent_count > 0 {
                    session.status = SessionStatus::Working;
                } else {
                    session.status = SessionStatus::Idle;
                }
                session.active_tool = None;
            }
            EventType::Compacting => {
                session.status = SessionStatus::Compacting;
                session.active_tool = None;
            }
            EventType::SubagentStart => {
                // Track the in-flight subagent so a subsequent `Idle` event
                // doesn't prematurely mark the parent as done. Status itself
                // is not changed here — the next ToolStart/ToolEnd from the
                // subagent drives the visible status. If the parent was
                // already Idle (e.g., the user dispatched a subagent from a
                // fresh prompt), bump it back to Working so the card
                // reflects active background work.
                session.active_subagent_count = session.active_subagent_count.saturating_add(1);
                if session.status == SessionStatus::Idle {
                    session.status = SessionStatus::Working;
                }
            }
            EventType::SubagentStop => {
                // Track whether saturating_sub actually decremented — a
                // spurious Stop (e.g., duplicated hook event, or a Stop
                // arriving without a preceding Start) must not be allowed
                // to flip a legitimately-Working session to Idle, because
                // that case is exactly the "stuck at non-hook prompt"
                // scenario the Pending heuristic is designed to catch.
                let count_actually_decreased = session.active_subagent_count > 0;
                session.active_subagent_count = session.active_subagent_count.saturating_sub(1);
                // If the parent's last `Idle` event was deferred to Working
                // because subagents were in flight, the card can return to
                // Idle now that the count has reached zero — but only if no
                // tool is currently running and no fresh non-subagent event
                // has nudged the status elsewhere (Thinking, WaitingForInput,
                // etc., all stay put).
                if count_actually_decreased
                    && session.active_subagent_count == 0
                    && session.active_tool.is_none()
                    && session.status == SessionStatus::Working
                {
                    session.status = SessionStatus::Idle;
                }
            }
            EventType::Error => {
                session.status = SessionStatus::Error;
            }
            EventType::SessionEnd => unreachable!(),
        }

        session.recent_events.push_back(event);
        if session.recent_events.len() > MAX_RECENT_EVENTS {
            session.recent_events.pop_front();
        }
    }
}

/// Known agent tool names whose entire purpose is to block on user input.
/// When one of these starts (see `apply_event`'s `ToolStart` arm) the session
/// goes straight to `Pending`, because the user's attention is exactly what the
/// tool is waiting for.
///
/// Match is case-insensitive on the tool name only. Currently observed:
///
/// * `ask_user` — Copilot CLI's clarifying-question / menu-choice
///   tool. Observed in production logs as `active_tool=ask_user` for
///   2+ minutes while the user reads the prompt.
///
/// Extend this list when new agents are observed to use a similar
/// blocking-on-stdin pattern under a different tool name.
const INTERACTIVE_PROMPT_TOOL_NAMES: &[&str] = &["ask_user"];

/// Helper: returns true when `name` is in `INTERACTIVE_PROMPT_TOOL_NAMES`
/// (case-insensitive comparison).
fn is_interactive_prompt_tool(name: &str) -> bool {
    INTERACTIVE_PROMPT_TOOL_NAMES
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known))
}

/// Build the dot-agent-deck placeholder session ID for a given pane.
/// Centralised here so the format stays in lockstep with the
/// `is_placeholder_session_id` checker below.
fn placeholder_session_id(pane_id: &str) -> String {
    format!("{PLACEHOLDER_SESSION_ID_PREFIX}{pane_id}")
}

/// Returns true when `id` looks like a placeholder session ID inserted
/// by `insert_placeholder_session` (i.e., not a real agent-assigned
/// session ID like a Copilot CLI UUID).
///
/// Used by the pane-id-based session-merge logic in `apply_event` so a
/// late-arriving agent SessionStart can *promote* an existing
/// placeholder card to the real session ID — without the promotion,
/// the placeholder string (`"pane-3"`, etc.) would persist as the
/// canonical session_id forever and downstream features like Session
/// Bookmarks would store unusable IDs.
///
/// Also used by `SavedSession::snapshot` to skip placeholder sessions
/// when recording per-pane resume metadata.
pub fn is_placeholder_session_id(id: &str) -> bool {
    id.starts_with(PLACEHOLDER_SESSION_ID_PREFIX)
}

const PLACEHOLDER_SESSION_ID_PREFIX: &str = "pane-";

/// Returns true when `id` looks like an LLM *tool-call* identifier rather
/// than a resumable agent session id.
///
/// Copilot CLI runs subagents in the same pane as their parent, and those
/// subagent hook events arrive carrying the spawning **tool-call id** in the
/// `sessionId` field instead of a real session GUID. The two providers
/// Copilot CLI fronts use distinct, stable prefixes: Anthropic models emit
/// `toolu_…` and OpenAI models emit `call_…`.
///
/// Such an id must never be written to `SessionState::session_id` or saved
/// for resume: workspace/bookmark restore would build
/// `copilot --resume toolu_…`, which the CLI rejects with
/// "No session or name matched '…'". Real Copilot/Claude session ids are
/// UUID-shaped and never collide with these prefixes.
pub fn is_tool_call_id(id: &str) -> bool {
    id.starts_with("toolu_") || id.starts_with("call_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_event(session_id: &str, event_type: EventType) -> AgentEvent {
        AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/tmp".to_string()),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            // Default pane_id so tests can fire events through
            // `apply_event` without each being rejected by the
            // pane-id-required gate. Tests that don't fire
            // `EventType::SessionStart` first (and therefore don't
            // auto-register this pane) must call
            // `state.register_pane("test-pane".into())` explicitly.
            pane_id: Some("test-pane".to_string()),
        }
    }

    #[test]
    fn full_session_lifecycle() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Idle);

        let mut tool_event = make_event("s1", EventType::ToolStart);
        tool_event.tool_name = Some("Read".to_string());
        tool_event.tool_detail = Some("main.rs".to_string());
        state.apply_event(tool_event);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(
            state.sessions["s1"].active_tool.as_ref().unwrap().name,
            "Read"
        );

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert!(state.sessions["s1"].active_tool.is_none());

        state.apply_event(make_event("s1", EventType::SessionEnd));
        assert!(!state.sessions.contains_key("s1"));
    }

    #[test]
    fn concurrent_sessions() {
        let mut state = AppState::default();

        // Two sessions on two distinct panes so the pane-id-based merge
        // logic doesn't collapse them.
        let mut s1_start = make_event("s1", EventType::SessionStart);
        s1_start.pane_id = Some("pane-1".to_string());
        state.apply_event(s1_start);
        let mut s2_start = make_event("s2", EventType::SessionStart);
        s2_start.pane_id = Some("pane-2".to_string());
        state.apply_event(s2_start);
        assert_eq!(state.sessions.len(), 2);

        let mut tool_event = make_event("s1", EventType::ToolStart);
        tool_event.pane_id = Some("pane-1".to_string());
        tool_event.tool_name = Some("Write".to_string());
        state.apply_event(tool_event);

        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(state.sessions["s2"].status, SessionStatus::Idle);
    }

    #[test]
    fn reuse_session_for_same_pane() {
        let mut state = AppState::default();
        state.register_pane("pane-1".to_string());

        let mut first = make_event("s1", EventType::SessionStart);
        first.pane_id = Some("pane-1".to_string());
        state.apply_event(first);

        let mut restart = make_event("s2", EventType::SessionStart);
        restart.pane_id = Some("pane-1".to_string());
        state.apply_event(restart);

        assert!(state.sessions.contains_key("s1"));
        assert!(!state.sessions.contains_key("s2"));
        assert_eq!(state.sessions["s1"].pane_id.as_deref(), Some("pane-1"));
    }

    #[test]
    fn auto_create_unknown_session() {
        let mut state = AppState::default();
        // The pane needs to be registered before non-SessionStart events
        // are accepted (otherwise apply_event rejects them via the
        // managed-pane gate that prevents phantom external sessions).
        state.register_pane("test-pane".to_string());

        let mut tool_event = make_event("unknown", EventType::ToolStart);
        tool_event.tool_name = Some("Bash".to_string());
        state.apply_event(tool_event);

        assert!(state.sessions.contains_key("unknown"));
        assert_eq!(state.sessions["unknown"].status, SessionStatus::Working);
    }

    #[test]
    fn external_event_without_pane_id_is_rejected_even_when_no_panes_managed() {
        // Regression guard for the "phantom card" bug: when a user runs a
        // Copilot/Claude/etc. process *outside* dot-agent-deck (e.g., in a
        // separate terminal) while dot-agent-deck happens to be running
        // with no panes yet, the external process's hook events would
        // arrive over the daemon socket with `pane_id = None`. Earlier
        // logic accepted these because `managed_pane_ids.is_empty()` was
        // treated as "no constraint" — creating a ghost session entry
        // that the user could never focus, dismiss, or even map back to
        // a real pane. We must reject pane_id=None events
        // unconditionally.
        let mut state = AppState::default();
        assert!(state.managed_pane_ids.is_empty());

        let mut ext = make_event("external-uuid", EventType::SessionStart);
        ext.pane_id = None;
        state.apply_event(ext);

        assert!(
            !state.sessions.contains_key("external-uuid"),
            "events without a pane_id must never create a session entry, \
             even when no panes are managed yet"
        );
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn external_event_without_pane_id_is_rejected_after_panes_registered() {
        // Same guard, with managed panes already present. Belt-and-
        // suspenders so a future refactor that splits the rejection
        // logic still catches both cases.
        let mut state = AppState::default();
        state.register_pane("our-pane".to_string());

        let mut ext = make_event("external-uuid", EventType::ToolStart);
        ext.pane_id = None;
        ext.tool_name = Some("Bash".to_string());
        state.apply_event(ext);

        assert!(!state.sessions.contains_key("external-uuid"));
    }

    #[test]
    fn event_buffer_capping() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        for _ in 0..60 {
            state.apply_event(make_event("s1", EventType::Idle));
        }

        // 1 SessionStart + 60 Idle = 61, capped to 50
        assert_eq!(state.sessions["s1"].recent_events.len(), 50);
    }

    #[test]
    fn waiting_for_input_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(state.sessions["s1"].active_tool.is_none());
    }

    #[test]
    fn notification_during_active_tool_shows_waiting() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // A Notification during an active tool means a permission prompt —
        // PreToolUse fires before the Notification, so active_tool is set.
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(state.sessions["s1"].active_tool.is_some());
    }

    #[test]
    fn ask_user_question_shows_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("AskUserQuestion".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // AskUserQuestion is interactive — Notification transitions to WaitingForInput.
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn tool_count_increments_on_tool_end() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert_eq!(state.sessions["s1"].tool_count, 0);

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Read".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].tool_count, 0);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].tool_count, 1);

        let mut tool_start2 = make_event("s1", EventType::ToolStart);
        tool_start2.tool_name = Some("Write".to_string());
        state.apply_event(tool_start2);
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].tool_count, 2);
    }

    #[test]
    fn tool_end_clears_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        // Simulate: PreToolUse → PermissionRequest → tool runs → PostToolUse
        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Thinking);
    }

    #[test]
    fn toolstart_does_not_override_waiting_for_input() {
        // Regression: a concurrent subagent firing PreToolUse while a permission
        // prompt is active must not knock the status back to Working.
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        let mut subagent_tool = make_event("s1", EventType::ToolStart);
        subagent_tool.tool_name = Some("Explore".to_string());
        state.apply_event(subagent_tool);
        assert_eq!(
            state.sessions["s1"].status,
            SessionStatus::WaitingForInput,
            "ToolStart must not override WaitingForInput"
        );
        assert_eq!(
            state.sessions["s1"]
                .active_tool
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Explore"),
            "active_tool must still be updated even when status is preserved"
        );
    }

    #[test]
    fn toolstart_sets_working_when_not_waiting() {
        // Normal flow: ToolStart should still set Working when no permission prompt.
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(
            state.sessions["s1"]
                .active_tool
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Bash"),
            "active_tool must be set on normal ToolStart"
        );
    }

    #[test]
    fn tool_end_preserves_working_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".to_string());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // ToolEnd without permission request should keep Working→Working (not change)
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn error_status() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::Error));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Error);
    }

    #[test]
    fn last_user_prompt_set_and_persists() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert!(state.sessions["s1"].last_user_prompt.is_none());

        let mut prompt_event = make_event("s1", EventType::Thinking);
        prompt_event.user_prompt = Some("fix the bug".to_string());
        state.apply_event(prompt_event);
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the bug")
        );

        // Subsequent event without prompt should not clear it
        state.apply_event(make_event("s1", EventType::ToolEnd));
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the bug")
        );

        // New prompt replaces old one
        let mut prompt_event2 = make_event("s1", EventType::Thinking);
        prompt_event2.user_prompt = Some("add tests".to_string());
        state.apply_event(prompt_event2);
        assert_eq!(
            state.sessions["s1"].last_user_prompt.as_deref(),
            Some("add tests")
        );
    }

    #[test]
    fn first_prompts_captures_up_to_three() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        assert!(state.sessions["s1"].first_prompts.is_empty());

        let prompts = ["first", "second", "third"];
        for (i, text) in prompts.iter().enumerate() {
            let mut ev = make_event("s1", EventType::Thinking);
            ev.user_prompt = Some(text.to_string());
            state.apply_event(ev);
            assert_eq!(state.sessions["s1"].first_prompts.len(), i + 1);
            assert_eq!(state.sessions["s1"].first_prompts[i], *text);
        }
    }

    #[test]
    fn first_prompts_no_overwrite_after_cap() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        for text in &["p1", "p2", "p3", "p4", "p5"] {
            let mut ev = make_event("s1", EventType::Thinking);
            ev.user_prompt = Some(text.to_string());
            state.apply_event(ev);
        }

        assert_eq!(state.sessions["s1"].first_prompts.len(), 3);
        assert_eq!(state.sessions["s1"].first_prompts[0], "p1");
        assert_eq!(state.sessions["s1"].first_prompts[1], "p2");
        assert_eq!(state.sessions["s1"].first_prompts[2], "p3");
    }

    #[test]
    fn first_prompts_persist_across_events() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));

        let mut ev = make_event("s1", EventType::Thinking);
        ev.user_prompt = Some("only prompt".to_string());
        state.apply_event(ev);

        state.apply_event(make_event("s1", EventType::ToolEnd));
        state.apply_event(make_event("s1", EventType::Idle));
        state.apply_event(make_event("s1", EventType::Thinking));

        assert_eq!(state.sessions["s1"].first_prompts.len(), 1);
        assert_eq!(state.sessions["s1"].first_prompts[0], "only prompt");
    }

    #[test]
    fn aggregate_stats_empty() {
        let state = AppState::default();
        let stats = state.aggregate_stats();
        assert_eq!(stats, DashboardStats::default());
    }

    #[test]
    fn aggregate_stats_mixed_sessions() {
        let mut state = AppState::default();
        // Five distinct sessions need five distinct panes so they
        // don't collapse via the pane-id auto-merge in apply_event.
        let with_pane = |sid: &str, ev: EventType, pane: &str| {
            let mut e = make_event(sid, ev);
            e.pane_id = Some(pane.to_string());
            e
        };

        state.apply_event(with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Read".to_string());
        state.apply_event(tool);
        // s1: Working

        state.apply_event(with_pane("s2", EventType::SessionStart, "p2"));
        state.apply_event(with_pane("s2", EventType::WaitingForInput, "p2"));
        // s2: WaitingForInput

        state.apply_event(with_pane("s3", EventType::SessionStart, "p3"));
        state.apply_event(with_pane("s3", EventType::Error, "p3"));
        // s3: Error

        state.apply_event(with_pane("s4", EventType::SessionStart, "p4"));
        state.apply_event(with_pane("s4", EventType::Thinking, "p4"));
        // s4: Thinking

        state.apply_event(with_pane("s5", EventType::SessionStart, "p5"));
        // s5: Idle

        let stats = state.aggregate_stats();
        assert_eq!(stats.active, 5);
        assert_eq!(stats.working, 1);
        assert_eq!(stats.waiting, 1);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.thinking, 1);
        assert_eq!(stats.idle, 1);
    }

    #[test]
    fn aggregate_stats_tool_count_summation() {
        let mut state = AppState::default();

        state.apply_event(make_event("s1", EventType::SessionStart));
        let mut t1 = make_event("s1", EventType::ToolStart);
        t1.tool_name = Some("Read".to_string());
        state.apply_event(t1);
        state.apply_event(make_event("s1", EventType::ToolEnd));

        state.apply_event(make_event("s2", EventType::SessionStart));
        for _ in 0..3 {
            let mut t = make_event("s2", EventType::ToolStart);
            t.tool_name = Some("Bash".to_string());
            state.apply_event(t);
            state.apply_event(make_event("s2", EventType::ToolEnd));
        }

        let stats = state.aggregate_stats();
        assert_eq!(stats.total_tools, 4);
    }

    #[test]
    fn restarted_session_preserves_started_at_via_pane() {
        let mut state = AppState::default();
        state.register_pane("pane-42".to_string());

        // Register session with a pane
        let mut ev = make_event("s1", EventType::SessionStart);
        ev.pane_id = Some("pane-42".to_string());
        state.apply_event(ev);
        let original_started = state.sessions["s1"].started_at;

        // End the session (simulates /clear)
        let mut end_ev = make_event("s1", EventType::SessionEnd);
        end_ev.pane_id = Some("pane-42".to_string());
        state.apply_event(end_ev);
        // After SessionEnd, a placeholder is restored since the pane is still managed.
        // Key is "pane-pane-42" because pane_id="pane-42" and placeholder keys use "pane-{pane_id}".
        assert!(state.sessions.contains_key("pane-pane-42"));

        // New session on the same pane: the placeholder is promoted to
        // the real session ID ("s2"). started_at is preserved across
        // the transition because the placeholder copied it from
        // `pane_started_at`, and apply_event keeps that field when
        // it upgrades a session.
        let mut ev2 = make_event("s2", EventType::SessionStart);
        ev2.pane_id = Some("pane-42".to_string());
        state.apply_event(ev2);
        assert!(
            !state.sessions.contains_key("pane-pane-42"),
            "placeholder should be promoted to the real session ID"
        );
        assert_eq!(state.sessions["s2"].started_at, original_started);
    }

    #[test]
    fn permission_request_sets_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::PermissionRequest));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn tool_start_preserves_waiting_for_input() {
        let mut state = AppState::default();
        state.apply_event(make_event("s1", EventType::SessionStart));
        state.apply_event(make_event("s1", EventType::WaitingForInput));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);

        let mut tool_start = make_event("s1", EventType::ToolStart);
        tool_start.tool_name = Some("Bash".into());
        state.apply_event(tool_start);
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
    }

    #[test]
    fn placeholder_session_created() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            AgentType::None,
        );

        assert!(state.sessions.contains_key("pane-42"));
        let session = &state.sessions["pane-42"];
        assert_eq!(session.agent_type, AgentType::None);
        assert_eq!(session.status, SessionStatus::Idle);
        assert_eq!(session.pane_id.as_deref(), Some("42"));
        assert_eq!(session.cwd.as_deref(), Some("/tmp"));
        assert_eq!(session.tool_count, 0);
    }

    #[test]
    fn placeholder_transitions_to_real_session() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            AgentType::None,
        );

        let mut start = make_event("real-uuid-123", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        start.cwd = Some("/home".to_string());
        state.apply_event(start);

        // Real UUID becomes the canonical key (promoted from placeholder)
        // so downstream features like Session Bookmarks store an ID that
        // the agent CLI's own --resume command can actually consume.
        assert!(state.sessions.contains_key("real-uuid-123"));
        assert!(!state.sessions.contains_key("pane-42"));
        let session = &state.sessions["real-uuid-123"];
        assert_eq!(session.session_id, "real-uuid-123");
        assert_eq!(session.agent_type, AgentType::ClaudeCode);
        assert_eq!(session.cwd.as_deref(), Some("/home"));
        assert_eq!(session.pane_id.as_deref(), Some("42"));
    }

    #[test]
    fn placeholder_restored_after_session_end() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            AgentType::None,
        );

        // Transition to real session (placeholder is promoted away)
        let mut start = make_event("real-uuid", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        state.apply_event(start);
        assert_eq!(
            state.sessions["real-uuid"].agent_type,
            AgentType::ClaudeCode
        );

        // End the real session — placeholder should be restored, and it must
        // KEEP the agent identity (Fix A) so a mid-life SessionEnd shows
        // "Claude · Idle" rather than reverting the card to "No agent".
        let mut end = make_event("real-uuid", EventType::SessionEnd);
        end.pane_id = Some("42".to_string());
        state.apply_event(end);

        assert!(state.sessions.contains_key("pane-42"));
        assert!(!state.sessions.contains_key("real-uuid"));
        assert_eq!(
            state.sessions["pane-42"].agent_type,
            AgentType::ClaudeCode,
            "restored placeholder must inherit the ended session's agent identity"
        );
        assert_eq!(state.sessions["pane-42"].pane_id.as_deref(), Some("42"));
    }

    #[test]
    fn placeholder_not_restored_after_close() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            AgentType::None,
        );

        // Transition to real session (placeholder promoted to real UUID)
        let mut start = make_event("real-uuid", EventType::SessionStart);
        start.pane_id = Some("42".to_string());
        state.apply_event(start);

        // Simulate Ctrl+w: remove session and unregister pane (same as ui handler)
        state.sessions.remove("real-uuid");
        state.unregister_pane("42");

        assert!(state.sessions.is_empty());
        assert!(!state.managed_pane_ids.contains("42"));
    }

    #[test]
    fn placeholder_excluded_from_stats() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            AgentType::None,
        );

        // Add a real session on a different registered pane
        state.register_pane("99".to_string());
        let mut start = make_event("s1", EventType::SessionStart);
        start.pane_id = Some("99".to_string());
        state.apply_event(start);

        let stats = state.aggregate_stats();
        assert_eq!(stats.active, 1);
        assert_eq!(stats.idle, 1);
    }

    #[test]
    fn close_placeholder_session() {
        let mut state = AppState::default();
        state.register_pane("42".to_string());
        state.insert_placeholder_session(
            "42".to_string(),
            Some("/tmp".to_string()),
            AgentType::None,
        );

        // Simulate Ctrl+w on the placeholder
        state.sessions.remove("pane-42");
        state.unregister_pane("42");

        assert!(state.sessions.is_empty());
        assert!(!state.managed_pane_ids.contains("42"));
    }

    #[test]
    fn placeholder_promotion_preserves_real_uuid_for_bookmarks() {
        // Regression guard for the user-reported "Error: No session, task,
        // or name matched 'pane-3'" bookmark-resume failure. The bookmark
        // feature reads `SessionState::session_id` and saves it to disk;
        // later `copilot --resume <id>` consumes that string. If the
        // promotion logic ever regresses and leaves the placeholder
        // string as the canonical session_id, every bookmark created
        // before the agent's first hook will be unresumable.
        let mut state = AppState::default();
        state.register_pane("3".to_string());
        state.insert_placeholder_session(
            "3".to_string(),
            Some("/repo".to_string()),
            AgentType::None,
        );

        // Agent finally fires SessionStart with a real UUID.
        let real_uuid = "fe179f3b-5594-4ff0-9f05-94ad962db12f";
        let mut start = make_event(real_uuid, EventType::SessionStart);
        start.agent_type = AgentType::CopilotCli;
        start.pane_id = Some("3".to_string());
        state.apply_event(start);

        // The dashboard's view of this session — what
        // `open_bookmark_note_modal` reads — must now be the real UUID,
        // not the placeholder.
        let session = state
            .sessions
            .get(real_uuid)
            .expect("session should be keyed by real UUID after promotion");
        assert_eq!(
            session.session_id, real_uuid,
            "SessionState.session_id must also be the real UUID, not the placeholder"
        );
        assert!(
            !state.sessions.contains_key("pane-3"),
            "placeholder must be removed once promoted"
        );
    }

    #[test]
    fn real_to_real_session_restart_keeps_existing_key() {
        // The promotion path is *only* for placeholder → real. A real
        // → real restart (e.g., Claude Code `/restart`) must still
        // collapse the new session into the existing key so the card
        // stays in place and any user-set display name is preserved.
        let mut state = AppState::default();
        state.register_pane("7".to_string());

        let mut first = make_event("real-uuid-A", EventType::SessionStart);
        first.pane_id = Some("7".to_string());
        state.apply_event(first);
        assert!(state.sessions.contains_key("real-uuid-A"));

        let mut restart = make_event("real-uuid-B", EventType::SessionStart);
        restart.pane_id = Some("7".to_string());
        state.apply_event(restart);

        // The new session collapses into the existing key — same as
        // pre-fix behavior. We don't churn the key on every restart.
        assert!(state.sessions.contains_key("real-uuid-A"));
        assert!(!state.sessions.contains_key("real-uuid-B"));
        // But the SessionState's session_id field reflects the live
        // id so workspace resume targets the current conversation.
        // Map key and field intentionally diverge after a restart.
        assert_eq!(
            state.sessions["real-uuid-A"].session_id, "real-uuid-B",
            "session_id field must update to the live id after restart"
        );
    }

    #[test]
    fn subagent_tool_call_id_does_not_corrupt_canonical_session_id() {
        // Regression: Copilot CLI subagents run in the parent's pane and
        // fire hook events whose `sessionId` is the spawning *tool-call
        // id* (`toolu_…` / `call_…`), not a resumable session GUID.
        // Before the fix these hit the "real → real restart" branch and
        // overwrote the parent's canonical `session_id`, so workspace /
        // bookmark resume later saved `copilot --resume toolu_…` and the
        // CLI rejected it with "No session or name matched '…'".
        let mut state = AppState::default();
        state.register_pane("7".to_string());

        let real_uuid = "0dc83c83-3bd6-4fb8-83c1-c8d25d820f86";
        let mut start = make_event(real_uuid, EventType::SessionStart);
        start.agent_type = AgentType::CopilotCli;
        start.pane_id = Some("7".to_string());
        state.apply_event(start);
        assert!(state.sessions.contains_key(real_uuid));

        // A subagent tool event arrives on the SAME pane carrying a
        // tool-call id as its session id (both Anthropic `toolu_` and
        // OpenAI `call_` shapes occur depending on the active model).
        for tool_call_id in [
            "toolu_018dZ3HtuEnKRQfjGwaGZEFc",
            "call_GGpCiUtRHsZ9gtsmEusrlbHH",
        ] {
            let mut sub = make_event(tool_call_id, EventType::ToolStart);
            sub.agent_type = AgentType::CopilotCli;
            sub.pane_id = Some("7".to_string());
            sub.tool_name = Some("Bash".to_string());
            state.apply_event(sub);

            // The event is still attributed to the parent card (status
            // updates) — but the canonical id must stay the real UUID.
            assert!(
                state.sessions.contains_key(real_uuid),
                "parent card must remain keyed by the real UUID"
            );
            assert!(
                !state.sessions.contains_key(tool_call_id),
                "no spurious card should be created under the tool-call id"
            );
            assert_eq!(
                state.sessions[real_uuid].session_id, real_uuid,
                "canonical session_id must NOT be overwritten by a tool-call id"
            );
        }
    }

    #[test]
    fn is_tool_call_id_recognises_tool_call_ids() {
        // Anthropic + OpenAI tool-call id prefixes Copilot CLI surfaces.
        assert!(is_tool_call_id("toolu_018dZ3HtuEnKRQfjGwaGZEFc"));
        assert!(is_tool_call_id("call_GGpCiUtRHsZ9gtsmEusrlbHH"));
        // Real session ids (UUID-shaped) and placeholders are NOT tool calls.
        assert!(!is_tool_call_id("0dc83c83-3bd6-4fb8-83c1-c8d25d820f86"));
        assert!(!is_tool_call_id("pane-3"));
        assert!(!is_tool_call_id(""));
    }

    #[test]
    fn is_placeholder_session_id_recognises_placeholders() {
        assert!(is_placeholder_session_id("pane-1"));
        assert!(is_placeholder_session_id("pane-42"));
        assert!(is_placeholder_session_id("pane-abc"));
        // Real Copilot UUIDs and other agent IDs do NOT start with "pane-".
        assert!(!is_placeholder_session_id(
            "fe179f3b-5594-4ff0-9f05-94ad962db12f"
        ));
        assert!(!is_placeholder_session_id("real-uuid"));
        assert!(!is_placeholder_session_id(""));
    }

    #[test]
    fn handle_delegate_stores_event() {
        let mut state = AppState::default();
        state
            .pane_role_map
            .insert("pane-1".into(), "orchestrator".into());
        state.orchestrator_pane_ids.insert("pane-1".into());

        let signal = crate::event::DelegateSignal {
            pane_id: "pane-1".into(),
            task: "Implement login".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        };
        state.handle_delegate(signal);

        assert_eq!(state.delegate_events.len(), 1);
        assert_eq!(state.delegate_events[0].task, "Implement login");
        assert_eq!(state.delegate_events[0].to, vec!["coder"]);
    }

    #[test]
    fn handle_delegate_unknown_pane_is_noop() {
        let mut state = AppState::default();

        let signal = crate::event::DelegateSignal {
            pane_id: "unknown-pane".into(),
            task: "Do something".into(),
            to: vec!["coder".into()],
            timestamp: Utc::now(),
        };
        state.handle_delegate(signal);

        assert!(state.delegate_events.is_empty());
    }

    #[test]
    fn handle_work_done_resolves_role_and_stores_event() {
        let mut state = AppState::default();
        state.pane_role_map.insert("pane-1".into(), "coder".into());
        state
            .pane_cwd_map
            .insert("pane-1".into(), "/tmp/test-wd".into());

        let signal = crate::event::WorkDoneSignal {
            pane_id: "pane-1".into(),
            task: "Implemented login".into(),
            done: false,
            timestamp: Utc::now(),
        };
        state.handle_work_done(signal);

        assert_eq!(state.work_done_events.len(), 1);
        assert_eq!(state.work_done_events[0].task, "Implemented login");

        // Verify summary file was written
        let file = std::path::Path::new("/tmp/test-wd/.dot-agent-deck/work-done-coder.md");
        assert!(file.exists());
        let content = std::fs::read_to_string(file).unwrap();
        assert_eq!(content, "Implemented login");

        // Clean up
        let _ = std::fs::remove_dir_all("/tmp/test-wd/.dot-agent-deck");
    }

    #[test]
    fn handle_work_done_unknown_pane_is_noop() {
        let mut state = AppState::default();

        let signal = crate::event::WorkDoneSignal {
            pane_id: "unknown-pane".into(),
            task: "Some work".into(),
            done: false,
            timestamp: Utc::now(),
        };
        state.handle_work_done(signal);

        assert!(state.work_done_events.is_empty());
    }

    #[test]
    fn handle_work_done_done_flag_stored() {
        let mut state = AppState::default();
        state
            .pane_role_map
            .insert("pane-1".into(), "orchestrator".into());

        let signal = crate::event::WorkDoneSignal {
            pane_id: "pane-1".into(),
            task: "All complete".into(),
            done: true,
            timestamp: Utc::now(),
        };
        state.handle_work_done(signal);

        assert_eq!(state.work_done_events.len(), 1);
        assert!(state.work_done_events[0].done);
    }

    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------
    // Pending status: ask_user → Pending (event-level)
    // -----------------------------------------------------------------------

    #[test]
    fn ask_user_tool_goes_pending_immediately() {
        // Copilot CLI's clarifying-question pattern presents as an active tool
        // named `ask_user` that blocks until the user picks an option. Because
        // that *definitionally* means "waiting on the user", the card goes
        // straight to Pending the moment the prompt appears.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("ask_user".into());
        state.apply_event(tool);

        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
        assert!(state.sessions["s1"].active_tool.is_some());

        // A normal (non-interactive) tool still goes Working, not Pending.
        // (Separate pane so the same-pane re-key logic doesn't merge it.)
        state.register_pane("p2".into());
        state.apply_event(make_event_with_pane("s2", EventType::SessionStart, "p2"));
        let mut bash = make_event_with_pane("s2", EventType::ToolStart, "p2");
        bash.tool_name = Some("Bash".into());
        state.apply_event(bash);
        assert_eq!(state.sessions["s2"].status, SessionStatus::Working);
    }

    #[test]
    fn ask_user_answered_clears_pending_to_thinking() {
        // Once the user answers, the agent is processing again — the ToolEnd
        // must clear the Pending attention state to Thinking rather than
        // lingering on "needs you" until the next (possibly laggy) hook.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("ask_user".into());
        state.apply_event(tool);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);

        let mut end = make_event_with_pane("s1", EventType::ToolEnd, "p1");
        end.tool_name = Some("ask_user".into());
        state.apply_event(end);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Thinking);
        assert!(state.sessions["s1"].active_tool.is_none());
    }

    #[test]
    fn is_interactive_prompt_tool_is_case_insensitive() {
        assert!(is_interactive_prompt_tool("ask_user"));
        assert!(is_interactive_prompt_tool("ASK_USER"));
        assert!(is_interactive_prompt_tool("Ask_User"));
        assert!(!is_interactive_prompt_tool("ask"));
        assert!(!is_interactive_prompt_tool("Bash"));
        assert!(!is_interactive_prompt_tool(""));
    }

    #[test]
    fn pending_clears_when_new_event_arrives() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));

        let mut ask_user = make_event_with_pane("s1", EventType::ToolStart, "p1");
        ask_user.tool_name = Some("ask_user".into());
        state.apply_event(ask_user);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);

        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Read".into());
        state.apply_event(tool);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_status_counts_in_aggregate_stats() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));

        let mut ask_user = make_event_with_pane("s1", EventType::ToolStart, "p1");
        ask_user.tool_name = Some("ask_user".into());
        state.apply_event(ask_user);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);

        let stats = state.aggregate_stats();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.working, 0);
    }

    // -----------------------------------------------------------------------
    // Subagent-aware status (Working stays Working while subagents are live)
    // -----------------------------------------------------------------------

    #[test]
    fn subagent_start_increments_count() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        assert_eq!(state.sessions["s1"].active_subagent_count, 1);
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        assert_eq!(state.sessions["s1"].active_subagent_count, 2);
    }

    #[test]
    fn subagent_stop_decrements_count_saturating() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStop, "p1"));
        // Saturating subtract: count never goes negative.
        assert_eq!(state.sessions["s1"].active_subagent_count, 0);

        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStop, "p1"));
        assert_eq!(state.sessions["s1"].active_subagent_count, 1);
    }

    #[test]
    fn subagent_start_from_idle_bumps_status_to_working() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        // Session starts Idle.
        assert_eq!(state.sessions["s1"].status, SessionStatus::Idle);
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn idle_event_keeps_status_working_while_subagents_in_flight() {
        // The literal bug the user reported: parent finishes its turn,
        // fires sessionEnd (=> EventType::Idle), but subagents are still
        // running. Card must stay Working, not slide to Idle.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        // Parent kicks off a subagent.
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        // Parent runs a tool to dispatch the subagent.
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("dispatch_subagent".into());
        state.apply_event(tool);
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        // Parent's turn ends (Copilot CLI fires sessionEnd reason=complete).
        state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
        assert_eq!(
            state.sessions["s1"].status,
            SessionStatus::Working,
            "Idle event must not flip status while a subagent is still in flight"
        );
        assert_eq!(state.sessions["s1"].active_subagent_count, 1);
    }

    #[test]
    fn idle_event_flips_to_idle_when_no_subagents_in_flight() {
        // Regression guard: ordinary Idle behaviour preserved.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Read".into());
        state.apply_event(tool);
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Idle);
    }

    #[test]
    fn last_subagent_stop_returns_status_to_idle() {
        // Pattern: parent's turn ended (Working held open by subagent), the
        // last subagent finishes — card should now go Idle.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        state.apply_event(make_event_with_pane("s1", EventType::SubagentStop, "p1"));
        assert_eq!(state.sessions["s1"].active_subagent_count, 0);
        assert_eq!(
            state.sessions["s1"].status,
            SessionStatus::Idle,
            "card should return to Idle once the last subagent stops"
        );
    }

    #[test]
    fn subagent_stop_does_not_clobber_active_tool_or_other_statuses() {
        // If the parent is running its own tool when a subagent finishes,
        // the parent's status must NOT be forced to Idle.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("ParentTool".into());
        state.apply_event(tool);
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStop, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert!(state.sessions["s1"].active_tool.is_some());

        // And: if status was Thinking (e.g., user re-prompted) we don't
        // touch it on a subagent stop.
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.sessions.get_mut("s1").unwrap().status = SessionStatus::Thinking;
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStop, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Thinking);
    }

    #[test]
    fn idle_event_preserves_waiting_for_input_even_with_subagents() {
        // Bug guard: a permission prompt (WaitingForInput) must survive an
        // Idle event arriving from the parent agent's turn-end. Before the
        // fix, the Idle handler unconditionally flipped status to
        // Working (when subagents were active) or Idle (when not),
        // silently hiding the prompt from the user. Test both subagent
        // count > 0 and count == 0 branches.
        for subagent_count in [0u32, 2] {
            let mut state = AppState::default();
            state.register_pane("p1".into());
            state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
            state.sessions.get_mut("s1").unwrap().status = SessionStatus::WaitingForInput;
            state.sessions.get_mut("s1").unwrap().active_subagent_count = subagent_count;
            state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
            assert_eq!(
                state.sessions["s1"].status,
                SessionStatus::WaitingForInput,
                "WaitingForInput must survive Idle (subagent_count = {subagent_count})"
            );
            // Subagent count must be unchanged by an Idle event.
            assert_eq!(state.sessions["s1"].active_subagent_count, subagent_count);
        }
    }

    #[test]
    fn idle_event_preserves_error_even_with_subagents() {
        // Same sticky-status guard for Error: a session that surfaced an
        // error must not have it silently buried by the parent agent's
        // turn-end Idle event.
        for subagent_count in [0u32, 2] {
            let mut state = AppState::default();
            state.register_pane("p1".into());
            state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
            state.sessions.get_mut("s1").unwrap().status = SessionStatus::Error;
            state.sessions.get_mut("s1").unwrap().active_subagent_count = subagent_count;
            state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
            assert_eq!(
                state.sessions["s1"].status,
                SessionStatus::Error,
                "Error must survive Idle (subagent_count = {subagent_count})"
            );
        }
    }

    #[test]
    fn idle_event_clears_active_tool_even_when_status_is_sticky() {
        // The Idle handler clears `active_tool` unconditionally — the
        // sticky-status guard only protects the status field. Verify
        // the side effect still happens so a stuck WaitingForInput
        // pane doesn't keep showing an old tool detail.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Bash".into());
        state.apply_event(tool);
        state.sessions.get_mut("s1").unwrap().status = SessionStatus::WaitingForInput;

        state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::WaitingForInput);
        assert!(
            state.sessions["s1"].active_tool.is_none(),
            "Idle must always clear active_tool"
        );
    }

    #[test]
    fn spurious_subagent_stop_does_not_flip_working_to_idle() {
        // Regression guard for the spurious-Stop edge case: a SubagentStop
        // arriving with no preceding Start (duplicated hook, out-of-order
        // event) must not be allowed to flip a legitimately-Working
        // session to Idle. That case is exactly the "stuck at non-hook
        // prompt" scenario the Pending heuristic is designed to catch —
        // silently resolving it via a phantom Stop would mask the bug
        // the dashboard exists to surface.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        // Get into Working without ever firing SubagentStart.
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(state.sessions["s1"].active_subagent_count, 0);

        // Now fire a spurious Stop.
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStop, "p1"));
        assert_eq!(
            state.sessions["s1"].active_subagent_count, 0,
            "saturating_sub keeps count pinned at zero"
        );
        assert_eq!(
            state.sessions["s1"].status,
            SessionStatus::Working,
            "spurious Stop must not flip Working → Idle without a real Start having fired"
        );
    }

    fn make_event_with_pane(session_id: &str, event_type: EventType, pane_id: &str) -> AgentEvent {
        let mut ev = make_event(session_id, event_type);
        ev.pane_id = Some(pane_id.to_string());
        ev
    }
}
