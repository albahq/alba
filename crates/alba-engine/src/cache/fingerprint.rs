//! Fingerprinting: one blake3 hash per beam over everything that must
//! invalidate its cache entry. Every item is length-prefixed before it is
//! fed to the hasher, so adjacent strings cannot collide by concatenation
//! (`["ab"]` versus `["a", "b"]`), and every section is preceded by its
//! name so an empty section still leaves a trace.

use std::io;
use std::path::Path;

/// Everything that participates in a beam's fingerprint. `files` is the
/// sorted `(relative path, content hash)` list `expand_globs` + [`hash_file`]
/// produce; `cwd` is the rendered directory the commands run in; `needs` is
/// each dependency's contribution, in `needs` order.
pub(crate) struct BeamFacts<'a> {
    pub files: &'a [(String, String)],
    pub commands: &'a [String],
    pub cwd: &'a str,
    pub env: &'a [(String, String)],
    pub args: &'a [String],
    pub needs: &'a [String],
    /// Which executor this beam dispatches to, and its full configuration
    /// (`"embedded"`, `"system"`, or a docker/plugin label carrying its
    /// image, volumes, or options): a beam replayed under a different
    /// executor or configuration must not be mistaken for a hit computed
    /// under another one.
    pub executor: &'a str,
}

/// The blake3 hex fingerprint of `facts`. Any change to how this feeds
/// the hasher must bump `store::FORMAT_VERSION`: a manifest written under
/// the old recipe would be compared against a hash the new one produces,
/// and the comparison would be silently meaningless.
pub(crate) fn fingerprint(facts: &BeamFacts<'_>) -> String {
    let mut hasher = blake3::Hasher::new();

    item(&mut hasher, "files");
    for (path, hash) in facts.files {
        item(&mut hasher, path);
        item(&mut hasher, hash);
    }
    item(&mut hasher, "commands");
    for command in facts.commands {
        item(&mut hasher, command);
    }
    item(&mut hasher, "executor");
    item(&mut hasher, facts.executor);
    // Right after the commands and the executor that runs them: where
    // they run completes what runs, and the same command in another
    // directory is another invocation.
    item(&mut hasher, "cwd");
    item(&mut hasher, facts.cwd);
    item(&mut hasher, "env");
    for (name, value) in facts.env {
        item(&mut hasher, name);
        item(&mut hasher, value);
    }
    item(&mut hasher, "args");
    for arg in facts.args {
        item(&mut hasher, arg);
    }
    item(&mut hasher, "needs");
    for need in facts.needs {
        item(&mut hasher, need);
    }

    hasher.finalize().to_hex().to_string()
}

/// What a beam that cannot be cached (no declared `inputs`) contributes to
/// its dependents' fingerprints: its static parts only — what it runs,
/// where, and with what. This keeps a non-cacheable dependency from
/// poisoning the cascade — if its actual output changes, the dependent's
/// own `inputs` catch that by content.
pub(crate) fn static_contribution(
    commands: &[String],
    cwd: &str,
    env: &[(String, String)],
    args: &[String],
    executor: &str,
) -> String {
    fingerprint(&BeamFacts {
        files: &[],
        commands,
        cwd,
        env,
        args,
        needs: &[],
        executor,
    })
}

/// The blake3 hex hash of a file's content. Reads the whole file: inputs
/// are source files, and blake3 hashes gigabytes per second — streaming
/// would be complexity without a case that needs it yet.
pub(crate) fn hash_file(path: &Path) -> io::Result<String> {
    Ok(blake3::hash(&std::fs::read(path)?).to_hex().to_string())
}

fn item(hasher: &mut blake3::Hasher, text: &str) {
    hasher.update(&(text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn facts_fingerprint(
        files: &[(&str, &str)],
        commands: &[&str],
        cwd: &str,
        env: &[(&str, &str)],
        args: &[&str],
        needs: &[&str],
        executor: &'static str,
    ) -> String {
        fingerprint(&BeamFacts {
            files: &pairs(files),
            commands: &strings(commands),
            cwd,
            env: &pairs(env),
            args: &strings(args),
            needs: &strings(needs),
            executor,
        })
    }

    #[test]
    fn identical_facts_produce_identical_fingerprints() {
        let a = facts_fingerprint(
            &[("src/a.rs", "h1")],
            &["build"],
            "/p",
            &[("K", "v")],
            &[],
            &[],
            "embedded",
        );
        let b = facts_fingerprint(
            &[("src/a.rs", "h1")],
            &["build"],
            "/p",
            &[("K", "v")],
            &[],
            &[],
            "embedded",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn every_field_participates_in_the_fingerprint() {
        let base = facts_fingerprint(
            &[("a", "h1")],
            &["cmd"],
            "/p",
            &[("K", "v")],
            &["arg"],
            &["n1"],
            "embedded",
        );

        let variants = [
            facts_fingerprint(
                &[("a", "h2")],
                &["cmd"],
                "/p",
                &[("K", "v")],
                &["arg"],
                &["n1"],
                "embedded",
            ),
            facts_fingerprint(
                &[("b", "h1")],
                &["cmd"],
                "/p",
                &[("K", "v")],
                &["arg"],
                &["n1"],
                "embedded",
            ),
            facts_fingerprint(
                &[("a", "h1")],
                &["cmd2"],
                "/p",
                &[("K", "v")],
                &["arg"],
                &["n1"],
                "embedded",
            ),
            facts_fingerprint(
                &[("a", "h1")],
                &["cmd"],
                "/q",
                &[("K", "v")],
                &["arg"],
                &["n1"],
                "embedded",
            ),
            facts_fingerprint(
                &[("a", "h1")],
                &["cmd"],
                "/p",
                &[("K", "w")],
                &["arg"],
                &["n1"],
                "embedded",
            ),
            facts_fingerprint(
                &[("a", "h1")],
                &["cmd"],
                "/p",
                &[("K", "v")],
                &["other"],
                &["n1"],
                "embedded",
            ),
            facts_fingerprint(
                &[("a", "h1")],
                &["cmd"],
                "/p",
                &[("K", "v")],
                &["arg"],
                &["n2"],
                "embedded",
            ),
            facts_fingerprint(
                &[("a", "h1")],
                &["cmd"],
                "/p",
                &[("K", "v")],
                &["arg"],
                &["n1"],
                "system",
            ),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    /// Length-prefixing is what makes `["ab"]` and `["a", "b"]` distinct;
    /// plain concatenation would collide them.
    #[test]
    fn adjacent_items_cannot_collide_by_concatenation() {
        let joined = facts_fingerprint(&[], &["ab"], "/p", &[], &[], &[], "embedded");
        let split = facts_fingerprint(&[], &["a", "b"], "/p", &[], &[], &[], "embedded");
        assert_ne!(joined, split);
    }

    #[test]
    fn the_static_contribution_ignores_files_and_needs() {
        let contribution = static_contribution(
            &strings(&["cmd"]),
            "/p",
            &pairs(&[("K", "v")]),
            &[],
            "embedded",
        );
        assert_eq!(
            contribution,
            facts_fingerprint(&[], &["cmd"], "/p", &[("K", "v")], &[], &[], "embedded")
        );
    }

    #[test]
    fn hash_file_reflects_content_not_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");

        std::fs::write(&path, "v1").unwrap();
        let first = hash_file(&path).unwrap();
        std::fs::write(&path, "v2").unwrap();
        let second = hash_file(&path).unwrap();
        std::fs::write(&path, "v1").unwrap();
        let third = hash_file(&path).unwrap();

        assert_ne!(first, second);
        assert_eq!(first, third);
    }

    #[test]
    fn hash_file_reports_a_missing_file_as_an_error() {
        assert!(hash_file(std::path::Path::new("/does/not/exist")).is_err());
    }
}
