//! The daemon counts what every model call used (`rkat_tui::daemon::usage`):
//! a turn no client saw, the same totals for every client, a helper's and
//! the classifier's calls under their own models, and the totals across a
//! daemon restart. A client only renders them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::sync::Arc;

use common::{MODEL, ScriptedLlmClient, Step, Workspace, within};
use rig::{Attached, Client, Running, Setup};
use rkat_tui::cost::SessionCost;
use rkat_tui::ui::{Departure, Effect, Msg, StatusUpdate};
use serde_json::json;

/// The `(input, output)` tokens `cost` holds for `model`.
fn of(cost: &SessionCost, model: &str) -> Option<(u64, u64)> {
    cost.per_model
        .get(model)
        .map(|totals| (totals.input, totals.output))
}

/// The newest totals `client` was shown.
fn shown(client: &Client) -> SessionCost {
    client
        .seen
        .iter()
        .rev()
        .find_map(|m| match m {
            Msg::Status(StatusUpdate::CostTotals(cost)) => Some(cost.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Whether `m` shows totals with `tokens` for [`MODEL`].
fn totals_of(m: &Msg, tokens: (u64, u64)) -> bool {
    matches!(m, Msg::Status(StatusUpdate::CostTotals(cost)) if of(cost, MODEL) == Some(tokens))
}

/// A turn that ran after its client detached is counted by the daemon: a
/// client attaching later shows it, and so does one attaching to the
/// daemon after a restart (the record was written when it stopped), with
/// the context gauge seeded from the same record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_no_client_saw_is_counted_and_survives_a_restart() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![Step::Gated(
        Arc::clone(&gate),
        Box::new(Step::TextUsage("done while away", 1_234)),
    )]);
    let setup = Setup::default();
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("work while I am away".into()));
    llm.wait_call(0).await;
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    within(async {
        while daemon
            .status()
            .await
            .agents
            .iter()
            .any(|row| row.clients > 0)
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    // The model call happens with nobody attached.
    gate.notify_one();
    daemon.idle(&daemon.agent).await;
    let mut later = Client::attach(&ws.root).await;
    later.saw(|m| totals_of(m, (1_234, 5))).await;
    later.quit().await;
    daemon.stop().await;

    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let mut again = Client::attach(&ws.root).await;
    again.saw(|m| totals_of(m, (1_234, 5))).await;
    // The context gauge is seeded from the daemon's record of the last
    // model call, also after the restart.
    // Everything sent plus the reply.
    let context = 1_234 + 5;
    again
        .saw(|m| {
            matches!(m, Msg::Status(StatusUpdate::Context { used: Some(used), approx: false })
                if *used == context)
        })
        .await;
    again.quit().await;
    daemon.stop().await;
}

/// Two clients of one agent are shown the same totals: the daemon's,
/// pushed to both after the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_show_the_same_totals() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let mut a = Attached::start(&ws.root, &llm).await;
    let mut b = Client::attach(&ws.root).await;
    a.send(Effect::StartTurn("first".into()));
    let _ = a.turn_finished().await;
    b.send(Effect::StartTurn("second".into()));
    let _ = b.turn_finished().await;
    a.saw(|m| totals_of(m, (20, 10))).await;
    b.saw(|m| totals_of(m, (20, 10))).await;
    assert_eq!(shown(&a), shown(&b));
    b.quit().await;
    a.quit().await;
}

/// A classifier call is counted to the conversation whose tool call it
/// judged, under the classifier's model, apart from the agent's own calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_classifier_s_calls_are_counted_under_its_model() {
    const JUDGE: &str = "claude-opus-4-8";
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "c1",
            name: "shell",
            args: json!({"command": "curl -s http://127.0.0.1:9/notes"}),
        },
        // The classifier's quick check.
        Step::TextUsage("ALLOW", 300),
        Step::Text("fetched"),
    ]);
    let setup = Setup {
        llm_classifier: true,
        ..Setup::args(&["--set", &format!("auto.classifier_model={JUDGE}")])
    };
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.send(Effect::StartTurn("fetch the notes".into()));
    let _ = rig.turn_finished().await;
    assert_eq!(llm.recorded()[1].model, JUDGE, "the classifier's call");
    rig.saw(|m| {
        matches!(m, Msg::Status(StatusUpdate::CostTotals(cost))
            if of(cost, JUDGE) == Some((300, 5)) && of(cost, MODEL) == Some((20, 10)))
    })
    .await;
    rig.quit().await;
}

/// A model call that completes right before the daemon stops is in the
/// usage record it leaves: the stop lets each agent's event watcher count
/// what its stream already holds before the ledger is flushed (it used to
/// abort the watchers first). The stop follows the turn's end at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_completed_just_before_the_stop_is_recorded() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("last words")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let session = daemon.session.clone();
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("one more".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "last words");
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    daemon.stop().await;
    let realm = common::realm_of(&ws.root);
    let record = rkat_tui::daemon::usage::read_record(
        &rkat_tui::daemon::usage::Account::Session(session).path(&realm),
    );
    assert_eq!(of(&record.cost, MODEL), Some((10, 5)), "{record:?}");
}
