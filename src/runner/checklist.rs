//! The Ctrl+T checklist's source: the engine's task store.
//!
//! The factory gives every session a [`SqliteTaskStore`] scoped to its id in
//! the realm's `tasks.db` ([`crate::bootstrap::tasks_db`]); the task tools
//! (`task_create`, `task_update`) write there and nothing else does. The
//! runner reads the session's tasks from that store when a session opens
//! (a resumed one gets its list back) and after each of its task tool calls
//! completes, and sends the UI the list. A store that cannot be read leaves
//! the checklist as it was and is named at the next Ctrl+T.

use std::path::Path;

use meerkat_core::SessionId;
use meerkat_tools::SqliteTaskStore;
use meerkat_tools::builtin::TaskStore;
use meerkat_tools::builtin::types::TaskError;

use super::Runner;
use crate::ui::{Checklist, ChecklistView, Msg, NoticeLevel};

/// Whether a completed call of `tool` may have changed the session's tasks.
pub(super) fn changes_tasks(tool: &str) -> bool {
    matches!(tool, "task_create" | "task_update")
}

/// The tasks of `session` in the store at `path`, newest-updated last. No
/// file yet means no tasks (the store is not created by reading it).
///
/// # Errors
///
/// The store's error when the file exists but cannot be read.
pub async fn read_checklist(path: &Path, session: &SessionId) -> Result<Checklist, TaskError> {
    let exists = tokio::fs::try_exists(path)
        .await
        .map_err(|err| TaskError::StorageError(format!("{}: {err}", path.display())))?;
    if !exists {
        return Ok(Checklist::default());
    }
    let tasks = SqliteTaskStore::for_session(path, session.to_string())
        .list()
        .await?;
    Ok(Checklist::from_tasks(&tasks))
}

impl Runner {
    /// Read the current session's tasks and send them to the UI.
    pub(super) async fn refresh_checklist(&mut self) {
        let path = self.runtime.tasks_db.clone();
        let session = self.session.id().clone();
        let msg = match read_checklist(&path, &session).await {
            Ok(list) => Msg::Checklist(list),
            Err(err) => {
                tracing::warn!(error = %err, "cannot read the session's tasks");
                Msg::ChecklistUnavailable(err.to_string())
            }
        };
        self.send(msg);
    }

    /// [`crate::ui::Effect::SetChecklistView`]: record Ctrl+T's choice as
    /// the realm's `[ui] checklist`.
    pub(super) fn set_checklist_view(&self, view: ChecklistView) {
        let realm_file = self
            .runtime
            .scope
            .realm_root()
            .join(crate::config::RULES_FILE_NAME);
        let saved =
            crate::config::write_trusted(&realm_file, Some(&self.runtime.approval.trust), |path| {
                crate::config::persist_realm_checklist(path, view)
            });
        if let Err(err) = saved {
            self.notice(
                NoticeLevel::Warn,
                format!("the checklist view applies to this run but was not saved: {err}"),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use meerkat_tools::builtin::types::{NewTask, TaskStatus, TaskUpdate};

    use crate::ui::ItemState;

    #[tokio::test]
    async fn the_checklist_is_the_sessions_tasks_in_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let mine = SessionId::new();
        // No file: no tasks, and reading does not create one.
        assert!(read_checklist(&path, &mine).await.unwrap().is_empty());
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

        let list = read_checklist(&path, &mine).await.unwrap();
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
