//! `match` expressions: evaluated through `load_str`, read back from a
//! beam's rendered `description` or its `run` template.

use alba_core::{CoreError, Project, load_str, render_template};
use alba_syntax::Span;

/// Loads a Beamfile whose beam `b` describes itself with `{t}`, the `let`
/// under test, and returns that rendered description.
fn described(lets: &str) -> String {
    let source = format!("{lets}\nbeam b {{ description \"{{t}}\" run \"x\" }}");
    let project = load_str(&source).unwrap_or_else(|err| panic!("{}", err.message));
    project.beams[0].description.clone().unwrap()
}

fn load_error(source: &str) -> CoreError {
    match load_str(source) {
        Ok(project) => panic!("expected a load error, loaded {:?}", beam_ids(&project)),
        Err(err) => err,
    }
}

fn beam_ids(project: &Project) -> Vec<String> {
    project.beams.iter().map(|beam| beam.id.0.clone()).collect()
}

#[test]
fn the_first_matching_arm_wins() {
    let value = described(
        "let branch = \"hotfix/x\"\n\
         let t = match branch {\n  'hotfix/*' => 'm'\n  'hotfix/**' => 'never'\n  _ => 'd'\n}",
    );
    assert_eq!(value, "m");
}

#[test]
fn a_star_does_not_cross_a_slash() {
    let value = described(
        "let branch = \"hotfix/x/y\"\nlet t = match branch { 'hotfix/*' => 'm' _ => 'd' }",
    );
    assert_eq!(value, "d");
}

#[test]
fn a_double_star_crosses_slashes() {
    let value = described(
        "let branch = \"hotfix/x/y\"\nlet t = match branch { 'hotfix/**' => 'm' _ => 'd' }",
    );
    assert_eq!(value, "m");
}

#[test]
fn an_alternative_pattern_matches() {
    let value = described(
        "let branch = \"master\"\nlet t = match branch { 'hotfix/*' | 'master' => 'm' _ => 'd' }",
    );
    assert_eq!(value, "m");
}

#[test]
fn an_arm_value_is_a_full_expression() {
    let value = described(
        "let branch = \"main\"\nlet suffix = \"!\"\n\
         let t = match branch { 'main' => 'on ' + branch + suffix _ => 'd' }",
    );
    assert_eq!(value, "on main!");
}

#[test]
fn an_invalid_glob_in_an_arm_not_taken_fails_the_load() {
    let source = "let branch = \"main\"\nlet t = match branch { 'main' => 'm' '[' => 'x' _ => 'd' }\n\
                  beam b { run \"x\" }";
    let err = load_error(source);
    assert!(
        err.message.contains("invalid `match` pattern `[`"),
        "{}",
        err.message
    );
    let start = source.find("'['").unwrap();
    assert_eq!(err.span, Some(Span::new(start, start + 3)));
}

#[test]
fn an_unknown_variable_in_an_arm_not_taken_fails_the_load() {
    let source = "let branch = \"main\"\nlet t = match branch { 'main' => 'm' 'x' => typo _ => 'd' }\n\
                  beam b { run \"x\" }";
    let err = load_error(source);
    assert_eq!(err.message, "unknown variable `typo`");
}

#[test]
fn a_boolean_subject_is_a_type_error() {
    let source = "let t = match true { _ => 'x' }\nbeam b { run \"x\" }";
    let err = load_error(source);
    assert!(
        err.message.contains("expected a string, found a boolean"),
        "{}",
        err.message
    );
    // A boolean literal carries no span of its own, so the error falls
    // back to the whole `match`.
    let start = source.find("match").unwrap();
    let end = source.find('}').unwrap() + 1;
    assert_eq!(err.span, Some(Span::new(start, end)));
}

#[test]
fn a_boolean_subject_only_known_when_the_beam_runs_is_a_type_error() {
    let source = "beam b(flag) { run \"echo {match flag == 'y' { _ => 'x' }}\" }";
    let project = load_str(source).unwrap();
    let beam = &project.beams[0];
    let scope = beam
        .scope
        .with_params(&["flag".to_string()], &["y".to_string()]);
    let err = render_template(&beam.run[0], &scope).unwrap_err();
    assert!(
        err.message.contains("expected a string, found a boolean"),
        "{}",
        err.message
    );
}

#[test]
fn match_works_inside_a_run_template() {
    let source = "let branch = \"main\"\nbeam b { run \"echo {match branch { _ => 'ok' }}\" }";
    let project = load_str(source).unwrap();
    let beam = &project.beams[0];
    assert_eq!(
        render_template(&beam.run[0], &beam.scope).unwrap(),
        "echo ok"
    );
}

#[test]
fn a_parameter_subject_is_matched_when_the_beam_runs() {
    let source =
        "beam deploy(env) { run \"deploy {match env { 'prod' => 'live' _ => 'staging' }}\" }";
    let project = load_str(source).unwrap();
    let beam = &project.beams[0];
    let params = ["env".to_string()];
    let prod = beam.scope.with_params(&params, &["prod".to_string()]);
    let dev = beam.scope.with_params(&params, &["dev".to_string()]);
    assert_eq!(render_template(&beam.run[0], &prod).unwrap(), "deploy live");
    assert_eq!(
        render_template(&beam.run[0], &dev).unwrap(),
        "deploy staging"
    );
}

#[test]
fn a_parameter_cannot_reach_a_load_time_field_through_a_match() {
    let source =
        "let x = \"a\"\nbeam b(p) { description \"{match x { 'a' => p _ => 'd' }}\" run \"x\" }";
    let err = load_error(source);
    assert_eq!(
        err.message,
        "beam parameters cannot be used in `description`"
    );
}

#[test]
fn a_doubled_brace_pattern_is_a_glob_alternation() {
    let value = described(
        "let branch = \"master\"\nlet t = match branch { '{{main,master}}' => 'm' _ => 'd' }",
    );
    assert_eq!(value, "m");
}

#[test]
fn a_nested_match_inside_an_interpolation_renders() {
    let source = "let os = \"linux\"\nlet arch = \"arm64\"\n\
                  beam b { run \"build {match os { 'linux' => match arch { 'arm*' => 'la' _ => 'lx' } _ => 'other' }} done\" }";
    let project = load_str(source).unwrap_or_else(|err| panic!("{}", err.message));
    let beam = &project.beams[0];
    assert_eq!(
        render_template(&beam.run[0], &beam.scope).unwrap(),
        "build la done"
    );
}
