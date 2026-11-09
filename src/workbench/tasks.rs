//! The Ctrl+T checklist's source: the engine's task store.
//!
//! The factory gives every session a [`SqliteTaskStore`] scoped to its id in
//! the realm's `tasks.db` ([`crate::bootstrap::tasks_db`]); the task tools
//! (`task_create`, `task_update`) write there and nothing else does. A
//! runner reads the session's tasks when a session opens (a resumed one
//! gets its list back) and after each of its task tool calls completes, and
//! sends the UI the list: the in-process runner from the store itself, the
//! attach runner through the daemon's `rkat/tasks`, which reads it here. A
//! store that cannot be read leaves the checklist as it was and is named at
//! the next Ctrl+T.

use std::path::Path;

use meerkat_core::SessionId;
use meerkat_tools::SqliteTaskStore;
use meerkat_tools::builtin::TaskStore;
use meerkat_tools::builtin::types::{Task, TaskError};

/// Whether a completed call of `tool` may have changed the session's tasks.
pub fn changes_tasks(tool: &str) -> bool {
    matches!(tool, "task_create" | "task_update")
}

/// The tasks of `session` in the store at `path`, as the store lists them
/// (what the daemon's `rkat/tasks` answers). No file yet means no tasks.
///
/// # Errors
///
/// The store's error when the file exists but cannot be read.
pub async fn read_tasks(path: &Path, session: &SessionId) -> Result<Vec<Task>, TaskError> {
    let exists = tokio::fs::try_exists(path)
        .await
        .map_err(|err| TaskError::StorageError(format!("{}: {err}", path.display())))?;
    if !exists {
        return Ok(Vec::new());
    }
    SqliteTaskStore::for_session(path, session.to_string())
        .list()
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use meerkat_tools::builtin::types::{NewTask, TaskStatus, TaskUpdate};

    use crate::ui::{Checklist, ItemState};

    #[tokio::test]
    async fn the_checklist_is_the_sessions_tasks_in_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let mine = SessionId::new();
        // No file: no tasks, and reading does not create one.
        assert!(Checklist::from_tasks(&read_tasks(&path, &mine).await.unwrap()).is_empty());
        assert!(!path.exists());

        let store = SqliteTaskStore::for_session(&path, mine.to_string());
        let new = |subject: &str| NewTask {
            subject: subject.into(),
            description: String::new(),
            ..NewTask::default()
        };
        let first = store.create(new("read the code"), None).await.unwrap();
        store.create(new("write the fix"), None).await.unwrap();
        let other = SqliteTaskStore::for_session(&path, SessionId::new().to_string());
        other.create(new("someone else's"), None).await.unwrap();
        // Stamps have millisecond resolution: let the update land later.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store
            .update(
                &first.id,
                TaskUpdate {
                    status: Some(TaskStatus::Completed),
                    ..TaskUpdate::default()
                },
                None,
            )
            .await
            .unwrap();

        let list = Checklist::from_tasks(&read_tasks(&path, &mine).await.unwrap());
        let got: Vec<(&str, ItemState)> = list
            .items()
            .iter()
            .map(|i| (i.text.as_str(), i.state))
            .collect();
        assert_eq!(
            got,
            [
                ("write the fix", ItemState::Pending),
                ("read the code", ItemState::Done),
            ]
        );
    }

    #[test]
    fn only_the_writing_task_tools_refresh() {
        assert!(changes_tasks("task_create"));
        assert!(changes_tasks("task_update"));
        assert!(!changes_tasks("task_list"));
        assert!(!changes_tasks("task_get"));
        assert!(!changes_tasks("shell"));
    }
}
