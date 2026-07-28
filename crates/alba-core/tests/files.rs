//! Behaviour tests for `expand_globs` and `outputs_satisfied`: the
//! filesystem half of the cache's fingerprinting, exercised against real
//! temporary directories.

use std::path::Path;

use alba_core::{expand_globs, outputs_satisfied};

fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn relative_paths(dir: &Path, patterns: &[&str]) -> Vec<String> {
    let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
    expand_globs(dir, &patterns)
        .into_iter()
        .map(|(relative, _)| relative)
        .collect()
}

#[test]
fn matches_files_by_glob_and_returns_them_sorted() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/b.rs", "b");
    write(dir.path(), "src/a.rs", "a");
    write(dir.path(), "src/nested/c.rs", "c");
    write(dir.path(), "readme.md", "docs");

    assert_eq!(
        relative_paths(dir.path(), &["src/**/*.rs"]),
        vec!["src/a.rs", "src/b.rs", "src/nested/c.rs"]
    );
}

#[test]
fn a_literal_pattern_matches_exactly_one_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Cargo.toml", "[package]");
    write(dir.path(), "Cargo.lock", "lock");

    assert_eq!(
        relative_paths(dir.path(), &["Cargo.toml"]),
        vec!["Cargo.toml"]
    );
}

#[test]
fn respects_gitignore_even_outside_a_git_repository() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "generated/\n");
    write(dir.path(), "src/a.rs", "a");
    write(dir.path(), "generated/b.rs", "b");

    assert_eq!(relative_paths(dir.path(), &["**/*.rs"]), vec!["src/a.rs"]);
}

#[test]
fn never_walks_git_or_alba_directories() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".git/objects/x.rs", "not source");
    write(dir.path(), ".alba/cache/y.rs", "not source");
    write(dir.path(), "src/a.rs", "a");

    assert_eq!(relative_paths(dir.path(), &["**/*.rs"]), vec!["src/a.rs"]);
}

#[test]
fn hidden_files_are_still_matched() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "SECRET=1");

    assert_eq!(relative_paths(dir.path(), &[".env"]), vec![".env"]);
}

#[test]
fn a_pattern_matching_nothing_yields_an_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.txt", "a");

    assert_eq!(relative_paths(dir.path(), &["*.rs"]), Vec::<String>::new());
}

#[test]
fn outputs_are_satisfied_by_literal_existence_and_glob_matches() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "target/release/app", "binary");
    write(dir.path(), "dist/bundle.js", "js");

    let satisfied = |patterns: &[&str]| {
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        outputs_satisfied(dir.path(), &patterns)
    };

    assert!(satisfied(&["target/release/app"]));
    assert!(satisfied(&["dist/*.js"]));
    assert!(satisfied(&[]), "no declared outputs means nothing to check");
    assert!(!satisfied(&["target/release/missing"]));
    assert!(!satisfied(&["dist/*.css"]));
}

#[test]
fn outputs_check_ignores_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "target/\n");
    write(dir.path(), "target/release/app", "binary");

    let patterns = vec!["target/*/app".to_string()];
    assert!(
        outputs_satisfied(dir.path(), &patterns),
        "outputs live in ignored directories; the check must query the raw disk"
    );
}
