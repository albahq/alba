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
