//! User-global `rkat-tui` directories.
//!
//! Everything the TUI keeps per user (rather than per workspace) lives in one
//! config directory: user-global `AGENTS.md` and `SYSTEM.md`, the global
//! `tui.toml` rules file, notes, the input history and the host trust
//! records.
//!
//! Resolution order (first match wins):
//!
//! 1. `--config-dir <dir>`
//! 2. `<user config root>/.rkat/tui` (created on first write), where the
//!    user config root is `--user-config-root`, else the home directory
//!    (the root Meerkat's own user config `~/.rkat` lives under)
//!
//! `$XDG_CONFIG_HOME` does not decide it.
//!
//! Re-creatable data (the community price list) lives in a cache directory
//! instead ([`GlobalDirs::cache`]):
//!
//! 1. `<--config-dir>/cache` (an explicit flag keeps everything in one place)
//! 2. `<--user-config-root>/.cache/rkat-tui`
//! 3. `$XDG_CACHE_HOME/rkat-tui` when set and non-empty
//! 4. `<platform cache dir>/rkat-tui` (`~/.cache` on Linux, `~/Library/Caches`
//!    on macOS)
//! 5. `<config>/cache` when no cache directory resolves

use std::path::{Path, PathBuf};

/// Name of the application directory inside a cache root.
pub const APP_DIR: &str = "rkat-tui";

/// The default config directory under the user config root: `.rkat/tui`.
pub const DEFAULT_CONFIG_DIR: [&str; 2] = [".rkat", "tui"];

/// User-global directories of `rkat-tui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalDirs {
    /// The user-global config directory.
    pub config: PathBuf,
    /// The user cache directory (re-creatable data).
    pub cache: PathBuf,
}

/// `<root>/.rkat/tui`.
#[must_use]
pub fn default_config_dir(root: &Path) -> PathBuf {
    DEFAULT_CONFIG_DIR
        .iter()
        .fold(root.to_path_buf(), |dir, part| dir.join(part))
}

impl GlobalDirs {
    /// Resolve the global config directory from the flags and the platform.
    ///
    /// `config_dir` is `--config-dir`; `user_config_root` is the raw
    /// `--user-config-root` flag (not the home-directory fallback). Returns
    /// `None` when nothing resolves.
    #[must_use]
    pub fn resolve(config_dir: Option<&Path>, user_config_root: Option<&Path>) -> Option<Self> {
        let home = dirs::home_dir();
        let mut resolved = Self::resolve_with(config_dir, user_config_root, home.as_deref())?;
        let xdg_cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
        let platform_cache = dirs::cache_dir();
        if let Some(cache) = cache_dir_with(
            config_dir,
            user_config_root,
            xdg_cache.as_deref(),
            platform_cache.as_deref(),
        ) {
            resolved.cache = cache;
        }
        Some(resolved)
    }

    /// [`GlobalDirs::resolve`] with the home directory supplied explicitly.
    #[must_use]
    pub fn resolve_with(
        config_dir: Option<&Path>,
        user_config_root: Option<&Path>,
        home: Option<&Path>,
    ) -> Option<Self> {
        let config = match config_dir {
            Some(dir) => dir.to_path_buf(),
            None => default_config_dir(user_config_root.or(home)?),
        };
        let cache = config.join("cache");
        Some(Self { config, cache })
    }

    /// Global approval rules file (`tui.toml`).
    #[must_use]
    pub fn rules_file(&self) -> PathBuf {
        self.config.join(crate::config::RULES_FILE_NAME)
    }

    /// User-global `AGENTS.md`.
    #[must_use]
    pub fn agents_md(&self) -> PathBuf {
        self.config.join("AGENTS.md")
    }

    /// User-global `SYSTEM.md` (replaces the base system prompt).
    #[must_use]
    pub fn system_md(&self) -> PathBuf {
        self.config.join("SYSTEM.md")
    }

    /// Directory of user-global notes.
    #[must_use]
    pub fn notes_dir(&self) -> PathBuf {
        self.config.join("notes")
    }

    /// Global input history file.
    #[must_use]
    pub fn history_file(&self) -> PathBuf {
        self.config.join("history")
    }

    /// Cache of the community price list (see [`crate::pricing`]), in the
    /// cache directory.
    #[must_use]
    pub fn pricing_cache(&self) -> PathBuf {
        self.cache.join(crate::pricing::CACHE_FILE)
    }
}

/// The cache directory (see the module docs), `None` when only the
/// `<config>/cache` fallback is left.
#[must_use]
pub fn cache_dir_with(
    config_dir: Option<&Path>,
    user_config_root: Option<&Path>,
    xdg_cache_home: Option<&Path>,
    platform_cache: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = config_dir {
        Some(dir.join("cache"))
    } else if let Some(root) = user_config_root {
        Some(root.join(".cache").join(APP_DIR))
    } else if let Some(xdg) = xdg_cache_home.filter(|p| !p.as_os_str().is_empty()) {
        Some(xdg.join(APP_DIR))
    } else {
        platform_cache.map(|dir| dir.join(APP_DIR))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    /// The config dir resolved.
    fn resolved(flag: Option<&str>, root: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        GlobalDirs::resolve_with(flag.map(p), root.map(p), home.map(p)).map(|d| d.config)
    }

    #[test]
    fn flag_wins_over_everything() {
        assert_eq!(
            resolved(Some("/f"), Some("/r"), Some("/h")),
            Some(PathBuf::from("/f"))
        );
    }

    #[test]
    fn the_default_is_under_the_user_config_root() {
        assert_eq!(
            resolved(None, None, Some("/h")),
            Some(PathBuf::from("/h/.rkat/tui"))
        );
        // `--user-config-root` replaces the home directory.
        assert_eq!(
            resolved(None, Some("/r"), Some("/h")),
            Some(PathBuf::from("/r/.rkat/tui"))
        );
        assert_eq!(resolved(None, None, None), None);
    }

    #[test]
    fn the_trust_records_move_with_the_config_dir() {
        // The record lives under the config dir; its key (workspace and
        // realm) does not depend on where that dir is.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let path = |dir: &Path| {
            crate::bootstrap::trust::HostTrustRecord::open(dir, ws.path(), "tui")
                .record_path()
                .map(Path::to_path_buf)
                .unwrap()
        };
        let (pa, pb) = (path(a.path()), path(b.path()));
        assert!(pa.starts_with(a.path()) && pb.starts_with(b.path()));
        assert_eq!(
            pa.strip_prefix(a.path()).unwrap(),
            pb.strip_prefix(b.path()).unwrap()
        );
    }

    #[test]
    fn accessors() {
        let dirs = GlobalDirs {
            config: PathBuf::from("/c"),
            cache: PathBuf::from("/k"),
        };
        assert_eq!(dirs.rules_file(), PathBuf::from("/c/tui.toml"));
        assert_eq!(dirs.agents_md(), PathBuf::from("/c/AGENTS.md"));
        assert_eq!(dirs.system_md(), PathBuf::from("/c/SYSTEM.md"));
        assert_eq!(dirs.notes_dir(), PathBuf::from("/c/notes"));
        assert_eq!(dirs.history_file(), PathBuf::from("/c/history"));
        assert_eq!(dirs.pricing_cache(), PathBuf::from("/k/pricing-cache.json"));
    }

    #[test]
    fn the_price_cache_lives_in_the_cache_dir() {
        let cache =
            |flag: Option<&str>, root: Option<&str>, xdg: Option<&str>, platform: Option<&str>| {
                cache_dir_with(flag.map(p), root.map(p), xdg.map(p), platform.map(p))
            };
        assert_eq!(
            cache(Some("/f"), Some("/r"), Some("/x"), Some("/p")),
            Some(PathBuf::from("/f/cache"))
        );
        assert_eq!(
            cache(None, Some("/r"), Some("/x"), Some("/p")),
            Some(PathBuf::from("/r/.cache/rkat-tui"))
        );
        assert_eq!(
            cache(None, None, Some("/x"), Some("/p")),
            Some(PathBuf::from("/x/rkat-tui"))
        );
        assert_eq!(
            cache(None, None, Some(""), Some("/p")),
            Some(PathBuf::from("/p/rkat-tui"))
        );
        assert_eq!(cache(None, None, None, None), None);
        // Never the config directory itself.
        let dirs = GlobalDirs::resolve(None, Some(p("/r"))).unwrap();
        assert_eq!(dirs.config, PathBuf::from("/r/.rkat/tui"));
        assert!(!dirs.pricing_cache().starts_with(&dirs.config));
    }
}
