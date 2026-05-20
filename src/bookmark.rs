//! Session bookmarks: a small curated list of Copilot CLI session GUIDs the
//! user wants to keep retrievable long-term, alongside a human-typed note and
//! the session's name (Copilot's auto-generated summary).
//!
//! The bookmark file lives at `~/.config/dot-agent-deck/bookmarked-sessions.json`
//! and is the only writer dot-agent-deck makes here. We never modify Copilot
//! CLI's `~/.copilot/session-store.db`; we just read the `summary` column from
//! it at bookmark-create time so the recorded `session_name` stays meaningful
//! even if Copilot CLI later loses the row.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::dirs_home;

/// A single bookmarked Copilot CLI session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Copilot CLI session GUID — what `copilot --resume` consumes.
    pub session_id: String,
    /// Copilot CLI's auto-generated session summary at the time the bookmark
    /// was created. For non-Copilot agents this is a best-effort fallback
    /// (first prompt or `(unnamed)`).
    pub session_name: String,
    /// User-typed free-form description ("app creation", "auth bug investigation").
    pub note: String,
    /// When the bookmark was first created or last updated.
    pub updated_at: DateTime<Utc>,
}

/// Resolve `~/.config/dot-agent-deck/bookmarked-sessions.json`, overridable
/// via the `DOT_AGENT_DECK_BOOKMARKS` env var (used by tests).
pub fn bookmarks_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_BOOKMARKS") {
        return PathBuf::from(p);
    }
    dirs_home().join(".config/dot-agent-deck/bookmarked-sessions.json")
}

/// Load all bookmarks from disk. Returns an empty list if the file doesn't
/// exist or is malformed (with a warning to stderr in the malformed case).
pub fn load() -> Vec<Bookmark> {
    let path = bookmarks_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) if contents.trim().is_empty() => Vec::new(),
        Ok(contents) => match serde_json::from_str::<Vec<Bookmark>>(&contents) {
            Ok(list) => list,
            Err(err) => {
                eprintln!("Invalid bookmarks file at {}: {err}", path.display());
                Vec::new()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            eprintln!("Failed to read bookmarks at {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// Write the bookmark list to disk atomically (temp file + rename) so a
/// concurrent reader never observes a half-written file.
pub fn save(list: &[Bookmark]) -> Result<(), String> {
    let path = bookmarks_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create bookmarks directory: {e}"))?;
    }
    let contents =
        serde_json::to_string_pretty(list).map_err(|e| format!("Failed to serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)
        .map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("Failed to commit bookmarks file: {e}"))?;
    Ok(())
}

/// Insert or update a bookmark for the given `session_id`. Returns whether
/// an existing entry was updated (`true`) or a new one inserted (`false`).
pub fn upsert(bookmark: Bookmark) -> Result<bool, String> {
    let mut list = load();
    let mut updated = false;
    for existing in list.iter_mut() {
        if existing.session_id == bookmark.session_id {
            existing.session_name = bookmark.session_name.clone();
            existing.note = bookmark.note.clone();
            existing.updated_at = bookmark.updated_at;
            updated = true;
            break;
        }
    }
    if !updated {
        list.push(bookmark);
    }
    save(&list)?;
    Ok(updated)
}

/// Remove a bookmark by GUID prefix (>=4 chars to avoid ambiguity) or exact
/// note match. Returns the number of entries removed.
pub fn delete(query: &str) -> Result<usize, String> {
    if query.len() < 4 {
        return Err(format!(
            "Query '{query}' is too short — provide at least the first 4 characters of the session ID, or the exact note text"
        ));
    }
    let mut list = load();
    let before = list.len();
    list.retain(|b| !b.session_id.starts_with(query) && b.note != query);
    let removed = before - list.len();
    if removed > 0 {
        save(&list)?;
    }
    Ok(removed)
}

/// Information looked up from Copilot CLI's session-store for a given GUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotSessionInfo {
    /// Copilot's auto-generated summary for the session — what the user
    /// would see when listing Copilot sessions.
    pub name: Option<String>,
    /// The working directory the session was originally run in. Used when
    /// re-opening the session to anchor the new pane in the right place.
    pub cwd: Option<String>,
}

/// Look up the session name and original cwd from Copilot CLI's session
/// store. Returns `None` if the DB doesn't exist, the row is missing, or
/// any read step fails — the caller should fall back to a different source
/// (e.g., the user's tracked `first_prompts[0]` for non-Copilot agents).
///
/// Read-only access: opens the DB with `mode=ro` URI so we cannot
/// accidentally modify Copilot CLI's storage.
pub fn lookup_copilot_session(session_id: &str) -> Option<CopilotSessionInfo> {
    let db_path = dirs_home().join(".copilot/session-store.db");
    if !db_path.is_file() {
        return None;
    }
    let uri = format!("file:{}?mode=ro", db_path.display());
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT summary, cwd FROM sessions WHERE id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    row.map(|(summary, cwd)| CopilotSessionInfo {
        name: summary.filter(|s| !s.trim().is_empty()),
        cwd: cwd.filter(|s| !s.trim().is_empty()),
    })
}

/// Thin wrapper that returns just the session name. Kept for convenience.
pub fn lookup_copilot_session_name(session_id: &str) -> Option<String> {
    lookup_copilot_session(session_id).and_then(|i| i.name)
}

/// Return true if any bookmark records the given session ID.
pub fn is_bookmarked(list: &[Bookmark], session_id: &str) -> bool {
    list.iter().any(|b| b.session_id == session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `DOT_AGENT_DECK_BOOKMARKS` since cargo
    /// runs unit tests in parallel.
    static BOOKMARKS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup_env(tmp_path: &std::path::Path) -> Option<String> {
        let prev = std::env::var("DOT_AGENT_DECK_BOOKMARKS").ok();
        // SAFETY: single-threaded under the lock above.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_BOOKMARKS", tmp_path.to_str().unwrap());
        }
        prev
    }

    fn restore_env(prev: Option<String>) {
        // SAFETY: single-threaded under the lock above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_BOOKMARKS", v),
                None => std::env::remove_var("DOT_AGENT_DECK_BOOKMARKS"),
            }
        }
    }

    fn sample(id: &str, note: &str) -> Bookmark {
        Bookmark {
            session_id: id.into(),
            session_name: format!("session-{id}"),
            note: note.into(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let prev = setup_env(&path);
        let list = load();
        assert!(list.is_empty());
        restore_env(prev);
    }

    #[test]
    fn save_and_load_round_trip() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let prev = setup_env(&path);

        let bookmarks = vec![sample("aaaa-1111", "first"), sample("bbbb-2222", "second")];
        save(&bookmarks).unwrap();

        let loaded = load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].session_id, "aaaa-1111");
        assert_eq!(loaded[1].note, "second");

        restore_env(prev);
    }

    #[test]
    fn upsert_inserts_then_updates() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let prev = setup_env(&path);

        // First call: insert.
        let was_update = upsert(sample("xxxx-9999", "original")).unwrap();
        assert!(!was_update);
        assert_eq!(load().len(), 1);
        assert_eq!(load()[0].note, "original");

        // Second call with same session_id: update in place, list size stays.
        let was_update = upsert(sample("xxxx-9999", "updated")).unwrap();
        assert!(was_update);
        let after = load();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].note, "updated");

        restore_env(prev);
    }

    #[test]
    fn delete_by_id_prefix() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let prev = setup_env(&path);

        save(&[
            sample("aaaa-1111", "a"),
            sample("bbbb-2222", "b"),
            sample("cccc-3333", "c"),
        ])
        .unwrap();

        let removed = delete("bbbb").unwrap();
        assert_eq!(removed, 1);
        let after = load();
        assert_eq!(after.len(), 2);
        assert!(!after.iter().any(|b| b.session_id.starts_with("bbbb")));

        restore_env(prev);
    }

    #[test]
    fn delete_by_exact_note() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let prev = setup_env(&path);

        save(&[
            sample("aaaa-1111", "investigation"),
            sample("bbbb-2222", "investigation"),
            sample("cccc-3333", "other"),
        ])
        .unwrap();

        // Exact note match removes both.
        let removed = delete("investigation").unwrap();
        assert_eq!(removed, 2);
        let after = load();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].session_id, "cccc-3333");

        restore_env(prev);
    }

    #[test]
    fn delete_rejects_too_short_query() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let prev = setup_env(&path);

        save(&[sample("aaaa-1111", "x")]).unwrap();
        assert!(delete("aa").is_err());
        assert!(delete("").is_err());
        // 4 chars accepted (matches "aaaa").
        assert_eq!(delete("aaaa").unwrap(), 1);
        restore_env(prev);
    }

    #[test]
    fn delete_no_match_returns_zero() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.json");
        let prev = setup_env(&path);

        save(&[sample("aaaa-1111", "x")]).unwrap();
        let removed = delete("zzzz-9999").unwrap();
        assert_eq!(removed, 0);
        assert_eq!(load().len(), 1);
        restore_env(prev);
    }

    #[test]
    fn is_bookmarked_lookup() {
        let list = vec![sample("aaaa", "x"), sample("bbbb", "y")];
        assert!(is_bookmarked(&list, "aaaa"));
        assert!(!is_bookmarked(&list, "cccc"));
    }

    #[test]
    fn save_creates_parent_dir() {
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/b.json");
        let prev = setup_env(&path);

        save(&[sample("aaaa", "x")]).unwrap();
        assert!(path.exists());
        restore_env(prev);
    }

    #[test]
    fn lookup_copilot_session_name_missing_db() {
        // Point the home dir at a tempdir so the lookup misses the real DB.
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("USERPROFILE").ok();
        let prev_h = std::env::var("HOME").ok();
        // SAFETY: protected by the lock.
        unsafe {
            std::env::set_var("USERPROFILE", dir.path().to_str().unwrap());
            std::env::set_var("HOME", dir.path().to_str().unwrap());
        }
        assert!(lookup_copilot_session_name("any-id").is_none());
        // SAFETY: restore env.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match prev_h {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn lookup_copilot_session_name_unknown_id() {
        // Create a real DB but with no matching row.
        let _guard = BOOKMARKS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let copilot_dir = dir.path().join(".copilot");
        std::fs::create_dir_all(&copilot_dir).unwrap();
        let db = copilot_dir.join("session-store.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, summary TEXT, cwd TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, summary, cwd) VALUES ('known-id', 'Hello', '/repo/foo')",
            [],
        )
        .unwrap();
        drop(conn);

        let prev_home = std::env::var("USERPROFILE").ok();
        let prev_h = std::env::var("HOME").ok();
        // SAFETY: lock-protected.
        unsafe {
            std::env::set_var("USERPROFILE", dir.path().to_str().unwrap());
            std::env::set_var("HOME", dir.path().to_str().unwrap());
        }

        assert_eq!(
            lookup_copilot_session_name("known-id"),
            Some("Hello".to_string())
        );
        assert_eq!(lookup_copilot_session_name("missing-id"), None);

        let info = lookup_copilot_session("known-id").unwrap();
        assert_eq!(info.name.as_deref(), Some("Hello"));
        assert_eq!(info.cwd.as_deref(), Some("/repo/foo"));

        // SAFETY: restore.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match prev_h {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
