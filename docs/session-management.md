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
| **Pending** | Agent appears stalled at an interactive prompt (e.g., a "pick one" multiple-choice question printed to its pane) — needs your attention, but not as a permission request. Inferred when a session has been Working for `pending.timeout_seconds` (default 10s) with no active tool and no new events. Configurable via `dot-agent-deck config set pending.timeout_seconds <N>`; set to `0` to disable. Bell fires by default on the transition — disable with `dot-agent-deck config set bell.on_pending false`. |
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

Mode tabs are also restored: each agent pane records which mode it belonged to, and `--continue` reopens the full mode tab — tab name, agent pane and its command, and all side panes with their commands — by looking up the mode config from the project's `.dot-agent-deck.toml`. The agent's internal conversation state is not restored; only the workspace structure is. If `.dot-agent-deck.toml` is missing or the mode was renamed at restore time, a warning is printed to stderr and the pane falls back to a plain dashboard pane.

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
- The agent's internal conversation state still isn't restored (same caveat as `--continue`). Use `claude --continue` or `opencode --resume` as the pane's saved command if you want the agent to resume its prior conversation.

### Where files live

- Default unnamed session: `~/.config/dot-agent-deck/session.toml`
- Named workspaces: `~/.config/dot-agent-deck/workspaces/<name>.toml`
