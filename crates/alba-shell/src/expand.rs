//! Word expansion: variables, quoting, field splitting, tilde
//! expansion, globbing, and command substitution.
//!
//! Each `Word` becomes a `Vec<Segment>` first (one segment per literal
//! or expanded part, tagged with whether it came from a quoted
//! context), then, for `expand_words`, split into fields on unquoted
//! whitespace and globbed. `expand_word_single` skips both steps: it
//! just concatenates every segment's text, which is what an assignment
//! value or a redirect target needs.

use glob::{MatchOptions, Pattern};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::ast::{Program, Word, WordPart};
use crate::interp::{Flow, ShellOutputLine, execute_captured};
use crate::state::ShellState;

/// What command substitution needs from its caller: where to forward
/// its stderr, and the cancellation token to honour.
pub(crate) struct ExpandCtx<'a> {
    pub output: &'a UnboundedSender<ShellOutputLine>,
    pub cancel: &'a CancellationToken,
}

/// One piece of a word's expansion: `Text` is always unquoted;
/// `SingleQuoted` and a whole `DoubleQuoted` part are always quoted;
/// `Var`/`CmdSubst` are quoted only when they appear inside a
/// `DoubleQuoted` part (in which case they are folded into that part's
/// single merged segment, never surfaced on their own).
struct Segment {
    text: String,
    quoted: bool,
}

/// Expands `word` with no splitting and no globbing: every segment's
/// text concatenated in order. Used for assignment values and redirect
/// targets, neither of which is field-split or globbed.
pub(crate) async fn expand_word_single(
    word: &Word,
    state: &mut ShellState,
    ctx: &ExpandCtx<'_>,
) -> Result<String, Flow> {
    let segments = build_segments(word, state, ctx).await?;
    Ok(segments.into_iter().map(|s| s.text).collect())
}

/// Expands `words` into the final argument fields a command sees:
/// variables and command substitutions resolved, unquoted results
/// split on whitespace, and glob-eligible fields resolved against the
/// shell's cwd.
pub(crate) async fn expand_words(
    words: &[Word],
    state: &mut ShellState,
    ctx: &ExpandCtx<'_>,
) -> Result<Vec<String>, Flow> {
    let mut fields = Vec::new();
    for word in words {
        let segments = build_segments(word, state, ctx).await?;
        for piece in split_into_fields(segments) {
            let plan = plan_field(&piece);
            match plan.pattern.as_deref().and_then(|p| resolve_glob(p, state)) {
                Some(matches) => fields.extend(matches),
                None => fields.push(plan.literal),
            }
        }
    }
    Ok(fields)
}

/// Builds the segment list for one word: literal text, quoted text
/// verbatim, variables read from `state`, and command substitutions run
/// (recursively) through the interpreter. Tilde expansion is applied
/// here too, since it only ever rewrites the word's first segment.
async fn build_segments(
    word: &Word,
    state: &mut ShellState,
    ctx: &ExpandCtx<'_>,
) -> Result<Vec<Segment>, Flow> {
    let mut segments = Vec::with_capacity(word.parts.len());
    for (index, part) in word.parts.iter().enumerate() {
        match part {
            WordPart::Text(text) => {
                let text = if index == 0 {
                    tilde_expand(text, word.parts.get(1), state).unwrap_or_else(|| text.clone())
                } else {
                    text.clone()
                };
                segments.push(Segment {
                    text,
                    quoted: false,
                });
            }
            WordPart::SingleQuoted(text) => segments.push(Segment {
                text: text.clone(),
                quoted: true,
            }),
            WordPart::DoubleQuoted(inner) => {
                let mut text = String::new();
                for inner_part in inner {
                    match inner_part {
                        WordPart::Text(t) | WordPart::SingleQuoted(t) => text.push_str(t),
                        WordPart::Var(name) => text.push_str(&expand_var(name, state)),
                        WordPart::CmdSubst(program) => {
                            text.push_str(&run_cmd_subst(program, state, ctx).await?);
                        }
                        // Not produced by the parser inside a
                        // `DoubleQuoted` (see `ast::WordPart`'s doc
                        // comment), kept only so this match stays
                        // exhaustive if that ever changes.
                        WordPart::DoubleQuoted(_) => {}
                    }
                }
                segments.push(Segment { text, quoted: true });
            }
            WordPart::Var(name) => segments.push(Segment {
                text: expand_var(name, state),
                quoted: false,
            }),
            WordPart::CmdSubst(program) => segments.push(Segment {
                text: run_cmd_subst(program, state, ctx).await?,
                quoted: false,
            }),
        }
    }
    Ok(segments)
}

/// `$VAR`/`${VAR}`: unset expands to the empty string, no `set -u`.
fn expand_var(name: &str, state: &ShellState) -> String {
    state.get(name).unwrap_or_default().to_string()
}

/// Runs `program` with its stdout captured, forwarding its stderr to
/// the outer output sender and discarding its exit code (the frozen
/// command-substitution simplification), then strips all trailing
/// newlines (interior ones survive, to split later if unquoted).
async fn run_cmd_subst(
    program: &Program,
    state: &mut ShellState,
    ctx: &ExpandCtx<'_>,
) -> Result<String, Flow> {
    if ctx.cancel.is_cancelled() {
        return Err(Flow::Exit(130));
    }
    let (_exit_code, captured) = execute_captured(program, state, ctx).await;
    Ok(captured.trim_end_matches('\n').to_string())
}

/// `~` at the very start of a word, unquoted, followed by `/` or the
/// end of the word, expands to `state.home()`. Anything else (a `~`
/// followed by other characters, `~` past the first part, or `~` inside
/// quotes) is left literal. No `~user`.
fn tilde_expand(
    first_text: &str,
    next_part: Option<&WordPart>,
    state: &ShellState,
) -> Option<String> {
    let rest = first_text.strip_prefix('~')?;
    let followed_by_slash_or_end = if rest.is_empty() {
        match next_part {
            None => true,
            Some(WordPart::Text(next)) => next.starts_with('/'),
            _ => false,
        }
    } else {
        rest.starts_with('/')
    };
    if !followed_by_slash_or_end {
        return None;
    }
    let home = state.home()?;
    Some(format!("{home}{rest}"))
}

/// Splits one word's segments into fields: an unquoted segment's text
/// is split on runs of ASCII space/tab/newline (a no-op for literal
/// `Text` segments, which never contain one — the lexer already split
/// on whitespace at the word boundary); a quoted segment is never split
/// and always keeps the field it's in "real", even when empty (`''` is
/// one empty field). A word that expands to nothing produces zero
/// fields.
fn split_into_fields(segments: Vec<Segment>) -> Vec<Vec<Segment>> {
    let mut fields = Vec::new();
    let mut current = Vec::new();
    let mut current_started = false;

    for segment in segments {
        if segment.quoted {
            current.push(Segment {
                text: segment.text,
                quoted: true,
            });
            current_started = true;
            continue;
        }

        let bytes = segment.text.as_bytes();
        let mut piece_start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if is_ascii_split_whitespace(bytes[i]) {
                if i > piece_start {
                    current.push(Segment {
                        text: segment.text[piece_start..i].to_string(),
                        quoted: false,
                    });
                    current_started = true;
                }
                if current_started {
                    fields.push(std::mem::take(&mut current));
                    current_started = false;
                }
                while i < bytes.len() && is_ascii_split_whitespace(bytes[i]) {
                    i += 1;
                }
                piece_start = i;
                continue;
            }
            i += 1;
        }
        if piece_start < bytes.len() {
            current.push(Segment {
                text: segment.text[piece_start..].to_string(),
                quoted: false,
            });
            current_started = true;
        }
    }

    if current_started {
        fields.push(current);
    }
    fields
}

fn is_ascii_split_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n')
}

/// What one field resolves to before globbing: its literal text (used
/// verbatim when it is not a glob, or as the fallback when a glob
/// matches nothing) and, when any unquoted piece contains a glob
/// metacharacter, the pattern to match (quoted pieces escaped with
/// `glob::Pattern::escape` so they can never themselves act as glob
/// syntax).
struct FieldPlan {
    literal: String,
    pattern: Option<String>,
}

fn plan_field(pieces: &[Segment]) -> FieldPlan {
    let mut literal = String::new();
    let mut pattern = String::new();
    let mut has_glob_char = false;

    for piece in pieces {
        literal.push_str(&piece.text);
        if piece.quoted {
            pattern.push_str(&Pattern::escape(&piece.text));
        } else {
            if piece.text.contains(['*', '?', '[']) {
                has_glob_char = true;
            }
            pattern.push_str(&piece.text);
        }
    }

    FieldPlan {
        literal,
        pattern: has_glob_char.then_some(pattern),
    }
}

/// Resolves `pattern` against `state.cwd`: `None` means no match (the
/// field stays literal); `Some` carries every match, relative to `cwd`,
/// forward-slashed on every platform, sorted lexicographically.
/// Directories match like files (no trailing-slash special case).
fn resolve_glob(pattern: &str, state: &ShellState) -> Option<Vec<String>> {
    let full_pattern = state.cwd.join(pattern);
    let options = MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    let mut matches: Vec<String> = glob::glob_with(&full_pattern.to_string_lossy(), options)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|path| {
            let relative = path.strip_prefix(&state.cwd).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    if matches.is_empty() {
        return None;
    }
    matches.sort();
    Some(matches)
}
