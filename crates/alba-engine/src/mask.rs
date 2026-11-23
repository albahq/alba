//! Masking a beam's secrets out of what its run emits and stores.
//!
//! A beam's `secrets` name variables; their values, looked up in the
//! beam's own rendered `env` first and then in the environment the run
//! inherits, are replaced by [`MASK`] in every output line before any
//! consumer sees it: the terminal, the interface, the JSON stream, and the
//! cache's stored logs. Output travels line by line, so a multi-line value
//! (a private key) is masked line by line.

use std::collections::HashMap;

/// What a masked value reads as.
pub(crate) const MASK: &str = "***";

/// The values one beam must never show.
#[derive(Debug, Clone, Default)]
pub(crate) struct Mask {
    /// Every non-empty line of every secret value, longest first, so a
    /// value that contains another is replaced whole rather than around
    /// the shorter one.
    values: Vec<String>,
}

impl Mask {
    /// The values `names` hold: a name the beam's `env` sets takes that
    /// value, any other one the run's inherited environment's. A name set
    /// nowhere has nothing to mask.
    pub(crate) fn new(
        names: &[String],
        env: &[(String, String)],
        ambient: &HashMap<String, String>,
    ) -> Self {
        let mut values: Vec<String> = names
            .iter()
            .filter_map(|name| {
                env.iter()
                    .find(|(own, _)| own == name)
                    .map(|(_, value)| value)
                    .or_else(|| ambient.get(name))
            })
            .flat_map(|value| value.lines())
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self { values }
    }

    /// `text` with every secret value replaced by [`MASK`].
    pub(crate) fn apply(&self, text: &str) -> String {
        self.values.iter().fold(text.to_string(), |text, value| {
            text.replace(value.as_str(), MASK)
        })
    }
}
