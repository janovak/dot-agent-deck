use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config_validation::sanitize_role_name;
use crate::event::{AgentEvent, AgentType, DelegateSignal, EventType, WorkDoneSignal};

const MAX_RECENT_EVENTS: usize = 50;
const MAX_FIRST_PROMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Thinking,
    Working,
    /// Agent appears to be waiting for non-permission user input (e.g.,
    /// a clarifying multiple-choice prompt printed to its pane). Inferred
    /// from a heuristic in `AppState::apply_pending_timeout`: when an
    /// agent has been in `Working` long enough with no active tool and
    /// no new events, it has almost certainly stalled at an interactive
    /// prompt. Distinct from `WaitingForInput`, which is the explicit
    /// permission-prompt state hooked from `PermissionRequest` events.
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
    /// Most recent time the embedded PTY for this session emitted any
    /// bytes from the agent. Updated out-of-band by the UI loop from
    /// `EmbeddedPaneController::last_pty_byte_at` before each Pending
    /// check; never bumped by `apply_event`. Display surfaces
    /// ("Last: 5s ago", idle-art timer, workspace-restore preference)
    /// deliberately keep using `last_activity` so they reflect
    /// meaningful agent activity rather than every spinner tick.
    ///
    /// Initialised to `last_activity` so a session with no recorded
    /// PTY traffic yet does not look artificially older than its hook
    /// events.
    pub last_pty_activity: DateTime<Utc>,
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
    /// Consecutive `apply_pending_timeout` checks where the session was
    /// found stale (eligible AND `elapsed >= timeout`) without a Pending
    /// flip having happened yet. Reset to 0 on any new event arriving
    /// via `apply_event`, and reset to 0 immediately after a Pending
    /// flip. The flip is gated on `strikes >= PENDING_CONFIRMATION_STRIKES`
    /// so a one-shot LLM-thinking gap right at the timeout boundary
    /// can't produce a visible Working → Pending → Working flicker.
    pub pending_strikes: u32,
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
                last_pty_activity: now,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                first_prompts: Vec::new(),
                active_subagent_count: 0,
                pending_strikes: 0,
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

    /// Optimistically flip a live agent session on `pane_id` from Idle to
    /// Working — called when the user submits a prompt (presses Enter) so the
    /// card reacts instantly instead of waiting on the agent's (often laggy)
    /// first hook event.
    ///
    /// No-op unless the session is a real agent (`agent_type` set — not a
    /// placeholder/shell pane) that is currently `Idle`, so it never fights a
    /// status the hooks have already advanced (Working/Thinking/
    /// WaitingForInput/Pending/etc.). Returns whether it changed anything.
    pub fn mark_pane_working_on_submit(&mut self, pane_id: &str) -> bool {
        if let Some(s) = self
            .sessions
            .values_mut()
            .find(|s| s.pane_id.as_deref() == Some(pane_id))
            && s.agent_type != AgentType::None
            && s.status == SessionStatus::Idle
        {
            s.status = SessionStatus::Working;
            s.last_activity = Utc::now();
            return true;
        }
        false
    }

    /// Record that the embedded PTY for `pane_id` emitted bytes from
    /// the agent at `at`. Updates `last_pty_activity` on every session
    /// bound to that pane (typically one) when `at` is more recent than
    /// the stored value. Used by the UI loop to bridge the PTY reader
    /// thread into the Pending-flip heuristic without changing the
    /// hook-event-driven `last_activity` semantics that drive display.
    pub fn bump_pty_activity(&mut self, pane_id: &str, at: DateTime<Utc>) {
        for session in self.sessions.values_mut() {
            if session.pane_id.as_deref() == Some(pane_id) && at > session.last_pty_activity {
                session.last_pty_activity = at;
            }
        }
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
                last_pty_activity: event.timestamp,
                recent_events: VecDeque::new(),
                tool_count: 0,
                last_user_prompt: None,
                first_prompts: Vec::new(),
                active_subagent_count: 0,
                pending_strikes: 0,
                pane_id: event.pane_id.clone(),
            });

        session.last_activity = event.timestamp;
        // Any event arriving for the session counts as activity — reset the
        // Pending-confirmation counter. Without this, a `apply_pending_timeout`
        // tick that previously banked a strike would still credit it the next
        // time the session went quiet, defeating the multi-tick gate.
        session.pending_strikes = 0;

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
                    session.status = SessionStatus::Working;
                }
                session.active_tool = Some(ActiveTool {
                    name: event.tool_name.clone().unwrap_or_default(),
                    detail: event.tool_detail.clone(),
                });
            }
            EventType::ToolEnd => {
                session.active_tool = None;
                session.tool_count += 1;
                if session.status == SessionStatus::WaitingForInput {
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

    /// Walk every session and transition `Working` or `Thinking` → `Pending`
    /// when the session has stalled for at least `timeout` without firing
    /// any new event and without an active tool. This catches two cases:
    ///
    ///   1. **Post-tool stall**: agent fires `postToolUse`, then prints an
    ///      interactive menu ("Did you mean 1, 2, or 3?") and waits on
    ///      stdin without firing any further hook event. Status is
    ///      `Working` at the time.
    ///   2. **Thinking stall**: agent fires `userPromptSubmitted` (→
    ///      `Thinking`) and then prints a clarifying question directly
    ///      *without* going through a tool. Status stays `Thinking`
    ///      indefinitely until the user responds.
    ///
    /// Both manifest in the dashboard as a frozen-looking spinner with no
    /// active tool, which is what the heuristic keys on.
    ///
    /// Returns the set of session IDs that just transitioned so callers
    /// can fire a bell or update other side-effects exactly once per
    /// transition.
    ///
    /// **Clock-skew defence.** Uses wall-clock `Utc::now()` rather than a
    /// monotonic source so a long laptop sleep, NTP correction, or manual
    /// clock change can spike `signed_duration_since(last_activity)` to
    /// arbitrarily large values. To prevent a barrage of false Pending
    /// flips (and bell rings) on system resume, durations above
    /// `CLOCK_SKEW_REJECTION_THRESHOLD` are treated as "the clock just
    /// jumped, can't trust this" and skipped. The threshold is chosen
    /// generously enough that no realistic timeout value would ever
    /// exceed it.
    ///
    /// **By-design caveat.** The heuristic intentionally cannot distinguish
    /// "agent is stuck at a non-hook prompt" from "agent is taking longer
    /// than `timeout` to think/decide". Both look like
    /// `status=Working|Thinking AND active_tool=None`. The default 30 s
    /// timeout is generous enough for typical LLM gaps; if a user runs
    /// into the false-positive case (very long Thinking on complex
    /// prompts) they can tune `pending.timeout_seconds` up or set it to
    /// 0 to disable the heuristic entirely.
    ///
    /// **Confirmation gate.** Even with a generous timeout, a single
    /// LLM gap right at the timeout boundary used to produce a visible
    /// Working → Pending → Working flicker. To eliminate that, a session
    /// must be observed stale on `PENDING_CONFIRMATION_STRIKES`
    /// consecutive calls (≈one call per second from `ui.rs`) before the
    /// flip actually fires. Any new event resets the strike counter via
    /// `apply_event`, so true silence is the only thing that accumulates
    /// strikes.
    pub fn apply_pending_timeout(&mut self, timeout: chrono::Duration) -> Vec<String> {
        let now = Utc::now();
        let mut transitioned = Vec::new();
        for (sid, session) in self.sessions.iter_mut() {
            // Both Working and Thinking are bell-eligible: Working catches
            // the post-tool-stall case, Thinking catches the
            // clarifying-question-without-tool case.
            if !matches!(
                session.status,
                SessionStatus::Working | SessionStatus::Thinking
            ) {
                // Other statuses are by design ineligible (Idle, Pending
                // itself, WaitingForInput, Compacting, Error). Skipping
                // silently — those are the common case, no diagnostic
                // value in logging them. Also reset strikes so a session
                // that re-enters Working/Thinking later starts fresh.
                session.pending_strikes = 0;
                continue;
            }
            // Compute elapsed early so we can include it in skip logs.
            // Use the more recent of the hook-event-driven `last_activity`
            // and the PTY-byte-driven `last_pty_activity` so a session
            // that is streaming response tokens (no hook events firing,
            // but bytes flowing) does not flip to Pending mid-stream.
            //
            // EXCEPT at an interactive prompt (`ask_user`): the agent is
            // parked waiting on the user, but its TUI keeps repainting chrome
            // (spinner, status-bar clock, cursor) which bumps
            // `last_pty_activity` every tick and would perpetually reset the
            // timer — so Pending never fires when it matters most. There, use
            // the hook-event time only, so Pending fires promptly. During real
            // work there is no interactive tool active, so streaming still gets
            // the PTY-suppression above.
            let at_interactive_prompt = session
                .active_tool
                .as_ref()
                .is_some_and(|t| is_interactive_prompt_tool(&t.name));
            let effective_last = if at_interactive_prompt {
                session.last_activity
            } else {
                session.last_activity.max(session.last_pty_activity)
            };
            let elapsed = now.signed_duration_since(effective_last);
            let elapsed_ms = elapsed.num_milliseconds();

            if let Some(ref tool) = session.active_tool {
                // Most active tools are legitimate work (Bash, Edit,
                // long-running searches). Skipping when one is running
                // prevents false-positive Pending on those.
                //
                // EXCEPT: Copilot CLI implements clarifying questions
                // ("Did you mean 1, 2, or 3?") as a tool called
                // `ask_user` that blocks until the user picks an
                // option. From the gate's perspective the session looks
                // like a long-running tool, but it's actually waiting
                // on the user — the *exact* case Pending exists to
                // surface. Treat known interactive-prompt tool names
                // as Pending-eligible despite being "active", and let
                // the normal elapsed-timeout check decide.
                if !is_interactive_prompt_tool(&tool.name) {
                    info!(
                        session_id = %sid,
                        status = ?session.status,
                        elapsed_ms,
                        active_tool = %tool.name,
                        "pending_check skipped: active_tool"
                    );
                    session.pending_strikes = 0;
                    continue;
                }
            }
            // Background subagents are legitimate work — don't false-positive
            // them into Pending. The parent agent simply hasn't fired events
            // because it's waiting on subagents to finish.
            if session.active_subagent_count > 0 {
                info!(
                    session_id = %sid,
                    status = ?session.status,
                    elapsed_ms,
                    subagent_count = session.active_subagent_count,
                    "pending_check skipped: active subagents"
                );
                session.pending_strikes = 0;
                continue;
            }
            // Reject negative (clock went backward) and absurdly large
            // values (clock jumped forward — sleep/NTP/manual change).
            if elapsed < chrono::Duration::zero() || elapsed > CLOCK_SKEW_REJECTION_THRESHOLD {
                info!(
                    session_id = %sid,
                    status = ?session.status,
                    elapsed_ms,
                    "pending_check skipped: clock skew rejected"
                );
                session.pending_strikes = 0;
                continue;
            }
            if elapsed >= timeout {
                session.pending_strikes = session.pending_strikes.saturating_add(1);
                if session.pending_strikes >= PENDING_CONFIRMATION_STRIKES {
                    info!(
                        session_id = %sid,
                        from_status = ?session.status,
                        elapsed_ms,
                        timeout_ms = timeout.num_milliseconds(),
                        strikes = session.pending_strikes,
                        active_tool = %session.active_tool.as_ref().map(|t| t.name.as_str()).unwrap_or(""),
                        "pending_check fired: transitioning to Pending"
                    );
                    session.status = SessionStatus::Pending;
                    session.pending_strikes = 0;
                    transitioned.push(sid.clone());
                } else {
                    info!(
                        session_id = %sid,
                        status = ?session.status,
                        elapsed_ms,
                        timeout_ms = timeout.num_milliseconds(),
                        strikes = session.pending_strikes,
                        required = PENDING_CONFIRMATION_STRIKES,
                        "pending_check banked strike: waiting for confirmation"
                    );
                }
            } else {
                // Below the timeout — a fresh-enough event has arrived.
                // Drop any banked strike so the next stall starts from zero.
                session.pending_strikes = 0;
                info!(
                    session_id = %sid,
                    status = ?session.status,
                    elapsed_ms,
                    timeout_ms = timeout.num_milliseconds(),
                    active_tool = %session.active_tool.as_ref().map(|t| t.name.as_str()).unwrap_or(""),
                    "pending_check waiting: eligible but below timeout"
                );
            }
        }
        transitioned
    }
}

/// Number of consecutive `apply_pending_timeout` checks during which a
/// session must be observed stale (eligible AND `elapsed >= timeout`)
/// before the Working → Pending flip actually fires. Because the caller
/// in `ui.rs` invokes this roughly once per second, the value is also
/// the additional seconds of grace beyond `pending.timeout_seconds`.
///
/// Set to `2` to eliminate the single-tick LLM-thinking-gap flicker
/// (Working → Pending → Working on the same render cycle) without
/// noticeably delaying a real stall — a stuck agent still flips within
/// `timeout + ~1 s`.
const PENDING_CONFIRMATION_STRIKES: u32 = 2;

/// Maximum trusted elapsed-time-since-last-activity that
/// `apply_pending_timeout` will act on. Values above this are taken as
/// evidence the system clock jumped (laptop sleep, NTP, manual clock
/// change) rather than as a real measurement, and the heuristic skips
/// the session to avoid firing a flood of false Pending flips on system
/// resume. 24 h is large enough that no realistic `pending.timeout_seconds`
/// value would be missed by this guard.
const CLOCK_SKEW_REJECTION_THRESHOLD: chrono::Duration = chrono::Duration::hours(24);

/// Known agent tool names whose entire purpose is to block on user
/// input. When one of these is the active tool, `apply_pending_timeout`
/// treats the session as Pending-eligible (despite an active tool)
/// because the user's attention is exactly what the tool is waiting for.
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
    // Pending status: Working → Pending heuristic
    // -----------------------------------------------------------------------

    #[test]
    fn pending_timeout_flips_stale_working_session_to_pending() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        // Tool finished — active_tool cleared, status still Working.
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert!(state.sessions["s1"].active_tool.is_none());

        // Force last_activity into the past so the timeout matches.
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // First check banks a strike but does NOT flip — the confirmation
        // gate suppresses single-tick LLM-gap flicker. Second check (no
        // intervening event to reset strikes) crosses the threshold.
        let first = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(first.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        let second = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(second, vec!["s1".to_string()]);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
    }

    #[test]
    fn pending_timeout_skips_sessions_with_active_tool() {
        // A genuinely long-running tool (e.g., `cargo test`) keeps active_tool
        // set throughout — the timeout must not flip it to Pending.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Bash".into());
        state.apply_event(tool);
        // Push activity back so the duration check would otherwise trigger.
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(60);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(60);
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(transitioned.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_timeout_ignores_non_eligible_statuses() {
        // Idle, WaitingForInput, Compacting, Error all stay — only
        // Working and Thinking are eligible to transition to Pending
        // (those are the two states that show as "agent is processing
        // but no tool is actively running" in the UI, which is where a
        // non-hook stall manifests). Pending itself is also excluded
        // so a repeated apply doesn't oscillate.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        // Force last_activity into the past.
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(60);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(60);
        for status in [
            SessionStatus::Idle,
            SessionStatus::WaitingForInput,
            SessionStatus::Compacting,
            SessionStatus::Error,
            SessionStatus::Pending,
        ] {
            state.sessions.get_mut("s1").unwrap().status = status.clone();
            let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
            assert!(
                transitioned.is_empty(),
                "should not transition from {status:?}"
            );
            assert_eq!(state.sessions["s1"].status, status);
        }
    }

    #[test]
    fn pending_timeout_flips_stale_thinking_session_to_pending() {
        // Regression guard for the Copilot CLI clarifying-question case:
        // when the agent fires userPromptSubmitted (→ Thinking) and then
        // prints a clarifying menu directly without going through a
        // tool, status stays Thinking until the user responds. The
        // heuristic must catch this stall too, not only the
        // post-tool variant.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::Thinking, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Thinking);
        assert!(state.sessions["s1"].active_tool.is_none());

        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);

        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // Two consecutive checks needed (confirmation gate).
        assert!(
            state
                .apply_pending_timeout(chrono::Duration::seconds(10))
                .is_empty()
        );
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(transitioned, vec!["s1".to_string()]);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
    }

    #[test]
    fn pending_timeout_fires_when_active_tool_is_ask_user() {
        // Copilot CLI's clarifying-question pattern presents as an active
        // tool named `ask_user` that blocks until the user picks an
        // option. The Pending heuristic must treat this case as
        // eligible — not skip on `active_tool.is_some()` — because
        // the user's attention is exactly what the tool is waiting for.
        // Confirmed in production logs: status=Working active_tool=ask_user
        // for 2+ minutes with no other events.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("ask_user".into());
        state.apply_event(tool);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert!(state.sessions["s1"].active_tool.is_some());

        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);

        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // Two consecutive checks needed (confirmation gate).
        assert!(
            state
                .apply_pending_timeout(chrono::Duration::seconds(10))
                .is_empty()
        );
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(transitioned, vec!["s1".to_string()]);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
    }

    #[test]
    fn pending_fires_at_ask_user_prompt_despite_fresh_pty_chrome() {
        // Crispness regression: at an `ask_user` prompt the agent is parked
        // waiting on the user, but its TUI keeps repainting chrome (spinner /
        // status-bar clock) which bumps `last_pty_activity` every tick. Pending
        // must still fire — the interactive-prompt case uses hook-event time
        // only, ignoring the PTY chrome that would otherwise reset the timer.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("ask_user".into());
        state.apply_event(tool);

        // The prompt appeared 30s ago (stale hook activity)...
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // ...but the TUI just repainted 200ms ago (fresh PTY chrome). Before
        // the fix, effective_last = max(...) would be fresh and suppress Pending.
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::milliseconds(200);

        assert!(
            state
                .apply_pending_timeout(chrono::Duration::seconds(10))
                .is_empty()
        );
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(transitioned, vec!["s1".to_string()]);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
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
    fn pending_timeout_still_skips_normal_active_tool() {
        // Regression guard for the interactive-prompt special case:
        // non-interactive tools (Bash, Edit, etc.) must still skip the
        // gate, otherwise every long-running tool would false-positive
        // into Pending.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Bash".into());
        state.apply_event(tool);
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(60);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(60);
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(transitioned.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_timeout_below_threshold_does_not_fire() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        // last_activity is recent — duration is well under 10s.
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(transitioned.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_clears_when_new_event_arrives() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // Two consecutive checks needed (confirmation gate).
        state.apply_pending_timeout(chrono::Duration::seconds(10));
        state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);

        // User answers the prompt; agent fires another tool call.
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Read".into());
        state.apply_event(tool);
        // ToolStart while status was Pending now drives back to Working.
        // (status != WaitingForInput, so the existing guard lets it through.)
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_status_counts_in_aggregate_stats() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // Two consecutive checks needed (confirmation gate).
        state.apply_pending_timeout(chrono::Duration::seconds(10));
        state.apply_pending_timeout(chrono::Duration::seconds(10));

        let stats = state.aggregate_stats();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.working, 0);
    }

    #[test]
    fn pending_confirmation_gate_suppresses_single_tick_flicker() {
        // Regression guard for the user-visible "card briefly flips to
        // Pending then back to Working" flicker. The scenario:
        //   1. Working session has a >timeout LLM gap (no events arrive).
        //   2. apply_pending_timeout fires — session is eligible AND stale.
        //   3. The agent then emits a fresh event before the *next* check.
        //
        // Before this fix the first check flipped status to Pending,
        // producing a one-render flash. With the strike gate, the first
        // check only banks a strike (status stays Working), the fresh
        // event resets the strike, and the user never sees Pending.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // First check: stale, but only one strike — no flip yet.
        let first = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(
            first.is_empty(),
            "single-tick staleness must NOT flip to Pending"
        );
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
        assert_eq!(state.sessions["s1"].pending_strikes, 1);

        // Agent emits a fresh tool — this is the case the gate protects
        // against. The banked strike must be cleared.
        let mut tool = make_event_with_pane("s1", EventType::ToolStart, "p1");
        tool.tool_name = Some("Read".into());
        state.apply_event(tool);
        assert_eq!(state.sessions["s1"].pending_strikes, 0);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_strikes_reset_when_status_leaves_eligible() {
        // If the session transitions out of Working/Thinking between
        // strike-banking checks (e.g., a permission prompt arrives), the
        // accumulated strikes must be cleared so the next stale Working
        // period starts the gate from zero.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(30);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(30);
        // Bank one strike while Working.
        state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(state.sessions["s1"].pending_strikes, 1);

        // Session pivots out of eligible status (e.g., permission prompt).
        state.sessions.get_mut("s1").unwrap().status = SessionStatus::WaitingForInput;
        // Next check: ineligible — strikes must be reset, not retained.
        state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(state.sessions["s1"].pending_strikes, 0);
    }

    #[test]
    fn pending_timeout_skips_negative_elapsed_from_clock_going_backward() {
        // If the system clock moved backward (NTP correction, manual
        // change), `signed_duration_since(last_activity)` returns a
        // negative value. The guard must skip these sessions rather than
        // pass the >= timeout check on negative numbers (chrono does the
        // sane thing here, but we want explicit coverage of the case).
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        // last_activity in the future = clock moved backward.
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() + chrono::Duration::seconds(60);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() + chrono::Duration::seconds(60);
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(transitioned.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_timeout_skips_implausibly_large_elapsed_from_laptop_sleep() {
        // Regression guard against the user-facing bug "every Working
        // session fires Pending on laptop resume". After a multi-hour
        // sleep, `Utc::now()` jumps forward and every session looks
        // ancient — but the right action is to skip them entirely
        // (treat as "clock just jumped") rather than fire a barrage of
        // bells the user will find at 9am.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        // Simulate ~48h of laptop sleep.
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::hours(48);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::hours(48);
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(
            transitioned.is_empty(),
            "48h elapsed should be treated as clock skew, not as a real Pending stall"
        );
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_timeout_fires_at_boundary_just_under_clock_skew_threshold() {
        // The skew rejection must not be so aggressive that it eats real
        // long timeouts. A session that's been Working for 23 hours with
        // a 23h timeout should still fire — we only skip *above* the
        // 24h threshold.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::hours(23);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::hours(23);
        // Two consecutive checks needed (confirmation gate).
        assert!(
            state
                .apply_pending_timeout(chrono::Duration::hours(22))
                .is_empty()
        );
        let transitioned = state.apply_pending_timeout(chrono::Duration::hours(22));
        assert_eq!(transitioned, vec!["s1".to_string()]);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
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
    fn pending_timeout_skips_sessions_with_active_subagents() {
        // Regression: a session that's only "Working" because of background
        // subagents must not be mis-flipped to Pending after the timeout.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::SubagentStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::Idle, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);

        // Force last_activity into the past so the timeout would otherwise
        // trigger.
        state.sessions.get_mut("s1").unwrap().last_activity =
            Utc::now() - chrono::Duration::seconds(60);
        state.sessions.get_mut("s1").unwrap().last_pty_activity =
            Utc::now() - chrono::Duration::seconds(60);
        let transitioned = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(transitioned.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
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

    #[test]
    fn mark_working_on_submit_flips_idle_agent() {
        // The user pressing Enter on a focused agent pane should make the card
        // react instantly rather than waiting on the agent's first hook event.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Idle);
        assert_ne!(state.sessions["s1"].agent_type, AgentType::None);

        assert!(state.mark_pane_working_on_submit("p1"));
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn mark_working_on_submit_ignores_placeholder_nonidle_and_unknown() {
        let mut state = AppState::default();
        state.register_pane("p1".into());

        // Placeholder with unknown agent (a plain shell) must NOT be flipped.
        state.insert_placeholder_session("p1".into(), None, AgentType::None);
        assert!(!state.mark_pane_working_on_submit("p1"));
        assert_eq!(state.sessions["pane-p1"].status, SessionStatus::Idle);

        // A live agent already past Idle must not be clobbered.
        state.insert_placeholder_session("p2".into(), None, AgentType::CopilotCli);
        state.sessions.get_mut("pane-p2").unwrap().status = SessionStatus::WaitingForInput;
        assert!(!state.mark_pane_working_on_submit("p2"));
        assert_eq!(
            state.sessions["pane-p2"].status,
            SessionStatus::WaitingForInput
        );

        // Unknown pane — no-op.
        assert!(!state.mark_pane_working_on_submit("nope"));
    }

    #[test]
    fn mark_working_on_submit_flips_agent_placeholder() {
        // A command-detected placeholder (Fix A: "Copilot · Idle" before any
        // hook) should also flip to Working on submit.
        let mut state = AppState::default();
        state.register_pane("p3".into());
        state.insert_placeholder_session("p3".into(), None, AgentType::CopilotCli);
        assert!(state.mark_pane_working_on_submit("p3"));
        assert_eq!(state.sessions["pane-p3"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_timeout_skipped_by_recent_last_pty_activity() {
        // last_activity is stale (60s ago) so the hook-event clock has
        // crossed the timeout, but last_pty_activity is fresh (1s ago) —
        // the agent is streaming response tokens. apply_pending_timeout
        // must use max(last_activity, last_pty_activity) and treat the
        // session as still working.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));

        let now = Utc::now();
        {
            let s = state.sessions.get_mut("s1").unwrap();
            s.last_activity = now - chrono::Duration::seconds(60);
            s.last_pty_activity = now - chrono::Duration::seconds(1);
        }

        // Two consecutive ticks would normally cross the strike gate,
        // but the PTY-byte freshness suppresses both.
        let first = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(first.is_empty());
        let second = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(second.is_empty());
        assert_eq!(state.sessions["s1"].status, SessionStatus::Working);
    }

    #[test]
    fn pending_timeout_fires_when_both_last_activity_and_pty_are_stale() {
        // last_activity AND last_pty_activity both 60s ago — no streaming,
        // no hook events. Genuine stall; must flip after the strike gate.
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolStart, "p1"));
        state.apply_event(make_event_with_pane("s1", EventType::ToolEnd, "p1"));

        let now = Utc::now();
        {
            let s = state.sessions.get_mut("s1").unwrap();
            s.last_activity = now - chrono::Duration::seconds(60);
            s.last_pty_activity = now - chrono::Duration::seconds(60);
        }

        let first = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert!(first.is_empty()); // strike #1
        let second = state.apply_pending_timeout(chrono::Duration::seconds(10));
        assert_eq!(second, vec!["s1".to_string()]); // strike #2 → flip
        assert_eq!(state.sessions["s1"].status, SessionStatus::Pending);
    }

    #[test]
    fn bump_pty_activity_only_updates_sessions_bound_to_pane() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.register_pane("p2".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));
        state.apply_event(make_event_with_pane("s2", EventType::SessionStart, "p2"));

        let baseline = Utc::now() - chrono::Duration::seconds(120);
        for s in state.sessions.values_mut() {
            s.last_pty_activity = baseline;
        }

        let bump_to = Utc::now();
        state.bump_pty_activity("p1", bump_to);

        // s1 bound to p1 — bumped. s2 bound to p2 — untouched.
        assert_eq!(state.sessions["s1"].last_pty_activity, bump_to);
        assert_eq!(state.sessions["s2"].last_pty_activity, baseline);
    }

    #[test]
    fn bump_pty_activity_never_goes_backward() {
        let mut state = AppState::default();
        state.register_pane("p1".into());
        state.apply_event(make_event_with_pane("s1", EventType::SessionStart, "p1"));

        let now = Utc::now();
        state.sessions.get_mut("s1").unwrap().last_pty_activity = now;

        // Try to bump backward — must be a no-op (only `at > current` updates).
        state.bump_pty_activity("p1", now - chrono::Duration::seconds(30));
        assert_eq!(state.sessions["s1"].last_pty_activity, now);
    }
}
