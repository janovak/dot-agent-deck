---
sidebar_position: 4
title: Session Management
---

# Session Management

## Session Statuses

Each session card shows the agent's current state:

| Status | Meaning |
|---|---|
| **Thinking** | Agent is reasoning before acting |
| **Working** | Agent is executing a tool (tool name shown) |
| **Pending** | Agent appears stalled at an interactive prompt (e.g., a "pick one" multiple-choice question printed to its pane) — needs your attention, but not as a permission request. Inferred when a session has been Working for `pending.timeout_seconds` (default 30s) with no active tool and no new events, confirmed over two consecutive checks to suppress one-shot LLM-gap flicker. Configurable via `dot-agent-deck config set pending.timeout_seconds <N>`; set to `0` to disable. Bell fires by default on the transition — disable with `dot-agent-deck config set bell.on_pending false`. |
| **Compacting** | Context window is being compressed |
| **WaitingForInput** | Agent needs user approval or input (explicit permission prompt) |
| **Idle** | Agent is between tasks |
| **Error** | Something went wrong |

Cards also display:

- **Title row** — card number, the pane's display name (or `agent_type · session_id` if it hasn't been renamed), an animated status dot, and the status label
- **`Dir:`** — the working directory (basename, truncated to fit)
- **`Last:`** — elapsed time since the agent's last activity, alongside **`Tools:`** showing the total tool-call count
- **`Prmt:`** — the most recent user prompt(s)
- **Recent tool calls** — the last commands the agent ran

![Single agent card showing directory, last activity, tool count, recent prompt, and recent tool calls](/img/session-management-card.jpg)

How many prompts and tool calls fit on a card depends on the auto-chosen density, which Agent Deck picks based on how many cards are on the dashboard and how much room is available:

| Density | Prompts shown | Recent tool calls shown |
|---|---|---|
| Spacious | up to 3 | up to 3 |
| Normal | 1 | up to 3 |
| Compact | 1 | 1 |

The more agents you run in parallel, the more cards Agent Deck has to fit on the screen, so each card automatically becomes more compact. This is deliberate — scrolling through cards would defeat the point of having a single dashboard.

![Five agents running in parallel — cards switch to Compact density to fit them all without scrolling](/img/home-hero-dashboard.jpg)

## Resuming Sessions

The dashboard automatically saves your open panes (directories, names, and commands) when you exit. To restore them next time:

```bash
dot-agent-deck --continue
```

Without `--continue`, the dashboard starts with a blank slate. If a saved directory no longer exists, that pane is skipped with a warning.

After restore the dashboard is shown first so you get an overview before switching to a specific tab.

Mode tabs are also restored: each agent pane records which mode it belonged to, and `--continue` reopens the full mode tab — tab name, agent pane and its command, and all side panes with their commands — by looking up the mode config from the project's `.dot-agent-deck.toml`. When the agent pane was running Copilot CLI or Claude Code with a bare invocation (just `copilot` or `claude` — no extra flags or wrappers), the launch command is rewritten to `<agent> --resume <session_id>` so the actual conversation is restored, not just the pane layout. If the pane's command was customized (extra flags, `npx`, wrapper scripts, etc.) the original command is preserved untouched. If `.dot-agent-deck.toml` is missing or the mode was renamed at restore time, a warning is printed to stderr and the pane falls back to a plain dashboard pane.

Session data is stored in `~/.config/dot-agent-deck/session.toml`.

## Named Workspaces

For switching between different sets of sessions, use **named workspaces**. Each workspace is a separate save file, so you can keep one set of panes for `client-x`, another for `personal`, etc., without one overwriting the other.

```bash
dot-agent-deck --workspace client-x         # load (or start fresh if new)
dot-agent-deck --workspace personal         # different set of panes
dot-agent-deck --workspace work-server      # yet another set
```

The first time you use a workspace name, the dashboard starts blank and creates the file on exit. From then on, that workspace's panes are restored every time you launch with the same `--workspace` flag — same behaviour as `--continue` but scoped to the named slot.

### Save semantics

Workspaces auto-save **after every pane open, close, or rename**, plus a final write when you exit cleanly. The save is incremental: dot-agent-deck only writes when the current snapshot differs from what's already on disk, so steady-state runs do zero disk I/O. This means closing the terminal window without quitting cleanly *still* leaves a recent snapshot — at most one frame of state can be lost (typically less than 16 ms of activity).

### Listing and deleting workspaces

```bash
dot-agent-deck workspaces list              # list all saved workspaces
dot-agent-deck workspaces delete client-x   # delete one
```

### Constraints

- Workspace names must be 1–64 characters from `[A-Za-z0-9_-]`. No spaces, slashes, dots, or other punctuation. Windows reserved device names (`con`, `prn`, `aux`, `nul`, `com1`…`com9`, `lpt1`…`lpt9`) are rejected.
- `--continue` and `--workspace` are mutually exclusive — pick one.
- Conversation resume works automatically for Copilot CLI and Claude Code panes whose saved command is a bare `copilot` or `claude` (optionally already carrying a stale `--resume <id>`). The save step captures each pane's live `session_id` and `agent_type` and the restore step rewrites the launch command to `<agent> --resume <session_id>`. OpenCode panes aren't rewritten (the UI doesn't have a tested `--resume` shape for it yet), and any pane with a customized command is preserved untouched so you don't lose hand-tuned flags. If multiple agents have run in the same pane since the last save, the most-recently-active conversation is the one that gets resumed.

### Where files live

- Default unnamed session: `~/.config/dot-agent-deck/session.toml`
- Named workspaces: `~/.config/dot-agent-deck/workspaces/<name>.toml`

## Bookmarked Sessions

A separate, curated list of **Copilot CLI session GUIDs** you want to keep retrievable long-term — alongside a short note for each. Use this when a session matters enough that you want to come back to it weeks or months later, regardless of which workspace you happen to be in. The list is intentionally small: you add bookmarks one at a time, by hand, when something is worth keeping.

### Adding a bookmark

From inside the dashboard, select a session card and press **`Ctrl+B`**. A note-input bar appears showing the session's name (Copilot's own summary) — type your own short description and press Enter. If the session is already bookmarked, the input is prefilled with the existing note so you can edit it.

The bookmark file stores:

- `session_id` — the GUID Copilot CLI uses for `--resume`
- `session_name` — Copilot's auto-generated summary at the time you bookmarked (looked up from `~/.copilot/session-store.db`; for non-Copilot agents, falls back to the first prompt or "(unnamed)")
- `note` — your free-form description
- `updated_at` — when the bookmark was last created or edited

### Browsing and opening bookmarks

Press **`Ctrl+Shift+B`** to open the bookmark picker. Navigate with `j`/`k` or arrows; press Enter to spawn a new pane running `copilot --resume <guid>` in the session's original working directory. Press `d` in the picker to delete the highlighted bookmark. Press Esc or `q` to dismiss.

Bookmarked sessions are marked with a **★** next to their name on the dashboard cards.

### CLI commands

```bash
dot-agent-deck bookmarks list              # show all bookmarks with notes
dot-agent-deck bookmarks delete <query>    # delete by GUID prefix (>=4 chars) or exact note
```

### Where the file lives

`~/.config/dot-agent-deck/bookmarked-sessions.json` (JSON array; written atomically).

### Caveats

- The bookmark is just a pointer — if Copilot CLI ever loses the session row from its database, the bookmark becomes a dead link. Based on observable behaviour, Copilot CLI doesn't auto-delete sessions, but a reinstall or major upgrade could.
- Bookmarks for non-Copilot agents work too, but the spawn command (`copilot --resume <guid>`) is currently Copilot-specific. Adapt the launched command manually for other agents.
