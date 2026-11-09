//! The in-process runner's Ctrl+T checklist: the session's tasks read
//! with [`crate::workbench::tasks`], the view saved in the realm file.

use super::Runner;
use crate::ui::{ChecklistView, Msg, NoticeLevel};
use crate::workbench::tasks::read_checklist;

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
