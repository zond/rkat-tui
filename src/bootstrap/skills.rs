//! The user's own skills directory, `<config dir>/skills/`
//! ([`super::GlobalDirs::skills_dir`]): a skill source of every agent of
//! every workspace, ahead of the project's `.rkat/skills`.
//!
//! The engine (Meerkat 0.9.13, `meerkat_skills::resolve`) reads skills
//! from its embedded ones, the realm config's `[skills] repositories` in
//! order, and last the project's `<workspace>/.rkat/skills`; it has no
//! user-level directory. [`apply_user_skills`] is a load-time default of
//! the realm config store ([`super::RealmConfigStore`]), never written to
//! the document: when the directory exists and `[skills] enabled` is on,
//! it puts a filesystem repository for it first, under the fixed source
//! [`user_source`] named [`USER_SOURCE_NAME`]. A repository the user
//! already has for that directory is used instead of a second one.
//!
//! The engine keys a skill by its source and its name, so a skill of the
//! same name in two sources is two skills, both listed. So that a cloned
//! repository's `.rkat/skills` cannot stand in for one of the user's own
//! skills, each name the user's directory holds is remapped from the
//! project source to the user's (`[skills.identity] remaps`, under one
//! `rename_or_relocate` lineage event from the project source to the
//! user's, which the engine requires of a remap across sources): the
//! engine then lists only the user's copy, and a load of the project's
//! key serves the user's. A remap the user configured for that key stays
//! and wins.
//!
//! The directory is read with the engine's own filesystem source, on a
//! blocking thread, so the names and the counts `/context` shows are the
//! ones a session build lists.
//!
//! The inventory (`<available_skills>`) lists names and descriptions only
//! while there are at most `[skills] inventory_threshold` skills, 12 by
//! default, which the engine's 12 embedded skills already reach.
//! [`default_inventory_threshold`] is another load-time default: a realm
//! config that leaves the threshold at the engine's default gets
//! [`INVENTORY_BOUND`] instead, so the user's and the project's skills
//! are named too, up to a bound. Past it the inventory is counts per
//! source and the agent lists them with `skill_read`
//! ([`SkillCatalog`], `crate::tools::SkillToolDispatcher`).

use std::path::{Path, PathBuf};

use meerkat_core::Config;
use meerkat_core::skills::{
    SkillFilter, SkillKey, SkillKeyRemap, SkillName, SkillScope, SkillSource,
    SourceIdentityLineage, SourceIdentityLineageEvent, SourceUuid,
};
use meerkat_core::skills_config::{SkillRepoTransport, SkillRepositoryConfig};
use serde::{Deserialize, Serialize};

use super::GlobalDirs;

/// The inventory threshold a realm config that leaves it at the engine's
/// default gets: the engine's 12 embedded skills and 20 of the user's and
/// the project's. A listed skill costs every request about 120 bytes plus
/// its name and description (`docs/evidence.md`, "The agent opens a
/// skill"); 20 skills of one-line descriptions stay under 5 KB.
pub const INVENTORY_BOUND: usize = 32;

/// Raise `[skills] inventory_threshold` to [`INVENTORY_BOUND`] when the
/// config has the engine's default (the engine merges config layers the
/// same way: a value equal to the default counts as unset). Never written.
pub fn default_inventory_threshold(config: &mut Config) {
    let default = meerkat_core::skills_config::SkillsConfig::default().inventory_threshold;
    if config.skills.inventory_threshold == default {
        config.skills.inventory_threshold = INVENTORY_BOUND.max(default);
    }
}

/// The skill runtime a session build of the daemon makes, for the
/// agents' `skill_read` and `/context`: the daemon's factory over the
/// realm config as its store serves it now (load-time defaults applied).
pub struct SkillCatalog {
    factory: meerkat::AgentFactory,
    config: std::sync::Arc<super::RealmConfigStore>,
}

impl SkillCatalog {
    /// The catalog of `factory` (a clone of the daemon's) over `config`.
    #[must_use]
    pub fn new(
        factory: meerkat::AgentFactory,
        config: std::sync::Arc<super::RealmConfigStore>,
    ) -> Self {
        Self { factory, config }
    }

    /// The realm config as a session build reads it.
    ///
    /// # Errors
    ///
    /// The store's error, in words.
    pub async fn config(&self) -> Result<Config, String> {
        meerkat_core::ConfigStore::get(&*self.config)
            .await
            .map_err(|err| format!("the realm config could not be read: {err}"))
    }
}

impl std::fmt::Debug for SkillCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillCatalog")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl crate::tools::SkillHost for SkillCatalog {
    async fn runtime(
        &self,
    ) -> Result<Option<std::sync::Arc<meerkat_core::skills::SkillRuntime>>, String> {
        let config = self.config().await?;
        self.factory
            .build_skill_runtime(&config)
            .await
            .map_err(|err| format!("the skills could not be read: {err}"))
    }
}

/// The directory's name inside the config directory.
pub const USER_SKILLS_DIR: &str = "skills";

/// The repository name of the user's directory (the engine's source
/// name, as the embedded skills are `embedded` and the project's
/// `project`).
pub const USER_SOURCE_NAME: &str = "user";

/// The id of the lineage event that relates the project source to the
/// user's (see the module docs).
const LINEAGE_EVENT: &str = "rkat-tui-user-skills";

/// The source id of the user's directory: fixed, so a skill's key
/// (`<source>/<name>`) stays the same across starts and workspaces.
#[must_use]
pub fn user_source() -> SourceUuid {
    SourceUuid::from_uuid(uuid::Uuid::from_u128(
        0x6f1c_2a9e_4b37_4d05_9c1e_7a2b_5d8f_3e01,
    ))
}

/// The valid skills of the directory `dir`, by name, as the engine's
/// filesystem source lists them (none when it is missing). Blocking: the
/// filesystem source does its I/O synchronously inside a future that
/// needs no tokio runtime, so a plain executor runs it (a whole skill
/// runtime, whose git and HTTP sources do need tokio, goes through
/// `crate::tools::with_skill_runtime` instead).
fn listed(dir: &Path) -> Vec<SkillName> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let fs = meerkat_skills::FilesystemSkillSource::new(dir, SkillScope::User);
    futures::executor::block_on(fs.list(&SkillFilter::default()))
        .map(|skills| skills.into_iter().map(|s| s.key.skill_name).collect())
        .unwrap_or_default()
}

/// Whether the filesystem repository `repo` reads `dir`.
fn reads(repo: &SkillRepositoryConfig, dir: &Path) -> bool {
    let SkillRepoTransport::Filesystem { path } = &repo.transport else {
        return false;
    };
    let path = Path::new(path);
    path == dir
        || matches!(
            (std::fs::canonicalize(path), std::fs::canonicalize(dir)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// What [`apply_user_skills`] adds, worked out on a blocking thread.
struct Plan {
    /// The repository to put first (`None`: the user has one for the
    /// directory, or one under [`user_source`]).
    repository: Option<SkillRepositoryConfig>,
    /// The source the user's skills are listed under.
    source: SourceUuid,
    /// The user's skill names.
    names: Vec<SkillName>,
}

fn plan(repositories: &[SkillRepositoryConfig], dir: PathBuf) -> Option<Plan> {
    if !dir.is_dir() {
        return None;
    }
    if let Some(own) = repositories.iter().find(|repo| reads(repo, &dir)) {
        let names = listed(&dir);
        return Some(Plan {
            repository: None,
            source: own.source_uuid.clone(),
            names,
        });
    }
    // A repository of the user's under the fixed id is theirs: a second
    // record of that id would be a source collision.
    if repositories
        .iter()
        .any(|repo| repo.source_uuid == user_source())
    {
        return None;
    }
    Some(Plan {
        repository: Some(SkillRepositoryConfig {
            name: USER_SOURCE_NAME.to_string(),
            source_uuid: user_source(),
            transport: SkillRepoTransport::Filesystem {
                path: dir.to_string_lossy().into_owned(),
            },
        }),
        source: user_source(),
        names: listed(&dir),
    })
}

/// Give `config` the user's skills directory as its first skill
/// repository, and the user's names precedence over the project's (see
/// the module docs). Nothing when `[skills] enabled` is off, there is no
/// config directory, or the directory does not exist.
pub async fn apply_user_skills(config: &mut Config, global: Option<&GlobalDirs>) {
    let Some(global) = global else {
        return;
    };
    if !config.skills.enabled {
        return;
    }
    let dir = global.skills_dir();
    let repositories = config.skills.repositories.clone();
    let Ok(Some(plan)) = tokio::task::spawn_blocking(move || plan(&repositories, dir)).await else {
        return;
    };
    let skills = &mut config.skills;
    if let Some(repository) = plan.repository {
        skills.repositories.insert(0, repository);
    }
    let project = SourceUuid::project_local();
    let mut remapped = false;
    for name in plan.names {
        let from = SkillKey::new(project.clone(), name.clone());
        if skills
            .identity
            .remaps
            .iter()
            .any(|remap| remap.from == from)
        {
            continue;
        }
        skills.identity.remaps.push(SkillKeyRemap {
            from,
            to: SkillKey::new(plan.source.clone(), name),
            reason: Some("the user's own skill of this name".to_string()),
        });
        remapped = true;
    }
    if remapped {
        skills.identity.lineage.push(SourceIdentityLineage {
            event_id: LINEAGE_EVENT.to_string(),
            recorded_at_unix_secs: 0,
            required_from_skills: Vec::new(),
            event: SourceIdentityLineageEvent::RenameOrRelocate {
                from: project,
                to: plan.source,
            },
        });
    }
}

/// Where a skills directory `/context` names is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPlace {
    /// The user's own, `<config dir>/skills`.
    User,
    /// The project's `.rkat/skills`.
    Project,
}

/// A skills directory and the skills a session build lists from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDirectory {
    /// Whose it is.
    pub place: SkillPlace,
    /// Its path.
    pub path: PathBuf,
    /// Its valid skills (`None`: the directory does not exist).
    pub skills: Option<usize>,
}

/// The skills directories of a session build, for `/context`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum SkillsReport {
    /// `[skills] enabled = false`: no session loads a skill.
    Off,
    /// The user's directory (always named), then the project's when it
    /// exists.
    On {
        /// The directories.
        directories: Vec<SkillDirectory>,
        /// What the inventory of a session built now lists (`None`: the
        /// skills could not be read).
        inventory: Option<Inventory>,
    },
}

/// The size of a session's skill inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    /// The skills it covers, the embedded ones included.
    pub skills: usize,
    /// `[skills] inventory_threshold`: up to this many it names each
    /// skill, above it only counts per source.
    pub threshold: usize,
}

/// The skills directories `config` (as the realm config store serves it)
/// builds with, for the workspace `context_root`. Blocking.
#[must_use]
pub fn report(config: &Config, global: Option<&GlobalDirs>, context_root: &Path) -> SkillsReport {
    if !config.skills.enabled {
        return SkillsReport::Off;
    }
    let count = |dir: &Path| dir.is_dir().then(|| listed(dir).len());
    let mut directories = Vec::new();
    if let Some(global) = global {
        let path = global.skills_dir();
        directories.push(SkillDirectory {
            place: SkillPlace::User,
            skills: count(&path),
            path,
        });
    }
    let project = context_root.join(".rkat").join("skills");
    if project.is_dir() {
        directories.push(SkillDirectory {
            place: SkillPlace::Project,
            skills: count(&project),
            path: project,
        });
    }
    SkillsReport::On {
        directories,
        inventory: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str) {
        let dir = dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
        )
        .unwrap();
    }

    fn global(root: &Path) -> GlobalDirs {
        GlobalDirs::resolve_with(Some(root), None, None).unwrap()
    }

    #[tokio::test]
    async fn the_directory_comes_first_and_its_names_win_over_the_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let global = global(tmp.path());
        skill(&global.skills_dir(), "review");
        // Not a skill: no SKILL.md.
        std::fs::create_dir_all(global.skills_dir().join("notes")).unwrap();
        let mut config = Config::default();
        apply_user_skills(&mut config, Some(&global)).await;
        let skills = &config.skills;
        assert_eq!(skills.repositories.len(), 1);
        assert_eq!(skills.repositories[0].name, USER_SOURCE_NAME);
        assert_eq!(skills.repositories[0].source_uuid, user_source());
        assert_eq!(skills.identity.remaps.len(), 1);
        let remap = &skills.identity.remaps[0];
        assert_eq!(remap.from.source_uuid, SourceUuid::project_local());
        assert_eq!(remap.to.source_uuid, user_source());
        assert_eq!(remap.to.skill_name.as_str(), "review");
        // The engine accepts the remap.
        let registry = skills.build_source_identity_registry().unwrap();
        let resolved = registry.resolve(&remap.from).unwrap();
        assert_eq!(resolved.key, remap.to);
    }

    #[tokio::test]
    async fn nothing_without_the_directory_or_with_skills_off() {
        let tmp = tempfile::tempdir().unwrap();
        let global = global(tmp.path());
        let mut config = Config::default();
        apply_user_skills(&mut config, Some(&global)).await;
        assert!(config.skills.repositories.is_empty());
        skill(&global.skills_dir(), "review");
        config.skills.enabled = false;
        apply_user_skills(&mut config, Some(&global)).await;
        assert!(config.skills.repositories.is_empty());
        assert!(config.skills.identity.remaps.is_empty());
        assert_eq!(
            report(&config, Some(&global), tmp.path()),
            SkillsReport::Off
        );
    }

    /// A repository the user has for the directory is used, not doubled;
    /// a remap the user wrote for a name stays.
    #[tokio::test]
    async fn the_users_own_repository_and_remap_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let global = global(tmp.path());
        skill(&global.skills_dir(), "review");
        skill(&global.skills_dir(), "deploy");
        let own = SourceUuid::parse("11111111-2222-4333-8444-555555555555").unwrap();
        let mut config = Config::default();
        config.skills.repositories.push(SkillRepositoryConfig {
            name: "mine".into(),
            source_uuid: own.clone(),
            transport: SkillRepoTransport::Filesystem {
                path: global.skills_dir().to_string_lossy().into_owned(),
            },
        });
        let project_deploy = SkillKey::new(
            SourceUuid::project_local(),
            SkillName::parse("deploy").unwrap(),
        );
        config.skills.identity.remaps.push(SkillKeyRemap {
            from: project_deploy.clone(),
            to: SkillKey::new(SourceUuid::project_local(), SkillName::parse("x").unwrap()),
            reason: None,
        });
        apply_user_skills(&mut config, Some(&global)).await;
        let skills = &config.skills;
        assert_eq!(skills.repositories.len(), 1);
        assert_eq!(skills.repositories[0].name, "mine");
        assert_eq!(skills.identity.remaps.len(), 2);
        let review = skills
            .identity
            .remaps
            .iter()
            .find(|remap| remap.from.skill_name.as_str() == "review")
            .unwrap();
        assert_eq!(review.to.source_uuid, own);
        assert!(
            skills
                .identity
                .remaps
                .iter()
                .any(|remap| remap.from == project_deploy && remap.to.skill_name.as_str() == "x")
        );
        skills.build_source_identity_registry().unwrap();
    }

    /// The engine's default threshold becomes the bound; a threshold the
    /// realm config sets stays.
    #[test]
    fn the_inventory_threshold_default_is_raised_to_the_bound() {
        let mut config = Config::default();
        default_inventory_threshold(&mut config);
        assert_eq!(config.skills.inventory_threshold, INVENTORY_BOUND);
        config.skills.inventory_threshold = 5;
        default_inventory_threshold(&mut config);
        assert_eq!(config.skills.inventory_threshold, 5);
    }

    #[test]
    fn the_report_counts_the_valid_skills_of_each_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let global = global(&tmp.path().join("cfg"));
        let workspace = tmp.path().join("w");
        let config = Config::default();
        assert_eq!(
            report(&config, Some(&global), &workspace),
            SkillsReport::On {
                directories: vec![SkillDirectory {
                    place: SkillPlace::User,
                    path: global.skills_dir(),
                    skills: None,
                }],
                inventory: None,
            }
        );
        skill(&global.skills_dir(), "review");
        skill(&global.skills_dir(), "deploy");
        // A SKILL.md whose name is not its directory's is not a skill.
        let bad = global.skills_dir().join("other");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: else\ndescription: d\n---\n",
        )
        .unwrap();
        skill(&workspace.join(".rkat/skills"), "review");
        let SkillsReport::On { directories, .. } = report(&config, Some(&global), &workspace)
        else {
            panic!("skills are on");
        };
        assert_eq!(directories[0].skills, Some(2));
        assert_eq!(directories[1].place, SkillPlace::Project);
        assert_eq!(directories[1].skills, Some(1));
    }
}
