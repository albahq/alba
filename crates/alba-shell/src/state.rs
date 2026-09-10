//! The interpreter's mutable state: shell variables and the working
//! directory. Separate from `ShellEnv` (the crate's public entry point)
//! because `ShellEnv` describes how a run starts, while `ShellState` is
//! what the walker mutates as it goes (`cd`, `export`, assignments).

use std::collections::HashMap;
use std::path::PathBuf;

/// A single shell variable: its value, and whether it is exported into
/// the environment of spawned children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Var {
    pub value: String,
    pub exported: bool,
}

/// Shell variables and the working directory the interpreter walks
/// with. Constructed from `ShellEnv.env`: every entry starts as an
/// exported var, since `ShellEnv.env` is the complete environment the
/// caller wants the run to start with.
#[derive(Debug, Clone)]
pub(crate) struct ShellState {
    pub vars: HashMap<String, Var>,
    pub cwd: PathBuf,
    /// The exit code of the last completed command, read by a bare
    /// `exit` (no argument) to decide what code to stop the program
    /// with. Tracked as a dedicated field rather than a `$?` entry in
    /// `vars` so it never leaks into `exported_env`.
    pub last_exit: i32,
}

impl ShellState {
    pub fn new(env: Vec<(String, String)>, cwd: PathBuf) -> Self {
        let vars = env
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    Var {
                        value,
                        exported: true,
                    },
                )
            })
            .collect();
        Self {
            vars,
            cwd,
            last_exit: 0,
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(|v| v.value.as_str())
    }

    /// Sets a variable's value, preserving its export flag if it already
    /// existed (an assignment to an already-exported variable does not
    /// un-export it), otherwise creating it unexported.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        match self.vars.get_mut(&name) {
            Some(var) => var.value = value,
            None => {
                self.vars.insert(
                    name,
                    Var {
                        value,
                        exported: false,
                    },
                );
            }
        }
    }

    /// Marks a variable as exported, optionally setting its value.
    /// Creates the variable (empty value) if it does not already exist.
    pub fn export(&mut self, name: &str, value: Option<String>) {
        match self.vars.get_mut(name) {
            Some(var) => {
                var.exported = true;
                if let Some(value) = value {
                    var.value = value;
                }
            }
            None => {
                self.vars.insert(
                    name.to_string(),
                    Var {
                        value: value.unwrap_or_default(),
                        exported: true,
                    },
                );
            }
        }
    }

    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
    }

    /// The environment to hand to a spawned child: every exported
    /// variable, name/value pairs.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(_, var)| var.exported)
            .map(|(name, var)| (name.clone(), var.value.clone()))
            .collect()
    }

    /// `$PATH`, for external command lookup. Matched case-insensitively
    /// when the exact spelling is absent: windows names the variable
    /// `Path`, and a run started from the process environment inherits
    /// it under that name. Only the lookup is lenient — the variable
    /// keeps whatever name it was given, and so does every child that
    /// inherits it.
    pub fn path(&self) -> Option<&str> {
        self.get("PATH").or_else(|| {
            self.vars
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
                .map(|(_, var)| var.value.as_str())
        })
    }

    /// `$HOME`, falling back to `$USERPROFILE` (windows), for `cd` with
    /// no argument and for tilde expansion.
    pub fn home(&self) -> Option<&str> {
        self.get("HOME").or_else(|| self.get("USERPROFILE"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_falls_back_to_the_spelling_windows_uses() {
        let state = ShellState::new(vec![("Path".into(), "/bin".into())], PathBuf::from("/"));
        assert_eq!(state.path(), Some("/bin"));
    }

    #[test]
    fn path_prefers_the_exact_spelling_when_both_are_present() {
        let state = ShellState::new(
            vec![
                ("Path".into(), "/fallback".into()),
                ("PATH".into(), "/exact".into()),
            ],
            PathBuf::from("/"),
        );
        assert_eq!(state.path(), Some("/exact"));
    }
}
