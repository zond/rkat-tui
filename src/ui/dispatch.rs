//! Running a parsed slash command, and the idle guard
//! ([`App::blocker`]) the session-level ones share.

use std::time::Instant;

use super::deferred::Deferred;
use super::{
    App, AutoCommand, Busy, COMPACT_FOCUS_UNSUPPORTED, Command, CompactCommand, CompactFailure,
    Effect, Modal, ModelCommand, NoticeLevel, RulesCommand, commands,
};
use crate::approval::mode::PermissionMode;

impl App {
    /// `/context`'s first lines: the gauge (used / window, percent,
    /// estimated or not) and the compaction policy (as `/compact status`).
    pub fn context_header(&self) -> String {
        let s = &self.status;
        let est = |approx: bool| if approx { " (estimated)" } else { "" };
        let window = s.context_window.filter(|w| *w > 0);
        let gauge = match (s.context_used, window) {
            (Some(used), Some(window)) => format!(
                "context: {used} of {window} tokens ({}%){}; window{}",
                used.saturating_mul(100) / window,
                est(s.context_approx),
                if s.window_approx {
                    " estimated"
                } else {
                    " from the model profile"
                },
            ),
            (Some(used), None) => format!(
                "context: {used} tokens{}; window unknown",
                est(s.context_approx)
            ),
            (None, Some(window)) => format!(
                "context: unknown until the first model call; window {window} tokens{}",
                est(s.window_approx)
            ),
            (None, None) => "context: unknown until the first model call; window unknown".into(),
        };
        format!(
            "{gauge}\ncompaction: {}",
            self.gauge.policy().describe(s.context_window)
        )
    }

    /// What keeps a session-level command (`/rewind`, `/clear`,
    /// `/resume`, `/model`, `/init`, `/compact`) from running now, if
    /// anything. One guard for all of them so they cannot drift apart.
    pub(crate) fn blocker(&self) -> Option<Busy> {
        if self.turn.is_running() {
            Some(Busy::Turn)
        } else if self.compaction.busy() {
            Some(Busy::Compacting)
        } else if self.shell.running.is_some() {
            Some(Busy::Shell)
        } else if self.delivery.draining {
            // A held command runs now: what was held behind it waits.
            None
        } else if !self.pending.is_empty() || self.delivery.waits() {
            Some(Busy::Queued)
        } else {
            None
        }
    }

    /// Whether the session is idle; pushes what is in the way when not.
    pub(super) fn idle_or_notice(&mut self) -> bool {
        match self.blocker() {
            Some(busy) => {
                self.transcript
                    .push_notice(NoticeLevel::Warn, busy.to_string());
                self.scroll.bottom();
                false
            }
            None => true,
        }
    }

    /// `/model <id>` or a picker selection: switch when idle; during a
    /// turn the switch waits for its end.
    pub(super) fn switch_model(&mut self, model: String, fx: &mut Vec<Effect>) {
        if self.defer(
            format!("/model {model}"),
            Deferred::Choice(Effect::SetModel(model.clone())),
        ) {
            return;
        }
        if self.idle_or_notice() {
            fx.push(Effect::SetModel(model));
        }
    }

    /// `cmd` (typed as `text`) waits for the turn's end when it must
    /// ([`App::defers`]); whether it does.
    fn held_typed(&mut self, text: &str, cmd: Command) -> bool {
        self.defer(text.to_string(), Deferred::Typed(cmd))
    }

    /// `/resume <reference>` or the resume picker's choice: switch when
    /// idle; during a turn the switch waits for its end.
    pub(super) fn resume(&mut self, reference: String, fx: &mut Vec<Effect>) {
        if self.defer(
            format!("/resume {reference}"),
            Deferred::Choice(Effect::ResumeSession(reference.clone())),
        ) {
            return;
        }
        if self.idle_or_notice() {
            fx.push(Effect::ResumeSession(reference));
        }
    }

    pub(super) fn run_command(&mut self, cmd: Command, now: Instant, fx: &mut Vec<Effect>) {
        match cmd {
            Command::Quit => self.request_quit(now, fx),
            Command::Detach => self.request_detach(fx),
            Command::Help => {
                self.transcript
                    .push_notice(NoticeLevel::Info, commands::help_text());
            }
            Command::Copy(n) => self.copy_answer(n),
            Command::Context => fx.push(Effect::ContextReport(self.context_header())),
            Command::Btw(question) => self.ask_btw(question, now, fx),
            Command::Status => fx.push(Effect::StatusReport),
            Command::DaemonStop => fx.push(Effect::DaemonStop),
            Command::DaemonStart => fx.push(Effect::DaemonStart),
            Command::Agents(Some(name)) => {
                if !self.defer(
                    format!("/agents {name}"),
                    Deferred::Choice(Effect::Agents(Some(name.clone()))),
                ) {
                    fx.push(Effect::Agents(Some(name)));
                }
            }
            Command::Agents(None) => fx.push(Effect::Agents(None)),
            Command::Wire(target) => fx.push(Effect::Wire(target)),
            Command::Unwire(target) => fx.push(Effect::Unwire(target)),
            Command::Audit(lines) => fx.push(Effect::Audit(lines)),
            Command::Schedules(action) => fx.push(Effect::Schedules(action)),
            Command::Rehome(Some(reference)) => {
                if !self.defer(
                    format!("/rehome {reference}"),
                    Deferred::Choice(Effect::Rehome(Some(reference.clone()))),
                ) {
                    fx.push(Effect::Rehome(Some(reference)));
                }
            }
            Command::Rehome(None) => fx.push(Effect::Rehome(None)),
            Command::Init => {
                if self.held_typed("/init", Command::Init) {
                    return;
                }
                if self.idle_or_notice() {
                    self.start_turn(commands::INIT_PROMPT.to_string(), now, fx);
                }
            }
            Command::Memory(Some(target)) => fx.push(Effect::OpenMemory(target)),
            Command::Memory(None) => fx.push(Effect::RequestMemoryPicker),
            Command::Export(path) => fx.push(Effect::Export {
                path,
                text: self.transcript.export_text(),
            }),
            Command::Cost => {
                let text = self.status.cost.breakdown(&self.status.prices);
                self.transcript.push_notice(NoticeLevel::Info, text);
            }
            Command::Clear => {
                if self.held_typed("/new", Command::Clear) {
                    return;
                }
                if self.idle_or_notice() {
                    // The client runner resets the transcript and counters once
                    // the new session is live (and keeps them on failure).
                    fx.push(Effect::NewSession);
                }
            }
            // The picker opens at once; its choice waits for the turn's end.
            Command::Resume(None) => fx.push(Effect::RequestResumePicker),
            Command::Resume(Some(reference)) => self.resume(reference, fx),
            Command::Remember(text) => {
                self.transcript
                    .push_notice(NoticeLevel::Info, format!("remembering: {text}"));
                fx.push(Effect::Remember(text));
            }
            Command::Compact(CompactCommand::Status) => {
                let text = self.gauge.policy().describe(self.status.context_window);
                self.transcript.push_notice(NoticeLevel::Info, text);
                // The prepare instruction is the daemon's.
                fx.push(Effect::CompactPrepareStatus);
            }
            Command::Compact(CompactCommand::Prepare(change)) => {
                fx.push(Effect::SetSettings {
                    changes: change.settings(),
                    user: change.user,
                });
            }
            Command::Set { changes, user } => fx.push(Effect::SetSettings { changes, user }),
            Command::Settings(filter) => fx.push(Effect::RequestSettingsPicker(filter)),
            Command::Compact(CompactCommand::PrepareShow) => {
                fx.push(Effect::CompactPrepareShow);
            }
            Command::Compact(CompactCommand::PrepareRefused(why)) => {
                self.transcript.push_notice(NoticeLevel::Error, why);
            }
            Command::Compact(cmd @ (CompactCommand::Now | CompactCommand::Focused(_)))
                if self.turn.is_running() =>
            {
                // During a turn the request goes now: the engine compacts
                // before the run's next model request.
                if matches!(cmd, CompactCommand::Focused(_)) {
                    self.transcript
                        .push_notice(NoticeLevel::Warn, COMPACT_FOCUS_UNSUPPORTED);
                }
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    "asking the engine to compact before the run's next model request…",
                );
                self.scroll.bottom();
                fx.push(Effect::CompactNow);
            }
            Command::Compact(cmd @ (CompactCommand::Now | CompactCommand::Focused(_))) => {
                if !self.idle_or_notice() {
                    return;
                }
                if self.status.context_used.is_none() {
                    self.transcript
                        .push_notice(NoticeLevel::Info, CompactFailure::NothingYet.to_string());
                    return;
                }
                if matches!(cmd, CompactCommand::Focused(_)) {
                    self.transcript
                        .push_notice(NoticeLevel::Warn, COMPACT_FOCUS_UNSUPPORTED);
                }
                self.compaction.ask();
                self.transcript
                    .push_notice(NoticeLevel::Info, "asking the engine to compact…");
                self.scroll.bottom();
                fx.push(Effect::CompactNow);
            }
            Command::Model(ModelCommand::Set(model)) => self.switch_model(model, fx),
            // The picker opens at once (during a turn too); its choice waits
            // for the turn's end ([`Self::switch_model`]).
            Command::Model(cmd @ (ModelCommand::Pick | ModelCommand::Refresh)) => {
                fx.push(Effect::RequestModelPicker {
                    refresh: cmd == ModelCommand::Refresh,
                });
            }
            Command::Mode(None) => self.show_modal(Modal::List(super::picker::mode_picker(
                self.status.mode,
                self.status.auto_paused,
            ))),
            Command::Auto(AutoCommand::Status) => {
                let text = format!(
                    "permission mode: {} (Shift+Tab cycles manual → accept edits → plan → auto; /mode <name> sets one)",
                    self.status.mode_text()
                );
                self.transcript.push_notice(NoticeLevel::Info, text);
            }
            // Setting auto mode resumes it after a denial-limit pause.
            Command::Auto(AutoCommand::On | AutoCommand::Resume) => {
                fx.push(Effect::SetMode(PermissionMode::Auto));
            }
            Command::Auto(AutoCommand::Off) => fx.push(Effect::SetMode(PermissionMode::Manual)),
            Command::Auto(AutoCommand::Stats) => fx.push(Effect::AutoStats),
            Command::Auto(AutoCommand::Model(None, _)) => fx.push(Effect::RequestClassifierPicker),
            Command::Auto(AutoCommand::Model(Some(reference), user)) => {
                fx.push(Effect::SetClassifierModel(reference, user));
            }
            Command::Auto(AutoCommand::Check(selection)) => {
                fx.push(Effect::ClassifierCheck(selection));
            }
            Command::Mode(Some(mode)) => fx.push(Effect::SetMode(mode)),
            Command::Rules(RulesCommand::Pick(section)) => {
                fx.push(Effect::RequestPermissions(section));
            }
            Command::Rules(RulesCommand::List) => fx.push(Effect::ListRules),
            Command::Rules(RulesCommand::Add {
                effect,
                tool,
                pattern,
                user,
            }) => fx.push(Effect::AddRule {
                effect,
                tool,
                pattern,
                user,
            }),
            Command::Rules(RulesCommand::Remove(n)) => fx.push(Effect::RemoveRule(n)),
            Command::TrustProject => fx.push(Effect::TrustProject),
            Command::Helpers(cmd) => self.helpers_command(cmd),
        }
    }
}
