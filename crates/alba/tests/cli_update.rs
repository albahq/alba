//! End-to-end test for `alba update`.
//!
//! The update itself talks to GitHub and rewrites the running binary, so
//! it cannot run here; what can is the guard in front of it: with no
//! install receipt (a `cargo install`, a build from a checkout, this very
//! test binary), `alba update` must refuse clearly rather than try to
//! replace a binary the installers never wrote.
//!
//! The receipt lives under the user's configuration directory, so the
//! test points `axoupdater` at an empty temporary directory instead
//! (`AXOUPDATER_CONFIG_PATH`, its own override) rather than at the
//! developer's real home, where an installed `alba` may well have left a
//! receipt behind.

use predicates::prelude::*;

#[test]
fn update_without_a_receipt_refuses_and_points_at_cargo() {
    let config = tempfile::tempdir().unwrap();
    assert_cmd::Command::cargo_bin("alba")
        .unwrap()
        .arg("update")
        // An empty directory with no Beamfile: `update` must not even look
        // for one, so the failure reported is about the receipt alone.
        .current_dir(config.path())
        .env("AXOUPDATER_CONFIG_PATH", config.path())
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cargo install alba"))
        .stderr(predicates::str::contains("Beamfile").not());
}
