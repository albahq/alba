//! The interactive terminal interface: a consumer of the engine's
//! [`alba_engine::RunEvent`] stream and a producer of
//! [`alba_engine::SessionCommand`]s — the interactive mirror of the CLI's
//! headless renderers. Knows nothing about how a beam executes.

pub mod input;
pub mod logs;
pub mod search;
pub mod state;
pub mod terminal;
pub mod ui;
