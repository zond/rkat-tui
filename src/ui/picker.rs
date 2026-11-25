//! The list pickers of `/schedules`, `/permissions`, `/helpers`, `/mode`,
//! `/memory`, `/wire` and `/agents`: one [`ListPicker`] of rows, each carrying the keys that
//! act on it (an act, an act to confirm with `y`, or why it cannot), with
//! a selection, a scrolling window and an optional line under the rows.
//!
//! The rows are built where their data lives (the runner for the daemon's
//! schedules and rules, the workbench for the instruction files, the UI
//! for the mode and the helpers); the picker only moves, asks and answers
//! with a [`PickAct`], which [`App::picked`] turns into what the typed
//! command does. A picker stays open after its acts, its list refreshed
//! in place, until Esc, `q` or Ctrl-C closes it; only an act that goes
//! somewhere else (a mode, a helper's view, another agent) closes it.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use super::markdown::{truncate_to_width, wrap_text};
use super::modal::{
    ListCursor, ModalKey, ctrl_c_answer, has_ctrl_or_alt, ignored_key, is_held, push_line_text,
    window_lines,
};
use super::{App, BlockId, Effect, MemoryTarget, Modal, OpenHelperView, ScrollState};
use crate::approval::desk::RuleSpec;
use crate::approval::grants::GrantScope;
use crate::approval::mode::PermissionMode;
use crate::daemon::protocol::ScheduleOp;

/// Which command's picker a [`ListPicker`] is (a refresh replaces only a
/// picker of its own kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// `/schedules`.
    Schedules,
    /// `/permissions`.
    Permissions,
    /// `/helpers`.
    Helpers,
    /// `/mode`.
    Mode,
    /// `/memory`.
    Memory,
    /// `/wire`.
    Wire,
    /// `/agents`.
    Agents,
    /// `/settings`.
    Settings,
    /// `/rehome`.
    Rehome,
}

impl PickerKind {
    /// The picker box's title (a picker with sections adds them,
    /// [`ListPicker::title`]).
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Schedules => " Schedules ",
            Self::Permissions => " Permissions ",
            Self::Helpers => " Helpers ",
            Self::Mode => " Permission mode ",
            Self::Memory => " Instruction files ",
            Self::Wire => " Wires to other workspaces ",
            Self::Agents => " Agents ",
            Self::Settings => " Settings ",
            Self::Rehome => " Workspace state ",
        }
    }
}

/// The most characters the settings picker's field takes.
const SETTING_CHARS: usize = 2_000;

/// What a pending question's keys are, as its line says them.
const ANSWERS: &str = "y yes · n or Esc no";

/// What replaces [`ANSWERS`] after a key that does not answer.
const NUDGE: &str = "press y to confirm, n or Esc to cancel";

/// How many lines a row that is not highlighted shows of its text, and
/// of its note, before an ellipsis.
const ROW_CAP: usize = 2;

/// What a picker's key does; each is what a typed command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAct {
    /// `/schedules pause|resume|delete <id>`, then the list again.
    Schedule(ScheduleOp, String),
    /// `/permissions rm <n>`, then the list again.
    RemoveRule(u64),
    /// Remove session grant `id`, then the list again.
    RemoveGrant(u64),
    /// Remove statement `id`, then the list again.
    RemoveStatement(u64),
    /// Approve blocked call `id` for the scope, then the list again.
    Approve(u64, GrantScope),
    /// Allow the kind of action `text` describes for the scope, typed
    /// from scratch (no blocked call), then the list again.
    AddStatement {
        /// How long it holds (this session or always).
        scope: GrantScope,
        /// The statement as the user typed it.
        text: String,
    },
    /// Add an approval rule (as `/permissions add`, or a literal one),
    /// then the list again.
    AddRule(RuleForm),
    /// Replace rule `n`, which read `old`, with `new` in place, then the
    /// list again; the field stays open until the daemon answers (a
    /// refusal shows in it, its text kept).
    EditRule {
        /// The rule's number.
        n: u64,
        /// The rule as the dialog showed it.
        old: RuleSpec,
        /// The rule as edited.
        new: RuleSpec,
        /// It is in the user's own `tui.toml` (else the workspace's).
        user: bool,
    },
    /// Withdraw the approval once of call `id` (it is a blocked call
    /// again), then the list again.
    Withdraw(u64),
    /// Trust the host the user typed with the project's data, in this
    /// workspace or (`user`) every workspace, then the list again.
    TrustHost {
        /// As typed.
        host: String,
        /// The user's own `tui.toml`.
        user: bool,
    },
    /// Trust the hosts blocked call `id`'s D5 block names, in this
    /// workspace or (`user`) every workspace, then the list again.
    TrustCallHosts {
        /// The blocked call.
        id: u64,
        /// The user's own `tui.toml`.
        user: bool,
    },
    /// Stop trusting the host, then the list again.
    UntrustHost(String),
    /// Allow the kind of action `text` describes, as blocked call `id`'s
    /// approval, for the scope, then the list again.
    AllowEffect {
        /// The blocked call.
        id: u64,
        /// How long it holds (this session or always).
        scope: GrantScope,
        /// The statement as the user left it.
        text: String,
    },
    /// `/mode <mode>` (`/auto resume` for a paused auto mode).
    SetMode(PermissionMode),
    /// `/set <key> <value>` (`None`: `/unset <key>`), `--user` with
    /// `user`, then the list again with the picker's `filter`.
    SetSetting {
        /// The setting's key.
        key: String,
        /// The value (`None`: remove the level's value).
        value: Option<String>,
        /// The user's own `tui.toml`.
        user: bool,
        /// The filter the picker was opened with (the list comes again
        /// with it, so the highlight stays on the setting changed).
        filter: Option<String>,
    },
    /// `/memory [workspace | user | system]`.
    OpenMemory(MemoryTarget),
    /// The helper's full view (Ctrl+Up/Down, then Enter).
    ViewHelper(BlockId),
    /// Fold or expand the helper's block in the transcript.
    FoldHelper(BlockId),
    /// Stop the running helper (its run is cancelled and it is retired;
    /// the helpers it started keep running).
    StopHelper(BlockId),
    /// `/helpers`: list the ended helpers too (`true`), or only the live
    /// ones.
    ShowEndedHelpers(bool),
    /// `/wire <workspace>/<agent>`.
    Wire {
        /// The other agent's workspace.
        workspace: std::path::PathBuf,
        /// The other agent.
        agent: String,
    },
    /// Remove this agent's wire to `agent` of `workspace` (the `/wire`
    /// picker's `d` on a wired row).
    RemoveWire {
        /// The other agent's workspace.
        workspace: std::path::PathBuf,
        /// The other agent.
        agent: String,
    },
    /// `/agents <name>`: switch this client to the agent.
    SwitchAgent(String),
    /// The `/agents` picker's "new agent" row: seat a fresh agent for
    /// this client and switch to it.
    NewAgent,
    /// Retire the agent (the list comes again without it, or the refusal
    /// shows on the picker).
    RetireAgent(String),
    /// `/rehome <argument>` (`from <id>`, `to <directory>`), confirmed:
    /// the picker closes and the re-home runs as the typed command does.
    Rehome(String),
    /// The `/rehome` picker's directory as typed: the runner checks it
    /// (answered with [`super::Msg::RehomeChecked`]) while the field
    /// stays open.
    CheckRehome(String),
    /// `/rehome <argument>` again, the state `discard` (a realm id) that
    /// refused it deleted: the user confirmed it.
    RehomeDiscarding {
        /// The `/rehome` argument.
        argument: String,
        /// The realm to delete.
        discard: String,
    },
    /// Delete the state of realm `id` (as `rkat-tui --remove-state <id>
    /// --yes`), then the list again.
    RemoveState(String),
}

impl PickAct {
    /// Whether the picker closes after it: only an act whose point is to
    /// go somewhere else (the new mode, a helper's full view, another
    /// agent). Every other act leaves the picker open, its list refreshed
    /// in place, until Esc, `q` or Ctrl-C closes it.
    fn leaves_picker(&self) -> bool {
        matches!(
            self,
            Self::SetMode(_)
                | Self::ViewHelper(_)
                | Self::SwitchAgent(_)
                | Self::NewAgent
                | Self::Rehome(_)
                | Self::RehomeDiscarding { .. }
        )
    }
}

/// One key of a row: what it does there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickKey {
    /// The key.
    pub code: KeyCode,
    /// What it does.
    pub does: Does,
}

/// What a row's key does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Does {
    /// This act, at once.
    Act(PickAct),
    /// This question first; `y` then does the act.
    Ask(PickAct, String),
    /// Opens this one-line field under the rows.
    Open(Field),
    /// Nothing, for this reason.
    Refused(String),
}

impl PickKey {
    /// `code` does `act` at once.
    #[must_use]
    pub fn now(code: KeyCode, act: PickAct) -> Self {
        Self {
            code,
            does: Does::Act(act),
        }
    }

    /// `code` asks `question`; `y` then does `act`.
    #[must_use]
    pub fn confirmed(code: KeyCode, act: PickAct, question: impl Into<String>) -> Self {
        Self {
            code,
            does: Does::Ask(act, question.into()),
        }
    }

    /// `code` opens `field`.
    #[must_use]
    pub fn field(code: KeyCode, field: Field) -> Self {
        Self {
            code,
            does: Does::Open(field),
        }
    }

    /// `code` says `why` it does nothing here.
    #[must_use]
    pub fn refused(code: KeyCode, why: impl Into<String>) -> Self {
        Self {
            code,
            does: Does::Refused(why.into()),
        }
    }

    /// The act it does or asks for, if it does one.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn act(&self) -> Option<&PickAct> {
        match &self.does {
            Does::Act(act) | Does::Ask(act, _) => Some(act),
            Does::Open(_) | Does::Refused(_) => None,
        }
    }
}

/// A one-line field under a picker's rows: Enter acts with its text
/// ([`FieldFor::done`]), Tab switches where it applies, Backspace deletes
/// the last character, Esc closes it. It has no cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// What it is for.
    pub what: FieldFor,
    /// The text as typed so far.
    pub text: String,
}

/// What a [`Field`] is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldFor {
    /// Setting `key`'s value at the user's level (`user`) or the
    /// workspace's; Enter is [`PickAct::SetSetting`].
    Setting {
        /// The setting's key.
        key: String,
        /// The user's own `tui.toml`.
        user: bool,
        /// The filter the picker was opened with.
        filter: Option<String>,
    },
    /// The kind of action blocked call `id` is to be allowed for, for
    /// `scope`; Enter is [`PickAct::AllowEffect`].
    Effect {
        /// The blocked call.
        id: u64,
        /// This session or always.
        scope: GrantScope,
    },
    /// A kind of action to allow from scratch, for `scope`; Enter is
    /// [`PickAct::AddStatement`].
    Statement {
        /// This session or always.
        scope: GrantScope,
    },
    /// A new approval rule, its text `<tool> [pattern]`: Tab switches
    /// allow and deny, Shift+Tab this workspace and the user's own file,
    /// ↑/↓ a pattern and a literal; Enter is [`PickAct::AddRule`].
    Rule {
        /// A deny rule.
        deny: bool,
        /// Written to the user's own `tui.toml`.
        user: bool,
        /// The text after the tool is matched exactly, not as a pattern.
        literal: bool,
    },
    /// Rule `n` (which reads `old`) as edited, its text `<tool>
    /// [pattern]`: Tab switches allow and deny, ↑/↓ a pattern and a
    /// literal; the file stays the rule's own. Enter is
    /// [`PickAct::EditRule`]; a refusal shows here, the field kept open
    /// with its text.
    EditRule {
        /// The rule's number.
        n: u64,
        /// The rule as the dialog showed it.
        old: RuleSpec,
        /// A deny rule.
        deny: bool,
        /// The text after the tool is matched exactly.
        literal: bool,
        /// The rule is in the user's own `tui.toml` (else the workspace's).
        user: bool,
        /// Why the last edit was refused (until the next key that edits).
        refused: Option<String>,
    },
    /// A host to trust with the project's data: Tab switches this
    /// workspace and every workspace; Enter is [`PickAct::TrustHost`]
    /// once the text is a valid entry
    /// ([`crate::approval::hosts::HostPattern::parse`]).
    Host {
        /// Written to the user's own `tui.toml`.
        user: bool,
    },
    /// The directory to move this workspace's state to (`/rehome to`);
    /// Enter is [`PickAct::CheckRehome`], and a refusal of the runner's
    /// check shows here, the field kept open with its text.
    Directory {
        /// Why the last directory typed was refused (until the next edit).
        refused: Option<String>,
    },
}

/// The most characters the `/rehome` picker's directory takes.
const DIRECTORY_CHARS: usize = 4_096;

/// A rule as the `/permissions` dialog's `r` form leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleForm {
    /// A deny rule (else an allow rule).
    pub deny: bool,
    /// The tool's name or pattern.
    pub tool: String,
    /// The argument's pattern, or with `literal` its exact text.
    pub pattern: Option<String>,
    /// `pattern` is matched exactly.
    pub literal: bool,
    /// Written to the user's own `tui.toml` (every workspace).
    pub user: bool,
}

/// The next of the three scopes a statement is allowed for (Tab).
fn next_statement_scope(scope: GrantScope) -> GrantScope {
    match scope {
        GrantScope::Session => GrantScope::Always,
        GrantScope::Always => GrantScope::AlwaysUser,
        _ => GrantScope::Session,
    }
}

/// What a statement's field says it allows for: the scope now, the one
/// Tab switches to.
fn statement_scope_words(scope: GrantScope) -> (&'static str, &'static str) {
    match scope {
        GrantScope::Always => (
            "always for this workspace (the realm's tui.toml)",
            "always in every workspace",
        ),
        GrantScope::AlwaysUser => (
            "always in every workspace (your tui.toml)",
            "for this session",
        ),
        _ => ("for this session", "always for this workspace"),
    }
}

impl FieldFor {
    /// The most characters the field takes.
    fn max(&self) -> usize {
        match self {
            Self::Setting { .. } | Self::Rule { .. } | Self::EditRule { .. } => SETTING_CHARS,
            Self::Effect { .. } | Self::Statement { .. } => {
                crate::approval::statements::MAX_STATEMENT_CHARS
            }
            // A host name's 253 and `*.`.
            Self::Host { .. } => 255,
            Self::Directory { .. } => DIRECTORY_CHARS,
        }
    }

    /// The text was edited: a refusal of the text before goes.
    fn edited(&mut self) {
        if let Self::Directory { refused } | Self::EditRule { refused, .. } = self {
            *refused = None;
        }
    }

    /// Keep the refusal `why` of Enter in the field itself (it stays
    /// open with its text); whether it did. Every other field closes
    /// and the refusal shows under the rows.
    fn hold_refusal(&mut self, why: &str) -> bool {
        match self {
            Self::Directory { refused } | Self::EditRule { refused, .. } => {
                *refused = Some(why.to_string());
                true
            }
            _ => false,
        }
    }

    /// Tab (`back`: Shift+Tab): the other level, the next scope; for a
    /// rule, allow or deny (Shift+Tab: this workspace or the user's file).
    fn toggle(&mut self, back: bool) {
        match self {
            Self::Setting { user, .. } | Self::Host { user } => *user = !*user,
            Self::Effect { scope, .. } | Self::Statement { scope } => {
                *scope = next_statement_scope(*scope);
            }
            Self::Rule { deny, user, .. } => {
                if back {
                    *user = !*user;
                } else {
                    *deny = !*deny;
                }
            }
            // The file is the rule's own: Shift+Tab changes nothing.
            Self::EditRule { deny, refused, .. } => {
                if !back {
                    *deny = !*deny;
                    *refused = None;
                }
            }
            Self::Directory { .. } => {}
        }
    }

    /// ↑/↓: a rule's pattern or literal; nothing in any other field.
    fn flip(&mut self) {
        if let Self::Rule { literal, .. } = self {
            *literal = !*literal;
        }
        if let Self::EditRule {
            literal, refused, ..
        } = self
        {
            *literal = !*literal;
            *refused = None;
        }
    }

    /// Enter: the act with `text` (trimmed), or why there is none yet
    /// (a rule with no tool, a literal with nothing to match).
    fn done(&self, text: &str) -> Result<PickAct, String> {
        let text = text.trim().to_string();
        Ok(match self {
            Self::Setting { key, user, filter } => PickAct::SetSetting {
                key: key.clone(),
                value: Some(text),
                user: *user,
                filter: filter.clone(),
            },
            Self::Effect { id, scope } => PickAct::AllowEffect {
                id: *id,
                scope: *scope,
                text,
            },
            Self::Statement { scope } => PickAct::AddStatement {
                scope: *scope,
                text,
            },
            Self::Host { user } => {
                if text.is_empty() {
                    return Err(format!(
                        "no host trusted: type one; {}",
                        crate::approval::hosts::VALID_HOST
                    ));
                }
                // The daemon checks it again; refused here, it is said at
                // once.
                if let Err(why) = crate::approval::hosts::HostPattern::parse(&text) {
                    return Err(format!(
                        "`{text}` was not trusted: {why}; {}",
                        crate::approval::hosts::VALID_HOST
                    ));
                }
                PickAct::TrustHost {
                    host: text,
                    user: *user,
                }
            }
            Self::Directory { .. } => {
                if text.is_empty() {
                    return Err(
                        "type the directory to move this workspace's state to (~ is your home)"
                            .to_string(),
                    );
                }
                PickAct::CheckRehome(text)
            }
            Self::Rule {
                deny,
                user,
                literal,
            } => {
                let (tool, rest) = text.split_once(char::is_whitespace).unwrap_or((&text, ""));
                let rest = rest.trim();
                if tool.is_empty() {
                    return Err(
                        "no rule added: type the tool's name (shell, write_file, …), then an optional pattern"
                            .to_string(),
                    );
                }
                if *literal && rest.is_empty() {
                    return Err(format!(
                        "no rule added: a literal rule needs the exact text after `{tool}` (↑/↓ makes it a pattern)"
                    ));
                }
                PickAct::AddRule(RuleForm {
                    deny: *deny,
                    tool: tool.to_string(),
                    pattern: (!rest.is_empty()).then(|| rest.to_string()),
                    literal: *literal,
                    user: *user,
                })
            }
            Self::EditRule {
                n,
                old,
                deny,
                literal,
                user,
                ..
            } => {
                let (tool, rest) = text.split_once(char::is_whitespace).unwrap_or((&text, ""));
                let rest = rest.trim();
                if tool.is_empty() {
                    return Err(format!(
                        "rule {n} not changed: type the tool's name (shell, write_file, …), then an optional pattern"
                    ));
                }
                if *literal && rest.is_empty() {
                    return Err(format!(
                        "rule {n} not changed: a literal rule needs the exact text after `{tool}` (↑/↓ makes it a pattern)"
                    ));
                }
                PickAct::EditRule {
                    n: *n,
                    old: old.clone(),
                    new: RuleSpec {
                        deny: *deny,
                        tool: tool.to_string(),
                        pattern: (!rest.is_empty()).then(|| rest.to_string()),
                        literal: *literal,
                    },
                    user: *user,
                }
            }
        })
    }

    /// The field that edits rule `n`, which reads `old`, of the user's own
    /// file (`user`) or the workspace's, its text the rule's own.
    #[must_use]
    pub fn edit_rule(n: u64, old: RuleSpec, user: bool) -> Field {
        let text = match &old.pattern {
            Some(pattern) => format!("{} {pattern}", old.tool),
            None => old.tool.clone(),
        };
        Field {
            what: Self::EditRule {
                n,
                deny: old.deny,
                literal: old.literal,
                old,
                user,
                refused: None,
            },
            text,
        }
    }

    /// The line over the field: what Enter does there, what Tab switches
    /// to.
    fn heading(&self) -> String {
        match self {
            Self::Setting { key, user, .. } => {
                let (now, other) = if *user {
                    ("your tui.toml (every workspace)", "this workspace")
                } else {
                    ("this workspace's tui.toml", "your tui.toml")
                };
                format!("{key} in {now} (Enter writes it · Tab: {other} · Esc cancels):")
            }
            Self::Effect { scope, .. } => {
                let (now, other) = statement_scope_words(*scope);
                format!(
                    "allow this kind of action {now}, judged by the classifier (edit it; Enter allows · Tab: {other} · Esc cancels):"
                )
            }
            Self::Statement { scope } => {
                let (now, other) = statement_scope_words(*scope);
                format!(
                    "new statement: allow a kind of action {now}, judged by the classifier (type it; Enter allows · Tab: {other} · Esc cancels):"
                )
            }
            Self::Rule {
                deny,
                user,
                literal,
            } => {
                let kind = if *deny { "deny" } else { "allow" };
                let file = if *user {
                    "your tui.toml (every workspace)"
                } else {
                    "this workspace (the realm's tui.toml)"
                };
                let arg = if *literal {
                    "<tool> <exact text>"
                } else {
                    "<tool> [pattern]"
                };
                format!(
                    "new rule: {kind} · {file} · type {arg} (Enter adds · Tab allow/deny · Shift+Tab workspace/your tui.toml · ↑/↓ pattern/literal · Esc cancels):"
                )
            }
            Self::EditRule {
                n,
                deny,
                literal,
                user,
                ..
            } => {
                let kind = if *deny { "deny" } else { "allow" };
                let file = if *user {
                    "your tui.toml"
                } else {
                    "the workspace's tui.toml"
                };
                let arg = if *literal {
                    "<tool> <exact text>"
                } else {
                    "<tool> [pattern]"
                };
                format!(
                    "edit rule {n}: {kind} · in {file}, where it stands · {arg} (Enter replaces it · Tab allow/deny · ↑/↓ pattern/literal · Esc keeps it):"
                )
            }
            Self::Host { user } => {
                let (now, other) = if *user {
                    ("every workspace (your tui.toml)", "this workspace")
                } else {
                    ("this workspace (the realm's tui.toml)", "every workspace")
                };
                format!(
                    "trust a host with this project's data · {now} · type a host name or *.domain (Enter trusts · Tab: {other} · Esc cancels):"
                )
            }
            Self::Directory { .. } => "move this workspace's state to another directory · type its path, ~ is your home (Enter checks it, then asks · Esc cancels):".to_string(),
        }
    }

    /// Why the text in the field was refused, shown under it.
    fn refusal(&self) -> Option<&str> {
        match self {
            Self::Directory { refused } | Self::EditRule { refused, .. } => refused.as_deref(),
            _ => None,
        }
    }
}

/// One row: its line, an optional dim line under it, and its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickRow {
    /// The row as shown.
    pub text: String,
    /// A second, dim line (an explanation, a path).
    pub note: Option<String>,
    /// What the keys do on it.
    pub keys: Vec<PickKey>,
    /// Shown dim (an ended helper among live ones).
    pub dim: bool,
    /// What the row is of (a schedule's id, a helper's block, an agent's
    /// name): a refreshed list keeps the highlight on the row of the same
    /// item ([`ListPicker::refreshed`]).
    pub id: Option<String>,
    /// More than the row shows (a blocked call's whole command): Ctrl+O
    /// shows it in place of the rows ([`ListPicker::detail`]).
    pub detail: Option<String>,
    /// Keys that act only once the detail was shown for this row (an
    /// approval once of a call longer than its row): before, they say
    /// to read it first ([`VIEW_FIRST`]).
    pub view_first: Vec<KeyCode>,
}

/// What a key that acts on the whole of a row says before Ctrl+O showed
/// it ([`PickRow::view_first`]).
pub const VIEW_FIRST: &str =
    "the row shows only part of the call: Ctrl+O shows all of it; approve it once after reading it";

/// The detail of the highlighted row, shown in place of the rows
/// ([`PickRow::detail`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailView {
    /// The first line shown.
    pub scroll: usize,
    /// The last useful [`Self::scroll`], recorded by the renderer (it
    /// knows the width).
    pub scroll_max: std::cell::Cell<usize>,
}

impl PickRow {
    /// A row of `text` with `keys`.
    #[must_use]
    pub fn new(text: impl Into<String>, keys: Vec<PickKey>) -> Self {
        Self {
            text: text.into(),
            note: None,
            keys,
            dim: false,
            id: None,
            detail: None,
            view_first: Vec::new(),
        }
    }

    /// The row with `detail` behind Ctrl+O, and `view_first` keys that act
    /// only once it was shown.
    #[must_use]
    pub fn detailed(mut self, detail: impl Into<String>, view_first: Vec<KeyCode>) -> Self {
        self.detail = Some(detail.into());
        self.view_first = view_first;
        self
    }

    /// The row as that of item `id` ([`Self::id`]).
    #[must_use]
    pub fn keyed(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The row shown dim.
    #[must_use]
    pub fn dimmed(mut self) -> Self {
        self.dim = true;
        self
    }

    /// The row with `note` under it.
    #[must_use]
    pub fn noted(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// What `code` does on this row.
    fn key(&self, code: KeyCode) -> Option<&PickKey> {
        self.keys.iter().find(|key| key.code == code)
    }
}

/// How a row's showing of its detail is remembered: by its item, else by
/// its text.
fn viewed_key(row: &PickRow) -> String {
    row.id.clone().unwrap_or_else(|| row.text.clone())
}

/// How many lines PgUp/PgDn move a row's detail.
const DETAIL_PAGE: usize = 10;

/// The line under a picker's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerLine {
    /// A key that asks first: `y` does `act`, `n` or Esc cancels the
    /// question; every other key leaves it up and points at those keys.
    Confirm {
        /// What is asked.
        question: String,
        /// What `y` does.
        act: PickAct,
        /// A key that does not answer was pressed: the line says which do.
        nudged: bool,
    },
    /// Why the key pressed does nothing on this row.
    Refused(String),
    /// What the last act did ("approved once: …", "retired main-ab12").
    Done(String),
    /// A one-line field ([`Field`]).
    Field(Field),
}

/// A picker over rows ([`PickerKind`] says whose).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPicker {
    /// Whose picker.
    pub kind: PickerKind,
    /// The rows, in order.
    pub rows: Vec<PickRow>,
    /// The highlighted row.
    pub cursor: ListCursor,
    /// A question being asked, or why a key did nothing.
    pub line: Option<PickerLine>,
    /// What shows when there are no rows.
    pub empty: String,
    /// The key hint at the bottom.
    pub footer: String,
    /// How many of the last rows are acts rather than items (the
    /// `/agents` picker's "new agent" row): a refresh that drops the
    /// highlighted item lands on the last item, not on them.
    pub actions: usize,
    /// Keys of the picker itself, on any row and with none (a row's own
    /// key of the same code wins).
    pub keys: Vec<PickKey>,
    /// The picker's sections, when it has them (`/permissions`): the
    /// fields above are the one shown, Tab and Shift+Tab switch.
    pub sections: Option<Sections>,
    /// The highlighted row's detail, shown in place of the rows (Ctrl+O).
    pub detail: Option<DetailView>,
    /// The rows (by [`PickRow::id`]) whose detail was shown: their
    /// [`PickRow::view_first`] keys act.
    pub viewed: std::collections::BTreeSet<String>,
    /// A line over the rows (what the rows are about: `/rehome`'s
    /// workspace and its state).
    pub header: Option<String>,
}

/// One section of a picker with sections: its rows and what goes with
/// them, as [`ListPicker`]'s fields of the same names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The rows, in order.
    pub rows: Vec<PickRow>,
    /// The highlighted row.
    pub cursor: ListCursor,
    /// What shows when there are no rows.
    pub empty: String,
    /// The key hint at the bottom.
    pub footer: String,
    /// Keys of the section itself.
    pub keys: Vec<PickKey>,
}

impl Page {
    /// A section of `rows`, the first highlighted.
    #[must_use]
    pub fn new(
        rows: Vec<PickRow>,
        empty: impl Into<String>,
        footer: impl Into<String>,
        keys: Vec<PickKey>,
    ) -> Self {
        Self {
            rows,
            cursor: ListCursor::default(),
            empty: empty.into(),
            footer: footer.into(),
            keys,
        }
    }
}

/// A picker's sections ([`ListPicker::sections`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sections {
    /// Each section's name, in order.
    pub names: Vec<&'static str>,
    /// The section shown: its page is the picker's own fields.
    pub current: usize,
    /// The other sections' pages, by index (`None` at `current`).
    pages: Vec<Option<Page>>,
}

impl ListPicker {
    /// A picker of `kind` over `pages` (each named), showing section
    /// `current` (the last when past them).
    #[must_use]
    pub fn sectioned(kind: PickerKind, pages: Vec<(&'static str, Page)>, current: usize) -> Self {
        let current = current.min(pages.len().saturating_sub(1));
        let mut names = Vec::with_capacity(pages.len());
        let mut stored = Vec::with_capacity(pages.len());
        for (name, page) in pages {
            names.push(name);
            stored.push(Some(page));
        }
        let mut picker = Self::new(kind, Vec::new(), "", "");
        picker.sections = Some(Sections {
            names,
            current,
            pages: Vec::new(),
        });
        let shown = stored.get_mut(current).and_then(Option::take);
        if let Some(sections) = &mut picker.sections {
            sections.pages = stored;
        }
        if let Some(page) = shown {
            picker.load(page);
        }
        picker
    }

    /// The shown section's page, out of the picker's own fields.
    fn page(&self) -> Page {
        Page {
            rows: self.rows.clone(),
            cursor: self.cursor,
            empty: self.empty.clone(),
            footer: self.footer.clone(),
            keys: self.keys.clone(),
        }
    }

    /// `page` as the picker's own fields.
    fn load(&mut self, page: Page) {
        self.rows = page.rows;
        self.cursor = page.cursor;
        self.empty = page.empty;
        self.footer = page.footer;
        self.keys = page.keys;
    }

    /// The section shown, when the picker has sections.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn section(&self) -> Option<usize> {
        self.sections.as_ref().map(|sections| sections.current)
    }

    /// Show section `index` (nothing when the picker has none, or no
    /// such section): the one shown keeps its rows and highlight for
    /// when it comes back; the line under the rows goes.
    pub fn show_section(&mut self, index: usize) {
        let Some(sections) = &self.sections else {
            return;
        };
        if index == sections.current || index >= sections.names.len() {
            return;
        }
        let shown = self.page();
        let Some(sections) = &mut self.sections else {
            return;
        };
        let from = sections.current;
        let next = sections.pages.get_mut(index).and_then(Option::take);
        if let Some(slot) = sections.pages.get_mut(from) {
            *slot = Some(shown);
        }
        sections.current = index;
        if let Some(page) = next {
            self.load(page);
        }
        self.line = None;
        self.detail = None;
    }

    /// Tab (`back`: Shift+Tab): the next section, or the one before,
    /// round.
    fn switch_section(&mut self, back: bool) {
        let Some(sections) = &self.sections else {
            return;
        };
        let n = sections.names.len();
        if n < 2 {
            return;
        }
        let at = sections.current;
        self.show_section(if back { (at + n - 1) % n } else { (at + 1) % n });
    }

    /// How many rows section `index` has.
    fn rows_of(&self, index: usize) -> usize {
        match &self.sections {
            Some(sections) if index == sections.current => self.rows.len(),
            Some(sections) => sections
                .pages
                .get(index)
                .and_then(Option::as_ref)
                .map_or(0, |page| page.rows.len()),
            None => self.rows.len(),
        }
    }

    /// The rows of section `index` (the shown one's own), when it exists.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn section_rows(&self, index: usize) -> Option<&[PickRow]> {
        let sections = self.sections.as_ref()?;
        if index == sections.current {
            return Some(&self.rows);
        }
        sections
            .pages
            .get(index)?
            .as_ref()
            .map(|page| page.rows.as_slice())
    }

    /// The lines the picker's box keeps at `width` in `max_lines`: the
    /// most any of its sections takes (a picker without sections has its
    /// one), each with its own rows and the row that grows most when
    /// highlighted highlighted, and nothing open over them. Tab, Shift+Tab
    /// and ↑/↓ then leave the box's outline as it is; only a question, a
    /// field or a row's detail taller than that grows it.
    #[must_use]
    pub(super) fn steady_lines(&self, width: usize, max_lines: usize) -> usize {
        let count = self.sections.as_ref().map_or(1, |s| s.names.len());
        (0..count)
            .map(|index| {
                let mut view = self.clone();
                view.show_section(index);
                view.line = None;
                view.detail = None;
                let tallest = (0..view.rows.len()).max_by_key(|&i| {
                    let row = &view.rows[i];
                    row_lines(row, width, true) - row_lines(row, width, false)
                });
                view.cursor = ListCursor::at(tallest.unwrap_or(0));
                list_picker_parts(&view, width, usize::MAX).0.len()
            })
            .max()
            .unwrap_or(0)
            .min(max_lines)
    }

    /// The box's title: the kind's, and with sections each one's name
    /// and rows, the shown one marked: ` Permissions: [Blocked 2] · In force 14 `.
    #[must_use]
    pub fn title(&self) -> String {
        let Some(sections) = &self.sections else {
            return self.kind.title().to_string();
        };
        let parts: Vec<String> = sections
            .names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let n = self.rows_of(i);
                if i == sections.current {
                    format!("[{name} {n}]")
                } else {
                    format!("{name} {n}")
                }
            })
            .collect();
        format!(" {}: {} ", self.kind.title().trim(), parts.join(" · "))
    }
    /// A picker of `kind` over `rows`, the first highlighted; `empty`
    /// shows when there are none, `footer` is the key hint.
    #[must_use]
    pub fn new(
        kind: PickerKind,
        rows: Vec<PickRow>,
        empty: impl Into<String>,
        footer: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            rows,
            cursor: ListCursor::default(),
            line: None,
            empty: empty.into(),
            footer: footer.into(),
            actions: 0,
            keys: Vec::new(),
            sections: None,
            detail: None,
            viewed: std::collections::BTreeSet::new(),
            header: None,
        }
    }

    /// The picker with `header` over its rows ([`Self::header`]).
    #[must_use]
    pub fn with_header(mut self, header: impl Into<String>) -> Self {
        self.header = Some(header.into());
        self
    }

    /// The picker with `keys` of its own ([`Self::keys`]).
    #[must_use]
    pub fn with_keys(mut self, keys: Vec<PickKey>) -> Self {
        self.keys = keys;
        self
    }

    /// The picker with row `index` highlighted (the last when past it).
    #[must_use]
    pub fn at(mut self, index: usize) -> Self {
        self.cursor = ListCursor::at(index.min(self.rows.len().saturating_sub(1)));
        self
    }

    /// The highlighted row.
    #[must_use]
    pub fn selected(&self) -> Option<&PickRow> {
        self.rows.get(self.cursor.index())
    }

    /// ↑/↓ (or k/j) move; a row's own key acts, asks first or says why it
    /// does nothing; Esc, `q` or Ctrl-C close. A held key only moves.
    ///
    /// While a question is pending only `y` (acts), `n` and Esc (cancel the
    /// question, not the picker) answer it: every other key, Enter and the
    /// arrows too, leaves it up and points at those keys, and nothing
    /// reaches the row's own keys. Ctrl-C still closes the picker, acting
    /// on nothing.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<PickAct> {
        if let Some(answer) = ctrl_c_answer(key) {
            return answer;
        }
        if self.detail_key(key) {
            return ModalKey::Stay;
        }
        if let Some(PickerLine::Confirm { act, nudged, .. }) = &mut self.line {
            if is_held(key) || has_ctrl_or_alt(key) {
                return ModalKey::Stay;
            }
            return match key.code {
                KeyCode::Char('y') => {
                    let act = act.clone();
                    self.line = None;
                    ModalKey::Choose(act)
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.line = None;
                    ModalKey::Stay
                }
                _ => {
                    *nudged = true;
                    ModalKey::Stay
                }
            };
        }
        if ignored_key(key) {
            return ModalKey::Stay;
        }
        if let Some(PickerLine::Field(field)) = &mut self.line {
            return match key.code {
                KeyCode::Enter => match field.what.done(&field.text) {
                    Ok(act) => {
                        self.line = None;
                        ModalKey::Choose(act)
                    }
                    Err(why) => {
                        if !field.what.hold_refusal(&why) {
                            self.line = Some(PickerLine::Refused(why));
                        }
                        ModalKey::Stay
                    }
                },
                KeyCode::Esc => {
                    self.line = None;
                    ModalKey::Stay
                }
                KeyCode::Tab => {
                    field.what.toggle(false);
                    ModalKey::Stay
                }
                KeyCode::BackTab => {
                    field.what.toggle(true);
                    ModalKey::Stay
                }
                KeyCode::Up | KeyCode::Down => {
                    field.what.flip();
                    ModalKey::Stay
                }
                KeyCode::Backspace => {
                    field.text.pop();
                    field.what.edited();
                    ModalKey::Stay
                }
                KeyCode::Char(c) => {
                    let mut buf = [0; 4];
                    push_line_text(&mut field.text, c.encode_utf8(&mut buf), field.what.max());
                    field.what.edited();
                    ModalKey::Stay
                }
                _ => ModalKey::Stay,
            };
        }
        // A refusal or an act's outcome shows until the next key.
        self.line = None;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor.up(),
            KeyCode::Down | KeyCode::Char('j') => self.cursor.down(self.rows.len()),
            KeyCode::Esc | KeyCode::Char('q') => return ModalKey::Close,
            // Between sections only here: no question or field is open.
            KeyCode::Tab if self.sections.is_some() => self.switch_section(false),
            KeyCode::BackTab if self.sections.is_some() => self.switch_section(true),
            code if self.selected().is_some_and(|row| {
                row.detail.is_some()
                    && row.view_first.contains(&code)
                    && !self.viewed.contains(&viewed_key(row))
            }) =>
            {
                self.line = Some(PickerLine::Refused(VIEW_FIRST.to_string()));
            }
            code => match self
                .selected()
                .and_then(|row| row.key(code))
                .or_else(|| self.keys.iter().find(|key| key.code == code))
                .map(|key| key.does.clone())
            {
                Some(Does::Act(act)) => return ModalKey::Choose(act),
                Some(Does::Ask(act, question)) => {
                    self.line = Some(PickerLine::Confirm {
                        question,
                        act,
                        nudged: false,
                    });
                }
                Some(Does::Open(field)) => self.line = Some(PickerLine::Field(field)),
                Some(Does::Refused(why)) => self.line = Some(PickerLine::Refused(why)),
                None => {}
            },
        }
        ModalKey::Stay
    }

    /// Ctrl+O on a row with more than it shows ([`PickRow::detail`]) shows
    /// all of it in place of the rows (and records that it was shown);
    /// while it is shown ↑/↓, PgUp/PgDn, Home/End scroll it, Ctrl+O, Esc or
    /// `q` go back to the rows, and any other key goes back and acts on
    /// the row. Whether the key was taken.
    fn detail_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl_o =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o');
        let Some(view) = &mut self.detail else {
            if !ctrl_o || is_held(key) || matches!(self.line, Some(PickerLine::Field(_))) {
                return false;
            }
            if let Some(row) = self.selected().filter(|row| row.detail.is_some()) {
                let shown = viewed_key(row);
                self.viewed.insert(shown);
                self.detail = Some(DetailView::default());
                self.line = None;
            }
            return true;
        };
        let max = view.scroll_max.get();
        let scroll = match key.code {
            _ if ctrl_o => None,
            KeyCode::Esc | KeyCode::Char('q') => None,
            KeyCode::Up | KeyCode::Char('k') => Some(view.scroll.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => Some(view.scroll.saturating_add(1)),
            KeyCode::PageUp => Some(view.scroll.saturating_sub(DETAIL_PAGE)),
            KeyCode::PageDown => Some(view.scroll.saturating_add(DETAIL_PAGE)),
            KeyCode::Home => Some(0),
            KeyCode::End => Some(max),
            // Back to the rows, and the key acts there.
            _ => {
                self.detail = None;
                return false;
            }
        };
        match scroll {
            Some(scroll) => view.scroll = scroll.min(max),
            None => self.detail = None,
        }
        true
    }

    /// Whether a one-line field is open (a paste goes into it).
    #[must_use]
    pub fn editing(&self) -> bool {
        matches!(self.line, Some(PickerLine::Field(_)))
    }

    /// Type (or paste) `text` into the open one-line field, on one line,
    /// without control characters; nothing while none is open.
    pub fn type_text(&mut self, text: &str) {
        if let Some(PickerLine::Field(field)) = &mut self.line {
            push_line_text(&mut field.text, text, field.what.max());
            field.what.edited();
        }
    }

    /// `fresh` in place of this picker, which stays open as it was: the
    /// highlight on the row of the same item ([`PickRow::id`]) wherever
    /// it moved; when that item is gone (or rows have no ids), on the row
    /// now in its place, its neighbour (an item highlighted stays on an
    /// item: the last when the list got shorter, never one of the
    /// [`Self::actions`] rows after them). The line under the rows stays
    /// (an outcome, a refusal), but for a question or a field about an
    /// item that is gone. A picker with sections stays on the section it
    /// shows, each section's highlight kept the same way.
    #[must_use]
    pub fn refreshed(&self, mut fresh: Self) -> Self {
        let Some(open) = &self.sections else {
            return self.refreshed_page(fresh);
        };
        fresh.show_section(open.current);
        let mut out = self.refreshed_page(fresh);
        if let Some(sections) = &mut out.sections {
            for (old, new) in open.pages.iter().zip(sections.pages.iter_mut()) {
                if let (Some(old), Some(new)) = (old, new.as_mut()) {
                    let old = Self::new(self.kind, old.rows.clone(), "", "").at(old.cursor.index());
                    let moved = old.refreshed_page(Self::new(self.kind, new.rows.clone(), "", ""));
                    new.cursor = moved.cursor;
                }
            }
        }
        out
    }

    /// [`Self::refreshed`] of the rows shown.
    fn refreshed_page(&self, fresh: Self) -> Self {
        let at = self.cursor.index();
        let id = self.selected().and_then(|row| row.id.as_deref());
        let same = id.and_then(|id| {
            fresh
                .rows
                .iter()
                .position(|row| row.id.as_deref() == Some(id))
        });
        let gone = id.is_some() && same.is_none();
        let at = same.unwrap_or_else(|| {
            let on_item = at < self.rows.len().saturating_sub(self.actions);
            let items = fresh.rows.len().saturating_sub(fresh.actions);
            if on_item {
                at.min(items.saturating_sub(1))
            } else {
                at
            }
        });
        let line = match &self.line {
            Some(PickerLine::Confirm { .. } | PickerLine::Field(_)) if gone => None,
            line => line.clone(),
        };
        let mut fresh = fresh.at(at);
        fresh.line = line;
        // What was read stays read; the detail shown stays on its item.
        fresh.viewed.clone_from(&self.viewed);
        fresh.detail = same.and(self.detail.clone());
        fresh
    }
}

/// The picker's lines as [`list_picker_parts`] draws them.
#[cfg(test)]
pub(super) fn list_picker_lines(
    picker: &ListPicker,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    list_picker_parts(picker, width, max_lines).0
}

/// The picker's lines ([`list_picker_parts`]) at least `min_lines` long (capped by
/// `max_lines`): the blank lines go above the question and the key hint,
/// so the hint stays on the box's last line.
pub(super) fn list_picker_lines_padded(
    picker: &ListPicker,
    width: usize,
    max_lines: usize,
    min_lines: usize,
) -> Vec<Line<'static>> {
    let (mut lines, tail) = list_picker_parts(picker, width, max_lines);
    let want = min_lines.min(max_lines);
    if lines.len() < want {
        let at = lines.len().saturating_sub(tail);
        lines.splice(
            at..at,
            std::iter::repeat_n(Line::default(), want - lines.len()),
        );
    }
    lines
}

/// How many lines `row` takes at `width`: `full` as the highlighted row,
/// else capped as the others.
fn row_lines(row: &PickRow, width: usize, full: bool) -> usize {
    let note = row.note.as_ref().map_or(0, |note| {
        let inner = width.saturating_sub(NOTE_INDENT.len()).max(1);
        capped(wrap_text(note, inner), full, inner).len()
    });
    capped(wrap_text(&row.text, width), full, width).len() + note
}

/// The rows (each with its note), else the empty line; then the question
/// or refusal; then the key hint. At most `max_lines` lines: the rows
/// scroll to keep the selection visible. With them, how many of the last
/// lines are the tail (the question or refusal and the key hint).
fn list_picker_parts(
    picker: &ListPicker,
    width: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, usize) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let warn = Style::default().fg(Color::Yellow);
    let mut tail = Vec::new();
    match &picker.line {
        Some(PickerLine::Confirm {
            question, nudged, ..
        }) => {
            let ask = warn.add_modifier(Modifier::BOLD);
            tail.extend(
                wrap_text(&format!("{question} {ANSWERS}"), width)
                    .into_iter()
                    .map(|line| Line::styled(line, ask)),
            );
            if *nudged {
                tail.extend(
                    wrap_text(NUDGE, width)
                        .into_iter()
                        .map(|line| Line::styled(line, ask.add_modifier(Modifier::REVERSED))),
                );
            }
        }
        Some(PickerLine::Refused(why)) => tail.extend(
            wrap_text(why, width)
                .into_iter()
                .map(|line| Line::styled(line, warn)),
        ),
        Some(PickerLine::Done(what)) => tail.extend(
            wrap_text(what, width)
                .into_iter()
                .map(|line| Line::styled(line, Style::default().fg(Color::Green))),
        ),
        Some(PickerLine::Field(field)) => {
            tail.extend(
                wrap_text(&field.what.heading(), width)
                    .into_iter()
                    .map(|line| Line::styled(line, warn)),
            );
            tail.extend(
                wrap_text(&format!("> {}▏", field.text), width)
                    .into_iter()
                    .map(Line::from),
            );
            if let Some(why) = field.what.refusal() {
                let refused = Style::default().fg(Color::Red);
                tail.extend(
                    wrap_text(why, width)
                        .into_iter()
                        .map(|line| Line::styled(line, refused)),
                );
            }
        }
        None => {}
    }
    if let (Some(view), Some(row)) = (&picker.detail, picker.selected())
        && let Some(detail) = &row.detail
    {
        return detail_lines(row, detail, view, width, max_lines, tail);
    }
    tail.extend(
        wrap_text(&picker.footer, width)
            .into_iter()
            .map(|line| Line::styled(line, dim)),
    );
    let head: Vec<Line<'static>> = picker
        .header
        .as_deref()
        .map(|header| {
            wrap_text(header, width)
                .into_iter()
                .map(|line| Line::styled(line, Style::default().add_modifier(Modifier::BOLD)))
                .collect()
        })
        .unwrap_or_default();
    let tail_len = tail.len();
    if picker.rows.is_empty() {
        let mut lines = head;
        lines.extend(wrap_text(&picker.empty, width).into_iter().map(Line::from));
        lines.extend(tail);
        return (lines, tail_len);
    }
    let selected = picker.cursor.index();
    let rows: Vec<Vec<Line<'static>>> = picker
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let full = i == selected;
            let text_style = match (full, row.dim) {
                (true, _) => Style::default().add_modifier(Modifier::REVERSED),
                (false, true) => dim,
                (false, false) => Style::default(),
            };
            let mut lines: Vec<Line<'static>> = capped(wrap_text(&row.text, width), full, width)
                .into_iter()
                .map(|line| Line::styled(line, text_style))
                .collect();
            if let Some(note) = &row.note {
                let inner = width.saturating_sub(NOTE_INDENT.len()).max(1);
                lines.extend(
                    capped(wrap_text(note, inner), full, inner)
                        .into_iter()
                        .map(|line| Line::styled(format!("{NOTE_INDENT}{line}"), dim)),
                );
            }
            lines
        })
        .collect();
    let room = max_lines.saturating_sub(tail.len() + head.len());
    let mut lines = head;
    lines.extend(window_lines(rows, selected, room));
    lines.extend(tail);
    (lines, tail_len)
}

/// The highlighted `row`'s whole `detail` in place of the rows: the row's
/// first line, a window of the detail at the view's scroll (it records how
/// far the detail scrolls at this width), where the window is, then `tail`
/// and the keys of the view.
fn detail_lines(
    row: &PickRow,
    detail: &str,
    view: &DetailView,
    width: usize,
    max_lines: usize,
    tail: Vec<Line<'static>>,
) -> (Vec<Line<'static>>, usize) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let text = wrap_text(detail, width);
    let keys = wrap_text(
        "↑/↓ PgUp/PgDn Home/End scroll · Ctrl+O, Esc back to the list · another key acts on the row",
        width,
    );
    let head = wrap_text(&row.text, width)
        .into_iter()
        .next()
        .unwrap_or_default();
    // The row's line, the place line, the tail and the keys.
    let room = max_lines.saturating_sub(2 + tail.len() + keys.len()).max(1);
    let total = text.len();
    let max = total.saturating_sub(room);
    view.scroll_max.set(max);
    let start = view.scroll.min(max);
    let end = (start + room).min(total);
    let mut lines = vec![Line::styled(
        head,
        Style::default().add_modifier(Modifier::REVERSED),
    )];
    lines.push(Line::styled(
        format!("in full: lines {}-{end} of {total}", start + 1),
        dim,
    ));
    lines.extend(text.into_iter().skip(start).take(room).map(Line::from));
    let tail_len = tail.len() + keys.len();
    lines.extend(tail);
    lines.extend(keys.into_iter().map(|line| Line::styled(line, dim)));
    (lines, tail_len)
}

/// How far a row's note is indented under its text.
const NOTE_INDENT: &str = "    ";

/// `lines` (wrapped to `width`) in `full`, else at most [`ROW_CAP`] of
/// them, the last ending in an ellipsis when some were left out.
fn capped(mut lines: Vec<String>, full: bool, width: usize) -> Vec<String> {
    if lines.is_empty() {
        lines.push(String::new());
    }
    if full || lines.len() <= ROW_CAP {
        return lines;
    }
    lines.truncate(ROW_CAP);
    if let Some(last) = lines.last_mut() {
        let room = width.saturating_sub(1);
        *last = format!("{}…", truncate_to_width(last, room).trim_end_matches('…'));
    }
    lines
}

/// One line on what each mode asks and allows (the `/mode` picker's).
fn mode_explained(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Manual => "every edit and command asks you",
        PermissionMode::AcceptEdits => "edits inside the project run; commands ask you",
        PermissionMode::Plan => "read-only: the agent plans and asks before changing anything",
        PermissionMode::Auto => "a classifier reviews what is not always allowed",
    }
}

/// The `/mode` picker: the four modes, `current` marked and highlighted;
/// Enter switches. A `paused` auto mode says so, and Enter on it resumes
/// it (as `/auto resume`).
#[must_use]
pub fn mode_picker(current: Option<PermissionMode>, paused: bool) -> ListPicker {
    let rows = PermissionMode::ALL
        .iter()
        .map(|&mode| {
            let is_current = current == Some(mode);
            let mark = if is_current { "* " } else { "  " };
            let enter = match mode {
                PermissionMode::Auto if is_current && paused => {
                    PickKey::now(KeyCode::Enter, PickAct::SetMode(mode))
                }
                _ if is_current => {
                    PickKey::refused(KeyCode::Enter, format!("{} is the mode now", mode.label()))
                }
                _ => PickKey::now(KeyCode::Enter, PickAct::SetMode(mode)),
            };
            let state = match mode {
                PermissionMode::Auto if is_current && paused => {
                    " · PAUSED after too many blocks: Enter resumes it"
                }
                _ if is_current => " · now",
                _ => "",
            };
            PickRow::new(format!("{mark}{}{state}", mode.label()), vec![enter])
                .noted(mode_explained(mode))
                .keyed(mode.label())
        })
        .collect();
    let at = current
        .and_then(|mode| PermissionMode::ALL.iter().position(|m| *m == mode))
        .unwrap_or(0);
    ListPicker::new(
        PickerKind::Mode,
        rows,
        "",
        "↑/↓ move · Enter switch · Esc close · Shift+Tab cycles the modes",
    )
    .at(at)
}

/// One agent of the workspace as the `/agents` picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    /// Its name (`main-<id>`).
    pub name: String,
    /// `running`, `idle` or `unknown`.
    pub run: &'static str,
    /// Its conversation's short id, when it runs as one.
    pub conversation: Option<String>,
    /// That conversation's title, when known.
    pub title: Option<String>,
    /// How many clients work with it.
    pub clients: usize,
    /// It was left running without a client.
    pub detached: bool,
    /// It is this client's own agent.
    pub mine: bool,
    /// Why the retire key refuses it (`None`: it may be retired).
    pub unretirable: Option<String>,
    /// The agents of other workspaces it is wired to (`/wire`).
    pub wires: Vec<String>,
}

/// The `/agents` picker: the workspace's agents, this client's own
/// highlighted, then a "new agent" row. Enter switches to the agent (or
/// seats a new one on the last row); `d` asks to retire it (`y` retires)
/// or says why it cannot.
#[must_use]
pub fn agent_picker(agents: &[AgentEntry]) -> ListPicker {
    let retire = KeyCode::Char('d');
    let mut rows: Vec<PickRow> = agents
        .iter()
        .map(|agent| {
            let d = match &agent.unretirable {
                Some(why) => PickKey::refused(retire, format!("not retired: {why}")),
                None => PickKey::confirmed(
                    retire,
                    PickAct::RetireAgent(agent.name.clone()),
                    format!("Retire {}? Its conversation stays in /resume.", agent.name),
                ),
            };
            PickRow::new(
                agent_row(agent),
                vec![
                    PickKey::now(KeyCode::Enter, PickAct::SwitchAgent(agent.name.clone())),
                    d,
                ],
            )
            .keyed(agent.name.clone())
        })
        .collect();
    rows.push(PickRow::new(
        "+ new agent",
        vec![
            PickKey::now(KeyCode::Enter, PickAct::NewAgent),
            PickKey::refused(
                retire,
                "not retired: pick an agent to retire (the new-agent row is none)",
            ),
        ],
    ));
    let mine = agents.iter().position(|agent| agent.mine).unwrap_or(0);
    let mut picker = ListPicker::new(
        PickerKind::Agents,
        rows,
        "",
        "↑/↓ move · Enter switch · d retire · Esc close",
    )
    .at(mine);
    picker.actions = 1;
    picker
}

/// `* main-ab12 · idle · 01234567 fix the test · 1 client · this rkat-tui`
/// (the mark and the last part on this client's own agent; `detached`, or
/// `no client`, for one nobody works with).
fn agent_row(agent: &AgentEntry) -> String {
    let mut row = format!(
        "{} {} · {}",
        if agent.mine { "*" } else { " " },
        agent.name,
        agent.run
    );
    match (&agent.conversation, &agent.title) {
        (Some(id), Some(title)) => row.push_str(&format!(" · {id} {title}")),
        (Some(id), None) => row.push_str(&format!(" · {id}")),
        (None, _) => row.push_str(" · no conversation"),
    }
    row.push_str(&format!(
        " · {}",
        crate::text::count(agent.clients, "client")
    ));
    if agent.clients == 0 {
        row.push_str(if agent.detached {
            " · detached"
        } else {
            " · no client yet"
        });
    }
    if !agent.wires.is_empty() {
        row.push_str(&format!(" · wired to {}", agent.wires.join(", ")));
    }
    if agent.mine {
        row.push_str(" · this rkat-tui");
    }
    row
}

impl App {
    /// Open a picker `modal` over nothing (or over the help); behind an
    /// approval dialog it waits until the dialogs are answered (replacing
    /// a picker waiting there); over any other dialog it is dropped.
    pub(super) fn show_modal(&mut self, modal: Modal) {
        match self.modal {
            None | Some(Modal::Help(_)) => self.modal = Some(modal),
            Some(Modal::Escalation { .. }) => self.parked = Some(modal),
            Some(_) => {}
        }
    }

    /// Show `picker`: over nothing (or the keys panel) it opens; behind an
    /// approval dialog it waits until the dialogs are answered. A
    /// `refresh` (the list again after a picker's own act) only replaces
    /// a picker of its kind that is open or waiting, on the same row.
    pub(super) fn show_picker(&mut self, picker: ListPicker, refresh: bool) {
        let same = |modal: &Option<Modal>| matches!(modal, Some(Modal::List(open)) if open.kind == picker.kind);
        if same(&self.modal) {
            if let Some(Modal::List(open)) = &self.modal {
                self.modal = Some(Modal::List(open.refreshed(picker)));
            }
            return;
        }
        if same(&self.parked) {
            if let Some(Modal::List(open)) = &self.parked {
                self.parked = Some(Modal::List(open.refreshed(picker)));
            }
            return;
        }
        if refresh {
            return;
        }
        self.show_modal(Modal::List(picker));
    }

    /// Why a picker's act was refused: a picker of `kind` open or waiting
    /// shows it under its rows.
    pub(super) fn picker_refused(&mut self, kind: PickerKind, why: &str) {
        self.picker_says(kind, &PickerLine::Refused(why.to_string()));
    }

    /// What a picker's act did: a picker of `kind` open or waiting shows
    /// it under its rows (the transcript has it as a notice too).
    pub(super) fn picker_done(&mut self, kind: PickerKind, what: &str) {
        self.picker_says(kind, &PickerLine::Done(what.to_string()));
    }

    /// The `/helpers` picker, open or waiting behind an approval dialog,
    /// as the transcript has its helpers now (a stopped helper's new
    /// state as soon as the daemon's word of its ending arrives), on the
    /// same helper.
    pub(super) fn follow_helper_picker(&mut self) {
        let open = |slot: &Option<Modal>| matches!(slot, Some(Modal::List(open)) if open.kind == PickerKind::Helpers);
        if !open(&self.modal) && !open(&self.parked) {
            return;
        }
        let fresh = self.helper_picker();
        for slot in [&mut self.modal, &mut self.parked] {
            if let Some(Modal::List(open)) = slot
                && open.kind == PickerKind::Helpers
            {
                *open = open.refreshed(fresh.clone());
            }
        }
    }

    /// The runner's check of the `/rehome` picker's directory `typed`: a
    /// question whose `y` re-homes there, or the refusal in the field,
    /// which stays open with its text. Nothing when the picker is gone or
    /// its field holds other text now.
    pub(super) fn rehome_checked(
        &mut self,
        typed: &str,
        answer: Result<(String, std::path::PathBuf), String>,
    ) {
        for slot in [&mut self.modal, &mut self.parked] {
            let Some(Modal::List(picker)) = slot else {
                continue;
            };
            let Some(PickerLine::Field(field)) = &mut picker.line else {
                continue;
            };
            if picker.kind != PickerKind::Rehome || field.text != typed {
                continue;
            }
            match &answer {
                Ok((question, path)) => {
                    picker.line = Some(PickerLine::Confirm {
                        question: question.clone(),
                        act: PickAct::Rehome(format!("to {}", path.display())),
                        nudged: false,
                    });
                }
                Err(why) => {
                    field.what = FieldFor::Directory {
                        refused: Some(why.clone()),
                    };
                }
            }
        }
    }

    /// `line` under the rows of a picker of `kind` open or waiting; a
    /// question pending there stays.
    fn picker_says(&mut self, kind: PickerKind, line: &PickerLine) {
        for slot in [&mut self.modal, &mut self.parked] {
            if let Some(Modal::List(picker)) = slot
                && picker.kind == kind
                && !matches!(picker.line, Some(PickerLine::Confirm { .. }))
            {
                // A field that holds its refusals (a rule's edit) keeps
                // its text and says why there.
                if let (PickerLine::Refused(why), Some(PickerLine::Field(field))) =
                    (line, &mut picker.line)
                    && matches!(field.what, FieldFor::EditRule { .. })
                    && field.what.hold_refusal(why)
                {
                    continue;
                }
                picker.line = Some(line.clone());
            }
        }
    }

    /// A picker's answer: what the typed command does. The picker stays
    /// open (its list comes again in place) but for the acts that go
    /// somewhere else ([`PickAct::leaves_picker`]).
    pub(super) fn picked(&mut self, act: PickAct, fx: &mut Vec<Effect>) {
        if act.leaves_picker() {
            self.modal = None;
        }
        match act {
            PickAct::Schedule(op, id) => fx.push(Effect::PickedSchedule(op, id)),
            PickAct::RemoveRule(n) => fx.push(Effect::PickedRuleRemoval(n)),
            PickAct::RemoveGrant(id) => fx.push(Effect::PickedGrantRemoval(id)),
            PickAct::RemoveStatement(id) => fx.push(Effect::PickedStatementRemoval(id)),
            PickAct::Approve(id, scope) => fx.push(Effect::PickedApproval(id, scope, None)),
            PickAct::Withdraw(id) => fx.push(Effect::PickedWithdrawal(id)),
            PickAct::TrustHost { host, user } => fx.push(Effect::PickedHostTrust { host, user }),
            PickAct::TrustCallHosts { id, user } => {
                fx.push(Effect::PickedCallHostTrust { id, user });
            }
            PickAct::UntrustHost(host) => fx.push(Effect::PickedHostRemoval(host)),
            PickAct::AllowEffect { id, scope, text } => {
                fx.push(Effect::PickedApproval(id, scope, Some(text)));
            }
            PickAct::AddStatement { scope, text } => {
                fx.push(Effect::PickedStatement { scope, text });
            }
            PickAct::AddRule(form) => fx.push(Effect::PickedRule(form)),
            // The field stays open, its text kept, until the daemon's
            // answer: the refreshed list (the row changed) or a refusal
            // in the field.
            PickAct::EditRule { n, old, new, user } => {
                if let Some(Modal::List(picker)) = &mut self.modal
                    && picker.kind == PickerKind::Permissions
                {
                    let mut field = FieldFor::edit_rule(n, new.clone(), user);
                    if let FieldFor::EditRule { old: shown, .. } = &mut field.what {
                        shown.clone_from(&old);
                    }
                    picker.line = Some(PickerLine::Field(field));
                }
                fx.push(Effect::PickedRuleEdit { n, old, new });
            }
            PickAct::SetMode(mode) => fx.push(Effect::SetMode(mode)),
            PickAct::SetSetting {
                key,
                value,
                user,
                filter,
            } => {
                fx.push(Effect::PickedSetting {
                    key,
                    value,
                    user,
                    filter,
                });
            }
            PickAct::OpenMemory(target) => fx.push(Effect::OpenMemory(target)),
            PickAct::Wire { workspace, agent } => fx.push(Effect::WireTo { workspace, agent }),
            PickAct::RemoveWire { workspace, agent } => {
                fx.push(Effect::RemoveWire { workspace, agent });
            }
            // A switch during a turn waits for its end.
            PickAct::SwitchAgent(name) => {
                if !self.defer(
                    format!("/agents {name}"),
                    super::deferred::Deferred::Choice(Effect::Agents(Some(name.clone()))),
                ) {
                    fx.push(Effect::Agents(Some(name)));
                }
            }
            PickAct::NewAgent => {
                if !self.defer(
                    "/agents: a new agent",
                    super::deferred::Deferred::Choice(Effect::NewAgent),
                ) {
                    fx.push(Effect::NewAgent);
                }
            }
            PickAct::RetireAgent(name) => fx.push(Effect::RetireAgent(name)),
            // As typed: during a turn it waits for its end.
            PickAct::Rehome(argument) => {
                if !self.defer(
                    format!("/rehome {argument}"),
                    super::deferred::Deferred::Choice(Effect::Rehome(Some(argument.clone()))),
                ) {
                    fx.push(Effect::Rehome(Some(argument)));
                }
            }
            PickAct::RehomeDiscarding { argument, discard } => {
                fx.push(Effect::RehomeDiscarding { argument, discard });
            }
            PickAct::RemoveState(id) => fx.push(Effect::RemoveState(id)),
            // The field stays open, its text kept, until the check's answer.
            PickAct::CheckRehome(typed) => {
                if let Some(Modal::List(picker)) = &mut self.modal
                    && picker.kind == PickerKind::Rehome
                {
                    picker.line = Some(PickerLine::Field(Field {
                        what: FieldFor::Directory { refused: None },
                        text: typed.clone(),
                    }));
                }
                fx.push(Effect::RehomeCheck(typed));
            }
            PickAct::ViewHelper(header) => {
                self.helper_view = Some(OpenHelperView {
                    header,
                    scroll: ScrollState::default(),
                    confirm_stop: false,
                });
                self.clear_selection();
            }
            // The helpers' picker follows the transcript after every
            // update ([`App::follow_helper_picker`]).
            PickAct::FoldHelper(header) => {
                self.toggle_helper(header);
            }
            PickAct::StopHelper(header) => self.stop_helper(header, fx),
            PickAct::ShowEndedHelpers(all) => self.helpers_all = all,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn picker() -> ListPicker {
        ListPicker::new(
            PickerKind::Schedules,
            vec![
                PickRow::new(
                    "a",
                    vec![
                        PickKey::now(
                            KeyCode::Enter,
                            PickAct::Schedule(ScheduleOp::Pause, "1".into()),
                        ),
                        PickKey::confirmed(
                            KeyCode::Char('d'),
                            PickAct::Schedule(ScheduleOp::Delete, "1".into()),
                            "delete a?",
                        ),
                    ],
                ),
                PickRow::new("b", vec![PickKey::refused(KeyCode::Char('d'), "b stays")]),
            ],
            "none",
            "keys",
        )
    }

    /// Rows act at once, ask first or say why not; a key a row does not
    /// have does nothing; ↑/↓ and j/k move within the rows; Esc, `q` and
    /// Ctrl-C close; a held key only moves.
    #[test]
    fn a_list_picker_acts_asks_refuses_moves_and_closes() {
        let mut p = picker();
        assert_eq!(
            p.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Pause, "1".into()))
        );
        assert_eq!(p.key(&press(KeyCode::Char('d'))), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Confirm { question, nudged: false, .. }) if question == "delete a?")
        );
        // `n` cancels the question; `y` after a fresh `d` acts.
        assert_eq!(p.key(&press(KeyCode::Char('n'))), ModalKey::Stay);
        assert_eq!(p.line, None);
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(
            p.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Delete, "1".into()))
        );
        assert_eq!(p.line, None);
        // The second row refuses with its reason; Enter there does nothing.
        p.key(&press(KeyCode::Char('j')));
        p.key(&press(KeyCode::Down));
        assert_eq!(p.cursor.index(), 1);
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(p.line, Some(PickerLine::Refused("b stays".into())));
        assert_eq!(p.key(&press(KeyCode::Enter)), ModalKey::Stay);
        assert_eq!(p.line, None);
        p.key(&press(KeyCode::Char('k')));
        assert_eq!(p.cursor.index(), 0);
        let mut held = press(KeyCode::Enter);
        held.kind = KeyEventKind::Repeat;
        assert_eq!(p.key(&held), ModalKey::Stay);
        let mut held_down = press(KeyCode::Down);
        held_down.kind = KeyEventKind::Repeat;
        p.key(&held_down);
        assert_eq!(p.cursor.index(), 1);
        assert_eq!(p.key(&press(KeyCode::Char('q'))), ModalKey::Close);
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Close);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(p.key(&ctrl_c), ModalKey::Close);
        // An empty picker moves nowhere and acts on nothing.
        let mut empty = ListPicker::new(PickerKind::Schedules, vec![], "none", "keys");
        assert_eq!(empty.key(&press(KeyCode::Enter)), ModalKey::Stay);
        assert_eq!(empty.key(&press(KeyCode::Down)), ModalKey::Stay);
        assert_eq!(empty.selected(), None);
    }

    /// While a question is pending only `y`, `n` and Esc answer it: Enter
    /// (the row's own act), the arrows and every other key leave it up,
    /// act on nothing, move nothing and point at the keys that answer.
    #[test]
    fn a_pending_question_takes_only_y_n_and_esc() {
        let mut p = picker();
        p.key(&press(KeyCode::Char('d')));
        for code in [
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('j'),
            KeyCode::Char('q'),
            KeyCode::Char('d'),
            KeyCode::Char('s'),
            KeyCode::Tab,
        ] {
            assert_eq!(p.key(&press(code)), ModalKey::Stay, "{code:?}");
            assert_eq!(p.cursor.index(), 0, "{code:?}");
            assert!(
                matches!(&p.line, Some(PickerLine::Confirm { question, nudged: true, .. }) if question == "delete a?"),
                "{code:?}: {:?}",
                p.line
            );
        }
        let text = list_picker_lines(&p, 60, 20)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("delete a? y yes · n or Esc no"), "{text}");
        assert!(
            text.contains("press y to confirm, n or Esc to cancel"),
            "{text}"
        );
        // `y` still answers after the nudge.
        assert_eq!(
            p.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Delete, "1".into()))
        );
    }

    /// Esc cancels a pending question, not the picker; a second Esc closes
    /// it. The question's line is in the warning style and names its keys.
    #[test]
    fn esc_cancels_the_question_then_closes_the_picker() {
        let mut p = picker();
        p.key(&press(KeyCode::Char('d')));
        let lines = list_picker_lines(&p, 60, 20);
        let ask = lines
            .iter()
            .find(|line| line.to_string().contains("delete a?"))
            .unwrap();
        assert_eq!(ask.style.fg, Some(Color::Yellow));
        assert!(ask.to_string().ends_with("y yes · n or Esc no"), "{ask}");
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Stay);
        assert_eq!(p.line, None);
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Close);
    }

    /// What an act did shows under the rows (a question pending is not
    /// replaced by it) until the next key.
    #[test]
    fn an_acts_outcome_is_stated_on_the_picker() {
        let mut app = App::new(crate::ui::StatusInfo::default());
        app.update(crate::ui::Msg::ShowPicker {
            picker: picker(),
            refresh: false,
        });
        app.update(crate::ui::Msg::PickerDone {
            kind: PickerKind::Schedules,
            text: "deleted schedule a".into(),
        });
        let Some(Modal::List(open)) = &app.modal else {
            panic!("the picker is open")
        };
        assert_eq!(
            open.line,
            Some(PickerLine::Done("deleted schedule a".into()))
        );
        let lines = list_picker_lines(open, 60, 20);
        assert!(
            lines
                .iter()
                .any(|line| line.to_string() == "deleted schedule a"
                    && line.style.fg == Some(Color::Green))
        );
        // Another kind's outcome does not land here.
        app.update(crate::ui::Msg::PickerDone {
            kind: PickerKind::Agents,
            text: "retired main-ab12".into(),
        });
        let Some(Modal::List(open)) = &mut app.modal else {
            panic!("the picker is open")
        };
        assert_eq!(
            open.line,
            Some(PickerLine::Done("deleted schedule a".into()))
        );
        open.key(&press(KeyCode::Char('d')));
        app.update(crate::ui::Msg::PickerDone {
            kind: PickerKind::Schedules,
            text: "late".into(),
        });
        let Some(Modal::List(open)) = &mut app.modal else {
            panic!("the picker is open")
        };
        assert!(matches!(open.line, Some(PickerLine::Confirm { .. })));
    }

    /// A row's text and note wrap to the dialog's width: the highlighted
    /// row in full, every other capped at two lines of each with an
    /// ellipsis; the window keeps the highlighted row whole.
    #[test]
    fn rows_wrap_the_highlighted_in_full_the_others_capped() {
        let long = "word ".repeat(30);
        let rows = (0..3)
            .map(|i| {
                PickRow::new(format!("row{i} {long}"), vec![]).noted(format!("note{i} {long}"))
            })
            .collect();
        let mut p = ListPicker::new(PickerKind::Permissions, rows, "none", "keys");
        let text = |p: &ListPicker, room| -> Vec<String> {
            list_picker_lines(p, 30, room)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        let all = text(&p, 100);
        // Row 0 (highlighted): every word of its text and note is shown.
        let row0: Vec<&String> = all
            .iter()
            .take_while(|line| !line.starts_with("row1"))
            .collect();
        assert!(row0.len() > 4, "{all:#?}");
        assert!(row0.iter().all(|line| !line.contains('…')), "{all:#?}");
        assert_eq!(
            row0.iter()
                .map(|l| l.matches("word").count())
                .sum::<usize>(),
            60
        );
        assert!(
            all.iter().all(|line| line.chars().count() <= 30),
            "{all:#?}"
        );
        // Row 1: two lines of text, two of note, each pair ending in "…".
        let at = all
            .iter()
            .position(|line| line.starts_with("row1"))
            .unwrap();
        assert!(all[at + 1].ends_with('…'), "{all:#?}");
        assert!(all[at + 2].starts_with("    note1"), "{all:#?}");
        assert!(all[at + 3].ends_with('…'), "{all:#?}");
        assert!(all[at + 4].starts_with("row2"), "{all:#?}");
        // Every line of the highlighted row's text is reversed.
        let lines = list_picker_lines(&p, 30, 100);
        assert!(
            lines
                .iter()
                .take_while(|line| !line.to_string().starts_with("    note0"))
                .all(|line| line.style.add_modifier.contains(Modifier::REVERSED))
        );
        // Highlighting the last row in a short window shows it whole.
        p.key(&press(KeyCode::Down));
        p.key(&press(KeyCode::Down));
        let short = text(&p, 14);
        assert!(
            short.iter().any(|line| line.starts_with("row2")),
            "{short:#?}"
        );
        assert_eq!(
            short
                .iter()
                .filter(|line| !line.starts_with("keys"))
                .skip_while(|line| !line.starts_with("row2"))
                .map(|l| l.matches("word").count())
                .sum::<usize>(),
            60,
            "{short:#?}"
        );
    }

    /// A refreshed picker keeps the row the user was on, the last when the
    /// list got shorter.
    #[test]
    fn a_refresh_keeps_the_row() {
        let open = picker().at(1);
        let fresh = open.refreshed(picker());
        assert_eq!(fresh.cursor.index(), 1);
        let shorter = ListPicker::new(
            PickerKind::Schedules,
            vec![PickRow::new("a", vec![])],
            "none",
            "keys",
        );
        assert_eq!(open.refreshed(shorter).cursor.index(), 0);
    }

    /// Rows with ids: a refresh keeps the highlight on the same item
    /// wherever it moved, and the line under the rows with it; when the
    /// item is gone, on its neighbour (the row in its place), dropping a
    /// question about it but keeping an act's outcome.
    #[test]
    fn a_refresh_follows_the_item_and_keeps_the_line() {
        let list = |ids: &[&str]| {
            ListPicker::new(
                PickerKind::Schedules,
                ids.iter()
                    .map(|id| {
                        PickRow::new(
                            format!("row {id}"),
                            vec![PickKey::confirmed(
                                KeyCode::Char('d'),
                                PickAct::Schedule(ScheduleOp::Delete, (*id).into()),
                                format!("delete {id}?"),
                            )],
                        )
                        .keyed(*id)
                    })
                    .collect(),
                "none",
                "keys",
            )
        };
        let mut open = list(&["a", "b", "c"]).at(1);
        open.key(&press(KeyCode::Char('d')));
        // "a" went: "b" is now the first row, still highlighted, still asked.
        let fresh = open.refreshed(list(&["b", "c"]));
        assert_eq!(fresh.selected().unwrap().text, "row b");
        assert!(
            matches!(&fresh.line, Some(PickerLine::Confirm { question, .. }) if question == "delete b?")
        );
        // "b" went: on its neighbour, the question about it dropped.
        let gone = fresh.refreshed(list(&["c"]));
        assert_eq!(gone.selected().unwrap().text, "row c");
        assert_eq!(gone.line, None);
        // An outcome stays over a refresh.
        let mut done = list(&["a", "b"]).at(1);
        done.line = Some(PickerLine::Done("deleted b".into()));
        let after = done.refreshed(list(&["a"]));
        assert_eq!(after.selected().unwrap().text, "row a");
        assert_eq!(after.line, Some(PickerLine::Done("deleted b".into())));
    }

    /// Every act leaves its picker open (its list comes again in place)
    /// but for the ones that go somewhere else: a mode chosen, a helper's
    /// full view, another agent or a new one.
    #[test]
    fn only_acts_that_go_elsewhere_close_the_picker() {
        let id = BlockId(0);
        let stays = [
            PickAct::Schedule(ScheduleOp::Pause, "1".into()),
            PickAct::RemoveRule(1),
            PickAct::RemoveGrant(1),
            PickAct::RemoveStatement(1),
            PickAct::Approve(1, GrantScope::Once),
            PickAct::Withdraw(1),
            PickAct::AllowEffect {
                id: 1,
                scope: GrantScope::Session,
                text: "x".into(),
            },
            PickAct::AddStatement {
                scope: GrantScope::Always,
                text: "x".into(),
            },
            PickAct::AddRule(RuleForm {
                deny: false,
                tool: "shell".into(),
                pattern: None,
                literal: false,
                user: false,
            }),
            PickAct::SetSetting {
                key: "k".into(),
                value: None,
                user: false,
                filter: None,
            },
            PickAct::OpenMemory(MemoryTarget::User),
            PickAct::FoldHelper(id),
            PickAct::StopHelper(id),
            PickAct::ShowEndedHelpers(true),
            PickAct::Wire {
                workspace: "/w".into(),
                agent: "a".into(),
            },
            PickAct::RemoveWire {
                workspace: "/w".into(),
                agent: "a".into(),
            },
            PickAct::RetireAgent("a".into()),
        ];
        for act in stays {
            let mut app = App::new(crate::ui::StatusInfo::default());
            app.update(crate::ui::Msg::ShowPicker {
                picker: picker(),
                refresh: false,
            });
            let mut fx = Vec::new();
            app.picked(act.clone(), &mut fx);
            assert!(
                matches!(&app.modal, Some(Modal::List(p)) if p.kind == PickerKind::Schedules),
                "{act:?} closed the picker"
            );
        }
        for act in [
            PickAct::SetMode(PermissionMode::Plan),
            PickAct::ViewHelper(id),
            PickAct::SwitchAgent("a".into()),
            PickAct::NewAgent,
        ] {
            let mut app = App::new(crate::ui::StatusInfo::default());
            app.update(crate::ui::Msg::ShowPicker {
                picker: picker(),
                refresh: false,
            });
            let mut fx = Vec::new();
            app.picked(act.clone(), &mut fx);
            assert!(app.modal.is_none(), "{act:?} left the picker open");
        }
    }

    /// The mode picker: four rows, the current one marked and highlighted;
    /// Enter on another switches, on the current one says it is the mode
    /// now; a paused auto mode says so and Enter on it resumes it.
    #[test]
    fn the_mode_picker_marks_the_mode_and_resumes_a_paused_auto() {
        let mut p = mode_picker(Some(PermissionMode::Plan), false);
        let texts: Vec<&str> = p.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["  manual", "  accept edits", "* plan · now", "  auto"]
        );
        assert!(p.rows.iter().all(|r| r.note.is_some()));
        assert_eq!(p.cursor.index(), 2);
        p.key(&press(KeyCode::Enter));
        assert_eq!(
            p.line,
            Some(PickerLine::Refused("plan is the mode now".into()))
        );
        p.key(&press(KeyCode::Up));
        assert_eq!(
            p.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::SetMode(PermissionMode::AcceptEdits))
        );
        let mut paused = mode_picker(Some(PermissionMode::Auto), true);
        assert_eq!(paused.cursor.index(), 3);
        assert!(
            paused.rows[3].text.contains("PAUSED"),
            "{:?}",
            paused.rows[3]
        );
        assert_eq!(
            paused.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::SetMode(PermissionMode::Auto))
        );
        // Before the mode is known nothing is marked.
        let unknown = mode_picker(None, false);
        assert!(unknown.rows.iter().all(|r| r.text.starts_with("  ")));
    }

    fn entry(name: &str, mine: bool, unretirable: Option<&str>) -> AgentEntry {
        AgentEntry {
            name: name.into(),
            run: "idle",
            conversation: None,
            title: None,
            clients: usize::from(mine),
            detached: !mine,
            mine,
            unretirable: unretirable.map(str::to_string),
            wires: Vec::new(),
        }
    }

    fn agents() -> Vec<AgentEntry> {
        vec![
            entry("main-aaaa", false, None),
            entry(
                "main-bbbb",
                true,
                Some("main-bbbb is this rkat-tui's own agent"),
            ),
        ]
    }

    /// An agent's row names its wires to agents of other workspaces.
    #[test]
    fn an_agent_row_names_its_cross_workspace_wires() {
        let mut agent = agents()[1].clone();
        assert!(!agent_row(&agent).contains("wired"));
        agent.wires = vec!["api/main-zz99".into(), "web/main-yy88".into()];
        assert!(
            agent_row(&agent).ends_with(" · wired to api/main-zz99, web/main-yy88 · this rkat-tui"),
            "{}",
            agent_row(&agent)
        );
    }

    /// The agent picker opens on this client's own agent; Enter switches,
    /// the last row seats a new agent, `d` asks and `y` retires (`n` or
    /// Esc keeps the agent), `d` on an agent in use says why and retires
    /// nothing; Esc, `q` and Ctrl-C close.
    #[test]
    fn the_agent_picker_switches_seats_and_retires_after_a_confirmation() {
        let mut p = agent_picker(&agents());
        assert!(p.selected().unwrap().text.contains("main-bbbb"));
        assert_eq!(
            p.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::SwitchAgent("main-bbbb".into()))
        );
        // Its own agent is refused with the reason.
        assert_eq!(p.key(&press(KeyCode::Char('d'))), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Refused(why)) if why.contains("own agent")),
            "{:?}",
            p.line
        );
        assert_eq!(p.key(&press(KeyCode::Char('y'))), ModalKey::Stay);
        assert_eq!(p.line, None);
        // Up to the other one: `d` asks, `n` keeps it, `d` `y` retires it.
        p.key(&press(KeyCode::Up));
        assert_eq!(p.key(&press(KeyCode::Char('d'))), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Confirm { act: PickAct::RetireAgent(name), .. }) if name == "main-aaaa"),
            "{:?}",
            p.line
        );
        assert_eq!(p.key(&press(KeyCode::Char('n'))), ModalKey::Stay);
        assert_eq!(p.line, None);
        // A cancelled confirmation does not also act: Esc there keeps the
        // picker open, and Enter there does not switch.
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(p.key(&press(KeyCode::Enter)), ModalKey::Stay);
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Stay);
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(
            p.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::RetireAgent("main-aaaa".into()))
        );
        // The last row is a new agent; `d` there retires nothing.
        p.key(&press(KeyCode::Down));
        p.key(&press(KeyCode::Char('j')));
        p.key(&press(KeyCode::Down));
        assert_eq!(p.selected().unwrap().text, "+ new agent");
        p.key(&press(KeyCode::Char('d')));
        assert!(matches!(p.line, Some(PickerLine::Refused(_))));
        assert_eq!(
            p.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::NewAgent)
        );
        // A held key only moves; Esc, `q` and Ctrl-C close.
        let mut held = press(KeyCode::Char('d'));
        held.kind = KeyEventKind::Repeat;
        assert_eq!(p.key(&held), ModalKey::Stay);
        assert_eq!(p.line, None);
        assert_eq!(p.key(&press(KeyCode::Char('q'))), ModalKey::Close);
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Close);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(p.key(&ctrl_c), ModalKey::Close);
    }

    /// A refresh after the last agent was retired lands on the agent
    /// before it, not on the "new agent" row; one on that row stays there.
    #[test]
    fn an_agent_refresh_stays_on_an_agent() {
        let three = vec![
            entry("main-aaaa", true, Some("own")),
            entry("main-bbbb", false, None),
            entry("main-cccc", false, None),
        ];
        let open = agent_picker(&three).at(2);
        let fresh = open.refreshed(agent_picker(&three[..2]));
        assert!(fresh.selected().unwrap().text.contains("main-bbbb"));
        let on_new = agent_picker(&three).at(3);
        let fresh = on_new.refreshed(agent_picker(&three[..2]));
        assert_eq!(fresh.selected().unwrap().text, "+ new agent");
    }
}
