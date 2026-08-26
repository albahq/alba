//! Snapshot test parsing an example Beamfile that exercises every piece
//! of the DSL's grammar at once.

use alba_syntax::parse;

const EXAMPLE: &str = r#"version "1"

import "api/Beamfile" as api

let profile = env("PROFILE", "debug")
let release = profile == "release"

default build

beam build {
  description "Compile the workspace"
  needs [api:build, codegen]
  inputs ["src/**/*.rs", "Cargo.toml"]
  outputs ["target/{profile}/app"]
  run "cargo build {if release then '--release' else ''}"
}

beam test {
  needs [build]
  run "cargo test"
}

beam lint {
  allow_failure true
  run "cargo clippy -- -D warnings"
}

beam deploy(target) {
  description "Deploy to an environment"
  needs [test]
  executor docker { image "deployer:latest" }
  env { DEPLOY_TARGET = target }
  run "./scripts/deploy.sh {target}"
}
"#;

#[test]
fn parses_an_example_using_every_construct() {
    insta::assert_debug_snapshot!(parse(EXAMPLE).unwrap());
}

#[test]
fn executor_options_accept_bool_and_list_values() {
    let src = r#"beam b {
  executor podman { image "quay.io/x" remote true volumes ["a:/b", "c:/d"] }
  run "x"
}"#;
    let file = parse(src).unwrap();
    insta::assert_debug_snapshot!(file.beams[0].executor.as_ref().unwrap());
}

#[test]
fn executor_option_list_reports_a_malformed_entry() {
    let src = r#"beam b {
  executor docker { volumes ["a:/b", true] }
  run "x"
}"#;
    let err = parse(src).unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn a_repeated_plugin_executor_option_is_rejected() {
    let src = r#"beam b {
  executor podman { flag "a" flag "b" }
  run "x"
}"#;
    let err = parse(src).unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn a_repeated_docker_executor_option_is_rejected() {
    let src = r#"beam b {
  executor docker { image "a" image "b" }
  run "x"
}"#;
    let err = parse(src).unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn git_fields_parse_as_member_access() {
    let src = r#"let tag = git.short_sha + "-" + git.branch
beam b { run "echo {git.sha} {if git.dirty then 'dirty' else 'clean'}" }"#;
    let file = parse(src).unwrap();
    insta::assert_debug_snapshot!((&file.lets, &file.beams[0].run));
}

#[test]
fn hooks_parse_with_hyphenated_names_and_namespaced_targets() {
    let src = r#"hook pre-commit { beam check }
hook commit-msg { beam api:check_message }
beam check { run "x" }"#;
    let file = parse(src).unwrap();
    insta::assert_debug_snapshot!(file.hooks);
}

#[test]
fn a_hook_without_a_beam_is_rejected() {
    let err = parse("hook pre-commit { }").unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn a_hook_with_an_unknown_field_is_rejected() {
    let err = parse("hook pre-commit { needs [a] }").unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn a_hook_with_a_repeated_beam_field_is_rejected() {
    let err = parse("hook pre-commit { beam a beam b }").unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn git_is_reserved_as_a_let_name_and_a_parameter_name() {
    let err = parse("let git = \"x\"").unwrap_err();
    insta::assert_debug_snapshot!("let_git", err);
    let err = parse("beam b(git) { run \"x\" }").unwrap_err();
    insta::assert_debug_snapshot!("param_git", err);
}

#[test]
fn a_field_needs_an_identifier_after_the_dot() {
    let err = parse("let x = git.").unwrap_err();
    insta::assert_debug_snapshot!(err);
}
