Workspace restore now resumes wrapper-launched panes and resolves the
home directory more reliably on Windows.

Two related fixes:

1. **Wrappers + boolean flags now round-trip with `--resume`.** The
   gate that decides whether a saved pane command is safe to rewrite
   with `--resume <session_id>` previously only accepted bare
   `copilot` / `claude`. Panes launched as `agency copilot --allow-all`
   (or `npx claude --print`, `bunx copilot`, etc.) were preserved
   verbatim on restore, so the conversation always started fresh
   instead of resuming. The gate now accepts the shape
   `[wrapper] <agent> [--flag …]` where:
     - `wrapper` is one of `agency`, `agenchy`, `npx`, `pnpx`, `bunx`
       (single token only — `cmd /c <agent>` is still rejected
       because the `/c` flag belongs to the wrapper layer);
     - post-agent tokens are boolean flags (`--allow-all`) or
       `--flag=value` form. `--flag value` (space-separated value) is
       still rejected to avoid confusing flag values with positional
       sub-commands.
   `--resume <id>` is always appended at the end of the stripped
   command, after any wrapper and flag tokens. Idempotent across
   round-trips: re-saving and re-restoring updates the id rather than
   stacking flags. Shell metacharacters (`;`, `|`, `$`, backticks,
   quotes, etc.) in any token reject the rewrite as a defence-in-depth
   against command injection.

2. **`USERPROFILE` is the primary home-directory source on Windows.**
   On Windows profiles without `HOME` set (the default), dot-agent-deck
   was stranding its entire config tree at `C:\.config\dot-agent-deck\`
   (drive root) instead of the documented
   `%USERPROFILE%\.config\dot-agent-deck\`. The home-directory
   resolver now uses a proper fallback chain:
   `USERPROFILE → HOME → HOMEDRIVE + HOMEPATH → /` (USERPROFILE first
   so dot-agent-deck behaves like a Windows-native app and matches
   the rest of the codebase). The two other home-dir lookups
   (`copilot_manage::home_dir` for finding Copilot CLI's hooks dir,
   `ui::open_bookmark` cwd fallback) now delegate to the shared
   resolver so all three agree. A one-time idempotent migration
   runs at startup and moves any existing `/.config/dot-agent-deck/`
   tree to the new location — your bookmarks, workspaces,
   star-prompt state, and bell config all come along automatically.
   The migration is a no-op if the new location already exists
   (it never clobbers fresh state).
