Bookmark name now uses the renamed card name when one is set.

Previously, opening the bookmark-note modal auto-populated the bookmark
`session_name` from (in priority order) Copilot CLI's auto-generated
session summary, the first prompt sent in the conversation, or
`(unnamed)`. A card name set by the user with `r` (rename) was ignored,
even though it's the user's most explicit naming intent and is what's
shown on the card itself.

Now, if the user has renamed the card, that rename is used as the
bookmark name. The previous fallback chain still applies when no rename
is present (or the rename is blank).
