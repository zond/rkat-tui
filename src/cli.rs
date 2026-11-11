//! Command-line arguments for `rkat-tui`.

use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// Interactive coding harness for Meerkat.
#[derive(Parser, Debug, Clone)]
#[command(name = "rkat-tui", version = crate::BUILD)]
#[command(about = "Interactive terminal coding harness for Meerkat")]
#[command(
    after_help = "Examples:\n  rkat-tui\n  rkat-tui \"fix the failing test\"\n  rkat-tui --resume last\n  rkat-tui --model claude-sonnet-4-6 --classifier-model claude-haiku-5-5\n  rkat-tui --endpoint-def litellm=openai=http://localhost:4000,key_env=LITELLM_KEY --model litellm:gpt-5.5\n  rkat-tui --allow 'shell=cargo nextest run*' --set auto.compact=auto\n\nEvery tui.toml setting can be given with --set <dotted.key>=<value> (TOML value; bare words are strings).\nCommand-line settings apply after the global and realm tui.toml, before the project file's deny rules."
)]
pub struct Args {
    /// Optional initial prompt to send as the first turn.
    pub prompt: Option<String>,

    /// Model for the session: `<endpoint>:<id>`, or a bare id looked up in the
    /// endpoints' model lists (then inferred from its prefix). Defaults to the
    /// configured or provider default model.
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    /// Endpoint for a bare `--model` id and the default endpoint (a built-in
    /// `anthropic`/`openai`/`gemini`, or a `[endpoints.<name>]` from tui.toml).
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Set any tui.toml key for this run: `--set auto.compact=auto`,
    /// `--set endpoints.litellm.base_url=http://host:4000`,
    /// `--set 'allow=[{tool="shell",arg="cargo nextest run*"}]'`. Tables merge,
    /// scalars replace, arrays append. Repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,

    /// Add an allow rule for this run: `<tool>[=<glob>]` (e.g.
    /// `shell=cargo nextest run*`). Repeatable.
    #[arg(long, value_name = "TOOL[=PATTERN]")]
    pub allow: Vec<String>,

    /// Add a deny rule for this run: `<tool>[=<glob>]`. Repeatable.
    #[arg(long, value_name = "TOOL[=PATTERN]")]
    pub deny: Vec<String>,

    /// Define an endpoint for this run:
    /// `<name>=<dialect>=<url>[,key_env=VAR][,api_style=STYLE]` (dialect:
    /// anthropic, openai, gemini; api_style, openai only: responses or
    /// chat_completions). Repeatable.
    #[arg(
        long = "endpoint-def",
        value_name = "NAME=DIALECT=URL[,key_env=VAR][,api_style=STYLE]"
    )]
    pub endpoint_def: Vec<String>,

    /// Treat `cargo build|test|nextest|check|clippy` as tier 1 (`[auto]
    /// trust_project_builds = true`).
    #[arg(long, conflicts_with = "no_trust_project_builds")]
    pub trust_project_builds: bool,

    /// Review project builds like any other command (`[auto]
    /// trust_project_builds = false`).
    #[arg(long)]
    pub no_trust_project_builds: bool,

    /// Resume an existing session: `last`, `~N`, a session ID, or an ID
    /// prefix. Without a value, opens the session picker at start-up.
    #[arg(long, conflicts_with = "continue_last", num_args = 0..=1, value_name = "REF")]
    pub resume: Option<Option<String>>,

    /// Continue the most recent session of this workspace (same as `--resume last`).
    #[arg(short = 'c', long = "continue")]
    pub continue_last: bool,

    /// Workspace directory: the project root for path policy and conventions
    /// (AGENTS.md, .rkat/). Its session state lives outside it, in
    /// `<config dir>/workspaces/<id>/`. Defaults to the nearest ancestor of the
    /// current directory that has state there, else the current directory.
    #[arg(short = 'w', long = "workspace")]
    pub context_root: Option<PathBuf>,

    /// Stand-in for the home directory (defaults to it): the engine's
    /// user-global conventions (`<root>/.rkat`: AGENTS.md, skills, hooks,
    /// mcp.toml) and the default of `--config-dir` both move under it, so
    /// one flag isolates every user-level input.
    #[arg(long)]
    pub user_config_root: Option<PathBuf>,

    /// Directory for user-global rkat-tui files: AGENTS.md, SYSTEM.md, tui.toml
    /// rules, notes, history, trust records, and every workspace's state
    /// (`workspaces/<id>/`). Defaults to ~/.rkat/tui (under
    /// --user-config-root when given). Moves
    /// only rkat-tui's own files; the engine's conventions stay under
    /// --user-config-root. A confined shell may not reach it (with `-w ~`,
    /// pass another one or --no-confinement).
    #[arg(long)]
    pub config_dir: Option<PathBuf>,

    /// OS process confinement of the shell commands the session runs:
    /// `workspace` lets them write only the workspace, the temp dir, the
    /// Cargo home and the cache dir (network allowed), `strict` additionally
    /// denies IP network access, `off` runs them unconfined. A mode this host
    /// cannot enforce (e.g. no unprivileged user namespaces) refuses to start.
    #[arg(long, value_enum, value_name = "MODE", default_value_t = ConfinementArg::Workspace)]
    pub confinement: ConfinementArg,

    /// Launch processes unconfined (same as `--confinement off`).
    #[arg(long)]
    pub no_confinement: bool,

    /// Let confined commands read this directory (absolute, existing, no
    /// symbolic link on the way, disjoint from the workspace and the other
    /// grants). Repeatable; also `[confinement] extra_read` in your own
    /// tui.toml (this flag wins on overlap).
    #[arg(long, value_name = "DIR")]
    pub confinement_read: Vec<PathBuf>,

    /// Let confined commands read and write this directory (same rules as
    /// `--confinement-read`). Repeatable; also `[confinement] extra_write`
    /// in your own tui.toml (this flag wins on overlap).
    #[arg(long, value_name = "DIR")]
    pub confinement_write: Vec<PathBuf>,

    /// Start in manual mode instead of auto (`[auto] enabled = false`):
    /// every action the classifier would judge is asked of you instead.
    #[arg(long)]
    pub no_auto: bool,

    /// Model used by the auto-mode classifier (defaults per provider).
    #[arg(long)]
    pub classifier_model: Option<String>,

    /// Do not capture mouse events. The mouse is captured by default for wheel
    /// scrolling and click-to-expand tool output; most terminals still allow
    /// native text selection with Shift held. This flag restores plain selection.
    #[arg(long)]
    pub no_mouse: bool,

    /// Trust the project inputs (`.rkat/mcp.toml`, `.rkat/config.toml`'s
    /// hooks) for this run even though they are not in the host trust
    /// record (`/trust` records them for later runs).
    #[arg(long)]
    pub trust_project: bool,
    /// Block until every configured MCP server has finished connecting before
    /// the first turn (default: connect in the background).
    #[arg(long)]
    pub wait_for_mcp: bool,

    /// Compact the context automatically once it reaches the `[auto]`
    /// `compact_auto_percent` (overrides `compact` in tui.toml).
    #[arg(long, conflicts_with = "no_auto_compact")]
    pub auto_compact: bool,

    /// Only warn when the context fills up; never compact automatically
    /// (overrides `compact` in tui.toml).
    #[arg(long)]
    pub no_auto_compact: bool,

    /// Do not download the community price list for the cost readout; use
    /// the cached copy and `[pricing]` from tui.toml only.
    #[arg(long)]
    pub no_pricing_fetch: bool,

    /// Which `rkat-tui-mob` daemon runs the agent (the default, as `auto`:
    /// the workspace's, attaching to the running one, else starting one).
    /// `existing` only attaches to a running one; a DIRECTORY (containing
    /// `/` or starting with `.` or `~`) attaches to that workspace's
    /// running daemon; HOST:PORT attaches to a daemon reached through a
    /// tunnel, with `--attach-token-file`. The `=` is required: a word
    /// after a bare `--attach` is the prompt.
    /// `/detach` leaves the daemon running; `/quit` stops a daemon this
    /// rkat-tui started (else it detaches or asks); `/daemon stop` shuts it
    /// down.
    #[arg(
        long,
        value_parser = parse_attach,
        value_name = "auto|existing|DIRECTORY|HOST:PORT",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "auto"
    )]
    pub attach: Option<AttachTarget>,

    /// The token of the daemon `--attach=HOST:PORT` reaches: a file holding
    /// the token, or a copy of the daemon's endpoint file. It must not be
    /// readable by others (0600 expected).
    #[arg(long, value_name = "PATH")]
    pub attach_token_file: Option<PathBuf>,

    /// Let `--attach=HOST:PORT` reach a non-loopback address. The token
    /// never crosses the wire (both sides prove they hold it), but the
    /// session itself does, unencrypted: prompts, the agent's output, the
    /// approvals. Forward the daemon's port with `ssh -L` instead, which
    /// gives a loopback address and an encrypted tunnel.
    #[arg(long)]
    pub attach_insecure: bool,

    /// List every `rkat-tui-mob` daemon that has an endpoint file
    /// (workspace, address, pid, uptime, clients, conversation, running or
    /// idle; `stale` for one that does not answer, whose file is removed
    /// when its pid is gone), then every workspace's state (its id, the
    /// workspace, `gone` when that no longer exists, last use, its daemon),
    /// then exit.
    #[arg(long)]
    pub list: bool,

    /// Re-home the state of a workspace that moved: the state REF names
    /// (its id, or the path the workspace had; `--list` shows both) serves
    /// this workspace from now on. State this workspace already has is
    /// replaced only when it holds no conversation; an idle daemon of
    /// either is stopped first (a busy one refuses). Then exit.
    #[arg(long, value_name = "REF")]
    pub rehome: Option<String>,

    /// Print the workspace daemon's audit log (every gate decision and who
    /// made it, and the changes of mode, rules and trust; the rotated
    /// generation first), then exit.
    #[arg(long)]
    pub audit: bool,
}

impl Args {
    /// Whether the UI captures mouse events (on unless `--no-mouse`).
    #[must_use]
    pub fn mouse_capture(&self) -> bool {
        !self.no_mouse
    }

    /// The daemon `--attach` names; without it, the workspace's
    /// ([`AttachTarget::Auto`]: the running one, else a started one).
    #[must_use]
    pub fn attach_target(&self) -> AttachTarget {
        self.attach.clone().unwrap_or(AttachTarget::Auto)
    }

    /// The selected process confinement: `--no-confinement` wins over
    /// `--confinement`.
    #[must_use]
    pub fn effective_confinement(&self) -> ConfinementArg {
        if self.no_confinement {
            ConfinementArg::Off
        } else {
            self.confinement
        }
    }
}

/// `--attach=<value>`: which daemon, and whether a missing one is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachTarget {
    /// The workspace's daemon, started when none answers (`auto`).
    Auto,
    /// The workspace's running daemon only (`existing`).
    Existing,
    /// The running daemon of the workspace at (or above) this directory.
    Directory(PathBuf),
    /// A daemon reached at `HOST:PORT` (through a tunnel).
    Address(String),
}

impl AttachTarget {
    /// Whether a missing daemon is started (only the workspace's own, in
    /// `auto`).
    #[must_use]
    pub fn mode(&self) -> AttachMode {
        match self {
            Self::Auto => AttachMode::Auto,
            Self::Existing | Self::Directory(_) | Self::Address(_) => AttachMode::Existing,
        }
    }
}

/// Parse an `--attach` value: `auto`, `existing`, a directory (containing
/// `/` or starting with `.` or `~`; `~` is the home directory) or
/// `HOST:PORT`.
///
/// # Errors
///
/// What the value should have been.
pub fn parse_attach(value: &str) -> Result<AttachTarget, String> {
    match value {
        "auto" => return Ok(AttachTarget::Auto),
        "existing" => return Ok(AttachTarget::Existing),
        _ => {}
    }
    if value.contains('/') || value.starts_with('.') || value.starts_with('~') {
        let path = match value.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                let home = dirs::home_dir().ok_or("no home directory to expand `~`")?;
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(value),
        };
        return Ok(AttachTarget::Directory(path));
    }
    match value.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0) => {
            Ok(AttachTarget::Address(value.to_string()))
        }
        _ => Err(format!(
            "`{value}`: expected auto, existing, a directory (containing / or starting with . or ~) or HOST:PORT"
        )),
    }
}

/// Whether a missing daemon is started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AttachMode {
    /// Attach to the workspace's daemon, starting it when none answers.
    Auto,
    /// Attach only to a daemon that is already running.
    Existing,
}

/// `--confinement`: how the session confines the processes it launches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConfinementArg {
    /// Confine each launch to the workspace directory; network allowed.
    Workspace,
    /// Confine each launch to the workspace directory; no IP network access.
    Strict,
    /// Launch as the trusted host, with no OS confinement.
    Off,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `--version` of both binaries prints the build id the attach
    /// handshake compares: the crate version, plus the commit in a checkout.
    #[test]
    fn version_is_the_build_id() {
        use clap::CommandFactory;
        assert!(
            crate::BUILD == env!("CARGO_PKG_VERSION")
                || crate::BUILD.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")),
            "{}",
            crate::BUILD
        );
        assert_eq!(
            Args::command().render_version(),
            format!("rkat-tui {}\n", crate::BUILD)
        );
        assert_eq!(
            crate::daemon::DaemonCli::command().render_version(),
            format!("rkat-tui-mob {}\n", crate::BUILD)
        );
    }

    #[test]
    fn parses_bare_invocation() {
        let args = Args::try_parse_from(["rkat-tui"]).unwrap();
        assert!(args.prompt.is_none());
        assert!(!args.no_pricing_fetch);
        assert!(
            Args::try_parse_from(["rkat-tui", "--no-pricing-fetch"])
                .unwrap()
                .no_pricing_fetch
        );
        assert!(!args.no_auto);
    }

    #[test]
    fn parses_prompt_and_flags() {
        let args = Args::try_parse_from([
            "rkat-tui",
            "--model",
            "claude-sonnet-4-6",
            "--resume",
            "last",
            "--no-auto",
            "fix the tests",
        ])
        .unwrap();
        assert_eq!(args.prompt.as_deref(), Some("fix the tests"));
        assert_eq!(args.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(args.resume, Some(Some("last".to_string())));
        assert!(args.no_auto);
    }

    #[test]
    fn continue_is_short_c_and_conflicts_with_resume() {
        let args = Args::try_parse_from(["rkat-tui", "-c"]).unwrap();
        assert!(args.continue_last);
        assert!(Args::try_parse_from(["rkat-tui", "-c", "--resume", "last"]).is_err());
        let args = Args::try_parse_from(["rkat-tui", "-w", "/tmp/x"]).unwrap();
        assert_eq!(
            args.context_root.as_deref(),
            Some(std::path::Path::new("/tmp/x"))
        );
        // The old names are gone, and so are the shared-realm flags.
        for old in [
            "--context-root",
            "--provider",
            "--allow-project-mcp",
            "--realm",
            "--state-root",
            "--realm-backend",
        ] {
            assert!(
                Args::try_parse_from(["rkat-tui", old, "x"]).is_err(),
                "{old}"
            );
        }
        assert!(Args::try_parse_from(["rkat-tui", "--isolated"]).is_err());
    }

    #[test]
    fn parses_config_dir() {
        let args = Args::try_parse_from(["rkat-tui", "--config-dir", "/tmp/cfg"]).unwrap();
        assert_eq!(
            args.config_dir.as_deref(),
            Some(std::path::Path::new("/tmp/cfg"))
        );
        let args = Args::try_parse_from(["rkat-tui"]).unwrap();
        assert!(args.config_dir.is_none());
    }

    #[test]
    fn exclusive_flag_pairs() {
        assert!(Args::try_parse_from(["rkat-tui", "--auto-compact", "--no-auto-compact"]).is_err());
        assert!(
            Args::try_parse_from([
                "rkat-tui",
                "--trust-project-builds",
                "--no-trust-project-builds"
            ])
            .is_err()
        );
        assert_eq!(
            Args::try_parse_from(["rkat-tui", "--endpoint", "openai"])
                .unwrap()
                .endpoint
                .as_deref(),
            Some("openai")
        );
        let args = Args::try_parse_from([
            "rkat-tui", "--set", "a=1", "--set", "b=2", "--allow", "x", "--deny", "y",
        ])
        .unwrap();
        assert_eq!(args.set, vec!["a=1", "b=2"]);
        assert_eq!(args.allow, vec!["x"]);
        assert_eq!(args.deny, vec!["y"]);
    }

    #[test]
    fn confinement_defaults_to_workspace_and_no_confinement_wins() {
        let args = Args::try_parse_from(["rkat-tui"]).unwrap();
        assert_eq!(args.effective_confinement(), ConfinementArg::Workspace);
        let args = Args::try_parse_from(["rkat-tui", "--confinement", "strict"]).unwrap();
        assert_eq!(args.effective_confinement(), ConfinementArg::Strict);
        let args = Args::try_parse_from(["rkat-tui", "--confinement", "off"]).unwrap();
        assert_eq!(args.effective_confinement(), ConfinementArg::Off);
        let args =
            Args::try_parse_from(["rkat-tui", "--confinement", "strict", "--no-confinement"])
                .unwrap();
        assert_eq!(args.effective_confinement(), ConfinementArg::Off);
        assert!(Args::try_parse_from(["rkat-tui", "--confinement", "bogus"]).is_err());
        let args = Args::try_parse_from([
            "rkat-tui",
            "--confinement-read",
            "/a",
            "--confinement-read",
            "/b",
            "--confinement-write",
            "/c",
        ])
        .unwrap();
        assert_eq!(
            args.confinement_read,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert_eq!(args.confinement_write, vec![PathBuf::from("/c")]);
    }

    #[test]
    fn attach_values_follow_the_grammar_and_the_workspaces_daemon_is_the_default() {
        let mode = |argv: &[&str]| Args::try_parse_from(argv).unwrap().attach;
        assert_eq!(mode(&["rkat-tui"]), None);
        assert_eq!(
            Args::try_parse_from(["rkat-tui"]).unwrap().attach_target(),
            AttachTarget::Auto
        );
        assert_eq!(mode(&["rkat-tui", "--attach"]), Some(AttachTarget::Auto));
        assert_eq!(
            mode(&["rkat-tui", "--attach=auto"]),
            Some(AttachTarget::Auto)
        );
        assert_eq!(
            mode(&["rkat-tui", "--attach=existing"]),
            Some(AttachTarget::Existing)
        );
        // `--no-attach` went with the removed in-process runner.
        assert!(Args::try_parse_from(["rkat-tui", "--no-attach"]).is_err());
        // A prompt after a bare --attach is still the prompt.
        let args = Args::try_parse_from(["rkat-tui", "--attach", "fix it"]).unwrap();
        assert_eq!(args.prompt.as_deref(), Some("fix it"));
        let args = Args::try_parse_from(["rkat-tui", "--attach", "../other"]).unwrap();
        assert_eq!(args.prompt.as_deref(), Some("../other"));
        assert_eq!(args.attach, Some(AttachTarget::Auto));
        // Directories: a slash, a leading dot or tilde.
        for (value, path) in [
            ("../other", PathBuf::from("../other")),
            ("./x", PathBuf::from("./x")),
            (".", PathBuf::from(".")),
            ("/srv/project", PathBuf::from("/srv/project")),
            ("work/project", PathBuf::from("work/project")),
            ("host:1/x", PathBuf::from("host:1/x")),
        ] {
            assert_eq!(
                mode(&["rkat-tui", &format!("--attach={value}")]),
                Some(AttachTarget::Directory(path)),
                "{value}"
            );
        }
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                mode(&["rkat-tui", "--attach=~/p"]),
                Some(AttachTarget::Directory(home.join("p")))
            );
            assert_eq!(
                mode(&["rkat-tui", "--attach=~"]),
                Some(AttachTarget::Directory(home))
            );
        }
        // HOST:PORT.
        for value in [
            "localhost:4100",
            "127.0.0.1:4100",
            "[::1]:4100",
            "devbox:22",
        ] {
            assert_eq!(
                mode(&["rkat-tui", &format!("--attach={value}")]),
                Some(AttachTarget::Address(value.into())),
                "{value}"
            );
        }
        for bogus in ["bogus", "host:", "host:0", "host:99999", ":4100"] {
            assert!(
                Args::try_parse_from(["rkat-tui", &format!("--attach={bogus}")]).is_err(),
                "{bogus}"
            );
        }
        // The `existing` and directory forms never start a daemon.
        assert_eq!(AttachTarget::Auto.mode(), AttachMode::Auto);
        assert_eq!(
            AttachTarget::Directory(PathBuf::from("/x")).mode(),
            AttachMode::Existing
        );
        assert_eq!(
            AttachTarget::Address("h:1".into()).mode(),
            AttachMode::Existing
        );
    }

    #[test]
    fn help_renders_without_terminal() {
        let err = Args::try_parse_from(["rkat-tui", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }
}
