//! OS process confinement of the shell the session launches.
//!
//! `--confinement` ([`ConfinementArg`]) is lowered into an
//! [`ExecutionConfinement`] ([`lower`]), checked against this host's
//! backend once at start-up (`meerkat_sandbox::CompiledConfinement`), and
//! handed to the factory as the shell's [`ShellConfinement::Required`]
//! requirement (`AgentFactory::with_shell_confinement`):
//!
//! | mode | read and write | read only | IP network |
//! |---|---|---|---|
//! | `workspace` (default) | the workspace, the temp dir, the Cargo home, the user cache dir, each `--confinement-write` | the rustup home, `/usr/local`, `/usr/share`, `/usr/include`, `/usr/libexec`, `/opt`, `/etc/ssl`, `/etc/pki`, `/etc/ca-certificates`, `/etc/alternatives`, each `--confinement-read`; the files `/etc/{resolv.conf,hosts,nsswitch.conf,host.conf,gai.conf,services,protocols,passwd,group,gitconfig}` | allowed (`cargo fetch`) |
//! | `strict` | as `workspace` | as `workspace` | denied |
//! | `off` (`--no-confinement`) | the trusted host: no OS confinement | | the host's |
//!
//! Every launch also gets the Linux backend's `command_runtime_v1` baseline:
//! the directories `/bin`, `/sbin`, `/usr/bin`, `/usr/sbin`, `/lib`,
//! `/lib64`, `/usr/lib`, `/usr/lib64` and the files `/etc/ld.so.cache`,
//! `/dev/null`, `/dev/random`, `/dev/urandom`, read only. Nothing else of
//! `/etc`, `/run`, `/proc`, `/sys` or the home directory is visible: the
//! launch runs in an empty private mount view holding exactly these grants.
//!
//! The requirement is built here rather than through upstream's `workspace`
//! preset (`HostConfinement::resolve`). The Linux backend binds each grant
//! at its own path and refuses a grant that overlaps another or the
//! baseline (so `/etc` and `/usr` cannot be granted whole), that does not
//! exist, that passes through a symbolic link, or that is a file with more
//! than one hard link. One exception: a read-only file grant may itself be
//! a symbolic link (its directories still real), and the backend binds the
//! link's regular-file target at the link's own path. [`grants`] keeps each
//! detected path only when it exists, is canonical (a file grant: up to a
//! final link to a regular file, on Linux), and is disjoint from the other
//! grants and the baseline; single files (`PathAccess::Literal`) give a
//! confined command name resolution and user lookup without the rest of
//! `/etc`.
//!
//! **Name resolution.** `getent hosts localhost` works from the granted
//! `/etc/hosts` and `/etc/nsswitch.conf`; DNS from the granted
//! `/etc/resolv.conf`. Where that is a symbolic link (systemd-resolved:
//! `/run/systemd/resolve/stub-resolv.conf`), the confined command sees the
//! target's content, read-only, at `/etc/resolv.conf` and nothing else of the
//! target's directory; the stub resolver's `127.0.0.53` is reachable
//! wherever the IP network is (`workspace`, not `strict`).
//!
//! **Extra grants.** `--confinement-read <dir>` and
//! `--confinement-write <dir>` ([`ExtraGrants`]) add directories. Each must be absolute,
//! canonical (no symbolic link on the way), an existing directory, off the
//! baseline and `/proc`, `/sys`, `/dev`, and disjoint from the workspace and
//! the other extra grants. One inside a detected grant that already gives
//! at least that access (`--confinement-write /tmp`) leaves the sandbox as
//! it is and only tells the approval gate that the user named it. A
//! detected grant that overlaps an extra grant otherwise yields to it (the
//! backend refuses overlaps). A violation is a typed start-up error, never
//! a silent drop.
//!
//! **The approval gate.** The same grants are computed whether the shell
//! is confined or not ([`plan_grants`], [`ConfinementPlan::granted`]): the
//! classifier is told every directory outside the workspace they allow to
//! be written or read, and a file tool's edit in a directory the user
//! named for writing is tiered like an edit in the project
//! (`crate::approval::policy`). With `--no-confinement` they reach only
//! the gate.
//! The user's own `tui.toml` layers add more with `[confinement] extra_read
//! = [...]` / `extra_write = [...]` ([`ConfinementSection`]), validated the
//! same way; a file grant that overlaps a command-line grant yields to it
//! (the flags win), and one repeating an earlier file grant with no more
//! access is dropped.
//!
//! Only the shell (foreground calls, background jobs, monitors) is confined.
//! Command hooks and local MCP stdio servers keep running as the trusted
//! host: confining every launch kind makes a host guard its own inputs
//! (`HostConfinement::admit_host_inputs`). The workspace's realm lives
//! outside the workspace, under the config directory
//! ([`super::realm_home`]), and the start refuses a plan that reaches it.
//! What stays in the workspace is the project's own `.rkat/` (its
//! `tui.toml`, `mcp.toml`, hooks), which the Linux backend cannot protect
//! (it has no deny lists, and it refuses a grant nested in another, so a
//! read-only `.rkat` inside the writable workspace cannot be expressed;
//! recorded as an engine ask in `docs/evidence.md`, now for `.git/`). On
//! macOS the requirement denies writes to `<workspace>/.rkat`, as the
//! upstream preset does; on Linux a confined shell can write it, so those
//! files are project inputs, trusted only by digest
//! ([`super::project_trust`], [`super::trust`]).
//!
//! The shell's confinement is the command line's (`--confinement`, the
//! extra grants of the flags and the user's `tui.toml`) alone: a realm
//! config's `[confinement]` table does not decide it (a start-up notice
//! says so when the table is there).
//!
//! **Environment.** A confined command inherits nothing of rkat-tui's
//! environment: the backend clears it and sets only what the shell tool
//! passes. rkat-tui therefore passes [`ConfinementPlan::shell_env`], a
//! curated copy of its own environment ([`confined_environment`]): exactly
//! the variables [`CONFINED_ENV_ALLOWLIST`] names plus the `LC_*` family
//! (program lookup, home-relative tool directories, locale, TLS roots,
//! proxies). Every other variable, provider API keys included, stays
//! outside the sandbox. An unconfined command (`off`) inherits the whole
//! environment as before.
//!
//! A mode this host's backend cannot enforce refuses the start (fail
//! closed, no fallback) with [`ConfinementError::Unenforceable`], whose
//! message explains what blocked it ([`HostUsernsStatus`]: on Linux the
//! user-namespace switches under `/proc/sys`), how to make it work, and how
//! to run without confinement.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use meerkat_core::Config;
use meerkat_core::confinement::{
    ConfinementRefusal, ConfinementSpec, ExecutionConfinement, FilesystemAccess, IpNetworkAccess,
    PathAccess, PlatformBaseline,
};
use meerkat_tools::builtin::shell::ShellConfinement;
use serde::{Deserialize, Serialize};

use crate::cli::{Args, ConfinementArg};

/// Why the shell's confinement cannot be installed.
#[derive(Debug, thiserror::Error)]
pub enum ConfinementError {
    /// The workspace root is not absolute.
    #[error("the workspace root '{}' is not absolute", .0.display())]
    RelativeWorkspace(PathBuf),
    /// The lowered requirement is invalid.
    #[error("the confinement requirement is invalid: {0}")]
    Invalid(#[source] ConfinementRefusal),
    /// This host's backend cannot enforce the selected mode. The message
    /// explains what blocked it, how to make it work and how to run
    /// without confinement ([`unenforceable_help`]).
    #[error("--confinement={mode} cannot be enforced on this host: {source}\n\n{help}")]
    Unenforceable {
        /// The selected mode.
        mode: &'static str,
        /// What this host's user-namespace switches say.
        status: HostUsernsStatus,
        /// The explanation (see [`unenforceable_help`]).
        help: String,
        /// The backend's refusal.
        #[source]
        source: ConfinementRefusal,
    },
    /// An extra grant (`--confinement-read`/`--confinement-write`, or a
    /// `tui.toml` `[confinement] extra_read`/`extra_write` entry) is not
    /// grantable.
    #[error("{flag} {}: {problem}", .path.display())]
    ExtraGrant {
        /// Where it came from: the flag (`--confinement-read`,
        /// `--confinement-write`) or the `tui.toml` key
        /// (`[confinement] extra_read`, `[confinement] extra_write`).
        flag: &'static str,
        /// The path as given.
        path: PathBuf,
        /// What is wrong with it.
        problem: ExtraGrantProblem,
    },
}

/// Why an extra grant is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtraGrantProblem {
    /// Not an absolute path.
    #[error("not an absolute path")]
    NotAbsolute,
    /// Does not exist (or cannot be resolved).
    #[error("does not exist")]
    Missing,
    /// Passes through a symbolic link.
    #[error("passes through a symbolic link, which the sandbox refuses; pass {} instead", .0.display())]
    NotCanonical(PathBuf),
    /// Not a directory.
    #[error("not a directory (only directories can be granted)")]
    NotADirectory,
    /// The root, the backend's baseline, or `/proc`, `/sys`, `/dev`.
    #[error(
        "overlaps the sandbox's own system directories (/, /bin, /lib, /usr/bin, /usr/lib, /proc, /sys, /dev, …)"
    )]
    Reserved,
    /// Overlaps the workspace (or the writable directory holding it).
    #[error("overlaps the workspace grant {}", .0.display())]
    OverlapsWorkspace(PathBuf),
    /// Overlaps another extra grant.
    #[error("overlaps the extra grant {}", .0.display())]
    OverlapsGrant(PathBuf),
}

/// What blocks unprivileged user namespaces on this host, read from the
/// switches under `/proc/sys` (Linux; the first blocker found, in the order
/// below). Only consulted once the backend refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostUsernsStatus {
    /// `user.max_user_namespaces` is `0`: no user namespaces at all.
    MaxNamespacesZero,
    /// `kernel.unprivileged_userns_clone` is `0` (Debian and some other
    /// distribution kernels).
    UnprivilegedCloneDisabled,
    /// `kernel.apparmor_restrict_unprivileged_userns` is `1` (the default
    /// on Ubuntu 24.04 and later).
    AppArmorRestricted,
    /// None of the switches above blocks it (a container's seccomp
    /// profile, a missing capability, a kernel built without them, …).
    NoKnownBlocker,
    /// Not Linux: the backend is Seatbelt (macOS) or there is none.
    NotLinux,
}

impl HostUsernsStatus {
    /// The status of this host.
    #[must_use]
    pub fn detect() -> Self {
        if cfg!(target_os = "linux") {
            Self::detect_in(Path::new("/proc"))
        } else {
            Self::NotLinux
        }
    }

    /// The status read from the procfs mounted at `proc_root` (Linux).
    #[must_use]
    pub fn detect_in(proc_root: &Path) -> Self {
        let read = |path: &str| {
            std::fs::read_to_string(proc_root.join(path))
                .ok()
                .map(|value| value.trim().to_string())
        };
        if read("sys/user/max_user_namespaces").as_deref() == Some("0") {
            Self::MaxNamespacesZero
        } else if read("sys/kernel/unprivileged_userns_clone").as_deref() == Some("0") {
            Self::UnprivilegedCloneDisabled
        } else if read("sys/kernel/apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
            Self::AppArmorRestricted
        } else {
            Self::NoKnownBlocker
        }
    }
}

/// The sysctl drop-in file the explanations suggest.
const SYSCTL_DROP_IN: &str = "/etc/sysctl.d/60-rkat-tui-userns.conf";

/// The explanation of a [`ConfinementError::Unenforceable`]: what blocked
/// the mode (`status`, else the backend's `refusal`), how to make it work,
/// and how to run without confinement. `exe` is this binary (for the
/// AppArmor profile).
#[must_use]
pub fn unenforceable_help(
    status: HostUsernsStatus,
    refusal: &ConfinementRefusal,
    exe: Option<&Path>,
) -> String {
    let sysctl = |key: &str, value: &str, effect: &str| {
        format!(
            "To make it work, {effect}:\n    sudo sysctl -w {key}={value}\n  and keep it across reboots:\n    echo '{key}={value}' | sudo tee {SYSCTL_DROP_IN}\n"
        )
    };
    let (blocked, fix) = match status {
        HostUsernsStatus::MaxNamespacesZero => (
            "What blocked it: /proc/sys/user/max_user_namespaces is 0, so no process may create a user namespace, and the Linux sandbox needs one per confined command.".to_string(),
            sysctl(
                "user.max_user_namespaces",
                "15000",
                "allow user namespaces again (for every program on this host)",
            ),
        ),
        HostUsernsStatus::UnprivilegedCloneDisabled => (
            "What blocked it: /proc/sys/kernel/unprivileged_userns_clone is 0, so unprivileged processes may not create user namespaces, and the Linux sandbox needs one per confined command.".to_string(),
            sysctl(
                "kernel.unprivileged_userns_clone",
                "1",
                "allow unprivileged user namespaces (for every program on this host: a larger kernel attack surface)",
            ),
        ),
        HostUsernsStatus::AppArmorRestricted => {
            let exe = exe.map_or_else(|| "/path/to/rkat-tui".to_string(), |exe| exe.display().to_string());
            (
                "What blocked it: AppArmor restricts unprivileged user namespaces on this host (/proc/sys/kernel/apparmor_restrict_unprivileged_userns is 1, the default on Ubuntu 24.04 and later), and the Linux sandbox needs one per confined command.".to_string(),
                format!(
                    "To make it work, either:\n  - allow user namespaces for this binary only (narrower: the profile is attached by path, so it covers whatever binary sits at {exe}, and a binary installed elsewhere needs its own):\n      printf '%s\\n' 'abi <abi/4.0>,' 'include <tunables/global>' 'profile rkat-tui {exe} flags=(unconfined) {{' '  userns,' '}}' | sudo tee /etc/apparmor.d/rkat-tui\n      sudo apparmor_parser -r /etc/apparmor.d/rkat-tui\n  - or lift the restriction for every program (broader: any unprivileged program may then create user namespaces, a larger kernel attack surface):\n      sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\n    and keep it across reboots:\n      echo 'kernel.apparmor_restrict_unprivileged_userns=0' | sudo tee {SYSCTL_DROP_IN}\n"
                ),
            )
        }
        HostUsernsStatus::NoKnownBlocker => (
            format!(
                "What blocked it: the sandbox backend refused ({refusal}). None of the usual user-namespace switches is off (user.max_user_namespaces, kernel.unprivileged_userns_clone, kernel.apparmor_restrict_unprivileged_userns); inside a container, its seccomp profile or missing privileges commonly deny unshare(CLONE_NEWUSER)."
            ),
            "To make it work, run rkat-tui where unprivileged user namespaces are allowed (on the host rather than in the container, or with a container profile that permits them).\n".to_string(),
        ),
        HostUsernsStatus::NotLinux if cfg!(target_os = "macos") => (
            format!("What blocked it: macOS confines commands with Seatbelt (sandbox-exec), and it refused this requirement ({refusal})."),
            "To make it work, check that /usr/bin/sandbox-exec is present and not blocked by a management profile.\n".to_string(),
        ),
        HostUsernsStatus::NotLinux => (
            format!("What blocked it: this platform has no sandbox backend ({refusal}); confinement is supported on Linux (user namespaces) and macOS (Seatbelt)."),
            String::new(),
        ),
    };
    let off = "To run without OS confinement instead: rkat-tui --no-confinement (or --confinement=off). Commands then run as you, guarded by the approval policy only.";
    format!("{blocked}\n\n{fix}\n{off}")
}

/// The paths of the backends' `command_runtime_v1` baseline (mirrors the
/// Linux backend's list), plus the paths the backend never grants: no grant
/// may overlap them.
const BASELINE: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/etc/ld.so.cache",
    "/dev",
    "/proc",
    "/sys",
];

/// Read-only system directories a confined command commonly needs.
const SYSTEM_READ_DIRS: &[&str] = &[
    "/usr/local",
    "/usr/share",
    "/usr/include",
    "/opt",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    // Debian's `cc`, `ld` and friends are links through `/etc/alternatives`;
    // without it a confined `rustc` cannot find its linker.
    "/etc/alternatives",
    // gcc's own programs (`cc1`, `collect2`, the LTO plugin) live here.
    "/usr/libexec",
];

/// Read-only system files: name resolution, user lookup, git defaults.
const SYSTEM_READ_FILES: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/services",
    "/etc/protocols",
    "/etc/passwd",
    "/etc/group",
    "/etc/gitconfig",
];

/// Paths outside the workspace a confined shell commonly needs, as found on
/// this host (not yet filtered; see [`grants`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPaths {
    /// Writable directories: the temp dir, the Cargo home, the user cache dir.
    pub write: Vec<PathBuf>,
    /// Read-only directories: toolchains, shared data, TLS roots.
    pub read: Vec<PathBuf>,
    /// Read-only files: name resolution, user lookup.
    pub read_files: Vec<PathBuf>,
}

impl HostPaths {
    /// The usual paths of this host, from the environment.
    #[must_use]
    pub fn detect() -> Self {
        let home = dirs::home_dir();
        let env_or_home = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(fallback)))
        };
        let mut read: Vec<PathBuf> = env_or_home("RUSTUP_HOME", ".rustup").into_iter().collect();
        read.extend(SYSTEM_READ_DIRS.iter().map(PathBuf::from));
        let cargo = env_or_home("CARGO_HOME", ".cargo")
            .map(|home| CargoHome::of(&home))
            .unwrap_or_default();
        read.extend(cargo.read);
        Self {
            write: [Some(std::env::temp_dir()), dirs::cache_dir()]
                .into_iter()
                .flatten()
                .chain(cargo.write)
                .collect(),
            read,
            read_files: SYSTEM_READ_FILES
                .iter()
                .map(PathBuf::from)
                .chain(cargo.read_files)
                .collect(),
        }
    }
}

/// What a confined shell gets of the Cargo home: the registry and git
/// caches writable (cargo fetches into them), every other entry read-only
/// (`bin/`, `config.toml`, `env`: a confined command must not replace a
/// tool or point `build.rustc-wrapper` somewhere), and the credentials
/// (`credentials.toml`, the older `credentials`) not at all. The engine
/// takes no exclusion inside a grant, so each entry is its own sibling
/// grant, as found at start: an entry created later is not in the view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoHome {
    /// `registry/` and `git/`, as they exist.
    pub write: Vec<PathBuf>,
    /// The other directories.
    pub read: Vec<PathBuf>,
    /// The other files, the credentials left out.
    pub read_files: Vec<PathBuf>,
}

/// The Cargo home's entries a confined shell never sees.
pub const CARGO_CREDENTIALS: [&str; 2] = ["credentials.toml", "credentials"];

/// The Cargo home's directories a confined shell may write.
pub const CARGO_WRITABLE: [&str; 2] = ["registry", "git"];

impl CargoHome {
    /// The grants of the Cargo home at `home` (none when it is not a
    /// directory or cannot be listed).
    #[must_use]
    pub fn of(home: &Path) -> Self {
        let mut grants = Self::default();
        let Ok(entries) = std::fs::read_dir(home) else {
            return grants;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if CARGO_CREDENTIALS.contains(&name) {
                continue;
            }
            // Followed: a symbolic link is granted as what it names.
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() && CARGO_WRITABLE.contains(&name) {
                grants.write.push(entry.path());
            } else if meta.is_dir() {
                grants.read.push(entry.path());
            } else if meta.is_file() {
                grants.read_files.push(entry.path());
            }
        }
        grants
    }
}

/// Directories the user adds to a confined shell's view:
/// `--confinement-read` / `--confinement-write`, and the `[confinement]`
/// tables of the user's own `tui.toml` layers ([`ConfinementSection`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraGrants {
    /// Read-only directories (`--confinement-read`).
    pub read: Vec<PathBuf>,
    /// Read-write directories (`--confinement-write`).
    pub write: Vec<PathBuf>,
    /// The `tui.toml` grants; a command-line grant wins over an
    /// overlapping one.
    pub file: ConfinementSection,
}

impl ExtraGrants {
    /// The extra grants of the command line.
    #[must_use]
    pub fn from_args(args: &Args) -> Self {
        Self {
            read: args.confinement_read.clone(),
            write: args.confinement_write.clone(),
            file: ConfinementSection::default(),
        }
    }

    /// Add the grants of a `tui.toml` `[confinement]` table (they yield to
    /// the command line's, see [`grants_with`]).
    pub fn extend_from(&mut self, section: &ConfinementSection) {
        self.file.extend_from(section);
    }

    /// Whether nothing is added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty() && self.file.is_empty()
    }
}

/// A `[confinement]` table of a user's own `tui.toml` (the same grants as
/// `--confinement-read`/`--confinement-write`). Only user layers may widen
/// the sandbox; a project file must never carry one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfinementSection {
    /// Read-only directories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_read: Vec<PathBuf>,
    /// Read-write directories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_write: Vec<PathBuf>,
}

impl ConfinementSection {
    /// Append `other`'s grants (a later layer adds to an earlier one).
    pub fn extend_from(&mut self, other: &Self) {
        self.extra_read.extend(other.extra_read.iter().cloned());
        self.extra_write.extend(other.extra_write.iter().cloned());
    }

    /// Number of grants (directories) listed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.extra_read.len() + self.extra_write.len()
    }

    /// Whether no grant is listed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extra_read.is_empty() && self.extra_write.is_empty()
    }
}

/// The filesystem grants of a confined shell: `(read-write directories,
/// read-only directories, read-only files)`, every path existing, canonical
/// (not through a symbolic link), each once, none overlapping another or the
/// baseline. The workspace comes first among the writable directories (or
/// the writable directory containing it).
#[must_use]
pub fn grants(workspace: &Path, host: &HostPaths) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let on_baseline = |path: &Path| BASELINE.iter().any(|base| overlaps(path, Path::new(base)));
    let mut write: Vec<PathBuf> = Vec::new();
    for path in std::iter::once(workspace).chain(host.write.iter().map(PathBuf::as_path)) {
        let Some(path) = canonical(path, true) else {
            continue;
        };
        if on_baseline(&path) || write.iter().any(|kept| path.starts_with(kept)) {
            continue;
        }
        write.retain(|kept| !kept.starts_with(&path));
        write.push(path);
    }
    let mut read: Vec<PathBuf> = Vec::new();
    for path in &host.read {
        let Some(path) = canonical(path, true) else {
            continue;
        };
        if on_baseline(&path) || write.iter().chain(&read).any(|kept| overlaps(&path, kept)) {
            continue;
        }
        read.push(path);
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for path in &host.read_files {
        let Some(path) = canonical(path, false) else {
            continue;
        };
        if on_baseline(&path)
            || write.iter().chain(&read).any(|dir| path.starts_with(dir))
            || files.contains(&path)
        {
            continue;
        }
        files.push(path);
    }
    (write, read, files)
}

/// [`grants`] with the user's `extra` directories (see the module docs).
///
/// # Errors
///
/// [`ConfinementError::ExtraGrant`] naming the first extra grant that is
/// not grantable.
#[cfg(any(test, feature = "test-support"))]
pub fn grants_with(
    workspace: &Path,
    host: &HostPaths,
    extra: &ExtraGrants,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>), ConfinementError> {
    plan_grants(workspace, host, extra).map(|planned| (planned.write, planned.read, planned.files))
}

/// What [`plan_grants`] decides: the sandbox's grants and the directories
/// the user named, as the approval gate is told them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlannedGrants {
    /// The sandbox's read-write directories (as [`grants_with`]).
    pub write: Vec<PathBuf>,
    /// The sandbox's read-only directories.
    pub read: Vec<PathBuf>,
    /// The sandbox's read-only files.
    pub files: Vec<PathBuf>,
    /// The canonical directories the user named for writing, in the order
    /// given; one a detected grant already covers is here without
    /// changing the sandbox (it tells the gate the user trusts it).
    pub user_write: Vec<PathBuf>,
    /// The canonical directories the user named for reading.
    pub user_read: Vec<PathBuf>,
}

impl PlannedGrants {
    /// The directories outside `workspace` the approval gate is told the
    /// user allows: the user's own, and the sandbox's other grants as the
    /// defaults (the workspace itself left out), each list sorted.
    #[must_use]
    pub fn granted(&self, workspace: &Path) -> crate::approval::classifier::GrantedDirs {
        let workspace = canonical(workspace, true).unwrap_or_else(|| workspace.to_path_buf());
        let named =
            |path: &PathBuf| self.user_write.contains(path) || self.user_read.contains(path);
        let sorted = |mut paths: Vec<PathBuf>| {
            paths.sort();
            paths.dedup();
            paths
        };
        crate::approval::classifier::GrantedDirs {
            baseline_write: sorted(
                self.write
                    .iter()
                    .filter(|dir| **dir != workspace && !named(dir))
                    .cloned()
                    .collect(),
            ),
            baseline_read: sorted(
                self.read
                    .iter()
                    .chain(&self.files)
                    .filter(|dir| !named(dir))
                    .cloned()
                    .collect(),
            ),
            user_write: sorted(self.user_write.clone()),
            user_read: sorted(self.user_read.clone()),
        }
    }
}

/// [`grants_with`], with the directories the user named kept apart
/// ([`PlannedGrants::user_write`], [`PlannedGrants::user_read`]). An extra
/// grant a detected grant already covers with at least that access is
/// accepted for the gate and leaves the sandbox as it is.
///
/// # Errors
///
/// [`ConfinementError::ExtraGrant`] naming the first extra grant that is
/// not grantable.
pub fn plan_grants(
    workspace: &Path,
    host: &HostPaths,
    extra: &ExtraGrants,
) -> Result<PlannedGrants, ConfinementError> {
    let (mut write, mut read, mut files) = grants(workspace, host);
    let accepted = accept_extra(workspace, &write, &read, extra)?;
    let granted = || accepted.iter().filter(|grant| grant.sandbox);
    let clashes = |dir: &PathBuf| granted().any(|grant| overlaps(dir, &grant.path));
    write.retain(|dir| !clashes(dir));
    read.retain(|dir| !clashes(dir));
    files.retain(|file| !granted().any(|grant| file.starts_with(&grant.path)));
    let (mut user_write, mut user_read) = (Vec::new(), Vec::new());
    for grant in accepted {
        let (sandbox, user) = if grant.writable {
            (&mut write, &mut user_write)
        } else {
            (&mut read, &mut user_read)
        };
        if grant.sandbox {
            sandbox.push(grant.path.clone());
        }
        user.push(grant.path);
    }
    Ok(PlannedGrants {
        write,
        read,
        files,
        user_write,
        user_read,
    })
}

/// One accepted extra grant.
struct Accepted {
    path: PathBuf,
    writable: bool,
    from_cli: bool,
    /// Whether the sandbox gets it (`false`: a detected grant already
    /// covers it; only the gate is told).
    sandbox: bool,
}

/// The extra grants of `extra` validated against the workspace, the
/// detected grants `write` and `read`, and each other.
fn accept_extra(
    workspace: &Path,
    write: &[PathBuf],
    read: &[PathBuf],
    extra: &ExtraGrants,
) -> Result<Vec<Accepted>, ConfinementError> {
    let mut accepted: Vec<Accepted> = Vec::new();
    if extra.is_empty() {
        return Ok(accepted);
    }
    let workspace_dir = canonical(workspace, true);
    let workspace_grant = workspace_dir
        .as_ref()
        .and_then(|ws| write.iter().find(|dir| ws.starts_with(dir)).cloned());
    // (origin label, paths, writable, from the command line)
    let requested = [
        ("--confinement-write", &extra.write, true, true),
        ("--confinement-read", &extra.read, false, true),
        (
            "[confinement] extra_write",
            &extra.file.extra_write,
            true,
            false,
        ),
        (
            "[confinement] extra_read",
            &extra.file.extra_read,
            false,
            false,
        ),
    ];
    for (flag, paths, writable, from_cli) in requested {
        for given in paths {
            let refuse = |problem| ConfinementError::ExtraGrant {
                flag,
                path: given.clone(),
                problem,
            };
            let path = validate_extra(given).map_err(refuse)?;
            if let Some(ws) = workspace_dir.as_ref().filter(|ws| overlaps(&path, ws)) {
                return Err(refuse(ExtraGrantProblem::OverlapsWorkspace(ws.clone())));
            }
            if !from_cli {
                // The command line wins over the file; a file grant repeating
                // an earlier one with no more access adds nothing.
                let yields = accepted.iter().any(|other| {
                    (other.from_cli && overlaps(&path, &other.path))
                        || (other.path == path && (other.writable || !writable))
                });
                if yields {
                    continue;
                }
            }
            if let Some(other) = accepted.iter().find(|other| overlaps(&path, &other.path)) {
                return Err(refuse(ExtraGrantProblem::OverlapsGrant(other.path.clone())));
            }
            let covered = write
                .iter()
                .chain(if writable { &[][..] } else { read })
                .any(|dir| path.starts_with(dir));
            // One a detected grant already covers leaves the sandbox as it is
            // (`--confinement-write /tmp`): it tells the gate the user named it.
            if !covered && let Some(ws) = workspace_grant.as_ref().filter(|ws| overlaps(&path, ws))
            {
                return Err(refuse(ExtraGrantProblem::OverlapsWorkspace(ws.clone())));
            }
            accepted.push(Accepted {
                path,
                writable,
                from_cli,
                sandbox: !covered,
            });
        }
    }
    Ok(accepted)
}

/// `given` as a grantable directory: absolute, existing, canonical, a
/// directory, off the baseline.
fn validate_extra(given: &Path) -> Result<PathBuf, ExtraGrantProblem> {
    if !given.is_absolute() {
        return Err(ExtraGrantProblem::NotAbsolute);
    }
    let resolved = std::fs::canonicalize(given).map_err(|_| ExtraGrantProblem::Missing)?;
    if resolved != given {
        return Err(ExtraGrantProblem::NotCanonical(resolved));
    }
    if !resolved.is_dir() {
        return Err(ExtraGrantProblem::NotADirectory);
    }
    if resolved == Path::new("/")
        || BASELINE
            .iter()
            .any(|base| overlaps(&resolved, Path::new(base)))
    {
        return Err(ExtraGrantProblem::Reserved);
    }
    Ok(resolved)
}

/// Whether one of `a` and `b` contains the other.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// `path` when it exists as a directory (`dir`), absolute and canonical (no
/// symbolic link on the way), or as a file the sandbox backend accepts as a
/// read-only file grant ([`file_grantable`]).
fn canonical(path: &Path, dir: bool) -> Option<PathBuf> {
    if dir {
        let resolved = std::fs::canonicalize(path).ok()?;
        return (path.is_absolute() && resolved == path && resolved.is_dir()).then_some(resolved);
    }
    file_grantable(path).then(|| path.to_path_buf())
}

/// Whether the sandbox backend accepts `path` as a read-only file grant, by
/// its own rules (`meerkat-sandbox` keeps them private): `path` is absolute,
/// off `/proc`, `/sys` and `/dev`, every directory on the way is canonical,
/// and it names a regular file with one hard link, either directly or (on
/// Linux only) through a final symbolic link that the backend binds at the
/// link's path: one hop (the link's text, resolved lexically from its
/// directory, is the target's canonical path) that only root or this user
/// can retarget (the link and every directory from `/` down to its own are
/// owned by one of them, none writable by group or others unless sticky).
/// A detected file the backend would refuse is left out instead of making
/// the whole confinement fail.
fn file_grantable(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let reserved = |path: &Path| {
        ["/proc", "/sys", "/dev"]
            .iter()
            .any(|root| path.starts_with(root))
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    if !path.is_absolute()
        || reserved(path)
        || !path.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        || std::fs::canonicalize(parent).ok().as_deref() != Some(parent)
    {
        return false;
    }
    // `metadata` follows a final link: the object the grant would bind.
    let Ok(target) = std::fs::metadata(path) else {
        return false;
    };
    if !target.is_file() || target.nlink() != 1 {
        return false;
    }
    let Ok(named) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !named.file_type().is_symlink() {
        return true;
    }
    cfg!(target_os = "linux") && trusted_final_link(path, parent, &named, reserved)
}

/// [`file_grantable`]'s rule for a final symbolic link at `path` in the
/// canonical directory `parent`.
fn trusted_final_link(
    path: &Path,
    parent: &Path,
    link: &std::fs::Metadata,
    reserved: impl Fn(&Path) -> bool,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    const STICKY: u32 = 0o1000;
    const GROUP_OR_OTHER_WRITE: u32 = 0o022;
    let (Ok(canonical), Ok(text)) = (std::fs::canonicalize(path), std::fs::read_link(path)) else {
        return false;
    };
    if reserved(&canonical) {
        return false;
    }
    let mut named = parent.to_path_buf();
    for component in text.components() {
        match component {
            std::path::Component::RootDir => named = PathBuf::from("/"),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                named.pop();
            }
            std::path::Component::Normal(name) => named.push(name),
            std::path::Component::Prefix(_) => return false,
        }
    }
    if named != canonical {
        return false;
    }
    let user = nix::unistd::geteuid().as_raw();
    let trusted_owner =
        |metadata: &std::fs::Metadata| metadata.uid() == 0 || metadata.uid() == user;
    trusted_owner(link)
        && parent.ancestors().all(|ancestor| {
            std::fs::symlink_metadata(ancestor).is_ok_and(|metadata| {
                metadata.is_dir()
                    && trusted_owner(&metadata)
                    && (metadata.mode() & GROUP_OR_OTHER_WRITE == 0
                        || metadata.mode() & STICKY != 0)
            })
        })
}

/// What the TUI installs for the shell, and what to tell the user.
#[derive(Debug, Clone)]
pub struct ConfinementPlan {
    /// The shell's requirement (`TrustedHost` for `off`).
    pub shell: ShellConfinement,
    /// One-line start-up notices.
    pub notices: Vec<String>,
    /// The environment of a shell command: `Some` curated set of
    /// rkat-tui's own environment ([`confined_environment`]) when the shell
    /// is [`ShellConfinement::Required`] (a confined command inherits
    /// nothing, so rkat-tui passes this set), `None` when it is
    /// [`ShellConfinement::TrustedHost`] (the command inherits the whole
    /// environment, as before).
    pub shell_env: Option<HashMap<String, String>>,
    /// The directories outside the workspace the user allows, for the
    /// approval gate ([`PlannedGrants::granted`]), whatever the mode.
    pub granted: crate::approval::classifier::GrantedDirs,
}

/// The host variables a confined command gets, by exact name (the `LC_*`
/// family comes in through [`CONFINED_ENV_PREFIXES`]):
///
/// - program lookup and identity: `PATH` (so `~/.cargo/bin` and the like
///   resolve), `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`;
/// - terminal: `TERM`, `COLORTERM`, `NO_COLOR`;
/// - home-relative tool directories: `CARGO_HOME`, `RUSTUP_HOME`,
///   `RUSTUP_TOOLCHAIN`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`,
///   `XDG_DATA_HOME`;
/// - locale and time: `LANG`, `LANGUAGE`, `TZ`;
/// - TLS roots: `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`;
/// - proxies, in both spellings.
///
/// Provider API keys, cloud credentials, tokens and every other variable
/// stay outside the sandbox on purpose: a confined command sees only what
/// a build or a lookup needs.
pub const CONFINED_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "LANG",
    "LANGUAGE",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
];

/// Name prefixes a confined command gets: the locale categories (`LC_ALL`,
/// `LC_CTYPE`, ...).
pub const CONFINED_ENV_PREFIXES: &[&str] = &["LC_"];

/// The curated environment of a confined command: the variables of `vars`
/// that [`CONFINED_ENV_ALLOWLIST`] names or [`CONFINED_ENV_PREFIXES`]
/// covers. A name or value that is not UTF-8 is dropped.
#[must_use]
pub fn confined_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> HashMap<String, String> {
    vars.into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| {
            CONFINED_ENV_ALLOWLIST.contains(&name.as_str())
                || CONFINED_ENV_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        .collect()
}

impl ConfinementPlan {
    /// The lowered requirement, `None` for `off`.
    #[must_use]
    #[cfg(test)]
    pub fn requirement(&self) -> Option<&ExecutionConfinement> {
        match &self.shell {
            ShellConfinement::Required { requirement } => Some(requirement),
            ShellConfinement::TrustedHost => None,
        }
    }
}

/// The start-up label of a mode.
#[must_use]
pub fn describe(mode: ConfinementArg) -> &'static str {
    match mode {
        ConfinementArg::Workspace => "confinement: workspace (network allowed)",
        ConfinementArg::Strict => "confinement: strict (workspace, network denied)",
        ConfinementArg::Off => "confinement: off (commands run unconfined)",
    }
}

/// Lower `mode` over `workspace` into the shell's requirement, `None` for
/// `off`.
///
/// # Errors
///
/// [`ConfinementError`] when the requirement is invalid (a relative
/// workspace root).
#[cfg(any(test, feature = "test-support"))]
pub fn lower(
    mode: ConfinementArg,
    workspace: &Path,
    host: &HostPaths,
) -> Result<Option<ExecutionConfinement>, ConfinementError> {
    lower_with(mode, workspace, host, &ExtraGrants::default())
}

/// [`lower`] with the user's `extra` directories.
///
/// # Errors
///
/// [`ConfinementError`] when the requirement is invalid (a relative
/// workspace root) or an extra grant is not grantable.
#[cfg(any(test, feature = "test-support"))]
pub fn lower_with(
    mode: ConfinementArg,
    workspace: &Path,
    host: &HostPaths,
    extra: &ExtraGrants,
) -> Result<Option<ExecutionConfinement>, ConfinementError> {
    if mode == ConfinementArg::Off {
        return Ok(None);
    }
    lower_planned(mode, workspace, plan_grants(workspace, host, extra)?)
}

/// [`lower_with`] over grants already planned.
fn lower_planned(
    mode: ConfinementArg,
    workspace: &Path,
    planned: PlannedGrants,
) -> Result<Option<ExecutionConfinement>, ConfinementError> {
    let network = match mode {
        ConfinementArg::Workspace => IpNetworkAccess::Unrestricted,
        ConfinementArg::Strict => IpNetworkAccess::Denied,
        ConfinementArg::Off => return Ok(None),
    };
    if !workspace.is_absolute() {
        return Err(ConfinementError::RelativeWorkspace(workspace.to_path_buf()));
    }
    let PlannedGrants {
        write, read, files, ..
    } = planned;
    let spec = ConfinementSpec {
        baseline: PlatformBaseline::CommandRuntimeV1,
        read: FilesystemAccess::Paths(
            subtrees(&write)
                .chain(subtrees(&read))
                .chain(files.into_iter().map(PathAccess::Literal))
                .collect(),
        ),
        write: FilesystemAccess::Paths(subtrees(&write).collect()),
        deny_read: Vec::new(),
        deny_write: protected(workspace),
        network,
        unix_connect: Vec::new(),
        require_descendant_termination: false,
    };
    ExecutionConfinement::try_from(spec)
        .map(Some)
        .map_err(ConfinementError::Invalid)
}

/// Each of `paths` as a subtree grant.
fn subtrees(paths: &[PathBuf]) -> impl Iterator<Item = PathAccess> + '_ {
    paths.iter().cloned().map(PathAccess::Subtree)
}

/// The workspace's own Meerkat state, kept from writes where the backend
/// supports deny lists (macOS).
#[cfg(target_os = "macos")]
fn protected(workspace: &Path) -> Vec<PathAccess> {
    vec![PathAccess::Subtree(workspace.join(".rkat"))]
}

/// The Linux backend has no deny lists.
#[cfg(not(target_os = "macos"))]
fn protected(_workspace: &Path) -> Vec<PathAccess> {
    Vec::new()
}

/// Whether this host's backend can enforce `requirement`.
///
/// # Errors
///
/// The backend's refusal.
pub fn check(requirement: &ExecutionConfinement) -> Result<(), ConfinementRefusal> {
    meerkat_sandbox::CompiledConfinement::compile(requirement).map(|_| ())
}

/// Lower `mode` over `workspace` and check it against this host's
/// backend. `config` is the realm config, whose `[confinement]` table only
/// earns a notice (see the module docs).
///
/// # Errors
///
/// [`ConfinementError::Unenforceable`] when this host's backend cannot
/// enforce the requirement, otherwise when the requirement or an extra
/// grant is invalid.
pub fn plan(
    mode: ConfinementArg,
    workspace: &Path,
    config: &Config,
    host: &HostPaths,
    extra: &ExtraGrants,
) -> Result<ConfinementPlan, ConfinementError> {
    // The grants are planned (and the user's validated) in every mode: the
    // approval gate is told them whether the shell is confined or not.
    let planned = plan_grants(workspace, host, extra)?;
    let granted = planned.granted(workspace);
    let requirement = lower_planned(mode, workspace, planned)?;
    let mut notices = vec![describe(mode).to_string()];
    if !config.confinement.is_empty() {
        notices.push(
            "the realm config's [confinement] does not decide the shell's confinement; rkat-tui confines it as --confinement selects"
                .to_string(),
        );
    }
    let Some(requirement) = requirement else {
        if !extra.is_empty() {
            notices.push(
                "commands run unconfined: the --confinement-read/--confinement-write and tui.toml [confinement] directories tell only the approval gate where you allow the agent to work"
                    .to_string(),
            );
        }
        return Ok(ConfinementPlan {
            shell: ShellConfinement::TrustedHost,
            notices,
            shell_env: None,
            granted,
        });
    };
    check(&requirement).map_err(|source| {
        let status = HostUsernsStatus::detect();
        let exe = std::env::current_exe()
            .ok()
            .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe));
        ConfinementError::Unenforceable {
            mode: mode.name(),
            status,
            help: unenforceable_help(status, &source, exe.as_deref()),
            source,
        }
    })?;
    Ok(ConfinementPlan {
        shell: ShellConfinement::Required { requirement },
        notices,
        shell_env: Some(confined_environment(std::env::vars_os())),
        granted,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::{ConfinementConfig, LaunchConfinement, WorkspaceNetwork};

    fn none() -> ExtraGrants {
        ExtraGrants::default()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        ws: PathBuf,
        cache: PathBuf,
        tools: PathBuf,
        file: PathBuf,
        host: HostPaths,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let cache = root.join("cache");
        let tools = root.join("tools");
        let file = root.join("resolv.conf");
        for d in [&ws, &cache, &tools] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&file, "nameserver 127.0.0.53\n").unwrap();
        let host = HostPaths {
            write: vec![cache.clone()],
            read: vec![tools.clone()],
            read_files: vec![file.clone()],
        };
        Fixture {
            _dir: dir,
            ws,
            cache,
            tools,
            file,
            host,
        }
    }

    /// S3 of the security review: of the Cargo home only `registry/` and
    /// `git/` are writable, the rest is read only entry by entry, and the
    /// credentials are not granted at all.
    #[test]
    fn the_cargo_home_is_granted_entry_by_entry_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        for sub in ["registry", "git", "bin"] {
            std::fs::create_dir_all(home.join(sub)).unwrap();
        }
        for file in [
            "config.toml",
            "credentials.toml",
            "credentials",
            "env",
            ".package-cache",
        ] {
            std::fs::write(home.join(file), "x").unwrap();
        }
        let grants = CargoHome::of(home);
        assert_eq!(grants.write, vec![home.join("git"), home.join("registry")]);
        assert_eq!(grants.read, vec![home.join("bin")]);
        assert_eq!(
            grants.read_files,
            vec![
                home.join(".package-cache"),
                home.join("config.toml"),
                home.join("env")
            ]
        );
        // Lowered: the credentials are in no grant, the Cargo home itself
        // is not writable.
        let f = fixture();
        let host = HostPaths {
            write: grants.write.clone(),
            read: grants.read.clone(),
            read_files: grants.read_files,
        };
        let requirement = lower(ConfinementArg::Workspace, &f.ws, &host)
            .unwrap()
            .unwrap();
        let spec = requirement.specification();
        let canonical_home = std::fs::canonicalize(home).unwrap();
        let FilesystemAccess::Paths(read) = &spec.read else {
            panic!("{:?}", spec.read)
        };
        let FilesystemAccess::Paths(write) = &spec.write else {
            panic!("{:?}", spec.write)
        };
        for access in read.iter().chain(write) {
            let path = access.path();
            assert!(
                !path.ends_with("credentials.toml") && !path.ends_with("credentials"),
                "{path:?}"
            );
            assert_ne!(path, canonical_home.as_path());
        }
        assert!(
            write
                .iter()
                .all(|access| !access.path().starts_with(canonical_home.join("bin")))
        );
        // No Cargo home: nothing granted.
        assert_eq!(CargoHome::of(&home.join("missing")), CargoHome::default());
    }

    #[test]
    fn each_mode_lowers_to_its_requirement() {
        let f = fixture();
        assert!(
            lower(ConfinementArg::Off, &f.ws, &f.host)
                .unwrap()
                .is_none()
        );
        for (mode, network) in [
            (ConfinementArg::Workspace, IpNetworkAccess::Unrestricted),
            (ConfinementArg::Strict, IpNetworkAccess::Denied),
        ] {
            let requirement = lower(mode, &f.ws, &f.host).unwrap().unwrap();
            let spec = requirement.specification();
            assert_eq!(spec.network, network, "{mode:?}");
            assert_eq!(
                spec.write,
                FilesystemAccess::Paths(vec![
                    PathAccess::Subtree(f.ws.clone()),
                    PathAccess::Subtree(f.cache.clone()),
                ]),
                "{mode:?}"
            );
            assert_eq!(
                spec.read,
                FilesystemAccess::Paths(vec![
                    PathAccess::Subtree(f.ws.clone()),
                    PathAccess::Subtree(f.cache.clone()),
                    PathAccess::Subtree(f.tools.clone()),
                    PathAccess::Literal(f.file.clone()),
                ]),
                "{mode:?}"
            );
            assert_eq!(spec.baseline, PlatformBaseline::CommandRuntimeV1);
            assert!(!spec.require_descendant_termination);
            // A host that cannot enforce it refuses the plan, naming the
            // refusal and the way out.
            match plan(mode, &f.ws, &Config::default(), &f.host, &none()) {
                Ok(plan) => {
                    assert_eq!(plan.requirement(), Some(&requirement));
                    assert!(plan.shell_env.is_some(), "{mode:?}");
                }
                Err(err @ ConfinementError::Unenforceable { .. }) => {
                    let refusal = check(&requirement).unwrap_err();
                    let reason = err.to_string();
                    assert!(reason.contains(&refusal.to_string()), "{reason}");
                    assert!(reason.contains("What blocked it"), "{reason}");
                    assert!(reason.contains("rkat-tui --no-confinement"), "{reason}");
                }
                Err(other) => panic!("unexpected error: {other}"),
            }
        }
        let off = plan(
            ConfinementArg::Off,
            &f.ws,
            &Config::default(),
            &f.host,
            &none(),
        )
        .unwrap();
        assert_eq!(off.shell, ShellConfinement::TrustedHost);
        assert_eq!(off.notices, vec![describe(ConfinementArg::Off).to_string()]);
        assert_eq!(off.shell_env, None);
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn a_confined_command_gets_the_allowlist_and_nothing_else() {
        let mut host = vars(&[
            ("PATH", "/home/u/.cargo/bin:/usr/bin"),
            ("HOME", "/home/u"),
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("LC_CTYPE", "en_US.UTF-8"),
            ("https_proxy", "http://proxy:3128"),
            ("SSL_CERT_FILE", "/etc/ssl/cert.pem"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("GITHUB_TOKEN", "ghp"),
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("BASH_ENV", "/tmp/rc"),
            ("RUSTFLAGS", "-Dwarnings"),
        ]);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            host.push((OsString::from("TERM"), OsString::from_vec(vec![0xff, 0xfe])));
        }
        let env = confined_environment(host);
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "https_proxy",
            "SSL_CERT_FILE",
        ] {
            assert!(env.contains_key(name), "{name} kept");
        }
        assert_eq!(env["PATH"], "/home/u/.cargo/bin:/usr/bin");
        for name in [
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "LD_PRELOAD",
            "BASH_ENV",
            "RUSTFLAGS",
            "TERM",
        ] {
            assert!(!env.contains_key(name), "{name} dropped");
        }
        assert_eq!(env.len(), 7, "{env:?}");
    }

    #[test]
    fn no_allowlisted_name_is_one_the_sandbox_refuses() {
        const REFUSED_PREFIXES: [&str; 5] = ["DYLD_", "LD_", "_RLD_", "__XPC_", "BASH_FUNC_"];
        const REFUSED_KEYS: [&str; 6] = [
            "ENV",
            "BASH_ENV",
            "SHELLOPTS",
            "BASHOPTS",
            "ZDOTDIR",
            "GLIBC_TUNABLES",
        ];
        for name in CONFINED_ENV_ALLOWLIST.iter().chain(CONFINED_ENV_PREFIXES) {
            assert!(!REFUSED_KEYS.contains(name), "{name}");
            for prefix in REFUSED_PREFIXES {
                assert!(!name.starts_with(prefix), "{name}");
                // A prefix family must not overlap a refused family either.
                assert!(!prefix.starts_with(name), "{name}");
            }
        }
    }

    #[test]
    fn grants_are_disjoint_canonical_and_off_the_baseline() {
        let f = fixture();
        let parent = f.ws.parent().unwrap().to_path_buf();
        let link = parent.join("link");
        std::os::unix::fs::symlink(&f.tools, &link).unwrap();
        let host = HostPaths {
            write: vec![f.cache.clone(), f.cache.join("inner"), parent.clone()],
            read: vec![
                f.tools.clone(),
                link,
                PathBuf::from("/usr"),
                PathBuf::from("/does/not/exist"),
            ],
            read_files: vec![f.file.clone(), PathBuf::from("/etc/ld.so.cache")],
        };
        let (write, read, files) = grants(&f.ws, &host);
        // The workspace's parent contains every fixture path: it is the only
        // writable grant, and nothing inside it is granted again. `/usr`
        // overlaps the baseline; the link and the missing path are not
        // canonical.
        assert_eq!(write, vec![parent]);
        assert!(read.is_empty(), "{read:?}");
        assert!(files.is_empty(), "{files:?}");
        // Detected paths always satisfy the same rules.
        let (write, read, files) = grants(&f.ws, &HostPaths::detect());
        let all: Vec<&PathBuf> = write.iter().chain(&read).chain(&files).collect();
        for (i, path) in all.iter().enumerate() {
            // A file grant may be a final link; its directory is canonical.
            let named = if files.contains(path) {
                let parent = std::fs::canonicalize(path.parent().unwrap()).unwrap();
                parent.join(path.file_name().unwrap())
            } else {
                std::fs::canonicalize(path).unwrap()
            };
            assert_eq!(&&named, path);
            for other in &all[i + 1..] {
                assert!(
                    !path.starts_with(other) && !other.starts_with(path),
                    "{} overlaps {}",
                    path.display(),
                    other.display()
                );
            }
        }
    }

    #[test]
    fn a_file_grant_may_be_a_final_link_to_a_regular_file_on_linux() {
        let f = fixture();
        let root = f.ws.parent().unwrap().to_path_buf();
        let etc = root.join("etc");
        let run = root.join("run");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        // The backend grants a link only where no other user can retarget
        // it, so the link's directories must not be group-writable whatever
        // the umask made them.
        let set_mode = |path: &Path, mode: u32| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        set_mode(&root, 0o700);
        set_mode(&etc, 0o755);
        let target = run.join("stub-resolv.conf");
        std::fs::write(&target, "nameserver 127.0.0.53\n").unwrap();
        let link = etc.join("resolv.conf");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dir_link = etc.join("dir");
        std::os::unix::fs::symlink(&run, &dir_link).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&etc, &alias).unwrap();
        let linux = cfg!(target_os = "linux");

        assert_eq!(canonical(&link, false), linux.then(|| link.clone()));
        // A link another user could retarget (a group-writable directory) is
        // dropped, as the backend would refuse it.
        set_mode(&etc, 0o775);
        assert_eq!(canonical(&link, false), None);
        set_mode(&etc, 0o755);
        // A regular file stays grantable; a link to a directory, a file
        // reached through a symlinked directory, and a dangling link are
        // refused.
        assert_eq!(canonical(&target, false), Some(target.clone()));
        assert_eq!(canonical(&dir_link, false), None);
        assert_eq!(canonical(&alias.join("resolv.conf"), false), None);
        std::os::unix::fs::symlink(run.join("missing"), etc.join("dangling")).unwrap();
        assert_eq!(canonical(&etc.join("dangling"), false), None);

        // The link alone is granted, as a read-only file at its own path;
        // nothing of the target's directory is.
        let host = HostPaths {
            write: vec![],
            read: vec![],
            read_files: vec![link.clone()],
        };
        let (write, read, files) = grants(&f.ws, &host);
        assert_eq!(write, vec![f.ws.clone()]);
        assert!(read.is_empty(), "{read:?}");
        assert_eq!(files, if linux { vec![link] } else { vec![] });
        let host = HostPaths {
            write: vec![root.clone()],
            ..host
        };
        let (write, read, files) = grants(&f.ws, &host);
        assert_eq!(write, vec![root]);
        assert!(read.is_empty() && files.is_empty(), "{read:?} {files:?}");
    }

    fn realm_with(shell: LaunchConfinement) -> Config {
        Config {
            confinement: ConfinementConfig {
                shell: Some(shell),
                ..ConfinementConfig::default()
            },
            ..Config::default()
        }
    }

    fn workspace_preset(network: WorkspaceNetwork) -> LaunchConfinement {
        LaunchConfinement::Workspace(meerkat_core::WorkspaceConfinement {
            network,
            read: Vec::new(),
            write: Vec::new(),
        })
    }

    /// A realm config's `[confinement]` does not decide the shell's
    /// confinement: the command line does, and a notice says so.
    #[test]
    fn a_realm_confinement_table_does_not_decide_the_shell() {
        let f = fixture();
        let config = realm_with(workspace_preset(WorkspaceNetwork::Unrestricted));
        let off = plan(ConfinementArg::Off, &f.ws, &config, &f.host, &none()).unwrap();
        assert_eq!(off.shell, ShellConfinement::TrustedHost);
        assert_eq!(off.shell_env, None);
        assert!(
            off.notices
                .iter()
                .any(|n| n.contains("does not decide the shell's confinement")),
            "{:?}",
            off.notices
        );
        let plain = plan(
            ConfinementArg::Off,
            &f.ws,
            &Config::default(),
            &f.host,
            &none(),
        )
        .unwrap();
        assert_eq!(plain.notices.len(), 1, "{:?}", plain.notices);
    }

    #[test]
    fn a_relative_workspace_is_refused() {
        let err = lower(
            ConfinementArg::Workspace,
            Path::new("relative"),
            &HostPaths::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ConfinementError::RelativeWorkspace(_)),
            "{err}"
        );
    }

    /// A fake procfs holding the three user-namespace switches.
    fn fake_proc(
        max: Option<&str>,
        clone: Option<&str>,
        apparmor: Option<&str>,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, value) in [
            ("sys/user/max_user_namespaces", max),
            ("sys/kernel/unprivileged_userns_clone", clone),
            ("sys/kernel/apparmor_restrict_unprivileged_userns", apparmor),
        ] {
            if let Some(value) = value {
                let path = dir.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, format!("{value}\n")).unwrap();
            }
        }
        dir
    }

    #[test]
    fn userns_status_is_read_from_the_proc_switches() {
        for (max, clone, apparmor, expected) in [
            (
                Some("348787"),
                Some("1"),
                Some("1"),
                HostUsernsStatus::AppArmorRestricted,
            ),
            (
                Some("348787"),
                None,
                Some("1"),
                HostUsernsStatus::AppArmorRestricted,
            ),
            (
                Some("348787"),
                Some("0"),
                Some("1"),
                HostUsernsStatus::UnprivilegedCloneDisabled,
            ),
            (
                Some("0"),
                Some("0"),
                Some("1"),
                HostUsernsStatus::MaxNamespacesZero,
            ),
            (
                Some("348787"),
                Some("1"),
                Some("0"),
                HostUsernsStatus::NoKnownBlocker,
            ),
            (None, None, None, HostUsernsStatus::NoKnownBlocker),
        ] {
            let proc_root = fake_proc(max, clone, apparmor);
            assert_eq!(
                HostUsernsStatus::detect_in(proc_root.path()),
                expected,
                "{max:?} {clone:?} {apparmor:?}"
            );
        }
    }

    #[test]
    fn the_refusal_explains_the_blocker_the_fix_and_the_way_out() {
        let refusal = ConfinementRefusal::BackendUnavailable;
        let exe = Path::new("/opt/bin/rkat-tui");
        let apparmor =
            unenforceable_help(HostUsernsStatus::AppArmorRestricted, &refusal, Some(exe));
        for needle in [
            "apparmor_restrict_unprivileged_userns is 1",
            "sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0",
            "/etc/sysctl.d/",
            "profile rkat-tui /opt/bin/rkat-tui flags=(unconfined) {",
            "userns,",
            "sudo apparmor_parser -r /etc/apparmor.d/rkat-tui",
            "larger kernel attack surface",
            "rkat-tui --no-confinement (or --confinement=off)",
        ] {
            assert!(
                apparmor.contains(needle),
                "missing {needle:?} in:\n{apparmor}"
            );
        }
        let clone = unenforceable_help(HostUsernsStatus::UnprivilegedCloneDisabled, &refusal, None);
        assert!(
            clone.contains("sudo sysctl -w kernel.unprivileged_userns_clone=1"),
            "{clone}"
        );
        let max = unenforceable_help(HostUsernsStatus::MaxNamespacesZero, &refusal, None);
        assert!(
            max.contains("sudo sysctl -w user.max_user_namespaces="),
            "{max}"
        );
        let unknown = unenforceable_help(HostUsernsStatus::NoKnownBlocker, &refusal, None);
        assert!(unknown.contains(&refusal.to_string()), "{unknown}");
    }

    #[test]
    fn extra_grants_are_validated_and_take_precedence() {
        let f = fixture();
        let root = f.ws.parent().unwrap().to_path_buf();
        let extra_dir = root.join("extra");
        let data = root.join("data");
        std::fs::create_dir_all(&extra_dir).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let extra = ExtraGrants {
            read: vec![data.clone()],
            write: vec![extra_dir.clone()],
            ..ExtraGrants::default()
        };
        let (write, read, files) = grants_with(&f.ws, &f.host, &extra).unwrap();
        assert_eq!(write, vec![f.ws.clone(), f.cache.clone(), extra_dir]);
        assert_eq!(read, vec![f.tools.clone(), data.clone()]);
        assert_eq!(files, vec![f.file.clone()]);
        // A detected read grant overlapping a write grant yields to it.
        let inner = f.tools.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let (_, read, _) = grants_with(
            &f.ws,
            &f.host,
            &ExtraGrants {
                read: Vec::new(),
                write: vec![inner.clone()],
                ..ExtraGrants::default()
            },
        )
        .unwrap();
        assert!(!read.contains(&f.tools), "{read:?}");
        // Every violation is a typed error naming the flag and the path.
        let link = root.join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        let file = f.file.clone();
        for (extra, expected) in [
            (
                vec![PathBuf::from("relative")],
                ExtraGrantProblem::NotAbsolute,
            ),
            (vec![root.join("missing")], ExtraGrantProblem::Missing),
            (vec![link], ExtraGrantProblem::NotCanonical(data.clone())),
            (vec![file], ExtraGrantProblem::NotADirectory),
            (vec![PathBuf::from("/usr/lib")], ExtraGrantProblem::Reserved),
            (vec![PathBuf::from("/")], ExtraGrantProblem::Reserved),
            (
                vec![f.ws.join("..").join("ws")],
                ExtraGrantProblem::NotCanonical(f.ws.clone()),
            ),
            (
                vec![root.clone()],
                ExtraGrantProblem::OverlapsWorkspace(f.ws.clone()),
            ),
            (
                vec![data.clone(), data.clone()],
                ExtraGrantProblem::OverlapsGrant(data),
            ),
        ] {
            let err = grants_with(
                &f.ws,
                &f.host,
                &ExtraGrants {
                    read: extra.clone(),
                    write: Vec::new(),
                    ..ExtraGrants::default()
                },
            )
            .unwrap_err();
            match &err {
                ConfinementError::ExtraGrant { flag, problem, .. } => {
                    assert_eq!(*flag, "--confinement-read");
                    assert_eq!(problem, &expected, "{extra:?}");
                }
                other => panic!("unexpected error: {other}"),
            }
        }
        // A grant a detected grant already covers with that access leaves
        // the sandbox as it is and is kept as the user's, for the gate; a
        // write grant inside a read grant upgrades that part (above).
        let plain = grants(&f.ws, &f.host);
        for (read, write) in [
            (vec![inner], Vec::new()),
            (vec![f.tools.clone()], Vec::new()),
            (vec![f.cache.clone()], Vec::new()),
            (Vec::new(), vec![f.cache.clone()]),
        ] {
            let planned = plan_grants(
                &f.ws,
                &f.host,
                &ExtraGrants {
                    read: read.clone(),
                    write: write.clone(),
                    ..ExtraGrants::default()
                },
            )
            .unwrap();
            assert_eq!(
                (planned.write, planned.read, planned.files),
                plain.clone(),
                "{read:?} {write:?}"
            );
            assert_eq!(planned.user_read, read);
            assert_eq!(planned.user_write, write);
        }
    }

    /// `--confinement-write` naming the temp directory (the fixture's
    /// writable cache stands for it) is accepted, confined or not: the
    /// sandbox is unchanged and the gate is told the user named it.
    #[test]
    fn naming_the_temp_directory_is_accepted_confined_or_not() {
        let f = fixture();
        let extra = ExtraGrants {
            write: vec![f.cache.clone()],
            ..ExtraGrants::default()
        };
        let off = plan(
            ConfinementArg::Off,
            &f.ws,
            &Config::default(),
            &f.host,
            &extra,
        )
        .unwrap();
        assert_eq!(off.granted.user_write, vec![f.cache.clone()]);
        assert!(off.granted.baseline_write.is_empty(), "{:?}", off.granted);
        let confined = lower_with(ConfinementArg::Workspace, &f.ws, &f.host, &extra).unwrap();
        assert_eq!(
            confined,
            lower(ConfinementArg::Workspace, &f.ws, &f.host).unwrap(),
            "the sandbox is unchanged"
        );
    }

    /// Every grant is planned in every mode, for the approval gate: the
    /// defaults (the workspace left out) and the user's own apart.
    #[test]
    fn the_gate_is_told_the_grants_in_every_mode() {
        let f = fixture();
        let root = f.ws.parent().unwrap().to_path_buf();
        let data = root.join("data");
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let extra = ExtraGrants {
            read: vec![data.clone()],
            write: vec![scratch.clone()],
            ..ExtraGrants::default()
        };
        let granted = plan_grants(&f.ws, &f.host, &extra).unwrap().granted(&f.ws);
        assert_eq!(granted.user_write, vec![scratch]);
        assert_eq!(granted.user_read, vec![data]);
        assert_eq!(granted.baseline_write, vec![f.cache.clone()]);
        assert_eq!(granted.baseline_read, {
            let mut read = vec![f.tools.clone(), f.file.clone()];
            read.sort();
            read
        });
        // Unconfined, the plan carries the same directories.
        let off = plan(
            ConfinementArg::Off,
            &f.ws,
            &Config::default(),
            &f.host,
            &extra,
        )
        .unwrap();
        assert_eq!(off.granted, granted);
        // Without extra grants the defaults are all there is.
        let none = plan_grants(&f.ws, &f.host, &ExtraGrants::default())
            .unwrap()
            .granted(&f.ws);
        assert_eq!(none.baseline_write, vec![f.cache]);
        assert!(none.user_write.is_empty() && none.user_read.is_empty());
    }

    #[test]
    fn extra_grants_under_off_reach_only_the_gate() {
        let f = fixture();
        let data = f.ws.parent().unwrap().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let extra = ExtraGrants {
            read: vec![data],
            write: Vec::new(),
            ..ExtraGrants::default()
        };
        // Under `off` they reach only the gate, and a notice says so.
        let off = plan(
            ConfinementArg::Off,
            &f.ws,
            &Config::default(),
            &f.host,
            &extra,
        )
        .unwrap();
        assert!(
            off.notices
                .iter()
                .any(|n| n.contains("only the approval gate")),
            "{:?}",
            off.notices
        );
    }

    #[test]
    fn a_confinement_table_parses_into_extra_grants() {
        let section: ConfinementSection =
            toml::from_str("extra_read = [\"/a\"]\nextra_write = [\"/b\"]\n").unwrap();
        let mut extra = ExtraGrants::default();
        assert!(extra.is_empty());
        extra.extend_from(&section);
        assert!(!extra.is_empty());
        assert!(extra.read.is_empty() && extra.write.is_empty());
        assert_eq!(extra.file.extra_read, vec![PathBuf::from("/a")]);
        assert_eq!(extra.file.extra_write, vec![PathBuf::from("/b")]);
        assert_eq!(extra.file.len(), 2);
        assert!(toml::from_str::<ConfinementSection>("extra = []\n").is_err());
    }

    #[test]
    fn file_grants_are_validated_and_yield_to_the_command_line() {
        let f = fixture();
        let root = f.ws.parent().unwrap().to_path_buf();
        let data = root.join("data");
        let inner = data.join("inner");
        let other = root.join("other");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let file = |read: Vec<PathBuf>, write: Vec<PathBuf>| ConfinementSection {
            extra_read: read,
            extra_write: write,
        };
        // File grants alone are granted like flags.
        let extra = ExtraGrants {
            file: file(vec![data.clone()], vec![other.clone()]),
            ..ExtraGrants::default()
        };
        let (write, read, _) = grants_with(&f.ws, &f.host, &extra).unwrap();
        assert!(write.contains(&other), "{write:?}");
        assert!(read.contains(&data), "{read:?}");
        // A flag wins over an overlapping file grant, in either direction
        // and whatever the access.
        let extra = ExtraGrants {
            write: vec![data.clone()],
            file: file(vec![data.clone(), inner.clone()], Vec::new()),
            ..ExtraGrants::default()
        };
        let (write, read, _) = grants_with(&f.ws, &f.host, &extra).unwrap();
        assert!(write.contains(&data), "{write:?}");
        assert!(!read.contains(&data) && !read.contains(&inner), "{read:?}");
        let extra = ExtraGrants {
            read: vec![inner.clone()],
            file: file(Vec::new(), vec![data.clone()]),
            ..ExtraGrants::default()
        };
        let (write, read, _) = grants_with(&f.ws, &f.host, &extra).unwrap();
        assert!(
            read.contains(&inner) && !write.contains(&data),
            "{write:?} {read:?}"
        );
        // A grant repeated across layers is granted once, with the most
        // access any layer gives.
        let extra = ExtraGrants {
            file: file(vec![data.clone(), data.clone()], vec![data.clone()]),
            ..ExtraGrants::default()
        };
        let (write, read, _) = grants_with(&f.ws, &f.host, &extra).unwrap();
        assert_eq!(write.iter().filter(|d| **d == data).count(), 1);
        assert!(!read.contains(&data));
        // Overlapping file grants and invalid ones are typed errors naming
        // the tui.toml key.
        for (extra, key, expected) in [
            (
                file(vec![data.clone(), inner], Vec::new()),
                "[confinement] extra_read",
                ExtraGrantProblem::OverlapsGrant(data),
            ),
            (
                file(Vec::new(), vec![root.join("missing")]),
                "[confinement] extra_write",
                ExtraGrantProblem::Missing,
            ),
            (
                file(vec![root.clone()], Vec::new()),
                "[confinement] extra_read",
                ExtraGrantProblem::OverlapsWorkspace(f.ws.clone()),
            ),
        ] {
            let err = grants_with(
                &f.ws,
                &f.host,
                &ExtraGrants {
                    file: extra,
                    ..ExtraGrants::default()
                },
            )
            .unwrap_err();
            match &err {
                ConfinementError::ExtraGrant { flag, problem, .. } => {
                    assert_eq!(*flag, key);
                    assert_eq!(problem, &expected);
                }
                other => panic!("unexpected error: {other}"),
            }
        }
    }
}
