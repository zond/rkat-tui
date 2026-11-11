//! The workbench: what the client (`crate::client`) does on this side of
//! the agent.
//!
//! - [`shell`]: `!` shell mode: the user's own command, run here, its
//!   output handed to the agent as system context.
//! - [`memory`]: `/remember` and `/memory` (the instruction files are local;
//!   the new content reaches the agent as system context).
//! - [`export`]: `/export`, the transcript as plain text.
//! - [`context`]: `/context`'s breakdown of what the context holds.
//! - [`cost`] and [`cost_store`]: the price table (overrides, endpoint
//!   prices, the community list refreshed in the background) and the
//!   per-session cost record.
//! - [`models`]: the endpoints' model lists and the `/model` picker.
//! - [`unsent`]: the messages the agent never got, saved on quit and
//!   announced at the next start.
//! - [`app`]: the UI the client starts ([`app::build_app`]) and the log file.
//!
//! The agent is reached through one call, [`AgentContext::append_system_context`]
//! over the client's connection to the daemon. The turn machinery is not
//! here: the client tracks turns from the daemon's event stream.

pub mod app;
pub mod context;
pub mod cost;
pub mod cost_store;
pub mod export;
pub mod memory;
pub mod models;
pub mod shell;
pub mod unsent;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};

use crate::bootstrap::RuntimeScope;
use crate::endpoints::EndpointRegistry;
use crate::models::{FetchResults, ModelCache, ModelLister};
use crate::pricing::{PriceTable, PricingSetup};
use crate::ui::delivery::PendingId;
use crate::ui::{Msg, NoticeLevel};
use crate::user_shell::UserShell;

pub use app::AppSetup;
#[cfg(any(test, feature = "test-support"))]
pub use app::build_app;
pub use unsent::SavedUnsent;

/// Why the agent did not take a context note ([`AgentContext`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// No connection to the agent now.
    #[error("not attached to the daemon")]
    NotAttached,
    /// The daemon refused it or did not answer in time.
    #[error("{0}")]
    NotTaken(String),
}

/// What hands text to the agent as durable system context (the client's
/// connection to the daemon, `crate::client::runner`).
pub trait ContextSink: Send + Sync {
    /// Append `text` under the idempotency `key`.
    fn append(
        &self,
        text: String,
        key: String,
    ) -> futures::future::BoxFuture<'static, Result<(), ContextError>>;
}

/// The one thing the workbench asks of the agent: take text as durable
/// system context, idempotent per key, read from its next model call
/// (`None`: no agent to take it now).
#[derive(Clone)]
pub struct AgentContext(Option<Arc<dyn ContextSink>>);

impl AgentContext {
    /// The agent behind `sink`.
    #[must_use]
    pub fn new(sink: Option<Arc<dyn ContextSink>>) -> Self {
        Self(sink)
    }

    /// Append `text` under the idempotency `key`.
    ///
    /// # Errors
    ///
    /// [`ContextError`]: not attached, or the daemon's refusal or silence.
    pub async fn append_system_context(
        &self,
        text: String,
        key: String,
    ) -> Result<(), ContextError> {
        match &self.0 {
            Some(sink) => sink.append(text, key).await,
            None => Err(ContextError::NotAttached),
        }
    }
}

/// What a [`Workbench`] is built from.
pub struct WorkbenchParts {
    /// Where the runner's messages to the UI go.
    pub ui_tx: mpsc::UnboundedSender<Msg>,
    /// The workspace and realm.
    pub scope: RuntimeScope,
    /// The user's `!` shell, or why there is none.
    pub user_shell: Result<Arc<UserShell>, String>,
    /// The cost readout's prices at start and how to refresh them.
    pub pricing: PricingSetup,
    /// This process's LLM endpoints (the picker, `/model`).
    pub endpoints: EndpointRegistry,
    /// Fetches the endpoints' model lists.
    pub lister: ModelLister,
    /// Lists start-up already fetched (a bare `--model` id).
    pub models: Option<ModelCache>,
}

/// The state behind the workbench's effects.
pub struct Workbench {
    ui_tx: mpsc::UnboundedSender<Msg>,
    scope: RuntimeScope,
    /// What runs `!` commands, or why nothing does.
    user_shell: Result<Arc<UserShell>, String>,
    /// The `!` command running now.
    shell_task: Option<shell::ShellTask>,
    /// `/memory`: the file being edited and its bytes before (`None`:
    /// missing), to tell an edit from an unchanged close.
    memory_before: Option<(PathBuf, Option<Vec<u8>>)>,
    /// Prices for the cost readout.
    prices: Arc<PriceTable>,
    /// The start-up pricing, until [`Workbench::start`] spawns its refresh.
    pricing: Option<PricingSetup>,
    /// A community price-list refresh running in the background.
    price_fetch: Option<cost::PriceFetch>,
    endpoints: EndpointRegistry,
    lister: ModelLister,
    /// Model lists of every endpoint, once fetched.
    models: Option<ModelCache>,
    /// A model-list fetch running in the background.
    model_fetch: Option<models::ModelFetch>,
    /// The picker opens when the running fetch finishes.
    picker_waiting: bool,
    /// A `/model <id>` waiting for the lists.
    pending_set: Option<String>,
    /// Messages the UI still held when the user quit.
    held_unsent: Vec<(Option<PendingId>, String)>,
    /// The cost records, written off the client's loop.
    costs: Arc<cost::CostWriter>,
}

impl Workbench {
    /// A workbench on `parts`; [`Workbench::start`] starts it.
    #[must_use]
    pub fn new(parts: WorkbenchParts) -> Self {
        let WorkbenchParts {
            ui_tx,
            scope,
            user_shell,
            pricing,
            endpoints,
            lister,
            models,
        } = parts;
        Self {
            ui_tx,
            scope,
            user_shell,
            shell_task: None,
            memory_before: None,
            prices: Arc::new(pricing.table.clone()),
            pricing: Some(pricing),
            price_fetch: None,
            endpoints,
            lister,
            models,
            model_fetch: None,
            picker_waiting: false,
            pending_set: None,
            held_unsent: Vec::new(),
            costs: Arc::default(),
        }
    }

    /// Announce unsent messages of an earlier run, send the prices and
    /// start refreshing the community price list when it is due.
    pub fn start(&mut self) {
        if let Some(text) = unsent::unsent_notice(&self.scope.realm_root()) {
            self.notice(NoticeLevel::Warn, text);
        }
        self.push_prices();
        if let Some(pricing) = self.pricing.take()
            && let Some(url) = pricing.refresh_url()
        {
            self.price_fetch = Some(cost::spawn_price_fetch(
                url.to_string(),
                pricing.cache_path.clone(),
            ));
        }
    }

    /// The background work finishing: a community price refresh is applied
    /// here; a model-list fetch is returned for the runner to finish with
    /// [`Workbench::lists_arrived`]. Cancel-safe: pending while nothing
    /// runs.
    ///
    /// # Errors
    ///
    /// The model-list task's [`JoinError`] (it panicked or was aborted).
    pub async fn background(&mut self) -> Result<FetchResults, JoinError> {
        loop {
            tokio::select! {
                joined = join(&mut self.model_fetch) => {
                    self.model_fetch = None;
                    return joined;
                }
                joined = join(&mut self.price_fetch) => {
                    self.price_fetch = None;
                    self.price_fetch_finished(joined);
                }
            }
        }
    }

    /// Work in another workspace or realm from now on (a re-home): its
    /// scope, its `!` shell and its endpoints. A running `!` command is
    /// the caller's to cancel first.
    pub fn reroot(
        &mut self,
        scope: RuntimeScope,
        user_shell: Result<Arc<UserShell>, String>,
        endpoints: EndpointRegistry,
    ) {
        self.scope = scope;
        self.user_shell = user_shell;
        self.endpoints = endpoints;
        self.memory_before = None;
    }

    /// The cost readout's prices.
    #[must_use]
    pub fn prices(&self) -> &PriceTable {
        &self.prices
    }

    /// Hold the messages the UI still had when the user quit
    /// ([`crate::ui::Effect::SaveUnsent`]).
    pub fn hold_unsent(&mut self, held: Vec<(Option<PendingId>, String)>) {
        self.held_unsent.extend(held);
    }

    /// Quit: drop a running `!` command, then save the held messages the
    /// agent did not get (`delivered` names those it did).
    pub fn save_unsent(&mut self, delivered: &[PendingId]) -> Option<SavedUnsent> {
        unsent::save_unsent(
            &self.scope.realm_root(),
            std::mem::take(&mut self.held_unsent),
            delivered,
        )
    }

    /// The prompt as the agent receives it: the files and directories its
    /// `@` mentions name attached ([`crate::mentions::expand_mentions`]).
    /// What was attached, and the sealed paths left out, go to the UI as
    /// [`Msg::Mentions`], so its `+ N files` line names what was sent.
    pub fn expand_mentions(&self, text: &str) -> String {
        if !text.contains('@') {
            return text.to_string();
        }
        let expanded = crate::mentions::expand_mentions(text, &self.scope.project_root);
        if !expanded.attached.is_empty() || !expanded.sealed.is_empty() {
            self.send(Msg::Mentions {
                attached: expanded.attached,
                sealed: expanded.sealed,
            });
        }
        expanded.text
    }

    fn send(&self, msg: Msg) {
        let _ = self.ui_tx.send(msg);
    }

    fn notice(&self, level: NoticeLevel, text: impl Into<String>) {
        self.send(Msg::Notice {
            level,
            text: text.into(),
        });
    }
}

/// `task`'s result; pending while there is none.
async fn join<T>(task: &mut Option<JoinHandle<T>>) -> Result<T, JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}
