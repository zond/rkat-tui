//! Records the build id as `RKAT_TUI_BUILD` (`rkat_tui::BUILD`, which
//! `--version` prints and a client compares with its daemon's): the crate
//! version plus the git commit (`0.0.1+0123456789`) when the crate is built
//! from its own checkout, the version alone otherwise (a crates.io build,
//! whose sources sit in the registry and may be inside some other
//! repository, such as a home directory kept in git).

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // Only this crate's own checkout: the work tree's top is the manifest
    // directory, and a packaged crate (it carries `.cargo_vcs_info.json`)
    // never counts, wherever it was unpacked.
    let own_checkout = !manifest.join(".cargo_vcs_info.json").exists()
        && git(&manifest, &["rev-parse", "--show-toplevel"])
            .and_then(|top| PathBuf::from(top).canonicalize().ok())
            .zip(manifest.canonicalize().ok())
            .is_some_and(|(top, manifest)| top == manifest);
    let hash = own_checkout
        .then(|| git(&manifest, &["rev-parse", "--short=10", "HEAD"]))
        .flatten();
    match hash {
        Some(hash) => {
            println!("cargo:rustc-env=RKAT_TUI_BUILD={version}+{hash}");
            // A commit moves HEAD's ref (or HEAD itself, detached). In a
            // linked worktree HEAD is the worktree's, the refs are shared.
            let absolute = |flag: &str| {
                git(&manifest, &["rev-parse", "--path-format=absolute", flag]).map(PathBuf::from)
            };
            if let (Some(git_dir), Some(common)) =
                (absolute("--git-dir"), absolute("--git-common-dir"))
            {
                let head = git_dir.join("HEAD");
                println!("cargo:rerun-if-changed={}", head.display());
                println!(
                    "cargo:rerun-if-changed={}",
                    common.join("packed-refs").display()
                );
                if let Ok(text) = std::fs::read_to_string(&head)
                    && let Some(reference) = text.strip_prefix("ref: ")
                {
                    println!(
                        "cargo:rerun-if-changed={}",
                        common.join(reference.trim()).display()
                    );
                }
            }
        }
        None => {
            println!("cargo:rustc-env=RKAT_TUI_BUILD={version}");
            println!("cargo:rerun-if-changed=build.rs");
        }
    }
}
