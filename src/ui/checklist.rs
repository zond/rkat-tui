//! The task checklist under the input (Ctrl+T): the session's tasks as the
//! engine's task tools (`task_create`, `task_update`) keep them.
//!
//! The source is the engine's own task store: with session persistence the
//! factory gives each session a [`meerkat_tools::SqliteTaskStore`] scoped to
//! its id in `<store_path>/tasks.db`, so the runner reads that store (after
//! every task tool call of the session, and when a session opens or is
//! resumed) and hands the UI a [`Checklist`]. Nothing here parses tool
//! calls or results.

use meerkat_tools::builtin::types::{Task, TaskStatus};
use serde::{Deserialize, Serialize};

/// Most lines of the checklist panel: past this many items one line fewer
/// shows the newest-updated items and one says how many more there are.
pub const MAX_CHECKLIST_LINES: usize = 5;

/// Whether the checklist panel shows (`tui.toml` `[ui] checklist`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecklistView {
    /// Hidden until Ctrl+T (the default).
    #[default]
    Hidden,
    /// Shown under the input whenever the session has tasks.
    Shown,
}

impl ChecklistView {
    /// The other view.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Hidden => Self::Shown,
            Self::Shown => Self::Hidden,
        }
    }

    /// Whether the panel shows.
    #[must_use]
    pub fn is_shown(self) -> bool {
        self == Self::Shown
    }
}

/// Where a checklist item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Finished.
    Done,
}

impl ItemState {
    /// The box the panel draws before the item.
    #[must_use]
    pub fn glyph(self) -> char {
        match self {
            Self::Pending => '☐',
            Self::InProgress => '◐',
            Self::Done => '☑',
        }
    }
}

impl From<&TaskStatus> for ItemState {
    fn from(status: &TaskStatus) -> Self {
        match status {
            TaskStatus::Pending => Self::Pending,
            TaskStatus::InProgress => Self::InProgress,
            TaskStatus::Completed => Self::Done,
        }
    }
}

/// One checklist line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistItem {
    /// The task's subject.
    pub text: String,
    /// Its state.
    pub state: ItemState,
}

/// The session's tasks, newest-updated last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checklist {
    items: Vec<ChecklistItem>,
}

impl Checklist {
    /// The checklist of `tasks` (as the store lists them, oldest created
    /// first): ordered by last update, ties keeping the store's order.
    #[must_use]
    pub fn from_tasks(tasks: &[Task]) -> Self {
        let mut ordered: Vec<&Task> = tasks.iter().collect();
        ordered.sort_by_key(|task| updated_millis(task));
        Self {
            items: ordered
                .into_iter()
                .map(|task| ChecklistItem {
                    text: task.subject.clone(),
                    state: ItemState::from(&task.status),
                })
                .collect(),
        }
    }

    /// A checklist of exactly `items`, in this order.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_items(items: Vec<ChecklistItem>) -> Self {
        Self { items }
    }

    /// Every item, newest-updated last.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn items(&self) -> &[ChecklistItem] {
        &self.items
    }

    /// Whether there are no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// What the panel draws: the items it shows (the newest-updated, in
    /// order) and how many older ones it leaves out. Up to
    /// [`MAX_CHECKLIST_LINES`] items all show; beyond that one fewer, and
    /// a line counts the rest.
    #[must_use]
    pub fn window(&self) -> (&[ChecklistItem], usize) {
        if self.items.len() <= MAX_CHECKLIST_LINES {
            return (&self.items, 0);
        }
        let shown = MAX_CHECKLIST_LINES - 1;
        let more = self.items.len() - shown;
        (&self.items[more..], more)
    }

    /// Lines the panel takes.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.items.len().min(MAX_CHECKLIST_LINES)
    }
}

/// A task's last update in milliseconds since the epoch (an unreadable
/// stamp sorts first, keeping the store's order among such).
fn updated_millis(task: &Task) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&task.updated_at)
        .map(|at| at.timestamp_millis())
        .unwrap_or(i64::MIN)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use meerkat_tools::builtin::types::{TaskId, TaskPriority};

    fn task(subject: &str, status: TaskStatus, updated_at: &str) -> Task {
        Task {
            id: TaskId::from_string(subject),
            subject: subject.into(),
            description: String::new(),
            status,
            priority: TaskPriority::default(),
            labels: Vec::new(),
            blocks: Vec::new(),
            created_at: "2026-11-07T10:00:00+00:00".into(),
            updated_at: updated_at.into(),
            created_by_session: None,
            updated_by_session: None,
            owner: None,
            metadata: std::collections::HashMap::new(),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn tasks_map_to_items_newest_updated_last() {
        let list = Checklist::from_tasks(&[
            task("write", TaskStatus::InProgress, "2026-11-07T10:00:05+00:00"),
            task("read", TaskStatus::Completed, "2026-11-07T10:00:03.5+00:00"),
            task("test", TaskStatus::Pending, "2026-11-07T10:00:01+00:00"),
            task("ship", TaskStatus::Pending, "2026-11-07T10:00:01+00:00"),
        ]);
        let got: Vec<(&str, ItemState)> = list
            .items()
            .iter()
            .map(|i| (i.text.as_str(), i.state))
            .collect();
        assert_eq!(
            got,
            [
                ("test", ItemState::Pending),
                ("ship", ItemState::Pending),
                ("read", ItemState::Done),
                ("write", ItemState::InProgress),
            ]
        );
        assert_eq!(ItemState::Pending.glyph(), '☐');
        assert_eq!(ItemState::InProgress.glyph(), '◐');
        assert_eq!(ItemState::Done.glyph(), '☑');
    }

    #[test]
    fn the_window_keeps_the_newest_and_counts_the_rest() {
        let items = |n: usize| {
            Checklist::from_items(
                (0..n)
                    .map(|i| ChecklistItem {
                        text: format!("t{i}"),
                        state: ItemState::Pending,
                    })
                    .collect(),
            )
        };
        assert_eq!(items(0).window(), (&[][..], 0));
        let five = items(5);
        assert_eq!(five.window().0.len(), 5);
        assert_eq!(five.window().1, 0);
        assert_eq!(five.rows(), 5);
        let eight = items(8);
        let (shown, more) = eight.window();
        assert_eq!(more, 4);
        assert_eq!(
            shown.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["t4", "t5", "t6", "t7"]
        );
        assert_eq!(eight.rows(), 5);
    }

    #[test]
    fn the_view_toggles() {
        assert_eq!(ChecklistView::default(), ChecklistView::Hidden);
        assert_eq!(ChecklistView::Hidden.toggled(), ChecklistView::Shown);
        assert!(ChecklistView::Shown.is_shown());
        assert!(!ChecklistView::Shown.toggled().is_shown());
    }
}
