//! `alba cache <command>`: management of the on-disk cache. Dispatched in
//! `main.rs` *before* the Beamfile is loaded — see the call site.

use std::path::Path;

use crate::args::CacheCommand;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

pub fn run(beamfile: &Path, command: &CacheCommand) -> i32 {
    match command {
        CacheCommand::Clean => clean(beamfile),
    }
}

/// Removes the project's cache directory. A directory that does not exist
/// is a success — the user asked for there to be no cache, and there is
/// none.
fn clean(beamfile: &Path) -> i32 {
    let dir = super::run::cache_dir(beamfile);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            LineSink::stderr().line(&format!(
                "cannot clean the cache at {}: {error}",
                dir.display()
            ));
            EXIT_ALBA_ERROR
        }
    }
}
