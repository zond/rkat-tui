//! `CLAUDE.md`'s layout table keeps up with `src/`: every top-level module
//! file and every module directory is named in it (the table drifted after
//! every phase until this test). A nested directory may be named as
//! `src/<dir>/<sub>/` or as `` `<sub>/` `` in its parent's row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

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
