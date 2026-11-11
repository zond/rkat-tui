//! `CLAUDE.md`'s layout table keeps up with `src/`: every top-level module
//! file and every module directory is named in it (the table drifted after
//! every phase until this test). A nested directory may be named as
//! `src/<dir>/<sub>/` or as `` `<sub>/` `` in its parent's row. The
//! wrappers of engine traits forward every method of them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[test]
fn the_layout_table_names_every_module_directory_and_top_level_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    let table: String = claude
        .lines()
        .filter(|line| line.starts_with("| `"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if path.is_dir() {
            if !table.contains(&format!("`src/{name}/")) {
                missing.push(format!("src/{name}/"));
            }
            for sub in std::fs::read_dir(&path).unwrap() {
                let sub = sub.unwrap().path();
                if sub.join("mod.rs").exists() {
                    let sub = sub.file_name().unwrap().to_str().unwrap();
                    if !table.contains(&format!("`src/{name}/{sub}/`"))
                        && !table.contains(&format!("`{name}/{sub}/`"))
                        && !table.contains(&format!("`{sub}/`"))
                    {
                        missing.push(format!("src/{name}/{sub}/"));
                    }
                }
            }
        } else if name.ends_with(".rs")
            && name != "lib.rs"
            && !table.contains(&format!("`src/{name}`"))
        {
            missing.push(format!("src/{name}"));
        }
    }
    assert!(
        missing.is_empty(),
        "CLAUDE.md's layout table does not name: {missing:?}"
    );
}

/// The engine's source of the package `name` at the version this build
/// uses (`cargo metadata`, so neither the registry's path nor the version
/// is written here).
fn package_dir(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--manifest-path",
        ])
        .arg(root.join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let manifest = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == name)
        .and_then(|package| package["manifest_path"].as_str())
        .unwrap_or_else(|| panic!("no package {name}"));
    Path::new(manifest).parent().unwrap().to_path_buf()
}

/// The text of the first `.rs` file under `dir` that contains `needle`.
fn source_with(dir: &Path, needle: &str) -> Option<String> {
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                if text.contains(needle) {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// The names of the functions declared directly in the block that opens
/// on the line holding `header` (a trait or an impl), not those nested in
/// their bodies. Braces in comments, strings and character literals are
/// not counted.
fn methods(source: &str, header: &str) -> BTreeSet<String> {
    let start = source
        .find(header)
        .unwrap_or_else(|| panic!("no `{header}`"));
    let mut depth = 0usize;
    let mut names = BTreeSet::new();
    for line in source[start..].lines() {
        let code = line.split("//").next().unwrap_or("");
        let declared = code
            .trim_start()
            .trim_start_matches("pub ")
            .trim_start_matches("async ")
            .strip_prefix("fn ");
        if depth == 1
            && let Some(rest) = declared
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            names.insert(name);
        }
        let mut chars = code.chars().peekable();
        let mut in_string = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' if in_string => {
                    chars.next();
                }
                '"' => in_string = !in_string,
                '\'' if !in_string => {
                    // A character literal (`'{'`); a lifetime has no
                    // closing quote two characters on.
                    let mut ahead = chars.clone();
                    if ahead.next().is_some() && ahead.next() == Some('\'') {
                        chars.next();
                        chars.next();
                    }
                }
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return names;
                    }
                }
                _ => {}
            }
        }
    }
    panic!("`{header}` does not close")
}

/// The wrappers that forward an engine trait to an inner implementation
/// override every method of it: a method the engine adds later with a
/// default would otherwise be answered by that default, silently, instead
/// of by the wrapped implementation. When this fails after an engine
/// upgrade, forward the new method (and look at what it does to the
/// wrapper's purpose).
#[test]
fn forwarding_wrappers_override_every_method_of_their_engine_trait() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let core = package_dir("meerkat-core");
    for (header, file, implementation) in [
        (
            "pub trait AgentToolDispatcher",
            "src/daemon/deferral/mod.rs",
            "impl AgentToolDispatcher for Hiding",
        ),
        (
            "pub trait ToolDispatchAdmission",
            "src/mobs/council.rs",
            "impl ToolDispatchAdmission for Convening",
        ),
        (
            "pub trait ToolDispatchAdmission",
            "src/daemon/approvals.rs",
            "impl ToolDispatchAdmission for RootGate",
        ),
    ] {
        let engine = source_with(&core.join("src"), header)
            .unwrap_or_else(|| panic!("meerkat-core has no `{header}`"));
        let declared = methods(&engine, header);
        assert!(declared.len() > 1, "{header}: {declared:?}");
        let ours = std::fs::read_to_string(root.join(file)).unwrap();
        let forwarded = methods(&ours, implementation);
        let missing: Vec<_> = declared.difference(&forwarded).collect();
        assert!(
            missing.is_empty(),
            "{implementation} ({file}) does not forward {missing:?}"
        );
    }
}

/// The daemon does not depend on the UI: what both need lives in a
/// module of its own (`crate::cost`, ...). Its tests may use the UI.
#[test]
fn the_daemon_does_not_depend_on_the_ui() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut dirs = vec![root.join("src/daemon")];
    let mut found = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let code = text.split("mod tests {").next().unwrap_or_default();
            for (n, line) in code.lines().enumerate() {
                if line.contains("crate::ui") && !line.trim_start().starts_with("//") {
                    found.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
    }
    assert!(found.is_empty(), "the daemon uses the UI at {found:?}");
}
