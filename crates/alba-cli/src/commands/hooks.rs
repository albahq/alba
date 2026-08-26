//! `alba hooks install|uninstall`: the scripts under `.alba/hooks/` and
//! the `core.hooksPath` that points git at them.
//!
//! One identical script per hook git knows, declared or not: `alba hook
//! <name>` answers `0` for an undeclared one, so declaring a hook later
//! needs no reinstall. Only the first install per clone matters.

use std::path::Path;

use alba_core::git::{KNOWN_HOOKS, hooks_path, repository_root, set_hooks_path, unset_hooks_path};

use crate::args::HooksCommand;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

/// The same `sh` script for every hook. Git for Windows runs hooks through
/// its bundled `sh`, so one script serves every platform. A machine
/// without `alba` on the PATH gets a clear failure and a blocked
/// operation, never a silently skipped hook.
const SCRIPT: &str = "#!/bin/sh\n\
command -v alba >/dev/null 2>&1 || { echo \"alba: not found on PATH, install it or run 'alba hooks uninstall'\" >&2; exit 1; }\n\
exec alba hook \"$(basename \"$0\")\" \"$@\"\n";

/// The `core.hooksPath` value Alba installs: `.alba/hooks` next to the
/// Beamfile, spelled relative to the repository root (`api/.alba/hooks`
/// for a project in a subdirectory), with `/` separators as git expects.
/// Also what `alba check` compares the current value against.
pub(crate) fn expected_hooks_path(beamfile: &Path) -> Result<String, String> {
    let project = alba_engine::beamfile_dir(beamfile);
    let repository = repository_root(&project).map_err(|error| error.to_string())?;
    let project = project.canonicalize().unwrap_or(project);
    let relative = project.strip_prefix(&repository).map_err(|_| {
        format!(
            "the project at {} is not inside the repository at {}",
            project.display(),
            repository.display()
        )
    })?;
    let mut path = relative.to_string_lossy().replace('\\', "/");
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(".alba/hooks");
    Ok(path)
}

/// The Beamfile was loaded (and therefore validated) by `main.rs` before
/// this runs; the scripts themselves do not depend on which hooks it
/// declares, so the project is not needed here.
pub fn run(beamfile: &Path, command: &HooksCommand) -> i32 {
    let mut err = LineSink::stderr();
    let root = alba_engine::beamfile_dir(beamfile);
    let expected = match expected_hooks_path(beamfile) {
        Ok(expected) => expected,
        Err(message) => {
            err.line(&message);
            return EXIT_ALBA_ERROR;
        }
    };
    let current = match hooks_path(&root) {
        Ok(current) => current,
        Err(error) => {
            err.line(&error.to_string());
            return EXIT_ALBA_ERROR;
        }
    };
    let dir = root.join(".alba").join("hooks");

    match command {
        HooksCommand::Install => {
            if let Some(current) = current.filter(|current| *current != expected) {
                err.line(&format!(
                    "core.hooksPath already points to `{current}`, remove it or uninstall that tool first"
                ));
                return EXIT_ALBA_ERROR;
            }
            if let Err(error) = write_scripts(&dir) {
                err.line(&format!(
                    "cannot write the hook scripts under {}: {error}",
                    dir.display()
                ));
                return EXIT_ALBA_ERROR;
            }
            if let Err(error) = set_hooks_path(&root, &expected) {
                err.line(&error.to_string());
                return EXIT_ALBA_ERROR;
            }
            LineSink::stdout().line(&format!(
                "\u{2713} hooks installed: core.hooksPath = {expected}"
            ));
            0
        }
        HooksCommand::Uninstall => {
            if current.as_deref() != Some(expected.as_str()) {
                err.line(&match current {
                    Some(current) => {
                        format!("core.hooksPath points to `{current}`, which Alba did not install")
                    }
                    None => "no hooks installed: core.hooksPath is not set".to_string(),
                });
                return EXIT_ALBA_ERROR;
            }
            if let Err(error) = unset_hooks_path(&root) {
                err.line(&error.to_string());
                return EXIT_ALBA_ERROR;
            }
            if let Err(error) = std::fs::remove_dir_all(&dir)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                err.line(&format!("cannot remove {}: {error}", dir.display()));
                return EXIT_ALBA_ERROR;
            }
            LineSink::stdout().line("\u{2713} hooks uninstalled");
            0
        }
    }
}

fn write_scripts(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for hook in KNOWN_HOOKS {
        let path = dir.join(hook.name);
        std::fs::write(&path, SCRIPT)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}
