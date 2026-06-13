PTY-byte activity suppresses Pending flicker during long LLM streaming
gaps. The hook-event-driven `last_activity` was the only input to the
Pending heuristic, so a 60 s response stream with no hook events would
flip Working → Pending. Now the heuristic also consults a separate
`last_pty_activity` field bumped from the PTY reader thread.
