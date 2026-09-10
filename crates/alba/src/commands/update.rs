//! `alba update`: replace this binary with the latest release. Dispatched
//! in `main.rs` *before* the Beamfile is loaded — see the call site.
//!
//! The work is delegated to `axoupdater`, the updater that pairs with the
//! `dist` installers this project releases through: it reads the install
//! receipt those installers write next to the user's configuration,
//! fetches the latest GitHub release, and reruns the matching installer.
//! An `alba` that arrived any other way (`cargo install`, a checkout) has
//! no receipt, and is told to update the way it was installed rather than
//! having a binary the installers never wrote overwritten under it.

use axoupdater::{AxoUpdater, AxoupdateError, Version};

use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

pub fn run() -> i32 {
    let mut updater = AxoUpdater::new_for("alba");
    match updater.load_receipt() {
        Ok(_) => {}
        Err(AxoupdateError::NoReceipt { .. }) => {
            LineSink::stderr().line(
                "this alba was not installed by the release installers, so it cannot update \
                 itself: run `cargo install alba` if that is how it was installed, or \
                 reinstall from https://github.com/albahq/alba/releases",
            );
            return EXIT_ALBA_ERROR;
        }
        Err(error) => return fail(&error),
    }
    // The receipt records the version the installer wrote, which is the
    // truth for a binary it put in place. Prefer this binary's own
    // version anyway: it is what the user is actually running.
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is a valid semver version");
    if let Err(error) = updater.set_current_version(current.clone()) {
        return fail(&error);
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error),
    };
    match runtime.block_on(updater.run()) {
        Ok(Some(update)) => {
            LineSink::stdout().line(&format!(
                "updated alba from {current} to {}",
                update.new_version
            ));
            0
        }
        Ok(None) => {
            LineSink::stdout().line(&format!("alba {current} is already the latest release"));
            0
        }
        Err(error) => fail(&error),
    }
}

fn fail(error: &dyn std::fmt::Display) -> i32 {
    LineSink::stderr().line(&format!("cannot update alba: {error}"));
    EXIT_ALBA_ERROR
}
