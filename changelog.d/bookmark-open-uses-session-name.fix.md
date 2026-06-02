Opening a bookmark no longer uses the bookmark's note as the card title.

Previously, pressing Enter on a bookmark in the picker would name the
newly opened card using the bookmark's `note` (the free-form
description like "investigating intermittent build failures") whenever
the note was non-empty. The note is a description, not a name; long
notes overflowed the card title and didn't match what was shown on
the original card.

Now the card uses the bookmark's `session_name` (the renamed card name,
Copilot summary, or first prompt — whatever was captured at bookmark
creation time). The note stays as the bookmark's description and is
no longer copied into the card title. Hand-edited bookmarks with an
empty `session_name` get an empty card name and fall back to the
default `agent · id` card title, never the note.
