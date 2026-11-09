//! Slash-command parsing.

use std::fmt;

use crate::approval::mode::PermissionMode;

/// Whether a rule allows or denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleEffect {
    /// Allow matching calls without review.
    Allow,
    /// Deny matching calls.
    Deny,
}

/// `/rules` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesCommand {
    /// List the active rules.
    List,
    /// Add a rule.
    Add {
        /// Allow or deny.
        effect: RuleEffect,
        /// Tool name.
        tool: String,
        /// Optional argument pattern (rest of the line).
        pattern: Option<String>,
    },
    /// Remove rule number `n` (as shown by `/rules list`).
    Remove(usize),
    /// Trust the realm `tui.toml` as it is now (its summary is shown and its
    /// digest recorded in the host trust record).
    Trust,
}

/// `/auto` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCommand {
    /// Show the auto-mode state.
    Status,
    /// Switch to auto mode.
    On,
    /// Switch to manual mode (the human answers what auto mode reviews).
    Off,
    /// Resume after a denial-limit pause.
    Resume,
}

/// `/model` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCommand {
    /// `/model`: open the picker (from the cached provider list).
    Pick,
    /// `/model refresh`: refetch the provider list, then open the picker.
    Refresh,
    /// `/model <id>`: switch to this model.
    Set(String),
}

/// `/compact` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactCommand {
    /// `/compact` (or `/compact now`): compact before the next turn's first
    /// model call.
    Now,
    /// `/compact <instructions>`: as [`Self::Now`]; the engine has no
    /// instruction-guided compaction, so the instructions are not used.
    Focused(String),
    /// `/compact status`: report the compaction policy.
    Status,
}

/// Which instruction file `/memory` opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTarget {
    /// The workspace `AGENTS.md` (`/memory`).
    Workspace,
    /// The user-global `AGENTS.md` (`/memory user`).
    User,
    /// The user-global `SYSTEM.md` (`/memory system`).
    System,
}

/// Which helpers a `/helpers expand|fold` applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperTarget {
    /// Every helper.
    All,
    /// The newest helper with this label.
    Label(String),
}

/// `/helpers` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpersCommand {
    /// List the helpers with their state.
    List,
    /// Show the helpers' nested blocks.
    Expand(HelperTarget),
    /// Fold the helpers to one line.
    Fold(HelperTarget),
}

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/quit`, `/exit`.
    Quit,
    /// `/clear`: clear the transcript and start a new session.
    Clear,
    /// `/resume [ref]`.
    Resume(Option<String>),
    /// `/rewind` (`/checkpoint`, `/undo`): pick one of the session's
    /// messages and continue from just before it.
    Rewind,
    /// `/rules ...`.
    Rules(RulesCommand),
    /// `/auto [on|off|resume]`.
    Auto(AutoCommand),
    /// `/mode [default|accept-edits|plan|auto]`: show (`None`) or set the
    /// permission mode.
    Mode(Option<PermissionMode>),
    /// `/model [id | refresh]`.
    Model(ModelCommand),
    /// `/help`.
    Help,
    /// `/remember <text>` or a single-line `#text` input: append a line to the
    /// workspace AGENTS.md.
    Remember(String),
    /// `/compact [now | status | <instructions>]`.
    Compact(CompactCommand),
    /// `/cost`: the session's token cost by model.
    Cost,
    /// `/status`: version, model, session, workspace and trust state.
    Status,
    /// `/export [file]`: write the transcript as plain text.
    Export(Option<String>),
    /// `/init`: ask the agent to write or update the workspace `AGENTS.md`.
    Init,
    /// `/memory [user | system]`: open an instruction file in the user's
    /// editor.
    Memory(MemoryTarget),
    /// `/context`: the context gauge, the compaction policy and what the
    /// context holds, as far as the TUI knows.
    Context,
    /// `/copy [N]`: copy the Nth latest assistant answer (1, the default,
    /// is the latest) to the clipboard (OSC 52).
    Copy(usize),
    /// `/trust` (alias `/mcp trust`): trust every project input
    /// (`.rkat/mcp.toml`, `.rkat/config.toml`, the workspace realm's
    /// `config.toml`) as it is now; their servers and hooks run from the
    /// next session.
    TrustProject,
    /// `/helpers [expand|fold <label>|all]`.
    Helpers(HelpersCommand),
    /// `/daemon stop`: shut the attached `rkat-tui-mob` down (attach mode).
    DaemonStop,
}

/// Slash-command parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// The command name is not known.
    Unknown(String),
    /// The command is known but its arguments are wrong.
    Usage(&'static str),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "unknown command /{name} (try /help)"),
            Self::Usage(usage) => write!(f, "usage: {usage}"),
        }
    }
}

const RULES_USAGE: &str = "/permissions [list | add allow|deny <tool> [pattern] | rm <n> | trust]";
const AUTO_USAGE: &str = "/auto [on | off | resume]";
const MODE_USAGE: &str = "/mode [default | accept-edits | plan | auto]";
const HELPERS_USAGE: &str = "/helpers [expand <label>|all | fold <label>|all]";

/// The prompt `/init` sends as an ordinary user turn.
pub const INIT_PROMPT: &str = "Please inspect this project and write an AGENTS.md file in the workspace root (update the existing one if there is one, keeping what is still true) for future agents working here. Cover: the build, test, lint and run commands (including how to run a single test); the layout of the code (the main directories and what lives where); and the conventions a newcomer would not guess (style, naming, error handling, testing, commit habits). Be concrete and specific to this repository, skip generic advice, and keep the file under 200 lines.";

/// One slash command: its names, its arguments and its `/help` rows. The
/// parser dispatches through [`COMMANDS`] and `/help`'s "Commands:" section
/// is generated from it, so the two cannot drift apart.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    /// The command's name, without the `/`.
    pub name: &'static str,
    /// Other names that run the same command.
    pub aliases: &'static [&'static str],
    /// The arguments, as the completion menu shows them (empty: none).
    pub usage: &'static str,
    /// One line on what the command does (the completion menu's).
    pub summary: &'static str,
    /// `/help` rows: the left column and the description, whose `\n`
    /// separates continuation lines. Empty: the command is described in
    /// another one's rows (`/mcp trust` under `/trust`).
    pub help: &'static [(&'static str, &'static str)],
    /// Parse the text after the name (trimmed).
    parse: fn(&str) -> Result<Command, CommandError>,
}

impl CommandSpec {
    /// Whether `/word` runs this command.
    #[must_use]
    pub fn is_named(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }

    /// The spelling of this command that starts with `prefix` (compared
    /// case-insensitively): the name when it does, else the first alias that
    /// does.
    #[must_use]
    pub fn completion_for(&self, prefix: &str) -> Option<&'static str> {
        let prefix = prefix.to_lowercase();
        std::iter::once(self.name)
            .chain(self.aliases.iter().copied())
            .find(|name| name.starts_with(&prefix))
    }
}

/// Every slash command, in `/help` order.
pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        aliases: &["?"],
        usage: "",
        summary: "show the commands and keys",
        help: &[("/help, /?", "show this help")],
        parse: |_| Ok(Command::Help),
    },
    CommandSpec {
        name: "quit",
        aliases: &["exit", "q"],
        usage: "",
        summary: "leave rkat-tui",
        help: &[(
            "/quit, /exit, /q",
            "leave rkat-tui (asks again while a turn runs or messages are queued)\nand release its lease on the realm",
        )],
        parse: |_| Ok(Command::Quit),
    },
    CommandSpec {
        name: "clear",
        aliases: &["new"],
        usage: "",
        summary: "clear the transcript and start a new session",
        help: &[(
            "/clear, /new",
            "clear the transcript and start a new session (idle only)",
        )],
        parse: |_| Ok(Command::Clear),
    },
    CommandSpec {
        name: "resume",
        aliases: &[],
        usage: "[ref]",
        summary: "resume a session (no ref: pick one)",
        help: &[(
            "/resume [ref]",
            "resume a session (last, ~N, or id prefix); no ref opens a picker\n(so does rkat-tui --resume without a ref; archived sessions\nare not offered)",
        )],
        parse: parse_resume,
    },
    CommandSpec {
        name: "rewind",
        aliases: &["checkpoint", "undo"],
        usage: "",
        summary: "continue from before one of your messages (idle only)",
        help: &[(
            "/rewind, /checkpoint, /undo",
            "pick one of your messages (newest first) and continue from just\nbefore it: a new session gets the history up to there and the\nmessage goes back in the input; the full conversation stays\nin the previous session (/resume). Esc Esc on an empty input\nopens it too. Files are not restored (idle only)",
        )],
        parse: |rest| arg_free(rest, Command::Rewind, "/rewind"),
    },
    CommandSpec {
        name: "permissions",
        aliases: &["rules"],
        usage: "[list | add allow|deny <tool> [pattern] | rm <n> | trust]",
        summary: "list or edit the approval rules",
        help: &[
            (
                "/permissions [list]",
                "list approval rules (/rules is an alias)",
            ),
            (
                "/permissions add allow|deny <tool> [pattern]",
                "add an approval rule",
            ),
            ("/permissions rm <n>", "remove rule n (rm or remove)"),
            (
                "/permissions trust",
                "show the realm tui.toml and trust it as it is now",
            ),
        ],
        parse: |rest| parse_rules(&words(rest)).map(Command::Rules),
    },
    CommandSpec {
        name: "trust",
        aliases: &[],
        usage: "",
        summary: "trust the project's MCP and config files as they are now",
        help: &[(
            "/trust",
            "trust the project's .rkat/mcp.toml, .rkat/config.toml and\nworkspace realm config.toml as they are now (their MCP\nservers and hooks run from the next session; /mcp trust\nis an alias; rkat-tui --trust-project trusts them for one\nrun without recording)",
        )],
        parse: |rest| arg_free(rest, Command::TrustProject, "/trust"),
    },
    CommandSpec {
        name: "mcp",
        aliases: &[],
        usage: "trust",
        summary: "the same as /trust",
        help: &[],
        parse: |rest| match words(rest).as_slice() {
            ["trust"] => Ok(Command::TrustProject),
            _ => Err(CommandError::Usage("/trust (or /mcp trust)")),
        },
    },
    CommandSpec {
        name: "mode",
        aliases: &[],
        usage: "[default | accept-edits | plan | auto]",
        summary: "show or set the permission mode",
        help: &[
            (
                "/mode",
                "show the permission mode (Shift+Tab cycles manual → accept\nedits → plan → auto)",
            ),
            (
                "/mode default|accept-edits|plan|auto",
                "set it: default (manual) asks for every edit and command,\naccept-edits allows in-project edits, plan is read-only and\nends in a plan to approve, auto has the classifier review",
            ),
        ],
        parse: parse_mode,
    },
    CommandSpec {
        name: "auto",
        aliases: &[],
        usage: "[on | off | resume]",
        summary: "show the mode; switch auto mode on, off, or resume it",
        help: &[
            (
                "/auto",
                "show the permission mode and auto mode's denial counters",
            ),
            (
                "/auto on|off|resume",
                "auto mode (= /mode auto), manual mode (= /mode default), or\nresume auto mode after a denial pause",
            ),
        ],
        parse: parse_auto,
    },
    CommandSpec {
        name: "model",
        aliases: &[],
        usage: "[[endpoint:]id | refresh]",
        summary: "pick or switch the model",
        help: &[
            (
                "/model",
                "pick a model from every endpoint's list (type to filter)",
            ),
            (
                "/model [endpoint:]<id>",
                "switch to a model (idle only); a bare id is looked up",
            ),
            (
                "/model refresh",
                "refetch every endpoint's model list and pick",
            ),
        ],
        parse: parse_model,
    },
    CommandSpec {
        name: "remember",
        aliases: &[],
        usage: "<text>",
        summary: "add a line to the workspace AGENTS.md",
        help: &[(
            "/remember <text>, #<text>",
            "add a line to the workspace AGENTS.md and to this session\n(\\# sends a literal #)",
        )],
        parse: |rest| {
            if rest.is_empty() {
                Err(CommandError::Usage("/remember <text>"))
            } else {
                Ok(Command::Remember(rest.to_string()))
            }
        },
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        usage: "[now | status | <instructions>]",
        summary: "compact the context (idle only)",
        help: &[
            (
                "/compact [now]",
                "ask the engine to compact the context at the next\nmodel call (idle only)",
            ),
            (
                "/compact <instructions>",
                "the same; focus instructions are not supported yet (ignored)",
            ),
            (
                "/compact status",
                "show the compaction policy (tui.toml [auto] compact =\n\"warn\" (default) warns at 75% and 85%; \"auto\" or\n--auto-compact lets the runtime compact at 85%)",
            ),
        ],
        parse: |rest| {
            Ok(Command::Compact(match words(rest).as_slice() {
                [] | ["now"] => CompactCommand::Now,
                ["status"] => CompactCommand::Status,
                words => CompactCommand::Focused(words.join(" ")),
            }))
        },
    },
    CommandSpec {
        name: "status",
        aliases: &[],
        usage: "",
        summary: "version, model, session, workspace and trust state",
        help: &[(
            "/status",
            "version, model, session, workspace, realm, confinement, auto\nmode, trust of tui.toml and of each file /trust covers,\nhelper depth limit",
        )],
        parse: |rest| arg_free(rest, Command::Status, "/status"),
    },
    CommandSpec {
        name: "daemon",
        aliases: &[],
        usage: "stop",
        summary: "attach mode: shut the workspace's daemon down",
        help: &[(
            "/daemon stop",
            "attach mode (rkat-tui --attach): shut the workspace's\nrkat-tui-mob down (its sessions stay; /quit only detaches)",
        )],
        parse: |rest| match words(rest).as_slice() {
            ["stop"] => Ok(Command::DaemonStop),
            _ => Err(CommandError::Usage("/daemon stop")),
        },
    },
    CommandSpec {
        name: "memory",
        aliases: &[],
        usage: "[user | system]",
        summary: "edit AGENTS.md or SYSTEM.md in your editor",
        help: &[(
            "/memory [user | system]",
            "edit the workspace AGENTS.md, the user AGENTS.md or SYSTEM.md\nin $VISUAL / $EDITOR / vi; an edited AGENTS.md reaches this\nsession (also while a turn runs), SYSTEM.md the next one",
        )],
        parse: |rest| match words(rest).as_slice() {
            [] => Ok(Command::Memory(MemoryTarget::Workspace)),
            ["user"] => Ok(Command::Memory(MemoryTarget::User)),
            ["system"] => Ok(Command::Memory(MemoryTarget::System)),
            _ => Err(CommandError::Usage("/memory [user | system]")),
        },
    },
    CommandSpec {
        name: "init",
        aliases: &[],
        usage: "",
        summary: "ask the agent to write AGENTS.md for this project",
        help: &[(
            "/init",
            "ask the agent to write or update AGENTS.md for this project (idle only)",
        )],
        parse: |rest| arg_free(rest, Command::Init, "/init"),
    },
    CommandSpec {
        name: "export",
        aliases: &[],
        usage: "[file]",
        summary: "write the transcript as plain text",
        help: &[(
            "/export [file]",
            "write the transcript as plain text (default:\n.rkat/tui/export-<session>-<time>.txt in the workspace)",
        )],
        parse: |rest| match words(rest).as_slice() {
            [] => Ok(Command::Export(None)),
            words => Ok(Command::Export(Some(words.join(" ")))),
        },
    },
    CommandSpec {
        name: "context",
        aliases: &[],
        usage: "",
        summary: "context usage and what the context holds",
        help: &[(
            "/context",
            "context usage, the compaction policy and what the context holds",
        )],
        parse: |rest| match words(rest).as_slice() {
            [] | ["all"] => Ok(Command::Context),
            _ => Err(CommandError::Usage("/context")),
        },
    },
    CommandSpec {
        name: "copy",
        aliases: &[],
        usage: "[N]",
        summary: "copy the latest (or Nth latest) answer to the clipboard",
        help: &[(
            "/copy [N]",
            "copy the latest (or Nth latest) answer to the clipboard (OSC 52)",
        )],
        parse: parse_copy,
    },
    CommandSpec {
        name: "usage",
        aliases: &["cost", "stats"],
        usage: "",
        summary: "token usage and cost per model",
        help: &[(
            "/usage, /cost, /stats",
            "token usage and cost per model (footer: $0.42, ~ estimate, $? unpriced)",
        )],
        parse: |rest| arg_free(rest, Command::Cost, "/usage"),
    },
    CommandSpec {
        name: "helpers",
        aliases: &["tasks"],
        usage: "[expand|fold <label>|all]",
        summary: "list the helpers (sub-agents), or show or fold their work",
        help: &[
            (
                "/helpers, /tasks",
                "list the helpers (sub-agents) and their state",
            ),
            (
                "/helpers expand|fold <label>|all",
                "show a helper's work in the transcript, or fold it to one line",
            ),
        ],
        parse: parse_helpers,
    },
];

/// The command `/word` names, if any.
#[must_use]
pub fn spec(word: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.is_named(word))
}

/// The commands with a spelling that starts with `prefix` (case-insensitive),
/// in `/help` order, each with that spelling.
pub fn completions(
    prefix: &str,
) -> impl Iterator<Item = (&'static CommandSpec, &'static str)> + '_ {
    COMMANDS
        .iter()
        .filter_map(move |spec| spec.completion_for(prefix).map(|name| (spec, name)))
}

/// `command` when `rest` is empty, else a usage error.
fn arg_free(rest: &str, command: Command, usage: &'static str) -> Result<Command, CommandError> {
    if rest.is_empty() {
        Ok(command)
    } else {
        Err(CommandError::Usage(usage))
    }
}

fn words(rest: &str) -> Vec<&str> {
    rest.split_whitespace().collect()
}

fn parse_resume(rest: &str) -> Result<Command, CommandError> {
    match words(rest).as_slice() {
        [] => Ok(Command::Resume(None)),
        [r] => Ok(Command::Resume(Some((*r).to_string()))),
        _ => Err(CommandError::Usage("/resume [last | ~N | id-prefix]")),
    }
}

fn parse_auto(rest: &str) -> Result<Command, CommandError> {
    match words(rest).as_slice() {
        [] => Ok(Command::Auto(AutoCommand::Status)),
        ["on"] => Ok(Command::Auto(AutoCommand::On)),
        ["off"] => Ok(Command::Auto(AutoCommand::Off)),
        ["resume"] => Ok(Command::Auto(AutoCommand::Resume)),
        _ => Err(CommandError::Usage(AUTO_USAGE)),
    }
}

fn parse_mode(rest: &str) -> Result<Command, CommandError> {
    match words(rest).as_slice() {
        [] => Ok(Command::Mode(None)),
        [name] => PermissionMode::parse(name)
            .map(|mode| Command::Mode(Some(mode)))
            .ok_or(CommandError::Usage(MODE_USAGE)),
        _ => Err(CommandError::Usage(MODE_USAGE)),
    }
}

fn parse_model(rest: &str) -> Result<Command, CommandError> {
    match words(rest).as_slice() {
        [] => Ok(Command::Model(ModelCommand::Pick)),
        ["refresh"] => Ok(Command::Model(ModelCommand::Refresh)),
        [m] => Ok(Command::Model(ModelCommand::Set((*m).to_string()))),
        _ => Err(CommandError::Usage("/model [[endpoint:]id | refresh]")),
    }
}

fn parse_copy(rest: &str) -> Result<Command, CommandError> {
    const USAGE: &str = "/copy [N] (N ≥ 1, 1 = the latest answer)";
    match words(rest).as_slice() {
        [] => Ok(Command::Copy(1)),
        [n] => match n.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(Command::Copy(n)),
            _ => Err(CommandError::Usage(USAGE)),
        },
        _ => Err(CommandError::Usage(USAGE)),
    }
}

fn parse_helpers(rest: &str) -> Result<Command, CommandError> {
    let target = |t: &str| match t {
        "all" => HelperTarget::All,
        label => HelperTarget::Label(label.to_string()),
    };
    match words(rest).as_slice() {
        [] | ["list"] => Ok(Command::Helpers(HelpersCommand::List)),
        ["expand", t] => Ok(Command::Helpers(HelpersCommand::Expand(target(t)))),
        ["fold", t] => Ok(Command::Helpers(HelpersCommand::Fold(target(t)))),
        _ => Err(CommandError::Usage(HELPERS_USAGE)),
    }
}

/// Width of `/help`'s left column, its two-space indent included.
const HELP_COLUMN: usize = 33;

/// The keys and the approval prompt, as `/help` and the `?` panel list them.
const KEYS_HELP: &str = "\
Keys:
  ? (empty input)                show these keys; any key closes them (with text, ? is typed)
  Enter                          send; while the agent is working, queues the message
                                 and delivers it at the next step
  Ctrl+Enter                     send now: interrupt the turn and send the input next
                                 (sends normally when idle; same as Ctrl-X Ctrl-S)
  Shift+Enter, Alt+Enter, Ctrl+J insert a newline (so does \\ right before Enter at a line end)
  Ctrl-C                         interrupt the turn (as Esc) · clear the input · press twice to quit
  Esc                            interrupt the turn; queued messages are sent next
  Esc Esc (idle, with text)      clear the input and save it to the prompt history (↑ recalls it)
  Esc Esc (idle, empty input)    rewind: pick one of your messages to continue from (as /rewind)
  Up (empty input)               take queued messages back into the editor
  Ctrl-D                         quit on an empty input: press again within 3 s (while a turn
                                 runs or messages are queued the turn is then interrupted and
                                 undelivered messages are saved); with text, delete forward
  Ctrl-O                         show or hide detail: the last reasoning block and the
                                 output of the last tool call (a click toggles one tool)
  Ctrl-T                         show or hide the task checklist under the input (the agent's
                                 task_create/task_update list: ☐ pending, ◐ in progress, ☑ done;
                                 nothing shows while there are no tasks; saved as [ui] checklist)
  Ctrl+Up/Down                   select a helper in the panel under the input (newest first);
                                 Enter on an empty input opens its full view (Esc returns,
                                 PgUp/PgDn scroll it); a click unfolds a header in the transcript
  Ctrl-R                         search history (Ctrl-R again: older · Enter accept · Esc cancel)
  Ctrl-X Ctrl-S                  interrupt and send the input (sends normally when idle)
  Ctrl-G, Ctrl-X Ctrl-E          edit the input in $VISUAL / $EDITOR / vi (also while a turn runs)
  Up/Down, Ctrl-P/N              move between lines · recall prompt history
  Ctrl-A/E, Alt+B/F              line start/end, word left/right (Ctrl/Alt+Left/Right too)
  Ctrl-K/U, Ctrl-W, Alt+D        kill to line end/start, the word before/after (Ctrl-K at a
                                 line end kills the newline); Ctrl/Alt+Backspace = Ctrl-W
  Ctrl-Y, Alt+Y                  yank the last killed text; Alt+Y right after cycles older kills
  Ctrl-S                         stash the input (put it aside, clear); on an empty input,
                                 restore the stash (Ctrl-X Ctrl-S still sends now)
  Ctrl-_                         undo the last input edit (a word, a kill, a paste, a clear;
                                 Ctrl-7 or Ctrl-Shift-- in some terminals)
  PgUp/PgDn, Shift+Up/Down       scroll the transcript
  Ctrl-L                         clear the screen and redraw it
  Ctrl-Z                         suspend to the shell (fg resumes; Unix)
  Shift+Tab                      cycle the permission mode: manual → accept edits → plan → auto
                                 (the status bar shows it; /mode sets one directly)
  Alt+P                          pick a model (as /model) without clearing the input
  / (one-line input)             a menu of the matching commands opens above the input:
                                 ↑/↓ choose, Tab completes, Enter completes a partial name
                                 (a full one sends), Esc closes it
  ! (first character)            shell mode: the rest runs as a command in the agent's sandbox
                                 (no approval: you typed it; [shell] user_timeout_secs, 120 s);
                                 its output shows here and joins the context without a turn.
                                 While the agent works it waits for the turn to end and runs
                                 before queued messages (↑ takes it back); Esc or Ctrl-C
                                 cancels a running one. \\! sends a literal !
  @ (ending a word)              a menu of matching workspace paths (.gitignore honoured, .git
                                 and .rkat left out): Tab or Enter inserts one (a directory
                                 lists its contents next); on send each @file is attached
                                 (64 KiB each, 256 KiB in all) and @dir/ as a listing
  Home/End                       scroll to top/bottom when the input is empty (Ctrl+Home/End always)
Approval prompt:
  1, y                           yes, allow once
  2, a                           yes, allow and add the rule the row shows
  3, n, Esc                      no: type what the agent should do instead and Enter sends it
                                 with the denial (empty Enter or Esc denies without a comment)
  Up/Down, j/k, Enter            move between the rows and pick one
  s                              stop the turn (Ctrl-C stops every open prompt)";

/// The line that ends `/help`.
const HELP_FOOTER: &str = "Start a prompt with // to send a literal leading slash.";

/// Help text shown by `/help`: every slash command (from [`COMMANDS`]) and
/// key binding.
#[must_use]
pub fn help_text() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let mut out = String::from("Commands:\n");
        let pad = " ".repeat(HELP_COLUMN);
        for (left, text) in COMMANDS.iter().flat_map(|spec| spec.help) {
            let mut lines = text.split('\n');
            let first = lines.next().unwrap_or_default();
            if left.chars().count() + 2 < HELP_COLUMN {
                let width = HELP_COLUMN - 2;
                out.push_str(&format!("  {left:<width$}{first}\n"));
            } else {
                out.push_str(&format!("  {left}\n{pad}{first}\n"));
            }
            for line in lines {
                out.push_str(&format!("{pad}{line}\n"));
            }
        }
        out.push_str(KEYS_HELP);
        out.push('\n');
        out.push_str(HELP_FOOTER);
        out
    })
}

/// The "Keys" and "Approval prompt" parts of [`help_text`]: what the `?`
/// shortcut panel lists.
#[must_use]
pub fn keys_help() -> &'static str {
    KEYS_HELP
}

/// A single-line `#text` input (not `##…`) as a remember request: returns the
/// text after `#`, trimmed. Multi-line input and `\#` escapes are prompts.
#[must_use]
pub fn parse_remember(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.contains('\n') || trimmed.starts_with("##") {
        return None;
    }
    let text = trimmed.strip_prefix('#')?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Undo a leading `\#` escape (`\#foo` sends the prompt `#foo`).
#[must_use]
pub fn unescape_hash(input: &str) -> Option<String> {
    let start = input.len() - input.trim_start().len();
    let rest = input.get(start..)?.strip_prefix("\\#")?;
    Some(format!("#{rest}"))
}

/// A `!` shell-mode input: the command after the leading `!` (one optional
/// space after it dropped; later lines belong to the command), or `None`
/// when the input is not one (`!!…` is a prompt, and so is a `\!` escape).
/// The command may be empty (a bare `!`).
#[must_use]
pub fn parse_shell(input: &str) -> Option<&str> {
    let trimmed = input.trim_start();
    if trimmed.starts_with("!!") {
        return None;
    }
    let rest = trimmed.strip_prefix('!')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest).trim_end())
}

/// Undo a leading `\!` escape (`\!foo` sends the prompt `!foo`).
#[must_use]
pub fn unescape_bang(input: &str) -> Option<String> {
    let start = input.len() - input.trim_start().len();
    let rest = input.get(start..)?.strip_prefix("\\!")?;
    Some(format!("!{rest}"))
}

/// Parse a submitted line. Returns `None` when the line is not a slash command
/// (no leading `/`, or a `//` escape).
pub fn parse(input: &str) -> Option<Result<Command, CommandError>> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('/')?;
    if body.starts_with('/') {
        return None;
    }
    let body = body.trim_start();
    let name = body.split_whitespace().next().unwrap_or_default();
    // The rest verbatim (inner whitespace included, for `/remember`).
    let rest = body.get(name.len()..).unwrap_or_default().trim();
    Some(match spec(name) {
        Some(spec) => (spec.parse)(rest),
        None => Err(CommandError::Unknown(name.to_string())),
    })
}

fn parse_rules(args: &[&str]) -> Result<RulesCommand, CommandError> {
    match args {
        [] | ["list"] => Ok(RulesCommand::List),
        ["add", effect, tool, pattern @ ..] => {
            let effect = match *effect {
                "allow" => RuleEffect::Allow,
                "deny" => RuleEffect::Deny,
                _ => return Err(CommandError::Usage(RULES_USAGE)),
            };
            let pattern = (!pattern.is_empty()).then(|| pattern.join(" "));
            Ok(RulesCommand::Add {
                effect,
                tool: (*tool).to_string(),
                pattern,
            })
        }
        ["trust"] => Ok(RulesCommand::Trust),
        ["rm" | "remove", n] => n
            .parse::<usize>()
            .map(RulesCommand::Remove)
            .map_err(|_| CommandError::Usage(RULES_USAGE)),
        _ => Err(CommandError::Usage(RULES_USAGE)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Command {
        parse(s).unwrap().unwrap()
    }

    #[test]
    fn every_spec_parses_and_every_name_has_a_spec() {
        // Sample arguments for the commands that need some.
        let args = |name: &str| match name {
            "remember" => " a note",
            "mcp" => " trust",
            "daemon" => " stop",
            _ => "",
        };
        let mut seen = std::collections::HashSet::new();
        for spec in COMMANDS {
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                assert!(seen.insert(name), "/{name} is named twice");
                let line = format!("/{name}{}", args(spec.name));
                assert!(
                    matches!(parse(&line), Some(Ok(_))),
                    "{line}: {:?}",
                    parse(&line)
                );
                assert!(std::ptr::eq(super::spec(name).unwrap(), spec));
            }
            // Every spec is in /help: its own rows, or (/mcp) another's.
            assert!(
                help_text().contains(&format!("/{}", spec.name)),
                "{}",
                spec.name
            );
            assert!(!spec.summary.is_empty());
        }
        // The names the parser has always understood all have a spec.
        for name in [
            "quit",
            "exit",
            "q",
            "clear",
            "new",
            "help",
            "?",
            "memory",
            "init",
            "export",
            "status",
            "context",
            "usage",
            "cost",
            "stats",
            "copy",
            "compact",
            "resume",
            "model",
            "auto",
            "permissions",
            "rules",
            "trust",
            "mcp",
            "helpers",
            "tasks",
            "remember",
            "mode",
            "rewind",
            "checkpoint",
            "undo",
        ] {
            assert!(super::spec(name).is_some(), "/{name} has no spec");
        }
        assert_eq!(seen.len(), 32);
        // `/help`'s "Commands:" rows come from the specs, in their order.
        let commands = help_text().split_once("Keys:").unwrap().0;
        let mut at = 0;
        for (left, _) in COMMANDS.iter().flat_map(|spec| spec.help) {
            let found = commands[at..]
                .find(left)
                .unwrap_or_else(|| panic!("{left}"));
            at += found + left.len();
        }
    }

    #[test]
    fn completions_match_names_and_aliases_by_prefix() {
        let names = |prefix: &str| completions(prefix).map(|(_, n)| n).collect::<Vec<_>>();
        assert_eq!(names("he"), ["help", "helpers"]);
        assert_eq!(names("HE"), ["help", "helpers"]);
        // An alias offers itself when the name does not match.
        assert_eq!(names("ex"), ["exit", "export"]);
        assert_eq!(names("ta"), ["tasks"]);
        assert_eq!(names("zz"), Vec::<&str>::new());
        assert_eq!(completions("").count(), COMMANDS.len());
    }

    #[test]
    fn not_commands() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("//literal"), None);
    }

    #[test]
    fn simple_commands() {
        assert_eq!(ok("/quit"), Command::Quit);
        assert_eq!(ok(" /exit "), Command::Quit);
        assert_eq!(ok("/clear"), Command::Clear);
        assert_eq!(ok("/help"), Command::Help);
        assert_eq!(ok("/copy"), Command::Copy(1));
        assert_eq!(ok("/copy 3"), Command::Copy(3));
        for bad in ["/copy 0", "/copy x", "/copy 1 2"] {
            assert!(
                matches!(parse(bad), Some(Err(CommandError::Usage(_)))),
                "{bad}"
            );
        }
        assert_eq!(ok("/resume"), Command::Resume(None));
        assert_eq!(ok("/resume ~2"), Command::Resume(Some("~2".into())));
        assert_eq!(ok("/rewind"), Command::Rewind);
        assert_eq!(ok("/checkpoint"), Command::Rewind);
        assert_eq!(ok("/undo"), Command::Rewind);
        assert_eq!(
            parse("/rewind 2"),
            Some(Err(CommandError::Usage("/rewind")))
        );
        assert_eq!(
            ok("/model gpt-5.4"),
            Command::Model(ModelCommand::Set("gpt-5.4".into()))
        );
        assert_eq!(ok("/model"), Command::Model(ModelCommand::Pick));
        assert_eq!(ok("/model refresh"), Command::Model(ModelCommand::Refresh));
        assert!(matches!(
            parse("/model a b"),
            Some(Err(CommandError::Usage(_)))
        ));
    }

    #[test]
    fn remember_and_compact_commands() {
        assert_eq!(
            ok("/remember use  cargo nextest"),
            Command::Remember("use  cargo nextest".into())
        );
        assert_eq!(
            parse("/remember"),
            Some(Err(CommandError::Usage("/remember <text>")))
        );
        assert_eq!(ok("/status"), Command::Status);
        assert_eq!(ok("/export"), Command::Export(None));
        assert_eq!(ok("/init"), Command::Init);
        assert_eq!(ok("/memory"), Command::Memory(MemoryTarget::Workspace));
        assert_eq!(ok("/memory user"), Command::Memory(MemoryTarget::User));
        assert_eq!(ok("/memory system"), Command::Memory(MemoryTarget::System));
        assert_eq!(
            parse("/memory workspace"),
            Some(Err(CommandError::Usage("/memory [user | system]")))
        );
        assert_eq!(parse("/init x"), Some(Err(CommandError::Usage("/init"))));
        assert_eq!(
            ok("/export notes/chat.txt"),
            Command::Export(Some("notes/chat.txt".into()))
        );
        assert_eq!(
            parse("/status x"),
            Some(Err(CommandError::Usage("/status")))
        );
        assert_eq!(ok("/context"), Command::Context);
        assert_eq!(ok("/context all"), Command::Context);
        assert_eq!(
            parse("/context x"),
            Some(Err(CommandError::Usage("/context")))
        );
        assert_eq!(ok("/compact"), Command::Compact(CompactCommand::Now));
        assert_eq!(ok("/compact now"), Command::Compact(CompactCommand::Now));
        assert_eq!(
            ok("/compact status"),
            Command::Compact(CompactCommand::Status)
        );
        assert_eq!(
            ok("/compact keep the  test plan"),
            Command::Compact(CompactCommand::Focused("keep the test plan".into()))
        );
        for name in ["/usage", "/cost", "/stats"] {
            assert_eq!(ok(name), Command::Cost, "{name}");
            assert_eq!(
                parse(&format!("{name} now")),
                Some(Err(CommandError::Usage("/usage"))),
                "{name}"
            );
        }
        assert_eq!(
            ok("/compact later"),
            Command::Compact(CompactCommand::Focused("later".into()))
        );
    }

    #[test]
    fn hash_remember_and_escape() {
        assert_eq!(parse_remember("#use tabs "), Some("use tabs".into()));
        assert_eq!(parse_remember("  #  use tabs"), Some("use tabs".into()));
        assert_eq!(parse_remember("#"), None);
        assert_eq!(parse_remember("#   "), None);
        assert_eq!(parse_remember("## heading"), None);
        assert_eq!(parse_remember("#one\ntwo"), None);
        assert_eq!(parse_remember("\\#tag"), None);
        assert_eq!(parse_remember("plain"), None);
        assert_eq!(unescape_hash("\\#tag it"), Some("#tag it".into()));
        assert_eq!(unescape_hash("#tag"), None);
    }

    #[test]
    fn bang_is_shell_mode_and_escapes() {
        assert_eq!(parse_shell("!ls -la"), Some("ls -la"));
        assert_eq!(parse_shell("! ls -la "), Some("ls -la"));
        assert_eq!(parse_shell("!  ls"), Some(" ls"));
        assert_eq!(parse_shell("  !pwd"), Some("pwd"));
        assert_eq!(parse_shell("!"), Some(""));
        assert_eq!(parse_shell("! "), Some(""));
        assert_eq!(
            parse_shell("!for f in *; do\n  echo $f\ndone"),
            Some("for f in *; do\n  echo $f\ndone")
        );
        assert_eq!(parse_shell("!!"), None);
        assert_eq!(parse_shell("!! wow"), None);
        assert_eq!(parse_shell("\\!ls"), None);
        assert_eq!(parse_shell("say !ls"), None);
        assert_eq!(parse_shell("/help"), None);
        assert_eq!(unescape_bang("\\!important"), Some("!important".into()));
        assert_eq!(unescape_bang("  \\! x"), Some("! x".into()));
        assert_eq!(unescape_bang("!x"), None);
    }

    #[test]
    fn mode_commands() {
        use crate::approval::mode::PermissionMode;
        assert_eq!(ok("/mode"), Command::Mode(None));
        assert_eq!(
            ok("/mode default"),
            Command::Mode(Some(PermissionMode::Manual))
        );
        assert_eq!(
            ok("/mode manual"),
            Command::Mode(Some(PermissionMode::Manual))
        );
        assert_eq!(
            ok("/mode accept-edits"),
            Command::Mode(Some(PermissionMode::AcceptEdits))
        );
        assert_eq!(
            ok("/mode acceptEdits"),
            Command::Mode(Some(PermissionMode::AcceptEdits))
        );
        assert_eq!(ok("/mode plan"), Command::Mode(Some(PermissionMode::Plan)));
        assert_eq!(ok("/mode auto"), Command::Mode(Some(PermissionMode::Auto)));
        for bad in ["/mode yolo", "/mode plan auto"] {
            assert_eq!(
                parse(bad),
                Some(Err(CommandError::Usage(
                    "/mode [default | accept-edits | plan | auto]"
                ))),
                "{bad}"
            );
        }
    }

    #[test]
    fn auto_commands() {
        assert_eq!(ok("/auto"), Command::Auto(AutoCommand::Status));
        assert_eq!(ok("/auto on"), Command::Auto(AutoCommand::On));
        assert_eq!(ok("/auto off"), Command::Auto(AutoCommand::Off));
        assert_eq!(ok("/auto resume"), Command::Auto(AutoCommand::Resume));
        assert!(matches!(
            parse("/auto maybe"),
            Some(Err(CommandError::Usage(_)))
        ));
    }

    #[test]
    fn rules_commands() {
        assert_eq!(ok("/rules"), Command::Rules(RulesCommand::List));
        assert_eq!(ok("/rules list"), Command::Rules(RulesCommand::List));
        assert_eq!(
            ok("/rules add allow shell cargo test *"),
            Command::Rules(RulesCommand::Add {
                effect: RuleEffect::Allow,
                tool: "shell".into(),
                pattern: Some("cargo test *".into()),
            })
        );
        assert_eq!(
            ok("/rules add deny web_fetch"),
            Command::Rules(RulesCommand::Add {
                effect: RuleEffect::Deny,
                tool: "web_fetch".into(),
                pattern: None,
            })
        );
        assert_eq!(ok("/rules rm 3"), Command::Rules(RulesCommand::Remove(3)));
        assert_eq!(ok("/rules trust"), Command::Rules(RulesCommand::Trust));
        assert_eq!(ok("/mcp trust"), Command::TrustProject);
        assert_eq!(ok("/trust"), Command::TrustProject);
        assert!(parse("/trust all").is_some_and(|r| r.is_err()));
        assert!(parse("/mcp").is_some_and(|r| r.is_err()));
        assert!(matches!(
            parse("/rules rm x"),
            Some(Err(CommandError::Usage(_)))
        ));
        assert!(matches!(
            parse("/rules add maybe t"),
            Some(Err(CommandError::Usage(_)))
        ));
    }

    #[test]
    fn help_mentions_every_command_and_key() {
        for needle in [
            "/help",
            "/quit",
            "/exit",
            "/clear",
            "/resume",
            "/rules",
            "/permissions",
            "/usage",
            "/context",
            "/status",
            "/export [file]",
            "/init",
            "/memory [user | system]",
            "/stats",
            "/tasks",
            "/auto",
            "/model",
            "/remember",
            "#<text>",
            "/compact",
            "/cost",
            "/copy [N]",
            "Ctrl-K",
            "Alt+D",
            "Ctrl-Y",
            "Alt+Y",
            "Ctrl-_",
            "Ctrl-S",
            "Ctrl-L",
            "Ctrl-Z",
            "Shift+Tab",
            "Alt+P",
            "Enter",
            "Alt+Enter",
            "Ctrl+J",
            "Ctrl-C",
            "Esc",
            "Ctrl-D",
            "Ctrl-O",
            "Ctrl-T",
            "Ctrl-R",
            "Ctrl-X Ctrl-S",
            "Ctrl-G",
            "Ctrl-X Ctrl-E",
            "PgUp",
            "PgDn",
            "Home",
            "End",
            "1, y",
            "2, a",
            "3, n, Esc",
            "j/k, Enter",
            "stop the turn",
            "queued messages are sent next",
            "take queued messages back",
            "delivers it at the next step",
            "press again",
            "switch to a model (idle only)",
            "endpoint:",
            "/model refresh",
            "/helpers",
            "Ctrl+Up/Down",
            "? (empty input)",
            "Tab completes",
            "@ (ending a word)",
        ] {
            assert!(help_text().contains(needle), "help is missing {needle}");
        }
    }

    #[test]
    fn the_keys_panel_lists_the_keys_and_the_approval_prompt_only() {
        let keys = keys_help();
        assert!(keys.starts_with("Keys:\n  ? (empty input)"), "{keys}");
        assert!(keys.contains("Approval prompt:"));
        assert!(
            keys.trim_end().ends_with("stops every open prompt)"),
            "{keys}"
        );
        assert!(!keys.contains("/permissions"));
        assert!(!keys.contains("Start a prompt"));
    }

    #[test]
    fn permissions_and_rules_parse_identically() {
        for line in [
            "",
            " list",
            " add allow shell cargo test *",
            " add deny web_fetch",
            " rm 3",
            " trust",
            " rm x",
            " add maybe t",
        ] {
            assert_eq!(
                parse(&format!("/permissions{line}")),
                parse(&format!("/rules{line}")),
                "{line}"
            );
        }
        assert_eq!(
            parse("/permissions rm x"),
            Some(Err(CommandError::Usage(RULES_USAGE)))
        );
        assert!(RULES_USAGE.starts_with("/permissions"));
    }

    #[test]
    fn tasks_is_helpers() {
        for line in ["", " list", " expand scout", " fold all", " expand"] {
            assert_eq!(
                parse(&format!("/tasks{line}")),
                parse(&format!("/helpers{line}")),
                "{line}"
            );
        }
    }

    #[test]
    fn helpers_commands() {
        assert_eq!(ok("/helpers"), Command::Helpers(HelpersCommand::List));
        assert_eq!(
            ok("/helpers expand scout"),
            Command::Helpers(HelpersCommand::Expand(HelperTarget::Label("scout".into())))
        );
        assert_eq!(
            ok("/helpers fold all"),
            Command::Helpers(HelpersCommand::Fold(HelperTarget::All))
        );
        assert_eq!(
            parse("/helpers expand"),
            Some(Err(CommandError::Usage(HELPERS_USAGE)))
        );
    }

    #[test]
    fn unknown_command() {
        assert_eq!(
            parse("/frob"),
            Some(Err(CommandError::Unknown("frob".into())))
        );
        assert_eq!(
            CommandError::Unknown("frob".into()).to_string(),
            "unknown command /frob (try /help)"
        );
    }
}
