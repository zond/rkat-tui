//! Workspace and realm resolution.
//!
//! By default all state for a workspace (sessions, realm config, rules,
//! input history, logs, notes, the audit log) lives outside it, in a realm
//! of its own under the user's config directory:
//! `<config dir>/workspaces/<random id>/` ([`super::realm_home`]). The
//! realm's manifest names the workspace's canonical path and is the only
//! authority for which workspace owns it; nothing in the workspace is read
//! for it. A workspace that moves needs one explicit re-home
//! (`/rehome`, `rkat-tui --rehome`). A `.rkat/tui/` in the workspace is
//! never read. A workspace has exactly one realm, its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meerkat_core::{Config, ConfigRuntime, FileConfigStore};
use meerkat_store::realm_paths_in;

use super::dirs::GlobalDirs;
use super::realm_home::{self, Home};
use crate::cli::Args;
use crate::error::TuiError;
use crate::paths::{PathError, ProjectRoot};

/// Where this TUI process lives: project, conventions roots and realm.
#[derive(Debug, Clone)]
pub struct RuntimeScope {
    /// Canonical project root (path policy and tool resolution).
    pub project_root: ProjectRoot,
    /// Conventions root for `AGENTS.md`, `.rkat/` hooks, skills and MCP config.
    /// Equal to the project root path.
    pub context_root: PathBuf,
    /// User-global conventions root (`--user-config-root`, else the home
    /// directory). `None` when neither is available.
    pub user_config_root: Option<PathBuf>,
    /// User-global `rkat-tui` directories (`--config-dir`, else
    /// `<user config root>/.rkat/tui`; see [`GlobalDirs`]).
    pub global: GlobalDirs,
    /// The workspace's realm: the id of its directory under
    /// `<config dir>/workspaces/`.
    pub realm_id: String,
    /// Directory containing the realm directories
    /// (`<config dir>/workspaces/`).
    pub realms_root: PathBuf,
    /// The workspace's realm and its manifest.
    pub home: Home,
    /// The offer of realms whose workspace is gone, when this resolve made
    /// the workspace a new realm (shown by the process that made it).
    pub offer: Option<String>,
}

impl RuntimeScope {
    /// Root directory of the active realm (`<realms_root>/<realm_id>`).
    #[must_use]
    pub fn realm_root(&self) -> PathBuf {
        realm_paths_in(&self.realms_root, &self.realm_id).root
    }

    /// Path of the realm's `config.toml`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        realm_paths_in(&self.realms_root, &self.realm_id).config_path
    }

    /// Path of the realm config's generation sidecar
    /// (`<realm>/config_state.json`), which every surface's
    /// [`ConfigRuntime`] of the realm uses (its writer lock is
    /// `config_state.lock` beside it).
    #[must_use]
    pub fn config_state_path(&self) -> PathBuf {
        self.realm_root().join("config_state.json")
    }
}

/// Resolve the workspace, conventions roots and realm from the arguments.
///
/// The workspace is `--workspace`, else the nearest ancestor of the working
/// directory that has a realm (its manifest names it; searching no higher
/// than the git top-level, and never the home directory or above it), else
/// the working directory. The home directory itself (or an ancestor of it)
/// is used only when passed explicitly with `--workspace`. State lives in
/// the workspace's realm under the config directory, made on its first
/// start (see [`realm_home`]).
///
/// # Errors
///
/// [`TuiError::ProjectRoot`] when the root cannot be canonicalized or the
/// working directory is the home directory, [`TuiError::RealmHome`] when
/// the realms cannot be read or the workspace's made (two manifests naming
/// it, an unreadable one, no config directory).
pub fn resolve_scope(args: &Args) -> Result<RuntimeScope, TuiError> {
    let global = GlobalDirs::resolve(args.config_dir.as_deref(), args.user_config_root.as_deref())
        .ok_or(realm_home::HomeError::NoConfigDir)?;
    let root = realm_home::root(&global);
    let project_root = project_root_from(&workspace_of(args, Some(&root))?)?;
    let context_root = project_root.as_path().to_path_buf();
    let user_config_root = args.user_config_root.clone().or_else(dirs::home_dir);
    let opened = realm_home::open_or_create(&root, &context_root)?;
    let offer = if opened.created {
        realm_home::offer(&opened.orphans)
    } else {
        None
    };
    let project_root = seal(project_root, &opened.home.dir, &global);
    Ok(RuntimeScope {
        project_root,
        context_root,
        user_config_root,
        global,
        realm_id: opened.home.id.clone(),
        realms_root: root,
        home: opened.home,
        offer,
    })
}

/// The workspace `args` name (canonical) and the realm its manifest gives
/// it, without making one: what `--audit` and `--rehome` look at.
///
/// # Errors
///
/// [`TuiError::RealmHome`] without a config directory or when the realms
/// cannot be read, [`TuiError::ProjectRoot`] when the workspace cannot be
/// resolved.
pub fn lookup(args: &Args) -> Result<(PathBuf, PathBuf, Option<Home>), TuiError> {
    let global = GlobalDirs::resolve(args.config_dir.as_deref(), args.user_config_root.as_deref())
        .ok_or(realm_home::HomeError::NoConfigDir)?;
    let root = realm_home::root(&global);
    let workspace = project_root_from(&workspace_of(args, Some(&root))?)?
        .as_path()
        .to_path_buf();
    let home = realm_home::find(&root, &workspace)?;
    Ok((root, workspace, home))
}

/// The workspace `args` name, as it is given or found (not yet
/// canonical): `--workspace`, else found from the working directory by the
/// realms under `homes_root` ([`default_workspace`]).
///
/// # Errors
///
/// [`TuiError::ProjectRoot`] when the working directory cannot be read or
/// is the home directory, [`TuiError::RealmHome`] when the realms cannot
/// be read.
pub fn workspace_of(args: &Args, homes_root: Option<&Path>) -> Result<PathBuf, TuiError> {
    if let Some(root) = &args.context_root {
        return Ok(root.clone());
    }
    let cwd = std::env::current_dir().map_err(|source| TuiError::ProjectRoot {
        path: PathBuf::from("."),
        source,
    })?;
    let homes = match homes_root {
        Some(root) => realm_home::scan(root)?,
        None => Vec::new(),
    };
    default_workspace(&cwd, dirs::home_dir().as_deref(), &homes)
}

/// `path` with its longest existing ancestor canonicalized and the rest
/// appended (a directory that is not created yet still compares by where
/// it will be).
pub(crate) fn canonical_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |dir, part| dir.join(part));
        }
        match (
            existing.file_name().map(ToOwned::to_owned),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Message for a working directory that must not become the workspace.
const HOME_WORKSPACE_REFUSED: &str =
    "refusing to use your home directory as the workspace; pass --workspace or cd into a project";

/// The workspace when `--workspace` is not given: a workspace found above
/// `cwd` (one of `homes` names it), else `cwd` itself unless it is `home`
/// (or an ancestor of it).
fn default_workspace(cwd: &Path, home: Option<&Path>, homes: &[Home]) -> Result<PathBuf, TuiError> {
    let home = home.map(canonical_or_self);
    if let Some(found) = find_workspace_above(cwd, home.as_deref(), homes)? {
        return Ok(found);
    }
    if home
        .as_deref()
        .is_some_and(|home| home.starts_with(canonical_or_self(cwd)))
    {
        return Err(TuiError::ProjectRoot {
            path: cwd.to_path_buf(),
            source: std::io::Error::other(HOME_WORKSPACE_REFUSED),
        });
    }
    Ok(cwd.to_path_buf())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The workspace `--attach=DIRECTORY` names: the nearest ancestor of
/// `dir` (inclusive, canonical) that has a realm under `homes_root`, as
/// the default workspace is found from the current directory, else `dir`
/// itself.
#[must_use]
pub fn workspace_at(dir: &Path, homes_root: Option<&Path>) -> PathBuf {
    let home = dirs::home_dir().map(|home| canonical_or_self(&home));
    let homes = homes_root
        .and_then(|root| realm_home::scan(root).ok())
        .unwrap_or_default();
    find_workspace_above(dir, home.as_deref(), &homes)
        .ok()
        .flatten()
        .unwrap_or_else(|| canonical_or_self(dir))
}

/// The nearest ancestor of `start` (inclusive) that one of `homes` names,
/// if any.
///
/// The search stops at the git top-level (a directory containing `.git` is
/// the last one checked) and never yields `home` or an ancestor of it.
/// Nothing in the workspace is read for it: a `.rkat/tui/` marks
/// nothing. `home` is expected canonical.
///
/// # Errors
///
/// [`TuiError::RealmHome`] when two realms name a directory on the way.
fn find_workspace_above(
    start: &Path,
    home: Option<&Path>,
    homes: &[Home],
) -> Result<Option<PathBuf>, TuiError> {
    let mut current = canonical_or_self(start);
    loop {
        if home.is_some_and(|home| home.starts_with(&current)) {
            return Ok(None);
        }
        if realm_home::find_in(homes, &current)?.is_some() {
            return Ok(Some(current));
        }
        if current.join(".git").exists() || !current.pop() {
            return Ok(None);
        }
    }
}

/// Seal what a file tool must never write wherever it lies: the git
/// directories governing the project ([`crate::paths::git_dirs`]) and the
/// user config directory (global rules, trust records); and keep the realm
/// directory (its `config.toml`, rules, approvals, `tui.toml`, sessions,
/// the audit log) from the file tools altogether, reads included: it is a
/// secret root.
fn seal(project_root: ProjectRoot, realm_root: &Path, global: &GlobalDirs) -> ProjectRoot {
    let git = crate::paths::git_dirs(project_root.as_path());
    project_root
        .with_sealed_roots(
            git.into_iter()
                .chain(std::iter::once(global.config.clone())),
        )
        .with_secret_roots(std::iter::once(realm_root.to_path_buf()))
}

fn project_root_from(path: &Path) -> Result<ProjectRoot, TuiError> {
    ProjectRoot::new(path).map_err(|err| match err {
        PathError::Io { path, source } => TuiError::ProjectRoot { path, source },
        other => TuiError::ProjectRoot {
            path: path.to_path_buf(),
            source: std::io::Error::other(other.to_string()),
        },
    })
}

/// Load the realm config (`<realms_root>/<realm>/config.toml`) and apply
/// environment overrides: read through the realm's config runtime, under
/// its reader lock.
///
/// # Errors
///
/// Returns [`TuiError::Io`] when the realm directory cannot be created and
/// [`TuiError::Config`] when the file cannot be parsed or env overrides fail.
pub async fn load_config(scope: &RuntimeScope) -> Result<Config, TuiError> {
    let config_path = scope.config_path();
    if let Some(parent) = config_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| TuiError::io("failed to create realm config directory", e))?;
    }
    let document = Arc::new(FileConfigStore::new(
        config_path,
        meerkat_models::canonical(),
    ));
    let mut config = ConfigRuntime::new(document, scope.config_state_path())
        .get()
        .await
        .map_err(|source| TuiError::ConfigRuntime {
            context: "failed to load config",
            source,
        })?
        .config;
    config
        .apply_env_overrides()
        .map_err(|source| TuiError::Config {
            context: "failed to apply env overrides",
            source,
        })?;
    Ok(config)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["rkat-tui"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    /// A workspace `ws` and a config directory beside it, both canonical.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        (dir, ws, base.join("cfg"))
    }

    fn resolve(ws: &Path, cfg: &Path, extra: &[&str]) -> Result<RuntimeScope, TuiError> {
        let mut argv = vec![
            "-w",
            ws.to_str().unwrap(),
            "--config-dir",
            cfg.to_str().unwrap(),
        ];
        argv.extend_from_slice(extra);
        resolve_scope(&args(&argv))
    }

    #[test]
    fn default_state_lives_in_a_realm_under_the_config_dir() {
        let (_dir, ws, cfg) = fixture();
        let scope = resolve(&ws, &cfg, &[]).unwrap();
        let home = scope.home.clone();
        assert_eq!(scope.realm_id, home.id);
        assert_eq!(scope.realms_root, cfg.join(realm_home::WORKSPACES_DIR));
        assert_eq!(scope.realm_root(), cfg.join("workspaces").join(&home.id));
        assert_eq!(home.manifest.path, ws);
        // Nothing is written into the workspace.
        assert_eq!(std::fs::read_dir(&ws).unwrap().count(), 0);
        // The next start finds the same realm, through the manifest.
        assert_eq!(resolve(&ws, &cfg, &[]).unwrap().realm_id, home.id);
        // The realm is secret to the file tools: reads are refused too.
        let file = scope.realm_root().join("audit.jsonl");
        assert!(
            scope
                .project_root
                .resolve(file.to_str().unwrap())
                .unwrap()
                .secret
        );
    }

    /// A `.rkat/tui/` in the workspace (planted by a clone or the agent's
    /// shell) is never read: the start makes the workspace its own realm.
    #[test]
    fn a_planted_workspace_realm_is_ignored() {
        let (_dir, ws, cfg) = fixture();
        let planted = ws.join(".rkat/tui");
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::write(planted.join("tui.toml"), "[[allow]]\ntool = \"shell\"\n").unwrap();
        std::fs::write(planted.join("workspace.toml"), "path = \"/elsewhere\"\n").unwrap();
        let scope = resolve(&ws, &cfg, &[]).unwrap();
        assert_ne!(scope.realm_root(), planted);
        assert!(!scope.realm_root().starts_with(&ws));
        // Nothing of it is read as a layer.
        let (layered, _) = super::super::approval::load_layers(&scope, &args(&[])).unwrap();
        assert_eq!(layered.rules.allow().len(), 0);
        // And it marks no workspace for discovery from below.
        let sub = ws.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let homes = Vec::new();
        assert_eq!(find_workspace_above(&sub, None, &homes).unwrap(), None);
    }

    #[test]
    fn workspace_is_found_from_a_subdirectory_by_its_manifest() {
        let (_dir, ws, cfg) = fixture();
        resolve(&ws, &cfg, &[]).unwrap();
        let homes = realm_home::scan(&cfg.join("workspaces")).unwrap();
        let sub = ws.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            find_workspace_above(&sub, None, &homes).unwrap().as_deref(),
            Some(ws.as_path())
        );
        let other = tempfile::tempdir().unwrap();
        assert!(
            find_workspace_above(other.path(), None, &homes)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn discovery_stops_at_the_git_top_level() {
        let (_dir, outer, cfg) = fixture();
        // A workspace above the repository must not be picked up.
        resolve(&outer, &cfg, &[]).unwrap();
        let repo = outer.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let homes = realm_home::scan(&cfg.join("workspaces")).unwrap();
        assert!(find_workspace_above(&sub, None, &homes).unwrap().is_none());
        assert_eq!(default_workspace(&sub, None, &homes).unwrap(), sub);
        // A workspace at the repository root is still found.
        resolve(&repo, &cfg, &[]).unwrap();
        let homes = realm_home::scan(&cfg.join("workspaces")).unwrap();
        assert_eq!(
            find_workspace_above(&sub, None, &homes).unwrap(),
            Some(repo)
        );
    }

    #[test]
    fn discovery_never_returns_home_or_above() {
        let (_dir, root, cfg) = fixture();
        let home = root.join("home");
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        // Realms of the home directory and above it.
        resolve(&home, &cfg, &[]).unwrap();
        resolve(&root, &cfg, &[]).unwrap();
        let homes = realm_home::scan(&cfg.join("workspaces")).unwrap();
        assert!(
            find_workspace_above(&project, Some(&home), &homes)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            default_workspace(&project, Some(&home), &homes).unwrap(),
            project
        );
    }

    #[test]
    fn home_directory_is_refused_without_an_explicit_workspace() {
        let (_dir, home, cfg) = fixture();
        let err = default_workspace(&home, Some(&home), &[]).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to use your home directory"),
            "{err}"
        );
        // Above the home directory as well.
        let parent = home.parent().unwrap();
        assert!(default_workspace(parent, Some(&home), &[]).is_err());
        // An explicit --workspace is honoured.
        let scope = resolve(&home, &cfg, &[]).unwrap();
        assert_eq!(scope.context_root, home);
    }

    #[test]
    fn global_dirs_follow_flags() {
        let (_dir, ws, cfg) = fixture();
        let ws_arg = ws.to_str().unwrap();
        let scope = resolve(&ws, &cfg, &["--user-config-root", ws_arg]).unwrap();
        assert_eq!(
            scope.global,
            GlobalDirs {
                cache: cfg.join("cache"),
                config: cfg,
            }
        );

        let user = tempfile::tempdir().unwrap();
        let user_root = user.path().to_str().unwrap();
        let scope = resolve_scope(&args(&["-w", ws_arg, "--user-config-root", user_root])).unwrap();
        assert_eq!(scope.global.config, user.path().join(".rkat").join("tui"));
        assert!(scope.realm_root().starts_with(user.path()));
    }
}
