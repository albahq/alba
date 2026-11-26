//! Snapshot test parsing an example Beamfile that exercises every piece
//! of the DSL's grammar at once.

use alba_syntax::{Expr, MatchArm, Span, TemplatePart, parse};

const EXAMPLE: &str = r#"version "1"

import "api/Beamfile" as api

let profile = env("PROFILE", "debug")
let release = profile == "release"

default build

beam build {
  description "Compile the workspace"
  needs [api:build, codegen]
  inputs ["src/**/*.rs", "Cargo.toml"]
  env_inputs ["RUSTFLAGS"]
  secrets ["CARGO_REGISTRY_TOKEN"]
  outputs ["target/{profile}/app"]
  run "cargo build {if release then '--release' else ''}"
}

beam test {
  needs [build, *:test]
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

/// The span of the first occurrence of `needle` in `src`, for asserting
/// that an error points at exactly the offending text.
fn span_of(src: &str, needle: &str) -> Span {
    let start = src.find(needle).expect("needle is in the source");
    Span::new(start, start + needle.len())
}

/// The one-byte span of the first character of `needle` in `src`.
fn first_byte_of(src: &str, needle: &str) -> Span {
    let start = src.find(needle).expect("needle is in the source");
    Span::new(start, start + 1)
}

/// The value of the single `let` binding `src` declares.
fn let_value(src: &str) -> Expr {
    let mut file = parse(src).unwrap();
    file.lets.remove(0).value
}

fn literal(expr: &Expr) -> &str {
    match expr {
        Expr::Str(template) => match &template.parts[..] {
            [TemplatePart::Literal(text)] => text,
            other => panic!("expected a plain literal, got {other:?}"),
        },
        other => panic!("expected a string, got {other:?}"),
    }
}

fn pattern_values(arm: &MatchArm) -> Vec<&str> {
    arm.patterns.iter().map(|p| p.value.as_str()).collect()
}

#[test]
fn a_match_parses_arms_alternatives_and_default() {
    let src = "let t = match b { 'hotfix/*' | 'master' => 'm' \n 'release/*' => 'r' \n _ => 'd' }";
    let Expr::Match {
        subject,
        arms,
        default,
        span,
    } = let_value(src)
    else {
        panic!("expected a match expression");
    };

    assert!(matches!(*subject, Expr::Var(ref name) if name.value == "b"));
    assert_eq!(arms.len(), 2);
    assert_eq!(pattern_values(&arms[0]), ["hotfix/*", "master"]);
    assert_eq!(arms[0].patterns[0].span, span_of(src, "'hotfix/*'"));
    assert_eq!(literal(&arms[0].value), "m");
    assert_eq!(pattern_values(&arms[1]), ["release/*"]);
    assert_eq!(literal(&arms[1].value), "r");
    assert_eq!(literal(&default), "d");
    assert_eq!(span, Span::new(src.find("match").unwrap(), src.len()));
}

#[test]
fn match_arms_may_be_separated_by_commas() {
    let src = "let t = match b { 'a' => 'x', 'b' => 'y', _ => 'z', }";
    let Expr::Match { arms, default, .. } = let_value(src) else {
        panic!("expected a match expression");
    };
    assert_eq!(arms.len(), 2);
    assert_eq!(pattern_values(&arms[0]), ["a"]);
    assert_eq!(literal(&arms[0].value), "x");
    assert_eq!(pattern_values(&arms[1]), ["b"]);
    assert_eq!(literal(&arms[1].value), "y");
    assert_eq!(literal(&default), "z");
}

#[test]
fn a_match_pattern_decodes_escaped_braces() {
    let src = "let t = match b { '{{a,b}}' => 'x' _ => 'z' }";
    let Expr::Match { arms, .. } = let_value(src) else {
        panic!("expected a match expression");
    };
    assert_eq!(pattern_values(&arms[0]), ["{a,b}"]);
}

#[test]
fn a_match_without_default_is_rejected() {
    let src = "let t = match b { 'a' => 'x' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "`match` requires a final `_` arm, found `}`");
    assert_eq!(err.span, Span::new(src.len() - 1, src.len()));
    assert!(err.help.is_some());
}

#[test]
fn an_arm_after_the_default_is_rejected() {
    let src = "let t = match b { _ => 'd' 'a' => 'x' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "unreachable `match` arm after `_`");
    assert_eq!(err.span, span_of(src, "'a' => 'x'"));
    assert!(err.help.is_some());
}

#[test]
fn a_duplicate_pattern_is_rejected() {
    let src = "let t = match b { 'a' => 'x' 'b' | 'a' => 'y' _ => 'd' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "duplicate `match` pattern `a`");
    let second = src.rfind("'a'").unwrap();
    assert_eq!(err.span, Span::new(second, second + 3));
}

#[test]
fn an_interpolated_pattern_is_rejected() {
    let src = "let t = match b { 'x-{b}' => 'x' _ => 'd' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "a `match` pattern cannot interpolate");
    assert_eq!(err.span, first_byte_of(src, "{b"));
    assert!(err.help.is_some());
}

#[test]
fn the_default_cannot_be_an_alternative() {
    let src = "let t = match b { 'a' | _ => 'x' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.span, span_of(src, "_"));
    let src = "let t = match b { _ | 'a' => 'x' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.span, span_of(src, "|"));
}

#[test]
fn match_is_a_reserved_keyword() {
    let src = "let match = 'x'";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "expected an identifier, found `match`");
    assert_eq!(err.span, span_of(src, "match"));
}

#[test]
fn a_match_inside_an_interpolation_closes_on_its_own_brace() {
    let src = r#"beam b { run "echo {match x { 'a' => '1' _ => '2' }} done" }"#;
    let file = parse(src).unwrap();
    let parts = &file.beams[0].run[0].parts;
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(matches!(&parts[0], TemplatePart::Literal(text) if text == "echo "));
    assert!(matches!(&parts[1], TemplatePart::Expr(Expr::Match { .. })));
    assert!(matches!(&parts[2], TemplatePart::Literal(text) if text == " done"));
}

#[test]
fn extra_tokens_in_an_interpolation_are_rejected() {
    let src = r#"beam b { run "{a b}" }"#;
    let err = parse(src).unwrap_err();
    assert_eq!(
        err.message,
        "expected `}` to close the interpolation, found identifier `b`"
    );
    assert_eq!(err.span, first_byte_of(src, "b}"));
}

/// The text after an interpolation is literal text, never lexed: a lone
/// quote, an escaped quote, or a character the expression grammar has no
/// use for must keep working there.
#[test]
fn the_text_after_an_interpolation_is_not_lexed() {
    for (src, before, after) in [
        (r#"beam b { run "cp {src}'s-copy" }"#, "cp ", "'s-copy"),
        (
            r#"beam b { run "echo {x} \"quoted\"" }"#,
            "echo ",
            " \"quoted\"",
        ),
        (r#"beam b { run "ssh {user} @host" }"#, "ssh ", " @host"),
    ] {
        let file = parse(src).unwrap_or_else(|err| panic!("{src}: {err:?}"));
        let parts = &file.beams[0].run[0].parts;
        assert_eq!(parts.len(), 3, "{src}");
        assert!(
            matches!(&parts[0], TemplatePart::Literal(text) if text == before),
            "{src}"
        );
        assert!(
            matches!(&parts[1], TemplatePart::Expr(Expr::Var(_))),
            "{src}"
        );
        assert!(
            matches!(&parts[2], TemplatePart::Literal(text) if text == after),
            "{src}"
        );
    }
}

#[test]
fn an_interpolation_cut_short_by_a_lex_error_reports_it() {
    let src = r#"beam b { run "{a @ b}" }"#;
    let err = parse(src).unwrap_err();
    assert!(err.message.contains('@'), "{}", err.message);
    assert_eq!(err.span, span_of(src, "@"));
}

#[test]
fn an_unclosed_match_in_an_interpolation_is_an_unclosed_interpolation() {
    let src = r#"beam b { run "{match x { _ => 'a' }" }"#;
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "unclosed interpolation");
    assert_eq!(err.span, first_byte_of(src, "{match"));
}

#[test]
fn a_git_block_holds_a_base_expression() {
    let src = "git {\n  base env('GITHUB_BASE_REF', 'origin/develop')\n}\nbeam b { run \"x\" }";
    let file = parse(src).unwrap();
    let git = file.git.expect("a git declaration");
    assert!(matches!(git.base, Some(Expr::Call { .. })));
    assert_eq!(git.span, Span::new(0, src.find("\nbeam").unwrap()));
    assert_eq!(file.beams.len(), 1);
}

#[test]
fn an_empty_git_block_declares_no_base() {
    let file = parse("git { }").unwrap();
    assert_eq!(file.git.expect("a git declaration").base, None);
}

#[test]
fn a_second_git_block_is_rejected() {
    let src = "git { base 'a' }\ngit { base 'b' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "duplicate `git` block");
    assert_eq!(err.span, Span::new(17, 20));
}

#[test]
fn a_repeated_base_is_rejected() {
    let src = "git { base 'a' base 'b' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "duplicate `git` key `base`");
    assert_eq!(err.span, Span::new(15, 19));
}

#[test]
fn an_unknown_git_key_is_rejected_with_a_suggestion() {
    let src = "git { bse 'a' }";
    let err = parse(src).unwrap_err();
    assert_eq!(err.message, "unknown `git` key `bse`");
    assert_eq!(err.span, span_of(src, "bse"));
    assert_eq!(err.help.as_deref(), Some("did you mean `base`?"));
}

#[test]
fn a_far_git_key_gets_no_suggestion() {
    let err = parse("git { remote 'a' }").unwrap_err();
    assert_eq!(err.message, "unknown `git` key `remote`");
    assert_eq!(
        err.help.as_deref(),
        Some("`base` is the only key a `git` block takes")
    );
}

#[test]
fn git_stays_an_ordinary_name_outside_a_block() {
    let file = parse("let x = git.branch\nbeam b { run \"x\" }").unwrap();
    assert!(file.git.is_none());
    assert_eq!(file.lets.len(), 1);
}

#[test]
fn a_method_call_on_git_parses() {
    let src = "let x = git.changes('src/**/*.php')";
    let Expr::Method {
        receiver,
        method,
        args,
        span,
    } = let_value(src)
    else {
        panic!("expected a method call");
    };

    assert!(matches!(*receiver, Expr::Var(ref name) if name.value == "git"));
    assert_eq!(method.value, "changes");
    assert_eq!(method.span, span_of(src, "changes"));
    assert_eq!(args.len(), 1);
    assert_eq!(literal(&args[0]), "src/**/*.php");
    assert_eq!(span, Span::new(src.find("git").unwrap(), src.len()));
}

#[test]
fn method_calls_chain() {
    let src = "let x = git.changes('a').dirs('b')";
    let Expr::Method {
        receiver,
        method,
        args,
        span,
    } = let_value(src)
    else {
        panic!("expected a method call");
    };

    assert_eq!(method.value, "dirs");
    assert_eq!(literal(&args[0]), "b");
    assert_eq!(span, Span::new(src.find("git").unwrap(), src.len()));
    let Expr::Method {
        receiver: inner,
        method: inner_method,
        args: inner_args,
        span: inner_span,
    } = *receiver
    else {
        panic!("expected the receiver to be a method call");
    };
    assert!(matches!(*inner, Expr::Var(ref name) if name.value == "git"));
    assert_eq!(inner_method.value, "changes");
    assert_eq!(literal(&inner_args[0]), "a");
    assert_eq!(inner_span, span_of(src, "git.changes('a')"));
}

#[test]
fn a_field_is_still_a_field() {
    let src = "let x = git.branch";
    let Expr::Field { object, field } = let_value(src) else {
        panic!("expected a field access");
    };
    assert_eq!(object.value, "git");
    assert_eq!(field.value, "branch");
}

#[test]
fn a_method_call_in_an_interpolation_closes_on_its_own_brace() {
    let file =
        parse("beam b { inputs ['src/**'] run \"lint {git.changes('src/**')} done\" }").unwrap();
    let parts = &file.beams[0].run[0].parts;
    assert!(matches!(&parts[0], TemplatePart::Literal(text) if text == "lint "));
    assert!(matches!(&parts[1], TemplatePart::Expr(Expr::Method { .. })));
    assert!(matches!(&parts[2], TemplatePart::Literal(text) if text == " done"));
}
