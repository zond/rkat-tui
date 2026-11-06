//! Slash-command parsing.

use std::fmt;

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
    /// Enable auto mode.
    On,
    /// Disable auto mode (every non-trivial call escalates).
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
    /// `/rules ...`.
    Rules(RulesCommand),
    /// `/auto [on|off|resume]`.
    Auto(AutoCommand),
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
    /// `/context`: the context gauge, the compaction policy and what the
    /// context holds, as far as the TUI knows.
    Context,
    /// `/copy [N]`: copy the Nth latest assistant answer (1, the default,
    /// is the latest) to the clipboard (OSC 52).
    Copy(usize),
    /// `/mcp trust`: trust the project `.rkat/mcp.toml` as it is now; its
    /// servers start from the next session.
    McpTrust,
    /// `/helpers [expand|fold <label>|all]`.
    Helpers(HelpersCommand),
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
const HELPERS_USAGE: &str = "/helpers [expand <label>|all | fold <label>|all]";

/// Help text shown by `/help`: every slash command and key binding.
pub const HELP_TEXT: &str = "\
Commands:
  /help, /?                      show this help
  /quit, /exit, /q               leave rkat-tui (asks again while a turn runs or messages are queued)
  /clear, /new                   clear the transcript and start a new session (idle only)
  /resume [ref]                  resume a session (last, ~N, or id prefix); no ref opens a picker
                                 (so does rkat-tui --resume without a ref)
  /permissions [list]            list approval rules (/rules is an alias)
  /permissions add allow|deny <tool> [pattern]
                                 add an approval rule
  /permissions rm <n>            remove rule n
  /permissions trust             show the realm tui.toml and trust it as it is now
  /mcp trust                     trust the project .rkat/mcp.toml as it is now (its servers
                                 start from the next session)
  /auto                          show auto-mode state and denial counters
  /auto on|off|resume            enable, disable, or resume auto mode after a pause (Shift+Tab
                                 switches it off and on)
  /model                         pick a model from every endpoint's list (type to filter)
  /model [endpoint:]<id>         switch to a model (idle only); a bare id is looked up
  /model refresh                 refetch every endpoint's model list and pick
  /remember <text>, #<text>      add a line to the workspace AGENTS.md and to this session
                                 (\\# sends a literal #)
  /compact [now]                 compact: rebuild the session so its next model call
                                 compacts (idle only)
  /compact <instructions>        the same; focus instructions are not supported yet (ignored)
  /compact status                show the compaction policy (tui.toml [auto] compact =
                                 \"warn\" (default) warns at 75% and 85%; \"auto\" or
                                 --auto-compact lets the runtime compact at 85%)
  /status                        version, model, session, workspace, realm, confinement, auto
                                 mode, trust of tui.toml and mcp.toml, helper depth limit
  /context                       context usage, the compaction policy and what the context holds
  /copy [N]                      copy the latest (or Nth latest) answer to the clipboard (OSC 52)
  /usage, /cost, /stats          token usage and cost per model (footer: $0.42, ~ estimate, $? unpriced)
  /helpers, /tasks               list the helpers (sub-agents) and their state
  /helpers expand|fold <label>|all
                                 show a helper's work in the transcript, or fold it to one line
Keys:
  Enter                          send; while the agent is working, queues the message
                                 and delivers it at the next step
  Ctrl+Enter                     send now: interrupt the turn and send the input next
                                 (sends normally when idle; same as Ctrl-X Ctrl-S)
  Shift+Enter, Alt+Enter, Ctrl+J insert a newline (so does \\ right before Enter at a line end)
  Ctrl-C                         interrupt the turn (as Esc) · clear the input · press twice to quit
  Esc                            interrupt the turn; queued messages are sent next
  Esc Esc (idle, with text)      clear the input and save it to the prompt history (↑ recalls it)
  Up (empty input)               take queued messages back into the editor
  Ctrl-D                         quit on an empty input: press again within 3 s (while a turn
                                 runs or messages are queued the turn is then interrupted and
                                 undelivered messages are saved); with text, delete forward
  Ctrl-O                         show or hide detail: the last reasoning block and the
                                 output of the last tool call (a click toggles one tool)
  Ctrl+Up/Down                   select a helper in the panel under the input (newest first);
                                 Enter on an empty input opens its full view (Esc returns,
                                 PgUp/PgDn scroll it); a click unfolds a header in the transcript
  Ctrl-R                         search history (Ctrl-R again: older · Enter accept · Esc cancel)
  Ctrl-X Ctrl-S                  interrupt and send the input (sends normally when idle)
  Up/Down, Ctrl-P/N              move between lines · recall prompt history
  Ctrl-A/E, Alt+B/F              line start/end, word left/right
  Ctrl-K/U, Ctrl-W, Alt+D        kill to line end/start, the word before/after (Ctrl-K at a
                                 line end kills the newline); Ctrl/Alt+Backspace = Ctrl-W
  Ctrl-Y, Alt+Y                  yank the last killed text; Alt+Y right after cycles older kills
  Ctrl-S                         stash the input (put it aside, clear); on an empty input,
                                 restore the stash (Ctrl-X Ctrl-S still sends now)
  Ctrl-_                         undo the last input edit (a word, a kill, a paste, a clear)
  PgUp/PgDn, Shift+Up/Down       scroll the transcript
  Ctrl-L                         clear the screen and redraw it
  Shift+Tab                      switch auto mode off/on (same as /auto off|on)
  Home/End                       scroll to top/bottom when the input is empty (Ctrl+Home/End always)
Approval prompt:
  y allow once · a allow and add a rule · n/Esc deny · s stop the turn (Ctrl-C stops all)
Start a prompt with // to send a literal leading slash.";

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

/// Parse a submitted line. Returns `None` when the line is not a slash command
/// (no leading `/`, or a `//` escape).
pub fn parse(input: &str) -> Option<Result<Command, CommandError>> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('/')?;
    if body.starts_with('/') {
        return None;
    }
    let mut words = body.split_whitespace();
    let name = words.next().unwrap_or_default();
    if name == "remember" {
        // Keep the text verbatim (inner whitespace included).
        let text = body
            .trim_start()
            .get(name.len()..)
            .unwrap_or_default()
            .trim();
        return Some(if text.is_empty() {
            Err(CommandError::Usage("/remember <text>"))
        } else {
            Ok(Command::Remember(text.to_string()))
        });
    }
    let args: Vec<&str> = words.collect();
    Some(parse_command(name, &args))
}

fn parse_command(name: &str, args: &[&str]) -> Result<Command, CommandError> {
    match name {
        "quit" | "exit" | "q" => Ok(Command::Quit),
        "clear" | "new" => Ok(Command::Clear),
        "help" | "?" => Ok(Command::Help),
        "status" => match args {
            [] => Ok(Command::Status),
            _ => Err(CommandError::Usage("/status")),
        },
        "context" => match args {
            [] | ["all"] => Ok(Command::Context),
            _ => Err(CommandError::Usage("/context")),
        },
        "usage" | "cost" | "stats" => match args {
            [] => Ok(Command::Cost),
            _ => Err(CommandError::Usage("/usage")),
        },
        "copy" => match args {
            [] => Ok(Command::Copy(1)),
            [n] => match n.parse::<usize>() {
                Ok(n) if n >= 1 => Ok(Command::Copy(n)),
                _ => Err(CommandError::Usage("/copy [N] (N ≥ 1, 1 = the latest answer)")),
            },
            _ => Err(CommandError::Usage("/copy [N] (N ≥ 1, 1 = the latest answer)")),
        },
        "compact" => Ok(Command::Compact(match args {
            [] | ["now"] => CompactCommand::Now,
            ["status"] => CompactCommand::Status,
            words => CompactCommand::Focused(words.join(" ")),
        })),
        "resume" => match args {
            [] => Ok(Command::Resume(None)),
            [r] => Ok(Command::Resume(Some((*r).to_string()))),
            _ => Err(CommandError::Usage("/resume [last | ~N | id-prefix]")),
        },
        "model" => match args {
            [] => Ok(Command::Model(ModelCommand::Pick)),
            ["refresh"] => Ok(Command::Model(ModelCommand::Refresh)),
            [m] => Ok(Command::Model(ModelCommand::Set((*m).to_string()))),
            _ => Err(CommandError::Usage("/model [[endpoint:]id | refresh]")),
        },
        "auto" => match args {
            [] => Ok(Command::Auto(AutoCommand::Status)),
            ["on"] => Ok(Command::Auto(AutoCommand::On)),
            ["off"] => Ok(Command::Auto(AutoCommand::Off)),
            ["resume"] => Ok(Command::Auto(AutoCommand::Resume)),
            _ => Err(CommandError::Usage(AUTO_USAGE)),
        },
        "permissions" | "rules" => parse_rules(args).map(Command::Rules),
        "mcp" => match args {
            ["trust"] => Ok(Command::McpTrust),
            _ => Err(CommandError::Usage("/mcp trust")),
        },
        "helpers" | "tasks" => {
            let target = |t: &str| match t {
                "all" => HelperTarget::All,
                label => HelperTarget::Label(label.to_string()),
            };
            match args {
                [] | ["list"] => Ok(Command::Helpers(HelpersCommand::List)),
                ["expand", t] => Ok(Command::Helpers(HelpersCommand::Expand(target(t)))),
                ["fold", t] => Ok(Command::Helpers(HelpersCommand::Fold(target(t)))),
                _ => Err(CommandError::Usage(HELPERS_USAGE)),
            }
        }
        other => Err(CommandError::Unknown(other.to_string())),
    }
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
        assert_eq!(parse("/status x"), Some(Err(CommandError::Usage("/status"))));
        assert_eq!(ok("/context"), Command::Context);
        assert_eq!(ok("/context all"), Command::Context);
        assert_eq!(parse("/context x"), Some(Err(CommandError::Usage("/context"))));
        assert_eq!(ok("/compact"), Command::Compact(CompactCommand::Now));
        assert_eq!(ok("/compact now"), Command::Compact(CompactCommand::Now));
        assert_eq!(ok("/compact status"), Command::Compact(CompactCommand::Status));
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
        assert_eq!(ok("/mcp trust"), Command::McpTrust);
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
            "Shift+Tab",
            "Enter",
            "Alt+Enter",
            "Ctrl+J",
            "Ctrl-C",
            "Esc",
            "Ctrl-D",
            "Ctrl-O",
            "Ctrl-R",
            "Ctrl-X Ctrl-S",
            "PgUp",
            "PgDn",
            "Home",
            "End",
            "y allow",
            "a allow",
            "n/Esc deny",
            "s stop",
            "queued messages are sent next",
            "take queued messages back",
            "delivers it at the next step",
            "press again",
            "switch to a model (idle only)",
            "endpoint:",
            "/model refresh",
            "/helpers",
            "Ctrl+Up/Down",
        ] {
            assert!(HELP_TEXT.contains(needle), "help is missing {needle}");
        }
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
            Command::Helpers(HelpersCommand::Expand(HelperTarget::Label(
                "scout".into()
            )))
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
