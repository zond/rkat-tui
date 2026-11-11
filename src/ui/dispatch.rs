//! Running a parsed slash command, and the idle guard
//! ([`App::blocker`]) the session-level ones share.

use std::time::Instant;

use super::{
    App, AutoCommand, Busy, COMPACT_FOCUS_UNSUPPORTED, Command, CompactCommand, CompactFailure,
    Effect, ModelCommand, NoticeLevel, RulesCommand, commands,
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
        if self.running {
            Some(Busy::Turn)
        } else if self.compacting {
            Some(Busy::Compacting)
        } else if self.shell.running.is_some() {
            Some(Busy::Shell)
        } else if !self.pending.is_empty() || self.send_next.is_some() || self.recalling {
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

    /// `/model <id>` or a picker selection: switch when idle.
    pub(super) fn switch_model(&mut self, model: String, fx: &mut Vec<Effect>) {
        if self.idle_or_notice() {
            fx.push(Effect::SetModel(model));
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
            Command::Status => fx.push(Effect::StatusReport),
            Command::DaemonStop => fx.push(Effect::DaemonStop),
            Command::DaemonStart => fx.push(Effect::DaemonStart),
            Command::Agents(name) => fx.push(Effect::Agents(name)),
            Command::Audit(lines) => fx.push(Effect::Audit(lines)),
            Command::Schedules(action) => fx.push(Effect::Schedules(action)),
            Command::Rehome(reference) => fx.push(Effect::Rehome(reference)),
            Command::Init => {
                if self.idle_or_notice() {
                    self.start_turn(commands::INIT_PROMPT.to_string(), now, fx);
                }
            }
            Command::Memory(target) => fx.push(Effect::OpenMemory(target)),
            Command::Export(path) => fx.push(Effect::Export {
                path,
                text: self.transcript.export_text(),
            }),
            Command::Cost => {
                let text = self.status.cost.breakdown(&self.status.prices);
                self.transcript.push_notice(NoticeLevel::Info, text);
            }
            Command::Clear => {
                if self.idle_or_notice() {
                    // The integrator resets the transcript and counters once
                    // the new session is live (and keeps them on failure).
                    fx.push(Effect::NewSession);
                }
            }
            Command::Resume(reference) => {
                if self.idle_or_notice() {
                    fx.push(reference.map_or(Effect::RequestResumePicker, Effect::ResumeSession));
                }
            }
            Command::Remember(text) => {
                self.transcript
                    .push_notice(NoticeLevel::Info, format!("remembering: {text}"));
                fx.push(Effect::Remember(text));
            }
            Command::Compact(CompactCommand::Status) => {
                let text = self.gauge.policy().describe(self.status.context_window);
                self.transcript.push_notice(NoticeLevel::Info, text);
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
                self.compacting = true;
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    "asking the engine to compact at the next model call…",
                );
                self.scroll.bottom();
                fx.push(Effect::CompactNow);
            }
            Command::Model(ModelCommand::Set(model)) => self.switch_model(model, fx),
            Command::Model(cmd @ (ModelCommand::Pick | ModelCommand::Refresh)) => {
                if self.idle_or_notice() {
                    fx.push(Effect::RequestModelPicker {
                        refresh: cmd == ModelCommand::Refresh,
                    });
                }
            }
            Command::Auto(AutoCommand::Status) | Command::Mode(None) => {
                let text = format!(
                    "permission mode: {} (Shift+Tab cycles manual → accept edits → plan → auto; /mode <name> sets one)",
                    self.status.mode_text()
                );
                self.transcript.push_notice(NoticeLevel::Info, text);
            }
            Command::Auto(AutoCommand::On) => fx.push(Effect::SetMode(PermissionMode::Auto)),
            Command::Auto(AutoCommand::Off) => fx.push(Effect::SetMode(PermissionMode::Manual)),
            Command::Mode(Some(mode)) => fx.push(Effect::SetMode(mode)),
            Command::Auto(AutoCommand::Resume) => fx.push(Effect::ResumeAutoMode),
            Command::Rules(RulesCommand::List) => fx.push(Effect::ListRules),
            Command::Rules(RulesCommand::Add {
                effect,
                tool,
                pattern,
            }) => fx.push(Effect::AddRule {
                effect,
                tool,
                pattern,
            }),
            Command::Rules(RulesCommand::Remove(n)) => fx.push(Effect::RemoveRule(n)),
            Command::TrustProject => fx.push(Effect::TrustProject),
            Command::Helpers(cmd) => self.helpers_command(cmd),
        }
    }
}
