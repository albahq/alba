# Alba Embedded Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `alba-shell` crate: a home-grown, cross-platform, POSIX-like shell interpreter with built-in utilities, wired in as Alba's default executor, with `executor system_shell` as the per-beam opt-out and static validation in `alba check`.

**Architecture:** `alba-shell` is a new standalone crate at the bottom of the dependency graph (no dependency on any other Alba crate): hand-written lexer, recursive descent parser, async tree-walking interpreter on tokio, sixteen builtins with a minimal documented flag surface. `alba-executors` gains `EmbeddedShellExecutor` (parse then execute, behind the unchanged `Executor` trait), the engine learns per-beam executor selection, and the CLI wires the embedded shell as the default. Spec: `.claude/superpowers/specs/2026-07-28-alba-shell-design.md`.

**Tech Stack:** Rust (edition 2024, workspace), tokio, tokio-util (CancellationToken), globset (globbing), which (PATH resolution), os_pipe (pipeline wiring), thiserror, insta + tempfile for tests, nix (unix signalling).

## Global Constraints

- TDD everywhere: write the failing test first, watch it fail, implement, watch it pass, commit.
- Workspace lints must stay green: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` at every commit.
- Commit messages: gitmoji + Conventional Commits (`✨ feat(shell): ...`), English, no attribution trailers.
- `alba-shell` must not depend on `alba-syntax`, `alba-core`, `alba-engine`, `alba-executors`, or `alba-cli`.
- The `Executor` trait, the event channel, and the engine's public event types do not change shape (one exception: `alba_engine::run` gains per-beam executor selection, Task 9).
- Diagnostics are a product feature: error messages get insta snapshot tests, same as `alba-syntax`.
- Frozen semantics (the "POSIX-like deterministic" contract, identical on all three platforms):
  - Pipeline exit code is the last command's. No `pipefail`.
  - An unset variable expands to the empty string. No `set -u`.
  - Unquoted expansion results are field-split on ASCII space, tab, and newline. No IFS customization.
  - A glob with no match stays a literal field (POSIX default, no nullglob).
  - A newline in a command behaves exactly like `;`.
  - `echo` supports only `-n` and interprets no escape sequences.
  - Redirection fd digits: `1>` and `1>>` mean stdout, `2>` and `2>>` mean stderr; any other digit stays part of the word.
  - `2>&1` routes stderr into whatever stdout currently is; through the event channel those lines are tagged Stdout.
  - Exit codes: 127 command not found, 126 found but not executable/spawnable, 2 for builtin usage errors.
  - In a multi-stage pipeline every stage runs on a snapshot of the shell state: `cd`, `export`, `unset`, `exit`, and assignments only take effect from a stage-free (single-command) pipeline.
  - Builtins always win over PATH binaries of the same name; an explicit path (`/bin/echo`) bypasses builtins.
- Out-of-subset syntax is always a `ShellParseError` with a span and a suggestion, never a fallback.

---

### Task 1: Crate scaffold, token model, and lexer (`alba-shell`)

**Files:**
- Modify: `Cargo.toml` (workspace root: add `os_pipe`, `which` to `[workspace.dependencies]`)
- Create: `crates/alba-shell/Cargo.toml`
- Create: `crates/alba-shell/src/lib.rs`
- Create: `crates/alba-shell/src/token.rs`
- Create: `crates/alba-shell/src/error.rs`
- Create: `crates/alba-shell/src/lexer.rs`
- Test: inline `#[cfg(test)]` modules + `crates/alba-shell/src/snapshots/` (insta)

**Interfaces:**
- Consumes: nothing (new crate).
- Produces (used by Task 2 and later):
  - `token::Span { start: usize, end: usize }` (byte offsets into the source string)
  - `token::Token { kind: TokenKind, span: Span }`
  - `token::TokenKind`: `Word(Word)`, `AndAnd`, `OrOr`, `Pipe`, `Semi`, `RedirectOut { stderr: bool, append: bool }`, `RedirectIn`, `StderrToStdout`
  - `token::Word { parts: Vec<WordPart>, span: Span }`
  - `token::WordPart`: `Text(String)`, `SingleQuoted(String)`, `DoubleQuoted(Vec<WordPart>)`, `Var(String)`, `CmdSubst { source: String, span: Span }`
  - `error::ShellParseError { message: String, span: Span, suggestion: Option<String> }` with `fn render(&self, source: &str) -> String`
  - `lexer::lex(source: &str) -> Result<Vec<Token>, ShellParseError>` and `lexer::lex_at(source: &str, offset: usize) -> Result<Vec<Token>, ShellParseError>` (offset shifts every span; command substitution re-lexes a slice of the outer source and keeps absolute spans)

- [ ] **Step 1: Add the dependencies and the crate**

Append to the root `Cargo.toml` `[workspace.dependencies]` (keep the list alphabetical where it already is):

```toml
os_pipe = "1"
which = "7"
```

Create `crates/alba-shell/Cargo.toml`:

```toml
[package]
name = "alba-shell"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
glob = { workspace = true }
os_pipe = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
which = { workspace = true }

[target.'cfg(unix)'.dependencies]
nix = { workspace = true, features = ["signal"] }

[dev-dependencies]
insta = { workspace = true }
tempfile = { workspace = true }
```

Check how the existing crates enable `nix` features (see `crates/alba-executors/Cargo.toml`) and mirror that form exactly.

`crates/alba-shell/src/lib.rs` for now:

```rust
//! Alba's embedded shell: a deterministic, cross-platform, POSIX-like
//! interpreter for beam commands. Standalone by design: this crate
//! depends on no other Alba crate.

mod error;
mod lexer;
mod token;

pub use error::ShellParseError;
pub use token::Span;
```

- [ ] **Step 2: Write the failing lexer tests**

In `crates/alba-shell/src/lexer.rs`, a `#[cfg(test)] mod tests` with, at minimum, these cases (write them all now; helper `fn kinds(src: &str) -> Vec<TokenKind>` unwraps `lex`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{TokenKind, WordPart};

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn err(src: &str) -> crate::ShellParseError {
        lex(src).unwrap_err()
    }

    fn word_parts(kind: &TokenKind) -> &[WordPart] {
        match kind {
            TokenKind::Word(w) => &w.parts,
            other => panic!("expected word, got {other:?}"),
        }
    }

    #[test]
    fn splits_plain_words_on_whitespace() {
        let kinds = kinds("echo hello world");
        assert_eq!(kinds.len(), 3);
        assert_eq!(word_parts(&kinds[0]), &[WordPart::Text("echo".into())]);
    }

    #[test]
    fn recognizes_operators() {
        insta::assert_debug_snapshot!(kinds("a && b || c | d ; e"));
    }

    #[test]
    fn newline_lexes_like_a_semicolon() {
        assert_eq!(kinds("a\nb"), kinds("a;b"));
    }

    #[test]
    fn recognizes_redirections() {
        insta::assert_debug_snapshot!(kinds("a > f >> g < h 1> i 1>> j 2> k 2>> l 2>&1"));
    }

    #[test]
    fn a_digit_not_glued_to_a_redirect_stays_a_word() {
        // `echo 2 > f`: the 2 is an argument, not a file descriptor.
        insta::assert_debug_snapshot!(kinds("echo 2 > f"));
    }

    #[test]
    fn single_quotes_are_literal() {
        let kinds = kinds("echo 'a $b *'");
        assert_eq!(word_parts(&kinds[1]), &[WordPart::SingleQuoted("a $b *".into())]);
    }

    #[test]
    fn double_quotes_keep_vars_live() {
        insta::assert_debug_snapshot!(kinds(r#"echo "a $b ${c} $(d e)""#));
    }

    #[test]
    fn backslash_escapes_the_next_character_unquoted() {
        let kinds = kinds(r"echo a\ b");
        assert_eq!(kinds.len(), 2);
        assert_eq!(word_parts(&kinds[1]), &[WordPart::Text("a b".into())]);
    }

    #[test]
    fn dollar_without_a_name_is_literal() {
        let kinds = kinds("echo $ $1x");
        // `$` alone and `$1` (digits are not variable names) stay literal.
        insta::assert_debug_snapshot!(kinds);
    }

    #[test]
    fn adjacent_parts_form_one_word() {
        let kinds = kinds(r#"a$B'c'"d""#);
        assert_eq!(kinds.len(), 1);
        insta::assert_debug_snapshot!(word_parts(&kinds[0]));
    }

    #[test]
    fn command_substitution_captures_raw_source_with_balanced_parens() {
        let kinds = kinds("echo $(f $(g) ')')");
        match &word_parts(&kinds[1])[0] {
            WordPart::CmdSubst { source, .. } => assert_eq!(source, "f $(g) ')'"),
            other => panic!("expected cmd subst, got {other:?}"),
        }
    }

    #[test]
    fn spans_are_byte_offsets_into_the_source() {
        let tokens = lex("ab && cd").unwrap();
        assert_eq!((tokens[0].span.start, tokens[0].span.end), (0, 2));
        assert_eq!((tokens[1].span.start, tokens[1].span.end), (3, 5));
        assert_eq!((tokens[2].span.start, tokens[2].span.end), (6, 8));
    }

    #[test]
    fn lex_at_shifts_spans() {
        let tokens = lex_at("ab", 10).unwrap();
        assert_eq!((tokens[0].span.start, tokens[0].span.end), (10, 12));
    }

    // Out-of-subset syntax: every case is (source, snapshot of message,
    // span, and suggestion). Snapshot the full rendered diagnostic so the
    // copy is reviewed like a product feature.
    #[test]
    fn rejects_background_jobs() {
        insta::assert_snapshot!(err("watch & build").render("watch & build"));
    }

    #[test]
    fn rejects_subshells() {
        insta::assert_snapshot!(err("(cd sub; make)").render("(cd sub; make)"));
    }

    #[test]
    fn rejects_heredocs() {
        insta::assert_snapshot!(err("cat << EOF").render("cat << EOF"));
    }

    #[test]
    fn rejects_backquote_substitution() {
        insta::assert_snapshot!(err("echo `date`").render("echo `date`"));
    }

    #[test]
    fn rejects_arithmetic_expansion() {
        insta::assert_snapshot!(err("echo $((1+2))").render("echo $((1+2))"));
    }

    #[test]
    fn rejects_advanced_parameter_expansion() {
        insta::assert_snapshot!(err("echo ${X:-y}").render("echo ${X:-y}"));
    }

    #[test]
    fn rejects_brace_expansion() {
        insta::assert_snapshot!(err("touch f.{rs,md}").render("touch f.{rs,md}"));
    }

    #[test]
    fn plain_braces_are_literal() {
        // `find -exec {} +` and `{x}` must keep working: only an unquoted
        // brace group containing a comma is brace expansion.
        let kinds = kinds("find . -exec rm {} +");
        assert_eq!(kinds.len(), 6);
    }

    #[test]
    fn rejects_unclosed_single_quote() {
        insta::assert_snapshot!(err("echo 'oops").render("echo 'oops"));
    }

    #[test]
    fn rejects_unclosed_double_quote() {
        insta::assert_snapshot!(err("echo \"oops").render("echo \"oops"));
    }

    #[test]
    fn rejects_unclosed_command_substitution() {
        insta::assert_snapshot!(err("echo $(true").render("echo $(true"));
    }
}
```

Also in `crates/alba-shell/src/error.rs`, tests for `render`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    #[test]
    fn renders_message_caret_and_suggestion() {
        let error = ShellParseError {
            message: "background jobs (`&`) are not supported".into(),
            span: Span { start: 6, end: 7 },
            suggestion: Some("the engine already parallelizes beams; run one command per `run` entry".into()),
        };
        insta::assert_snapshot!(error.render("watch & build"));
    }

    #[test]
    fn renders_without_suggestion() {
        let error = ShellParseError {
            message: "unclosed single quote".into(),
            span: Span { start: 5, end: 6 },
            suggestion: None,
        };
        insta::assert_snapshot!(error.render("echo 'oops"));
    }
}
```

- [ ] **Step 3: Run the tests, verify they fail to compile**

Run: `cargo test -p alba-shell`
Expected: FAIL to compile (`lex`, `Token`, `ShellParseError` do not exist yet).

- [ ] **Step 4: Implement `token.rs`, `error.rs`, `lexer.rs`**

`crates/alba-shell/src/token.rs`:

```rust
//! The lexer's output: operators and words. A word is a sequence of
//! parts because quoting decides, later, what expands and what globs:
//! `a$B'c'` is one word of three parts.

/// A byte range into the command source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Word(Word),
    AndAnd,
    OrOr,
    Pipe,
    Semi,
    /// `>`/`>>` (stderr: false) and `2>`/`2>>` (stderr: true). `1>` and
    /// `1>>` lex as stderr: false: the digit names the fd POSIX-style.
    RedirectOut { stderr: bool, append: bool },
    RedirectIn,
    /// `2>&1`.
    StderrToStdout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub parts: Vec<WordPart>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WordPart {
    /// Unquoted text. Glob characters and `~` live here; expansion
    /// decides what they mean.
    Text(String),
    SingleQuoted(String),
    /// `"..."`: only `Text`, `Var`, and `CmdSubst` appear inside.
    DoubleQuoted(Vec<WordPart>),
    /// `$NAME` or `${NAME}`.
    Var(String),
    /// `$(...)`: the raw inner source, parsed recursively by the parser.
    /// The span covers the inner source in the outer string, so nested
    /// diagnostics point at the right place.
    CmdSubst { source: String, span: Span },
}
```

`crates/alba-shell/src/error.rs`:

```rust
//! The shell's single error type for lexing and parsing, and its
//! plain-text rendering (message, source excerpt, caret underline,
//! optional help). Rendering is here rather than in the CLI because the
//! executor turns a parse failure into beam output lines and must not
//! depend on CLI rendering.

use crate::token::Span;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ShellParseError {
    pub message: String,
    pub span: Span,
    pub suggestion: Option<String>,
}

impl ShellParseError {
    /// Renders the diagnostic against the source the span points into:
    ///
    /// ```text
    /// error: background jobs (`&`) are not supported
    ///   │ watch & build
    ///   │       ^
    ///   = help: the engine already parallelizes beams; ...
    /// ```
    ///
    /// Multi-line sources (a `run` string with embedded newlines) excerpt
    /// only the line containing the span start.
    pub fn render(&self, source: &str) -> String {
        let line_start = source[..self.span.start.min(source.len())]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let line_end = source[line_start..]
            .find('\n')
            .map_or(source.len(), |i| line_start + i);
        let line = &source[line_start..line_end];
        let caret_offset = self.span.start.saturating_sub(line_start);
        let caret_len = (self.span.end.min(line_end).saturating_sub(self.span.start)).max(1);

        let mut out = format!("error: {}\n", self.message);
        out.push_str(&format!("  \u{2502} {line}\n"));
        out.push_str(&format!(
            "  \u{2502} {}{}\n",
            " ".repeat(caret_offset),
            "^".repeat(caret_len)
        ));
        if let Some(help) = &self.suggestion {
            out.push_str(&format!("  = help: {help}\n"));
        }
        out
    }
}
```

`crates/alba-shell/src/lexer.rs`: a hand-written scanner. Structure:

```rust
//! Hand-written lexer: whitespace-separated words with POSIX quoting,
//! a small operator set, and hard errors for out-of-subset syntax.
//! Newlines lex as `;` so multi-line `run` strings behave like a
//! sequence of commands.

use crate::error::ShellParseError;
use crate::token::{Span, Token, TokenKind, Word, WordPart};

/// The suggestion attached to every "this construct is not supported"
/// error, shared so the copy stays consistent.
pub(crate) const OUT_OF_SUBSET_HELP: &str =
    "move the logic into a script invoked by `run`, or declare `executor system_shell` on this beam";

pub(crate) fn lex(source: &str) -> Result<Vec<Token>, ShellParseError> {
    lex_at(source, 0)
}

pub(crate) fn lex_at(source: &str, offset: usize) -> Result<Vec<Token>, ShellParseError> {
    Lexer { source, chars: source.char_indices().peekable(), offset }.run()
}

struct Lexer<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    offset: usize,
}
```

Implementation requirements (each one is covered by a Step 2 test):

- Skip spaces and tabs. A `\n` emits `Semi`.
- Operators, longest match first: `&&`, `||`, `|`, `;`, `>>`, `>`, `<`. A lone `&` is an error: message "background jobs (`&`) are not supported", suggestion "the engine already parallelizes beams; run one command per `run` entry". `<<` is an error: "heredocs are not supported" with `OUT_OF_SUBSET_HELP`.
- `(` and `)` at word position: "subshells and function definitions are not supported" with `OUT_OF_SUBSET_HELP` (the same error therefore covers `name() { ... }` definitions). A backquote: "backquote command substitution is not supported", suggestion "use `$(...)` instead".
- A `1` or `2` immediately followed by `>` at the start of a word lexes as `RedirectOut { stderr: digit == '2', append: next is > }`; `2>&1` (exactly) lexes as `StderrToStdout`. A digit followed by whitespace or anything else starts an ordinary word.
- Word scanning accumulates parts until whitespace or an operator character:
  - Unquoted `\` escapes the next character into the current `Text` (a trailing `\` is an "incomplete escape at end of command" error).
  - `'...'` pushes `SingleQuoted`; EOF first is "unclosed single quote" with the span on the opening quote.
  - `"..."` pushes `DoubleQuoted(parts)` where inner scanning recognizes `$NAME`, `${NAME}`, `$(...)`, and the escapes `\"`, `\\`, `\$`; everything else is literal. EOF first is "unclosed double quote".
  - `$` followed by `[A-Za-z_]` scans `[A-Za-z0-9_]*` into `Var`; `${` scans a name and requires `}` immediately: anything else (`:`, `-`, `#`, `%`, ...) is "advanced parameter expansions like `${VAR:-default}` are not supported" with `OUT_OF_SUBSET_HELP`; `$((` is "arithmetic expansion is not supported" with `OUT_OF_SUBSET_HELP`; `$(` scans the raw inner source tracking paren depth and both quote kinds (so `$(f $(g) ')')` closes correctly), unclosed is an error; any other `$` is a literal `$` in `Text`.
  - An unquoted `{` scans ahead in the raw source for a matching `}` on the same word with a `,` between them and no whitespace: that is "brace expansion is not supported" (suggestion "spell the alternatives out as separate arguments"); otherwise the brace is literal text.
- Every token records its `Span` (shifted by `offset`). A word's span runs from its first to last character.

Full implementation is on the implementer; the tests define the contract. Keep the lexer a single loop with helper methods (`fn word(&mut self) -> Result<Word, ...>`, `fn double_quoted(...)`, `fn command_substitution(...)`), each under ~40 lines.

- [ ] **Step 5: Run the tests until green, review snapshots, plus clippy/fmt**

Run: `cargo test -p alba-shell`, review every new snapshot with `cargo insta review` (or inspect `src/snapshots/`), then `cargo clippy --all-targets -- -D warnings && cargo fmt`.
Expected: PASS, snapshots accepted deliberately (they are product copy).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alba-shell
git commit -m "✨ feat(shell): add the embedded shell lexer"
```

---

### Task 2: Parser and AST (`alba-shell`)

**Files:**
- Create: `crates/alba-shell/src/ast.rs`
- Create: `crates/alba-shell/src/parser.rs`
- Modify: `crates/alba-shell/src/lib.rs` (export `parse`, `Program`)
- Test: inline `#[cfg(test)]` + insta snapshots

**Interfaces:**
- Consumes: Task 1 (`lexer::lex_at`, `Token`, `TokenKind`, `Word`, `WordPart`, `ShellParseError`, `OUT_OF_SUBSET_HELP`).
- Produces:
  - `ast::Program { items: Vec<AndOrList> }`
  - `ast::AndOrList { first: Pipeline, rest: Vec<(AndOrOp, Pipeline)> }`, `ast::AndOrOp { And, Or }`
  - `ast::Pipeline { negated: bool, commands: Vec<Command> }`
  - `ast::Command { assignments: Vec<Assignment>, words: Vec<ast::Word>, redirects: Vec<Redirect>, span: Span }`
  - `ast::Assignment { name: String, value: ast::Word }`
  - `ast::Word { parts: Vec<ast::WordPart>, span: Span }` where `ast::WordPart` mirrors `token::WordPart` except `CmdSubst(Box<Program>)` is fully parsed
  - `ast::Redirect`: `Out { stderr: bool, append: bool, target: ast::Word }`, `In { target: ast::Word }`, `StderrToStdout`
  - `parser::parse(source: &str) -> Result<Program, ShellParseError>` re-exported at the crate root: **this is one half of the crate's public API** (the spec's `parse`)

- [ ] **Step 1: Write the failing parser tests**

In `crates/alba-shell/src/parser.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ast(src: &str) -> crate::ast::Program {
        parse(src).unwrap()
    }

    fn err(src: &str) -> String {
        parse(src).unwrap_err().render(src)
    }

    #[test]
    fn parses_a_simple_command() {
        insta::assert_debug_snapshot!(ast("cargo build --release"));
    }

    #[test]
    fn parses_sequencing_and_and_or() {
        insta::assert_debug_snapshot!(ast("a; b && c || d"));
    }

    #[test]
    fn and_and_or_are_left_associative_and_equal_precedence() {
        // POSIX: `a && b || c` is `(a && b) || c`.
        insta::assert_debug_snapshot!(ast("a && b || c && d"));
    }

    #[test]
    fn parses_a_pipeline() {
        insta::assert_debug_snapshot!(ast("cat f | grep x | wc -l"));
    }

    #[test]
    fn parses_negation() {
        let program = ast("! grep -q TODO src");
        assert!(program.items[0].first.negated);
    }

    #[test]
    fn parses_redirections_in_any_position() {
        insta::assert_debug_snapshot!(ast("cmd > out.txt 2>&1 < in.txt"));
    }

    #[test]
    fn parses_assignment_prefixes() {
        insta::assert_debug_snapshot!(ast("RUST_LOG=debug FOO=$bar cargo test"));
    }

    #[test]
    fn an_assignment_alone_is_a_command_without_words() {
        let program = ast("GREETING='hello world'");
        let command = &program.items[0].first.commands[0];
        assert_eq!(command.assignments.len(), 1);
        assert!(command.words.is_empty());
    }

    #[test]
    fn an_equals_sign_past_the_first_word_is_literal() {
        // `env A=b` passes `A=b` as an argument, not an assignment.
        let program = ast("env A=b");
        let command = &program.items[0].first.commands[0];
        assert!(command.assignments.is_empty());
        assert_eq!(command.words.len(), 2);
    }

    #[test]
    fn parses_command_substitution_recursively() {
        insta::assert_debug_snapshot!(ast("echo $(git rev-parse HEAD | cut -c1-7)"));
    }

    #[test]
    fn an_empty_source_is_an_empty_program() {
        assert!(ast("").items.is_empty());
        assert!(ast("  \n ; ; \n").items.is_empty());
    }

    #[test]
    fn rejects_if_conditionals() {
        insta::assert_snapshot!(err("if true; then echo y; fi"));
    }

    #[test]
    fn rejects_for_loops() {
        insta::assert_snapshot!(err("for f in *.rs; do echo $f; done"));
    }

    #[test]
    fn rejects_while_loops() {
        insta::assert_snapshot!(err("while true; do sleep 1; done"));
    }

    #[test]
    fn rejects_case_statements() {
        insta::assert_snapshot!(err("case $x in a) echo a;; esac"));
    }

    #[test]
    fn rejects_function_definitions() {
        insta::assert_snapshot!(err("greet() { echo hi; }"));
    }

    #[test]
    fn reserved_words_are_only_reserved_at_command_position() {
        // `echo if` is fine: `if` is an argument there.
        assert!(parse("echo if then done").is_ok());
    }

    #[test]
    fn quoting_a_reserved_word_makes_it_ordinary() {
        assert!(parse("'if' --version").is_ok());
    }

    #[test]
    fn rejects_a_trailing_operator() {
        insta::assert_snapshot!(err("a &&"));
    }

    #[test]
    fn rejects_a_redirect_without_target() {
        insta::assert_snapshot!(err("echo hi >"));
    }

    #[test]
    fn an_out_of_subset_error_inside_command_substitution_points_at_the_outer_source() {
        let error = parse("echo $(for x in a; do echo $x; done)").unwrap_err();
        assert!(error.span.start > 7, "span must be inside the substitution");
    }
}
```

- [ ] **Step 2: Run the tests, verify they fail to compile**

Run: `cargo test -p alba-shell`
Expected: FAIL to compile (`parse`, `ast` module do not exist).

- [ ] **Step 3: Implement `ast.rs` and `parser.rs`**

`crates/alba-shell/src/ast.rs` (types exactly as in **Interfaces**, each with a one-line doc comment; `ast::Word`/`ast::WordPart` mirror the token layer but with parsed `CmdSubst(Box<Program>)`).

`crates/alba-shell/src/parser.rs` requirements:

- `pub fn parse(source: &str) -> Result<Program, ShellParseError>` calls `parse_at(source, 0)`; `parse_at` lexes with `lex_at` and runs a recursive descent over the token list.
- Grammar: `program := and_or ((';' | '\n') and_or)*` with empty items skipped; `and_or := pipeline (('&&' | '||') pipeline)*`; `pipeline := ['!'] command ('|' command)*`; `command := assignment* (word | redirect)+` (at least one word, assignment, or redirect).
- `!` is a word whose parts are exactly `[Text("!")]` at pipeline position.
- Reserved words: at command position (before any word of the command has been seen), a word that is exactly one unquoted `Text` part matching `if|then|elif|else|fi|for|while|until|do|done|case|esac|in|function` produces the out-of-subset error. Message copy, keyed by construct: `if/then/elif/else/fi` → "`if` conditionals are not supported by the embedded shell"; `for` → "`for` loops are not supported by the embedded shell"; `while/until` → "`while` loops are not supported by the embedded shell"; `case/esac` → "`case` statements are not supported by the embedded shell"; `function` → "shell functions are not supported by the embedded shell"; `do/done/in` → "`NAME` is a shell keyword the embedded shell does not support". All carry `OUT_OF_SUBSET_HELP` and the word's span.
- Function definitions without the keyword (`name() {`) surface via the lexer's `(` error; nothing extra to do, but keep the Step 1 test.
- Assignments: while at command start, a word whose **first** part is `Text(t)` where `t` matches `^[A-Za-z_][A-Za-z0-9_]*=` becomes `Assignment { name, value }`: the value is the remainder of that `Text` part plus all following parts. After the first non-assignment word, `=` is ordinary text.
- Command substitution: for each `token::WordPart::CmdSubst { source, span }`, call `parse_at(&source, span.start)` and wrap the resulting `Program`; errors bubble with their inner (absolute) spans.
- Redirect targets are exactly one word; a missing target or trailing operator is "expected a command after `&&`" / "expected a file after `>`" style errors with the operator's span.

Update `lib.rs`:

```rust
mod ast;
mod error;
mod lexer;
mod parser;
mod token;

pub use ast::Program;
pub use error::ShellParseError;
pub use parser::parse;
pub use token::Span;
```

- [ ] **Step 4: Run the tests until green, review snapshots, plus clippy/fmt**

Run: `cargo test -p alba-shell && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-shell
git commit -m "✨ feat(shell): parse commands into a typed AST"
```

---

### Task 3: Interpreter core: state, externals, sequencing, state builtins

**Files:**
- Create: `crates/alba-shell/src/interp.rs`
- Create: `crates/alba-shell/src/state.rs`
- Create: `crates/alba-shell/src/spawn.rs`
- Create: `crates/alba-shell/src/builtins/mod.rs`
- Create: `crates/alba-shell/src/expand.rs` (literal-only version; Task 4 completes it)
- Modify: `crates/alba-shell/src/lib.rs`
- Test: `crates/alba-shell/tests/interp.rs`

**Interfaces:**
- Consumes: Task 2 (`Program`, `ast::*`, `parse`).
- Produces (**the other half of the crate's public API**, stable from here on):
  - `pub struct ShellEnv { pub env: Vec<(String, String)>, pub cwd: PathBuf, pub output: tokio::sync::mpsc::UnboundedSender<ShellOutputLine>, pub cancel: tokio_util::sync::CancellationToken }`
  - `pub struct ShellOutputLine { pub stream: ShellStream, pub text: String }`
  - `pub enum ShellStream { Stdout, Stderr }` (Debug, Clone, Copy, PartialEq, Eq)
  - `pub struct ShellResult { pub exit_code: i32 }`
  - `pub async fn execute(program: &Program, env: ShellEnv) -> ShellResult`
- Internal (used by Tasks 4-6): `state::ShellState { vars: HashMap<String, Var>, cwd: PathBuf }` with `Var { value: String, exported: bool }`, `ShellState::exported_env() -> Vec<(String, String)>`, `ShellState::get(name) -> Option<&str>`; `builtins::find(name: &str) -> Option<Builtin>`; `spawn::run_external(...)`.

Semantics implemented here (all from the Global Constraints freezes):

- `ShellEnv.env` is the **complete** environment (the executor composes process env + beam env in Task 10); every entry starts as an exported var.
- Sequencing `;`, `&&` (run right only if left succeeded), `||` (only if failed), `!` (invert: 0 becomes 1, nonzero becomes 0). Program exit code is the last executed command's; an empty program exits 0.
- `exit [n]` stops the whole program with code `n` (default: last exit code). Internal control flow: `enum Flow { Next(i32), Exit(i32) }` threaded through the walker.
- Externals: PATH lookup via `which::which_in(name, state.get("PATH"), &state.cwd)` unless the name contains a path separator (then resolve against `cwd` directly). Not found: stderr line `alba-shell: command not found: NAME` (plus `did you mean \`X\`?` when a builtin name is within Levenshtein distance 2), exit 127. Spawn failure of a resolved path: stderr line, exit 126. Spawned with `.env_clear().envs(state.exported_env())`, `current_dir(&state.cwd)`, stdin null, stdout/stderr piped through line-reader tasks that forward to `ShellEnv.output` (port the `stream_lines` shape from `crates/alba-executors/src/shell.rs:126`, including CRLF stripping and the final unterminated line).
- Cancellation: checked between commands (return exit 130); a running external is terminated with the same escalation as `SystemShellExecutor` (unix: process-group SIGTERM via `process_group(0)` + negated pid, 5s grace, group SIGKILL; windows: `start_kill`). Reimplement in `spawn.rs` (this crate cannot depend on `alba-executors`); copy the doc comments' reasoning from `crates/alba-executors/src/shell.rs:158-234`.
- State builtins registered in `builtins/mod.rs`: `cd` (no args: `$HOME`, else error; missing dir: stderr `cd: no such directory: X`, exit 1; success updates `state.cwd` canonicalized), `pwd` (prints `state.cwd` display), `exit`, `true`, `false`, `export NAME[=VALUE]...` (marks exported, sets value if given; invalid name: stderr + exit 2), `unset NAME...`. Assignment-only commands set unexported shell vars.
- `FOO=bar cmd` assignment prefixes overlay the environment of that one command (exported for the child), restored after.

- [ ] **Step 1: Write the failing behaviour tests**

`crates/alba-shell/tests/interp.rs`:

```rust
use std::path::PathBuf;

use alba_shell::{ShellEnv, ShellStream, execute, parse};
use tokio_util::sync::CancellationToken;

/// Runs `src` in `cwd` and returns (exit code, output lines).
async fn run_in(src: &str, cwd: PathBuf) -> (i32, Vec<(ShellStream, String)>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse(src).expect("parse");
    let env = ShellEnv {
        env: std::env::vars().collect(),
        cwd,
        output: tx,
        cancel: CancellationToken::new(),
    };
    let result = execute(&program, env).await;
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push((line.stream, line.text));
    }
    (result.exit_code, lines)
}

async fn run(src: &str) -> (i32, Vec<(ShellStream, String)>) {
    run_in(src, std::env::current_dir().unwrap()).await
}

fn stdout(lines: &[(ShellStream, String)]) -> Vec<&str> {
    lines
        .iter()
        .filter(|(s, _)| *s == ShellStream::Stdout)
        .map(|(_, t)| t.as_str())
        .collect()
}

#[tokio::test]
async fn true_succeeds_and_false_fails() {
    assert_eq!(run("true").await.0, 0);
    assert_eq!(run("false").await.0, 1);
}

#[tokio::test]
async fn an_empty_command_succeeds() {
    assert_eq!(run("").await.0, 0);
}

#[tokio::test]
async fn sequencing_returns_the_last_exit_code() {
    assert_eq!(run("false; true").await.0, 0);
    assert_eq!(run("true; false").await.0, 1);
}

#[tokio::test]
async fn and_or_short_circuit() {
    assert_eq!(run("false && exit 3").await.0, 1);
    assert_eq!(run("true || exit 3").await.0, 0);
    assert_eq!(run("false || true").await.0, 0);
}

#[tokio::test]
async fn negation_inverts_the_exit_code() {
    assert_eq!(run("! false").await.0, 0);
    assert_eq!(run("! true").await.0, 1);
}

#[tokio::test]
async fn exit_stops_the_program_with_its_code() {
    let (code, lines) = run("exit 7; pwd").await;
    assert_eq!(code, 7);
    assert!(stdout(&lines).is_empty(), "nothing may run after exit");
}

#[tokio::test]
async fn pwd_prints_the_shell_cwd_and_cd_moves_it() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let (code, lines) = run_in("cd sub && pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    let printed = stdout(&lines).join("");
    assert!(printed.ends_with("sub"), "got: {printed}");
}

#[tokio::test]
async fn cd_to_a_missing_directory_fails_with_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cd nowhere", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(s, t)| *s == ShellStream::Stderr && t.contains("nowhere")));
}

#[tokio::test]
async fn an_unknown_command_reports_127_with_a_message() {
    let (code, lines) = run("definitely-not-a-command-alba").await;
    assert_eq!(code, 127);
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr
                && t.contains("command not found: definitely-not-a-command-alba"))
    );
}

#[tokio::test]
async fn a_near_miss_of_a_builtin_gets_a_suggestion() {
    // `pdw` is distance 2 from `pwd`, which exists from this task on
    // (`echo` only lands in Task 6, so it cannot anchor this test yet).
    let (_, lines) = run("pdw").await;
    assert!(lines.iter().any(|(_, t)| t.contains("did you mean `pwd`?")));
}

#[tokio::test]
async fn runs_an_external_command_and_streams_its_output() {
    // cargo is guaranteed present: this workspace builds with it.
    let (code, lines) = run("cargo --version").await;
    assert_eq!(code, 0);
    assert!(stdout(&lines).iter().any(|l| l.starts_with("cargo ")));
}

#[tokio::test]
async fn a_failing_external_reports_its_exit_code() {
    let (code, _) = run("cargo definitely-not-a-subcommand").await;
    assert_ne!(code, 0);
}

#[tokio::test]
async fn cancellation_stops_a_running_external() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse("cargo --version; cargo --version").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let env = ShellEnv {
        env: std::env::vars().collect(),
        cwd: std::env::current_dir().unwrap(),
        output: tx,
        cancel,
    };
    let started = std::time::Instant::now();
    let result = execute(&program, env).await;
    assert_eq!(result.exit_code, 130);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn an_assignment_alone_succeeds_silently() {
    let (code, lines) = run("GREETING=hello").await;
    assert_eq!(code, 0);
    assert!(lines.is_empty());
}

#[tokio::test]
async fn export_rejects_an_invalid_name() {
    let (code, lines) = run("export 1BAD=x").await;
    assert_eq!(code, 2);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}
```

- [ ] **Step 2: Run the tests, verify they fail to compile**

Run: `cargo test -p alba-shell --test interp`
Expected: FAIL to compile (`ShellEnv`, `execute` do not exist).

- [ ] **Step 3: Implement state, spawn, builtins, and the walker**

Module layout (each file focused, roughly):

- `state.rs`: `ShellState` (constructed from `ShellEnv.env`, every var exported), `get`, `set`, `export`, `unset`, `exported_env`, plus `home()` (`HOME`, else `USERPROFILE`, else None) for Task 4.
- `expand.rs`, literal-only for now:

```rust
//! Word expansion. This task: literal parts only (Text, SingleQuoted,
//! DoubleQuoted of Texts). Task 4 adds variables, splitting, tilde,
//! globbing, and command substitution.

use crate::ast::{Word, WordPart};
use crate::state::ShellState;

pub(crate) fn expand_word_literal(word: &Word, _state: &ShellState) -> String {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            WordPart::Text(t) | WordPart::SingleQuoted(t) => out.push_str(t),
            WordPart::DoubleQuoted(parts) => {
                for inner in parts {
                    if let WordPart::Text(t) = inner {
                        out.push_str(t);
                    }
                }
            }
            _ => {}
        }
    }
    out
}
```

- `builtins/mod.rs`: `pub(crate) enum Builtin { Cd, Pwd, Exit, True, False, Export, Unset }` (extended in Task 6), `find(name)`, `pub(crate) const NAMES: &[&str]` (fed both by `find` and the did-you-mean suggestion), and `run(builtin, args, state, io) -> Flow` where `io` is, for this task, a small struct holding the output sender (`Lines`); builtin stdout/stderr write whole lines through it.
- `spawn.rs`: `run_external(path: &Path, args: &[String], env: &[(String, String)], cwd: &Path, output: &UnboundedSender<ShellOutputLine>, cancel: &CancellationToken) -> i32` with the reader tasks, drain timeout (2s), and the terminate escalation, all ported from `crates/alba-executors/src/shell.rs` (grace 5s). Killed-by-signal exit maps to `-1` exactly as there; cancellation makes `execute` return 130 overall.
- `interp.rs`: the public types and `execute`; walks `Program → AndOrList → Pipeline → Command`. Pipelines in this task handle the single-command case only (`commands.len() == 1`); a multi-stage pipeline returns, for now, exit 0 after running stages sequentially without connecting them, and Task 5's tests replace that behaviour (do NOT test multi-stage pipelines in this task). Command dispatch: expand words with `expand_word_literal`; empty word list and nonempty assignments → apply assignments to state; else look up `builtins::find(&words[0])` (only when `words[0]` contains no path separator), then externals. Levenshtein helper for did-you-mean: distance <= 2 against `builtins::NAMES` (write a small local `fn distance(a, b) -> usize`, no new dependency).

Update `lib.rs` to export `ShellEnv`, `ShellOutputLine`, `ShellStream`, `ShellResult`, `execute`.

- [ ] **Step 4: Run the whole crate until green, plus clippy/fmt**

Run: `cargo test -p alba-shell && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-shell
git commit -m "✨ feat(shell): interpret commands, builtins, and externals"
```

---

### Task 4: Word expansion: variables, quoting, splitting, tilde, globs, command substitution

**Files:**
- Modify: `crates/alba-shell/src/expand.rs` (replace the literal-only version)
- Modify: `crates/alba-shell/src/interp.rs` (capture support for command substitution)
- Modify: `.claude/superpowers/specs/2026-07-28-alba-shell-design.md` (Non-core primitives row: `glob` instead of `globset`)
- Test: `crates/alba-shell/tests/expand.rs`

**Interfaces:**
- Consumes: Task 3 (`ShellState`, `execute` internals, `Flow`).
- Produces:
  - `expand::expand_words(words: &[ast::Word], state: &mut ShellState, ctx: &ExpandCtx<'_>) -> Result<Vec<String>, Flow>` (fields, after splitting and globbing)
  - `expand::expand_word_single(word: &ast::Word, state: &mut ShellState, ctx: &ExpandCtx<'_>) -> Result<String, Flow>` (no splitting, no globbing: assignment values and redirect targets)
  - `ExpandCtx` carries what command substitution needs: the output sender (substitution stderr forwards to it) and the cancellation token.
- Note: the spec's Key decisions table names `globset`, but `globset` only matches, it does not walk the filesystem; the `glob` crate (already a workspace dependency, already declared for `alba-shell` in Task 1) walks. Update the spec's Non-core primitives row accordingly in this task's commit.

Semantics (frozen):

- `$VAR` / `${VAR}` reads shell vars (exported or not); unset expands to empty.
- Unquoted expansion results split on ASCII space/tab/newline; an expansion that produces nothing yields zero fields. Quoted (`"..."`) never splits. Literal text never splits (it was already word-split by the lexer).
- `~` expands to `state.home()` only as the unquoted first character of a word, followed by `/` or end of word; otherwise literal (no `~user`).
- Command substitution runs the inner program with stdout captured in memory, stderr forwarded to the outer output sender; all trailing newlines are stripped; interior newlines survive (and then split when unquoted). The substitution's exit code is discarded (frozen simplification).
- Globbing: after splitting, a field whose unquoted segments contain `*`, `?`, or `[` becomes a pattern: quoted segments are escaped with `glob::Pattern::escape`, the pattern resolves against `state.cwd` (prefix the cwd, match, strip the prefix), results are relative, use forward slashes on every platform, and sort lexicographically. No match: the field stays exactly as written. Directories match like files (no trailing-slash special case).

- [ ] **Step 1: Write the failing tests**

`crates/alba-shell/tests/expand.rs` (reuse the `run_in`/`run`/`stdout` helpers from `tests/interp.rs` by copying them; test crates do not share code). Note: `echo` only lands in Task 6, so these tests observe expansion through `pwd`, external `cargo`, and exit codes; where a test below uses `echo`, mark it `#[ignore = "echo lands in task 6"]` now and un-ignore it in Task 6:

```rust
#[tokio::test]
async fn a_shell_variable_expands_in_a_later_command() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("D=sub; cd $D && pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(stdout(&lines).join("").ends_with("sub"));
}

#[tokio::test]
async fn an_unset_variable_expands_to_nothing() {
    // `cd $NOPE` with NOPE unset is `cd` with no argument: goes to HOME.
    // Instead observe field dropping: `true $NOPE` must not break.
    assert_eq!(run("true $NOPE_UNSET_VAR").await.0, 0);
}

#[tokio::test]
#[ignore = "echo lands in task 6"]
async fn double_quotes_prevent_field_splitting() {
    let (_, lines) = run(r#"A='x  y'; echo "$A""#).await;
    assert_eq!(stdout(&lines), vec!["x  y"]);
}

#[tokio::test]
#[ignore = "echo lands in task 6"]
async fn unquoted_expansion_field_splits() {
    let (_, lines) = run("A='x  y'; echo $A").await;
    assert_eq!(stdout(&lines), vec!["x y"]);
}

#[tokio::test]
async fn command_substitution_feeds_an_assignment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("D=$(pwd)/sub; cd $D && pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
    assert!(stdout(&lines).join("").ends_with("sub"));
}

#[tokio::test]
async fn command_substitution_strips_trailing_newlines_only() {
    let dir = tempfile::tempdir().unwrap();
    // pwd emits one trailing newline; without stripping, `cd` would fail.
    let (code, _) = run_in("cd $(pwd)", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn substitution_stderr_reaches_the_outer_output() {
    let (_, lines) = run("X=$(definitely-not-a-command-alba); true").await;
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t.contains("command not found"))
    );
}

#[tokio::test]
async fn tilde_expands_to_home_at_word_start() {
    let (code, _) = run_in("cd ~", std::env::temp_dir()).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn globs_resolve_sorted_relative_with_forward_slashes() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["b.txt", "a.txt", "c.md"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    // `cd` fails on multiple arguments only if expansion produced them:
    // use rm (task 6)? Not yet: assert through an external is fragile.
    // Frozen observable: a glob that matches exactly one entry can be
    // consumed by `cd` when it names a directory.
    std::fs::create_dir(dir.path().join("only_dir_here")).unwrap();
    let (code, _) = run_in("cd only_*", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn an_unmatched_glob_stays_literal() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cd no_such_*", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(_, t)| t.contains("no_such_*")));
}

#[tokio::test]
#[ignore = "echo lands in task 6"]
async fn quoted_glob_characters_do_not_glob() {
    let (_, lines) = run(r#"echo "*""#).await;
    assert_eq!(stdout(&lines), vec!["*"]);
}
```

Add the full-splitting/sorted-glob assertions as `#[ignore]`d `echo` tests now (they document the contract; Task 6 activates them):

```rust
#[tokio::test]
#[ignore = "echo lands in task 6"]
async fn globs_sort_and_use_forward_slashes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    for name in ["d/b.rs", "d/a.rs"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    let (_, lines) = run_in("echo d/*.rs", dir.path().to_path_buf()).await;
    assert_eq!(stdout(&lines), vec!["d/a.rs d/b.rs"]);
}
```

- [ ] **Step 2: Run the tests, verify the new ones fail**

Run: `cargo test -p alba-shell --test expand`
Expected: FAIL (variables expand to nothing yet, substitution unimplemented).

- [ ] **Step 3: Implement the full expander**

Shape of `expand.rs`:

- Internal representation: `Vec<Segment>` with `struct Segment { text: String, quoted: bool }`, built part by part (`Text` unquoted, `SingleQuoted`/`DoubleQuoted` quoted, `Var`/`CmdSubst` unquoted unless inside `DoubleQuoted`).
- `expand_word_single` concatenates all segment texts.
- `expand_words` per word: build segments (command substitution via a `Box::pin`ned recursive call into the interpreter with a capture target; see below), split unquoted segment text on whitespace to produce fields carrying their segments, then glob fields that have an unquoted glob character.
- Capture: add to `interp.rs` an internal `enum Out { Lines(UnboundedSender<ShellOutputLine>), Capture(Arc<Mutex<Vec<u8>>>) }` threaded to builtins and `spawn.rs` (external stdout goes to the capture buffer instead of the line reader when capturing). Keep `pub async fn execute` unchanged; add `pub(crate) fn execute_captured(program, state, ctx) -> impl Future<Output = (i32, String)>`.
- Glob matching: pattern = escaped-or-raw segment concatenation; `glob::glob_with(&cwd.join(pattern).to_string_lossy(), MatchOptions { require_literal_separator: true, ..Default::default() })`, strip the `cwd` prefix from each hit, convert `\` to `/`, sort, and substitute the field; empty hits keep the original field text.

- [ ] **Step 4: Run the crate until green, plus clippy/fmt**

Run: `cargo test -p alba-shell && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS (ignored tests stay ignored).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alba-shell .claude/superpowers/specs/2026-07-28-alba-shell-design.md
git commit -m "✨ feat(shell): expand variables, tildes, globs, and substitutions"
```

---

### Task 5: Pipelines and redirections

**Files:**
- Create: `crates/alba-shell/src/io.rs`
- Modify: `crates/alba-shell/src/interp.rs`, `crates/alba-shell/src/spawn.rs`, `crates/alba-shell/src/builtins/mod.rs`
- Test: `crates/alba-shell/tests/pipeline.rs`

**Interfaces:**
- Consumes: Tasks 3-4.
- Produces (internal, used by Task 6 builtins):
  - `io::OutTarget`: `Lines { tx: UnboundedSender<ShellOutputLine>, stream: ShellStream }`, `Capture(Arc<Mutex<Vec<u8>>>)`, `File(std::fs::File)`, `Pipe(os_pipe::PipeWriter)`
  - `io::InTarget`: `Null`, `File(std::fs::File)`, `Pipe(os_pipe::PipeReader)`
  - `io::CommandIo { stdin: InTarget, stdout: OutTarget, stderr: OutTarget }` with `OutTarget::try_clone()` (Lines clones the sender retagged Stdout for `2>&1`, Capture clones the Arc, File/Pipe `try_clone`), `OutTarget::into_stdio() -> (std::process::Stdio, Option<JoinHandle<()>>)` (Lines/Capture spawn a forwarding reader over a fresh pipe), `InTarget::into_stdio()`
  - Builtins gain a blocking-friendly writer: `OutTarget::writer() -> Box<dyn io::Write + Send>` (Lines buffers and flushes whole lines, final partial line on drop)

Semantics (frozen):

- Stage wiring: `a | b | c` connects a's stdout to b's stdin and b's to c's via `os_pipe::pipe()`; the last stage's stdout is the command's `CommandIo.stdout`. All stages start together and are awaited together; exit code = last stage.
- Multi-stage pipelines run every stage on a **clone** of the shell state: `cd`/`export`/`unset`/assignments/`exit` inside a pipeline stage do not leak out, and `exit` there only sets that stage's code.
- Builtin stages run in `tokio::task::spawn_blocking` (their io is synchronous pipe io); single-command builtins keep writing through the sender directly.
- Redirects apply left to right after target expansion (`expand_word_single`), relative to `state.cwd`: `>` create/truncate, `>>` create/append, `<` open for read; `2>` / `2>>` likewise for stderr; `2>&1` sets stderr to a clone of the **current** stdout. A redirect that cannot open its file prints `alba-shell: cannot open X: <os error>` to stderr and fails the command with exit 1 without running it.
- Cancellation covers every stage (each external gets the terminate escalation; builtin stages are dropped at the join).

- [ ] **Step 1: Write the failing tests**

`crates/alba-shell/tests/pipeline.rs` (same copied helpers; externals used sparingly and only `cargo`):

```rust
#[tokio::test]
async fn a_pipeline_connects_stdout_to_stdin() {
    // cat (task 6) is not here yet, so pipe external into external is the
    // only option; keep it minimal: exit code of the last stage wins.
    let (code, _) = run("cargo --version | cargo definitely-not-a-subcommand").await;
    assert_ne!(code, 0, "last stage decides");
    let (code, _) = run("cargo definitely-not-a-subcommand | cargo --version").await;
    assert_eq!(code, 0, "last stage decides");
}

#[tokio::test]
async fn stdout_redirects_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("pwd > out.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(stdout(&lines).is_empty(), "redirected output must not reach the channel");
    let contents = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert!(contents.trim_end().ends_with(&dir.path().file_name().unwrap().to_string_lossy().to_string()));
}

#[tokio::test]
async fn append_redirect_appends() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _) = run_in("pwd > out.txt; pwd >> out.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    let contents = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(contents.lines().count(), 2);
}

#[tokio::test]
async fn stderr_redirects_independently() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cd nowhere 2> err.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.is_empty(), "stderr went to the file");
    let contents = std::fs::read_to_string(dir.path().join("err.txt")).unwrap();
    assert!(contents.contains("nowhere"));
}

#[tokio::test]
async fn stderr_to_stdout_retags_lines() {
    let (_, lines) = run("cd definitely-nowhere 2>&1").await;
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stdout && t.contains("definitely-nowhere"))
    );
}

#[tokio::test]
async fn an_unopenable_redirect_fails_without_running_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("pwd > missing_dir/out.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(s, t)| *s == ShellStream::Stderr && t.contains("cannot open")));
}

#[tokio::test]
async fn pipeline_stages_do_not_leak_state() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("cd sub | true; pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(
        !stdout(&lines).join("").ends_with("sub"),
        "cd inside a pipeline must not move the shell"
    );
}

#[tokio::test]
async fn exit_inside_a_pipeline_does_not_stop_the_program() {
    let (code, _) = run("exit 5 | true; true").await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn input_redirect_feeds_an_external() {
    // Full stdin coverage arrives with `cat` in Task 6; here only assert
    // that `< file` on a missing file fails cleanly.
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("true < missing.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(_, t)| t.contains("cannot open")));
}
```

- [ ] **Step 2: Run the tests, verify they fail**

Run: `cargo test -p alba-shell --test pipeline`
Expected: FAIL (multi-stage pipelines are Task 3's stub; redirects unimplemented).

- [ ] **Step 3: Implement `io.rs` and rewire the interpreter**

- `io.rs` exactly as in **Interfaces**; the `Lines` writer buffers bytes, emits a `ShellOutputLine` per `\n` (CRLF-stripped), flushes the remainder on drop.
- `interp.rs`: a `Command` now resolves its `CommandIo` (defaults: stdin Null, stdout Lines/Stdout, stderr Lines/Stderr, or Capture when substituting), applies redirects in order, then dispatches. Pipelines build the pipe chain, clone the state per stage when `commands.len() > 1`, spawn all stages, join, and return the last exit code.
- `spawn.rs`: `run_external` now takes a `CommandIo` and converts targets with `into_stdio()`, keeping the reader-task/drain/terminate behaviour for `Lines` and `Capture` targets.
- `builtins/mod.rs`: builtin `run` takes `CommandIo` (writer-based); `spawn_blocking` where the io can block.

- [ ] **Step 4: Run the crate until green, plus clippy/fmt**

Run: `cargo test -p alba-shell && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-shell
git commit -m "✨ feat(shell): wire pipelines and redirections"
```

---

### Task 6: The utility builtins

**Files:**
- Create: `crates/alba-shell/src/builtins/text.rs` (`echo`, `cat`)
- Create: `crates/alba-shell/src/builtins/fs.rs` (`cp`, `mv`, `rm`, `mkdir`, `touch`)
- Create: `crates/alba-shell/src/builtins/util.rs` (`sleep`, `test`/`[`)
- Modify: `crates/alba-shell/src/builtins/mod.rs` (register them)
- Test: `crates/alba-shell/tests/builtins.rs`, plus un-ignore the Task 4 `echo` tests

**Interfaces:**
- Consumes: Task 5 (`CommandIo`, writers, state snapshot rules).
- Produces: the sixteen-builtin surface frozen in the spec. Flag surfaces (anything else: stderr usage line, exit 2):
  - `echo [-n] args...`: args joined by one space + `\n` (no `\n` with `-n`); no escapes; always exit 0. `-n` only counts as a flag as the first argument.
  - `cat [file...]`: no args copies stdin to stdout; missing file prints `cat: X: no such file` and continues; exit 1 if any failed, else 0.
  - `cp [-r] src... dst`: multiple sources require dst to be an existing directory; `-r` required for directory sources; errors `cp: ...` exit 1.
  - `mv src... dst`: rename first, fall back to copy+delete (files and directories); same multiple-source rule.
  - `rm [-r] [-f] path...`: directories need `-r`; missing path is an error unless `-f`; `-f` also suppresses the nonzero exit for missing paths; glob-unmatched literals therefore fail without `-f`.
  - `mkdir [-p] dir...`: without `-p`, existing or missing-parent errors.
  - `touch file...`: create empty if missing, else update mtime via `File::set_modified(SystemTime::now())`.
  - `sleep SECONDS`: decimal accepted (`0.2`); cancellable (`tokio::select!` with the token; cancelled → 130); bad argument exit 2.
  - `test ARGS...` / `[ ARGS... ]` (`[` requires a final `]`): unary `-f -d -e -z -n`, binary `= != -eq -ne -lt -le -gt -ge` (numeric operands must parse as i64, else exit 2 with a message), single argument tests non-emptiness; result 0/1.

- [ ] **Step 1: Write the failing tests**

`crates/alba-shell/tests/builtins.rs` (same helpers), covering per builtin at least: the happy path, one error path, and the flag-rejection path. Representative set (write all of these):

```rust
#[tokio::test]
async fn echo_joins_arguments_with_single_spaces() {
    let (_, lines) = run("echo one two   three").await;
    assert_eq!(stdout(&lines), vec!["one two three"]);
}

#[tokio::test]
async fn echo_n_suppresses_the_newline_and_unknown_flags_are_arguments() {
    let (_, lines) = run("echo -n x; echo -e y").await;
    // -n: still one line through the channel (line flushed on drop);
    // -e is NOT a flag: it is printed.
    assert_eq!(stdout(&lines), vec!["x", "-e y"]);
}

#[tokio::test]
async fn cat_reads_files_and_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let (code, lines) = run_in("cat a.txt | cat", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert_eq!(stdout(&lines), vec!["hello"]);
}

#[tokio::test]
async fn cat_reports_a_missing_file_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "ok\n").unwrap();
    let (code, lines) = run_in("cat nope.txt a.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert_eq!(stdout(&lines), vec!["ok"]);
}

#[tokio::test]
async fn cp_copies_a_file_and_r_copies_a_tree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    std::fs::write(dir.path().join("d/inner.txt"), "y").unwrap();
    let (code, _) = run_in("cp a.txt b.txt && cp -r d d2", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(dir.path().join("b.txt").is_file());
    assert!(dir.path().join("d2/inner.txt").is_file());
}

#[tokio::test]
async fn cp_refuses_a_directory_without_r() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    let (code, lines) = run_in("cp d d2", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn mv_renames() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (code, _) = run_in("mv a.txt b.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(!dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").is_file());
}

#[tokio::test]
async fn rm_needs_r_for_directories_and_f_forgives_missing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    assert_eq!(run_in("rm d", dir.path().to_path_buf()).await.0, 1);
    assert_eq!(run_in("rm -r d", dir.path().to_path_buf()).await.0, 0);
    assert_eq!(run_in("rm missing.txt", dir.path().to_path_buf()).await.0, 1);
    assert_eq!(run_in("rm -f missing.txt", dir.path().to_path_buf()).await.0, 0);
}

#[tokio::test]
async fn the_dogfood_line_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dist")).unwrap();
    std::fs::write(dir.path().join("dist/old.js"), "x").unwrap();
    let (code, _) = run_in("rm -r -f dist && mkdir dist", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(dir.path().join("dist").is_dir());
    assert!(!dir.path().join("dist/old.js").exists());
}

#[tokio::test]
async fn mkdir_p_creates_parents_and_tolerates_existing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run_in("mkdir -p a/b/c && mkdir -p a/b/c", dir.path().to_path_buf()).await.0, 0);
    assert_eq!(run_in("mkdir x/y", dir.path().to_path_buf()).await.0, 1);
}

#[tokio::test]
async fn touch_creates_and_updates() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run_in("touch new.txt", dir.path().to_path_buf()).await.0, 0);
    assert!(dir.path().join("new.txt").is_file());
    assert_eq!(run_in("touch new.txt", dir.path().to_path_buf()).await.0, 0);
}

#[tokio::test]
async fn sleep_accepts_decimals_and_rejects_garbage() {
    assert_eq!(run("sleep 0.05").await.0, 0);
    assert_eq!(run("sleep soon").await.0, 2);
}

#[tokio::test]
async fn test_covers_files_strings_and_numbers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "").unwrap();
    let cwd = dir.path().to_path_buf();
    assert_eq!(run_in("test -f f.txt", cwd.clone()).await.0, 0);
    assert_eq!(run_in("test -d f.txt", cwd.clone()).await.0, 1);
    assert_eq!(run_in("test -e f.txt && test -z '' && test -n x", cwd.clone()).await.0, 0);
    assert_eq!(run("test a = a && test a != b && test 2 -gt 1 && test 1 -le 1").await.0, 0);
    assert_eq!(run("test 2 -lt 1").await.0, 1);
    assert_eq!(run("test x -gt 1").await.0, 2);
    assert_eq!(run_in("[ -f f.txt ]", cwd).await.0, 0);
    assert_eq!(run("[ -f oops").await.0, 2);
}

#[tokio::test]
async fn a_builtin_wins_over_path_but_an_explicit_path_does_not() {
    // `echo` must be ours even on unix where /bin/echo exists: our echo
    // treats -e as an argument (bash's interprets it as a flag).
    let (_, lines) = run("echo -e tag").await;
    assert_eq!(stdout(&lines), vec!["-e tag"]);
}
```

Also remove `#[ignore]` from the Task 4 `echo`-based expansion tests and make them pass.

- [ ] **Step 2: Run, verify failures; Step 3: implement; Step 4: green + clippy/fmt**

Run: `cargo test -p alba-shell` between each. Implementation notes: `fs.rs` uses `std::fs` inside `spawn_blocking` via the Task 5 builtin runner; recursive copy is a small local `fn copy_tree(src, dst) -> io::Result<()>`; every error message is `NAME: <detail>` on stderr. `sleep` is async (not `spawn_blocking`).

- [ ] **Step 5: Commit**

```bash
git add crates/alba-shell
git commit -m "✨ feat(shell): add the file and utility builtins"
```

---

### Task 7: Cross-platform conformance suite

**Files:**
- Create: `crates/alba-shell/tests/conformance.rs`

**Interfaces:**
- Consumes: the whole public API (`parse`, `execute`, `ShellEnv`).
- Produces: the executable specification of "same Beamfile, same behavior, three platforms". CI (`.github/workflows/ci.yml`) already runs `cargo test --workspace` on ubuntu/macos/windows; no workflow change needed.

- [ ] **Step 1: Write the suite (it must pass immediately: it is a consolidation gate, not a feature)**

Table-driven, one case per frozen semantic, all running in a per-case tempdir prepared with a standard fixture tree (`a.txt` containing `alpha\n`, `b.txt` containing `beta\n`, directory `sub/` with `sub/c.txt`):

```rust
struct Case {
    name: &'static str,
    script: &'static str,
    want_exit: i32,
    want_stdout: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case { name: "echo_joins", script: "echo one  two", want_exit: 0, want_stdout: &["one two"] },
    Case { name: "quoting_preserves", script: "echo 'a  b' \"c  d\"", want_exit: 0, want_stdout: &["a  b c  d"] },
    Case { name: "var_roundtrip", script: "X=alba; echo $X${X}", want_exit: 0, want_stdout: &["albaalba"] },
    Case { name: "unset_var_is_empty", script: "echo start${NOPE}end", want_exit: 0, want_stdout: &["startend"] },
    Case { name: "subst", script: "echo $(echo inner)", want_exit: 0, want_stdout: &["inner"] },
    Case { name: "glob_sorted_forward_slashes", script: "echo sub/*.txt *.txt", want_exit: 0, want_stdout: &["sub/c.txt a.txt b.txt"] },
    Case { name: "unmatched_glob_literal", script: "echo *.zzz", want_exit: 0, want_stdout: &["*.zzz"] },
    Case { name: "quoted_glob_literal", script: "echo '*.txt'", want_exit: 0, want_stdout: &["*.txt"] },
    Case { name: "pipeline", script: "cat a.txt b.txt | cat", want_exit: 0, want_stdout: &["alpha", "beta"] },
    Case { name: "pipeline_exit_is_last", script: "false | true", want_exit: 0, want_stdout: &[] },
    Case { name: "and_or", script: "test -f a.txt && echo yes || echo no", want_exit: 0, want_stdout: &["yes"] },
    Case { name: "negation", script: "! test -f missing.txt", want_exit: 0, want_stdout: &[] },
    Case { name: "redirect_then_cat", script: "echo saved > out.txt && cat out.txt", want_exit: 0, want_stdout: &["saved"] },
    Case { name: "append", script: "echo 1 > o.txt; echo 2 >> o.txt; cat o.txt", want_exit: 0, want_stdout: &["1", "2"] },
    Case { name: "stdin_redirect", script: "cat < a.txt", want_exit: 0, want_stdout: &["alpha"] },
    Case { name: "cp_mv_rm_roundtrip", script: "cp a.txt c.txt && mv c.txt d.txt && rm d.txt && test -f a.txt", want_exit: 0, want_stdout: &[] },
    Case { name: "clean_rebuild_dir", script: "rm -r -f dist && mkdir dist && test -d dist", want_exit: 0, want_stdout: &[] },
    Case { name: "exit_code_propagates", script: "exit 4", want_exit: 4, want_stdout: &[] },
    Case { name: "not_found_is_127", script: "definitely-not-a-command-alba", want_exit: 127, want_stdout: &[] },
    Case { name: "assignment_scoped_to_line", script: "X=1; echo $X", want_exit: 0, want_stdout: &["1"] },
    Case { name: "newline_is_semicolon", script: "echo a\necho b", want_exit: 0, want_stdout: &["a", "b"] },
];

#[tokio::test]
async fn conformance() {
    for case in CASES {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "gamma\n").unwrap();
        let (code, lines) = run_in(case.script, dir.path().to_path_buf()).await;
        assert_eq!(code, case.want_exit, "{}: exit code (lines: {lines:?})", case.name);
        assert_eq!(stdout(&lines), case.want_stdout.to_vec(), "{}", case.name);
    }
}
```

No `cfg(...)` anywhere in this file: that absence is the point.

- [ ] **Step 2: Run on the dev machine, fix regressions it flushes out, then verify the full workspace**

Run: `cargo test -p alba-shell --test conformance` then `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS. If a case fails, the bug is in Tasks 3-6 code: fix it there (with a regression test in the matching test file if the gap was untested), never by weakening the case.

- [ ] **Step 3: Commit**

```bash
git add crates/alba-shell
git commit -m "✅ test(shell): add the cross-platform conformance suite"
```

---

### Task 8: DSL: `executor system_shell`

**Files:**
- Modify: `crates/alba-syntax/src/parser.rs` (`parse_executor_decl`, around line 321: the `{ ... }` block becomes optional)
- Modify: `crates/alba-core/src/model.rs` (new `ExecutorKind::SystemShell` variant)
- Modify: `crates/alba-core/src/eval.rs` (the executor mapping around line 944)
- Test: inline tests + snapshots in both crates

**Interfaces:**
- Consumes: the existing `ExecutorDecl { name, options }` AST and `ExecutorKind`.
- Produces: `ExecutorKind::SystemShell` (unit variant, doc comment: "the host shell, the per-beam opt-out from the embedded shell"), consumed by Task 9's `Executors::for_beam`.

- [ ] **Step 1: Write the failing tests**

In `crates/alba-syntax/src/parser.rs` tests (follow the file's existing test style and snapshot conventions):

```rust
#[test]
fn executor_without_a_block_parses() {
    // `executor system_shell` and `executor shell` need no options.
    insta::assert_debug_snapshot!(parse("beam b {\n  executor system_shell\n  run \"x\"\n}").unwrap());
}

#[test]
fn executor_with_a_block_still_parses() {
    insta::assert_debug_snapshot!(parse("beam b {\n  executor docker { image \"i\" }\n  run \"x\"\n}").unwrap());
}
```

In `crates/alba-core/src/eval.rs` tests (same style as the existing executor tests there):

```rust
#[test]
fn system_shell_executor_maps_to_its_kind() {
    let project = load_str("beam b {\n  executor system_shell\n  run \"x\"\n}").unwrap();
    assert_eq!(project.beams[0].executor, ExecutorKind::SystemShell);
}

#[test]
fn system_shell_rejects_options() {
    let error = load_str("beam b {\n  executor system_shell { image \"i\" }\n  run \"x\"\n}").unwrap_err();
    insta::assert_snapshot!(error_message_and_help(&error)); // reuse the file's existing error-formatting test helper
}

#[test]
fn unknown_executor_suggestions_include_system_shell() {
    let error = load_str("beam b {\n  executor system_shel\n  run \"x\"\n}").unwrap_err();
    // help must be: did you mean `system_shell`?
    insta::assert_snapshot!(error_message_and_help(&error));
}
```

(Adapt helper names to what the existing tests in each file actually use; add an equivalent helper if none exists.)

- [ ] **Step 2: Run, verify failures**

Run: `cargo test -p alba-syntax -p alba-core`
Expected: FAIL (parser demands `{`, `system_shell` unknown).

- [ ] **Step 3: Implement**

- `parser.rs` `parse_executor_decl`: after `eat_ident`, only parse the options block if the next token is `LBrace` (keep the loop unchanged inside); otherwise return `ExecutorDecl { name, options: Vec::new() }`.
- `model.rs`: add `SystemShell` to `ExecutorKind` (after `Shell`).
- `eval.rs` executor mapping: add the arm before `docker`:

```rust
"system_shell" => {
    if let Some((key, _)) = decl.options.first() {
        let _ = key;
        return Err(CoreError::new(
            "executor `system_shell` takes no options".to_string(),
            decl.name.span,
        ));
    }
    Ok(ExecutorKind::SystemShell)
}
```

and extend the suggestion list to `["shell", "system_shell", "docker"]`. While here, mirror the no-options rule for `"shell"` only if it already behaves that way; do not change `shell`'s existing behaviour otherwise.

- [ ] **Step 4: Workspace green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS (the new variant must not break any `match`; fix non-exhaustive matches by handling `SystemShell` the same as `Shell` wherever the engine or CLI matches on `ExecutorKind`; Task 9 gives it real behaviour).

- [ ] **Step 5: Commit**

```bash
git add crates/alba-syntax crates/alba-core crates/alba-engine crates/alba-cli
git commit -m "✨ feat(dsl): declare a beam's executor as system_shell"
```

---

### Task 9: Engine: per-beam executor selection

**Files:**
- Modify: `crates/alba-engine/src/scheduler.rs` (the `run` signature at line 90, `RunOptions` callers, validation around line 212, the per-beam task setup around line 154)
- Modify: `crates/alba-engine/src/lib.rs` (export `Executors`)
- Modify: `crates/alba-engine/src/cache/fingerprint.rs` (`BeamFacts` gains the executor label) and `crates/alba-engine/src/cache/store.rs` (bump `FORMAT_VERSION`)
- Modify: `crates/alba-cli/src/commands/run.rs` (call site, line ~92: temporary `Executors::uniform(Arc::new(SystemShellExecutor))`; Task 10 wires the real pair)
- Test: existing engine tests updated mechanically + new selection tests

**Interfaces:**
- Consumes: Task 8 (`ExecutorKind::SystemShell`), the `Executor` trait, `FakeExecutor`.
- Produces:

```rust
/// The executors a run can dispatch to, chosen per beam.
#[derive(Clone)]
pub struct Executors {
    /// `ExecutorKind::Shell`: the default (the embedded shell, once the
    /// CLI wires it in).
    pub embedded: Arc<dyn Executor>,
    /// `ExecutorKind::SystemShell`: the per-beam opt-out.
    pub system: Arc<dyn Executor>,
}

impl Executors {
    /// Both slots on the same executor: what tests and `alba check` want.
    pub fn uniform(executor: Arc<dyn Executor>) -> Self;
    fn for_beam(&self, kind: &ExecutorKind) -> Arc<dyn Executor>; // Docker unreachable: validation rejected it
}
```

`pub async fn run(project, target, options, executors: Executors, events, cancel)` replaces the `executor: Arc<dyn Executor>` parameter.

- [ ] **Step 1: Write the failing tests**

In the engine test suite (same file/pattern as the existing scheduler tests with `FakeExecutor`; find them with `grep -rn "FakeExecutor" crates/alba-engine`):

```rust
#[tokio::test]
async fn a_system_shell_beam_uses_the_system_executor() {
    // Two FakeExecutors with distinguishable behaviours; a project with
    // beam `a` (default) and beam `b` (executor system_shell). Assert each
    // fake saw exactly its own beam's command.
}

#[tokio::test]
async fn a_system_shell_beam_passes_validation() {
    // `executor system_shell` must not trip the docker rejection.
}

#[tokio::test]
async fn switching_executor_kind_invalidates_the_cache() {
    // Run a cached beam with kind Shell, flip the project's beam to
    // SystemShell, run again: it must execute, not replay.
}
```

Write these as real tests following the surrounding helpers (`load_str`, event collection); the existing tests show the exact project-building idiom.

- [ ] **Step 2: Run, verify failures to compile/pass**

Run: `cargo test -p alba-engine`
Expected: FAIL (no `Executors`, `run` signature mismatch).

- [ ] **Step 3: Implement**

- `Executors` as specified; `for_beam` maps `Shell` → embedded, `SystemShell` → system, `Docker` → unreachable (validation rejects docker before scheduling: keep that check).
- Thread `executors.for_beam(&beam.executor)` where the per-beam task currently clones the single executor (`scheduler.rs:154` and `:288`).
- Validation (line ~212): only `Docker` is rejected; wording unchanged.
- Fingerprint: add `pub executor: &'static str` to `BeamFacts` (values `"embedded"`, `"system"`), feed it into the hash (`item(&mut hasher, "executor"); item(&mut hasher, facts.executor);` next to `commands`), bump `FORMAT_VERSION` in `store.rs` (mandated by the recipe-change comment at `fingerprint.rs:23`).
- Update every `run(...)` call site (engine tests, CLI) to pass `Executors::uniform(...)` or a real pair; behaviour identical for now.

- [ ] **Step 4: Workspace green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-engine crates/alba-cli
git commit -m "✨ feat(engine): select a beam's executor per beam"
```

---

### Task 10: `EmbeddedShellExecutor`, the new default

**Files:**
- Modify: `crates/alba-executors/Cargo.toml` (add `alba-shell = { path = "../alba-shell" }`)
- Create: `crates/alba-executors/src/embedded.rs`
- Modify: `crates/alba-executors/src/lib.rs` (export `EmbeddedShellExecutor`)
- Modify: `crates/alba-cli/src/commands/run.rs` (`Executors { embedded: Arc::new(EmbeddedShellExecutor), system: Arc::new(SystemShellExecutor) }`)
- Test: `crates/alba-executors/tests/embedded.rs`, plus end-to-end cases in `crates/alba-cli/tests/cli_run.rs`

**Interfaces:**
- Consumes: `alba_shell::{parse, execute, ShellEnv, ShellStream, ShellOutputLine}`, the `Executor` trait, Task 9's CLI wiring.
- Produces: `pub struct EmbeddedShellExecutor;` implementing `Executor`.

Behaviour:

- Environment composition: `std::env::vars()` overlaid with `cmd.env` (matching `CommandSpec`'s documented "extends and overrides" contract, `crates/alba-executors/src/lib.rs:37-49`).
- Parse failure: `Err(ExecError { message: error.render(&cmd.command) })`, never a spawn. The engine already turns an `ExecError` into a failed beam with the message as output (`scheduler.rs:765-775`).
- Line mapping: `ShellStream::Stdout → Stream::Stdout`, `Stderr → Stderr`; forward every `ShellOutputLine` as an `OutputLine`.
- Exit code: `ShellResult.exit_code` verbatim.

- [ ] **Step 1: Write the failing tests**

`crates/alba-executors/tests/embedded.rs` (mirror the harness style of `crates/alba-executors/tests/shell.rs`):

```rust
use std::sync::Arc;

use alba_executors::{CommandSpec, ExecContext, Executor, EmbeddedShellExecutor, Stream};
use tokio_util::sync::CancellationToken;

async fn exec(command: &str, env: Vec<(String, String)>) -> (Result<i32, String>, Vec<(Stream, String)>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let spec = CommandSpec {
        command: command.to_string(),
        env,
        cwd: std::env::current_dir().unwrap(),
    };
    let ctx = ExecContext { output: tx, cancel: CancellationToken::new() };
    let result = EmbeddedShellExecutor
        .execute(spec, ctx)
        .await
        .map(|r| r.exit_code)
        .map_err(|e| e.to_string());
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push((line.stream, line.text));
    }
    (result, lines)
}

#[tokio::test]
async fn runs_a_builtin_identically_everywhere() {
    let (result, lines) = exec("echo -n one && echo two", vec![]).await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(lines, vec![(Stream::Stdout, "one".into()), (Stream::Stdout, "two".into())]);
}

#[tokio::test]
async fn beam_env_overlays_the_process_environment() {
    let (result, lines) = exec("echo $ALBA_EMBEDDED_TEST", vec![("ALBA_EMBEDDED_TEST".into(), "on".into())]).await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(lines, vec![(Stream::Stdout, "on".into())]);
}

#[tokio::test]
async fn path_from_the_process_environment_reaches_externals() {
    let (result, _) = exec("cargo --version", vec![]).await;
    assert_eq!(result.unwrap(), 0);
}

#[tokio::test]
async fn a_parse_error_is_an_exec_error_carrying_the_diagnostic() {
    let (result, lines) = exec("for x in a; do echo $x; done", vec![]).await;
    let message = result.unwrap_err();
    assert!(message.contains("`for` loops are not supported"));
    assert!(message.contains("executor system_shell"));
    assert!(message.contains('^'), "the rendered span must be present");
    assert!(lines.is_empty(), "nothing may have run");
}
```

End-to-end, in `crates/alba-cli/tests/cli_run.rs` (follow the file's tempdir + `assert_cmd` idiom):

```rust
#[test]
fn a_beam_runs_on_the_embedded_shell_by_default() {
    // Beamfile: beam hello { run "echo hello from alba" }
    // `alba run hello` succeeds and prints the line on every platform.
}

#[test]
fn out_of_subset_syntax_fails_the_beam_with_the_diagnostic() {
    // Beamfile: beam bad { run "for x in a; do echo $x; done" }
    // `alba run bad` exits 1; stderr/stdout carries "not supported" and
    // "executor system_shell".
}

#[test]
fn executor_system_shell_opts_a_beam_out() {
    // Beamfile: beam legacy { executor system_shell run "echo sys" }
    // Succeeds; on unix this went through sh, on windows powershell.
}
```

Write them fully in the file's existing style (real Beamfile strings, real assertions on stdout).

- [ ] **Step 2: Run, verify failures; Step 3: implement `embedded.rs` and the CLI wiring; Step 4: workspace green + clippy/fmt**

Run between steps: `cargo test -p alba-executors`, then `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt`.

`embedded.rs` core:

```rust
pub struct EmbeddedShellExecutor;

#[async_trait::async_trait]
impl Executor for EmbeddedShellExecutor {
    async fn execute(&self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError> {
        let program = alba_shell::parse(&cmd.command)
            .map_err(|error| ExecError { message: error.render(&cmd.command) })?;
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        for (name, value) in &cmd.env {
            match env.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = value.clone(),
                None => env.push((name.clone(), value.clone())),
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let forward = {
            let output = ctx.output.clone();
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    let stream = match line.stream {
                        alba_shell::ShellStream::Stdout => Stream::Stdout,
                        alba_shell::ShellStream::Stderr => Stream::Stderr,
                    };
                    let _ = output.send(OutputLine { stream, text: line.text });
                }
            })
        };
        let result = alba_shell::execute(
            &program,
            alba_shell::ShellEnv { env, cwd: cmd.cwd, output: tx, cancel: ctx.cancel },
        )
        .await;
        let _ = forward.await;
        Ok(ExecResult { exit_code: result.exit_code })
    }
}
```

- [ ] **Step 5: Commit**

```bash
git add crates/alba-executors crates/alba-cli Cargo.lock
git commit -m "✨ feat(executors): make the embedded shell the default"
```

---

### Task 11: `alba check` validates embedded-shell commands statically

**Files:**
- Modify: `crates/alba-cli/Cargo.toml` (add `alba-shell = { path = "../alba-shell" }`)
- Modify: `crates/alba-cli/src/commands/check.rs`
- Modify: `crates/alba-cli/src/main.rs` (only if `check::run`'s changed signature requires it)
- Test: `crates/alba-cli/tests/cli_check_list.rs`

**Interfaces:**
- Consumes: `alba_core::{Project, ExecutorKind, render_template}` (a `Beam` exposes `pub scope`, `crates/alba-core/src/model.rs:115`), `alba_shell::{parse, ShellParseError}`.
- Produces: `check::run(project: &Project) -> i32` keeps its name; return value becomes 2 when static shell validation fails (Alba-error exit code), 0 otherwise.

Rules (from the spec):

- Validate only beams with `ExecutorKind::Shell` (embedded) **and** `params.is_empty()`: their `run` templates render at check time with `beam.scope` alone. Beams with parameters, `system_shell` beams, and docker beams are skipped.
- For each rendered command that fails `alba_shell::parse`, print (through the existing `LineSink`) the beam name, then `ShellParseError::render` against the rendered command, and finally still print nothing else for that beam. If any beam failed: exit 2 and do not print the success line. Otherwise the existing `✓ Beamfile: N beams` line stands.
- A template that fails to render here (it should not: load already rendered-checked everything renderable) is skipped silently: rendering questions belong to load, not check.

- [ ] **Step 1: Write the failing end-to-end tests**

In `crates/alba-cli/tests/cli_check_list.rs`, following its existing harness:

```rust
#[test]
fn check_rejects_invalid_embedded_shell_syntax() {
    // Beamfile: beam bad { run "echo 'unclosed" }
    // `alba check` exits 2, output contains "bad", "unclosed single quote".
}

#[test]
fn check_skips_parameterized_and_system_shell_beams() {
    // beam deploy(target) { run "echo {target} 'x" }   <- unknowable, skipped
    // beam legacy { executor system_shell run "if [ 1 ]; then echo y; fi" } <- skipped
    // `alba check` exits 0.
}

#[test]
fn check_accepts_valid_embedded_commands() {
    // beam ok { run ["echo one", "cat a.txt | cat"] } exits 0 with the ✓ line.
}
```

Write them fully in the file's real idiom (tempdir, Beamfile string, `assert_cmd` assertions).

- [ ] **Step 2: Run, verify failures; Step 3: implement; Step 4: workspace green + clippy/fmt**

Implementation sketch for `check.rs`:

```rust
pub fn run(project: &Project) -> i32 {
    let sink = LineSink::stdout();
    let mut failed = false;
    for beam in &project.beams {
        if beam.executor != ExecutorKind::Shell || !beam.params.is_empty() {
            continue;
        }
        for template in &beam.run {
            let Ok(command) = render_template(template, &beam.scope) else {
                continue;
            };
            if let Err(error) = alba_shell::parse(&command) {
                failed = true;
                sink.line(&format!("beam `{}`: invalid embedded shell command", beam.id.0));
                for line in error.render(&command).lines() {
                    sink.line(line);
                }
            }
        }
    }
    if failed {
        return 2;
    }
    let count = project.beams.len();
    let noun = if count == 1 { "beam" } else { "beams" };
    sink.line(&format!("\u{2713} Beamfile: {count} {noun}"));
    0
}
```

Adapt to `LineSink`'s real construction/usage and `ExecutorKind`'s `PartialEq` (derive it if missing).

- [ ] **Step 5: Commit**

```bash
git add crates/alba-cli Cargo.lock
git commit -m "✨ feat(cli): check embedded shell commands statically"
```

---

### Task 12: Documentation and dogfood

**Files:**
- Modify: `README.md` (new `## Embedded shell` section)
- Modify: `Beamfile` (only if the dogfood check below surfaces an incompatibility; expected: no change, every command is `cargo ...` or builtin `echo`)

- [ ] **Step 1: Write the README section**

After the existing usage/caching documentation, in the README's established voice, covering: the embedded shell is the default executor and why (same Beamfile, same behavior on macOS, Linux, and Windows); the supported syntax in one paragraph (sequencing, pipelines, redirections, quoting, variables, command substitution, tilde, globs); the sixteen builtins and the builtins-win-over-PATH rule; the notable frozen semantics (unset vars empty, no pipefail, unmatched globs literal, echo -n only); what is out of subset and the exact error experience; `executor system_shell` as the opt-out; `alba check` catching syntax errors statically.

- [ ] **Step 2: Verify the dogfood loop end to end**

```bash
cargo build
./target/debug/alba check
./target/debug/alba run check
./target/debug/alba run check   # second run: fmt/lint/test replay as cached
```

Expected: check passes (static shell validation included), the full `check` beam graph runs through the embedded shell, and the cached second run still works (Task 9 changed the fingerprint recipe, so the first run repopulates the cache).

- [ ] **Step 3: Full workspace check, then commit**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add README.md Beamfile
git commit -m "📝 docs: document the embedded shell"
```

---

## Self-review checklist (run after writing, before handoff)

- Spec coverage: supported grammar (Tasks 1-2), frozen semantics (Tasks 3-6 + Global Constraints), builtins (Tasks 3 and 6), diagnostics with spans and suggestions (Tasks 1-2, 10), conformance (Task 7), `executor system_shell` (Tasks 8-9), default executor swap (Task 10), static check (Task 11), documentation and dogfood (Task 12), cancellation (Tasks 3, 5, 6), success criteria (Tasks 7, 10, 12 + CI matrix).
- The `parse`/`execute` public API matches the spec's architecture section exactly.
- Type names used across tasks are consistent (`ShellEnv`, `ShellOutputLine`, `ShellStream`, `ShellResult`, `Executors`, `EmbeddedShellExecutor`, `ExecutorKind::SystemShell`).

