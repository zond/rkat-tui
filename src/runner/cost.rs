//! A session's cost totals on the in-process runner (the price table and
//! the cost record are the workbench's, [`crate::workbench::cost`]). A
//! resumed session's record is reconciled with its authoritative
//! cumulative usage: usage the record does not account for is attributed to
//! the session's model and the readout becomes approximate (`~`). A session
//! the TUI never recorded (created by another surface) starts from its
//! cumulative usage, approximate.

use meerkat_core::Session;

use crate::ui::SessionCost;
use crate::workbench::cost_store::load_cost;

/// The cost totals of `session`: its record reconciled with its
/// authoritative cumulative usage, else that usage attributed to `model`
/// (approximate).
pub(super) fn session_cost(
    realm_root: &std::path::Path,
    session: &Session,
    model: &str,
) -> SessionCost {
    let usage = session.total_usage();
    match load_cost(realm_root, &session.id().to_string()) {
        Some(mut recorded) => {
            recorded.reconcile(model, &usage);
            recorded
        }
        None => SessionCost::derived(model, &usage),
    }
}
