In an agent pane with a visible drag-selection, pressing Ctrl+C now
copies the selection to the system clipboard (with agent frame glyphs
stripped) instead of sending Ctrl+C to the agent. With no selection
visible, Ctrl+C continues to forward to the agent as SIGINT.

This matches the Windows-explorer / GUI convention and complements the
existing right-click-to-copy path.
