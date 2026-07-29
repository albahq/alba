//! Filesystem-mutating builtins: `cp`, `mv`, `rm`, `mkdir`, `touch`. Each
//! resolves its paths against the shell's cwd (not the process's), never
//! touches stdin/stdout, and reports every failure as `NAME: detail` on
//! stderr.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::builtins::{command_error, take_flags, usage_error};
use crate::interp::Flow;
use crate::io::OutTarget;

/// `cp [-r] src... dst`: copies each `src` onto `dst`. More than one
/// source requires `dst` to already be an existing directory; a
/// directory source requires `-r`. The first failure stops the command
/// (remaining sources are not attempted) and exits 1; a malformed
/// invocation exits 2.
pub(crate) fn cp(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    let (flags, rest) = match take_flags(args, &["-r"]) {
        Ok(parsed) => parsed,
        Err(flag) => return usage_error(stderr, format_args!("cp: invalid option: {flag}")),
    };
    let Some((sources, dst)) = split_operands(rest) else {
        return usage_error(stderr, "cp: usage: cp [-r] src... dst");
    };
    let recursive = flags.contains(&"-r");

    let dst_path = cwd.join(dst);
    if sources.len() > 1 && !dst_path.is_dir() {
        return command_error(stderr, format_args!("cp: target is not a directory: {dst}"));
    }

    for source in sources {
        let src_path = cwd.join(source);
        let target = resolve_target(&dst_path, &src_path);
        let result = if src_path.is_dir() {
            if !recursive {
                return command_error(
                    stderr,
                    format_args!("cp: {source}: is a directory (not copied without -r)"),
                );
            }
            copy_tree(&src_path, &target)
        } else {
            std::fs::copy(&src_path, &target).map(|_| ())
        };
        if let Err(error) = result {
            return command_error(stderr, format_args!("cp: {source}: {error}"));
        }
    }
    Flow::Next(0)
}

/// `mv src... dst`: renames each `src` onto `dst`, falling back to a
/// copy-then-delete when the rename itself fails (crossing filesystems,
/// for instance) — for a directory just as much as for a file. Same
/// multiple-source rule as `cp`. No flags: a leading `-`-prefixed
/// argument is a usage error, not a source or destination name.
pub(crate) fn mv(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    let rest = match take_flags(args, &[]) {
        Ok((_, rest)) => rest,
        Err(flag) => return usage_error(stderr, format_args!("mv: invalid option: {flag}")),
    };
    let Some((sources, dst)) = split_operands(rest) else {
        return usage_error(stderr, "mv: usage: mv src... dst");
    };

    let dst_path = cwd.join(dst);
    if sources.len() > 1 && !dst_path.is_dir() {
        return command_error(stderr, format_args!("mv: target is not a directory: {dst}"));
    }

    for source in sources {
        let src_path = cwd.join(source);
        let target = resolve_target(&dst_path, &src_path);
        if let Err(error) = rename_or_copy(&src_path, &target) {
            return command_error(stderr, format_args!("mv: {source}: {error}"));
        }
    }
    Flow::Next(0)
}

/// `rm [-r] [-f] path...`: a directory needs `-r`; a path that does not
/// exist is an error unless `-f`, which also suppresses the nonzero
/// exit for it (a `-f`'d directory without `-r` is still an error: `-f`
/// only forgives missing paths, not the wrong type). Every path is
/// attempted; the exit code is 1 if any of them failed.
pub(crate) fn rm(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    let (flags, rest) = match take_flags(args, &["-r", "-f"]) {
        Ok(parsed) => parsed,
        Err(flag) => return usage_error(stderr, format_args!("rm: invalid option: {flag}")),
    };
    if rest.is_empty() {
        return usage_error(stderr, "rm: missing operand");
    }
    let recursive = flags.contains(&"-r");
    let force = flags.contains(&"-f");

    let mut writer = stderr.writer();
    let mut failed = false;
    for path in rest {
        let target = cwd.join(path);
        if !target.exists() {
            if !force {
                failed = true;
                let _ = writeln!(writer, "rm: {path}: no such file or directory");
            }
            continue;
        }
        let result = if target.is_dir() {
            if recursive {
                std::fs::remove_dir_all(&target)
            } else {
                failed = true;
                let _ = writeln!(writer, "rm: {path}: is a directory");
                continue;
            }
        } else {
            std::fs::remove_file(&target)
        };
        if let Err(error) = result {
            failed = true;
            let _ = writeln!(writer, "rm: {path}: {error}");
        }
    }
    Flow::Next(if failed { 1 } else { 0 })
}

/// `mkdir [-p] dir...`: without `-p`, an existing directory or a missing
/// parent is an error — exactly what `create_dir` already reports,
/// unlike `create_dir_all`, which tolerates both. Every directory is
/// attempted; the exit code is 1 if any of them failed.
pub(crate) fn mkdir(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    let (flags, rest) = match take_flags(args, &["-p"]) {
        Ok(parsed) => parsed,
        Err(flag) => return usage_error(stderr, format_args!("mkdir: invalid option: {flag}")),
    };
    if rest.is_empty() {
        return usage_error(stderr, "mkdir: missing operand");
    }
    let parents = flags.contains(&"-p");

    let mut writer = stderr.writer();
    let mut failed = false;
    for dir in rest {
        let target = cwd.join(dir);
        let result = if parents {
            std::fs::create_dir_all(&target)
        } else {
            std::fs::create_dir(&target)
        };
        if let Err(error) = result {
            failed = true;
            let _ = writeln!(writer, "mkdir: {dir}: {error}");
        }
    }
    Flow::Next(if failed { 1 } else { 0 })
}

/// `touch file...`: creates an empty file if it does not exist, else
/// updates its mtime to now. Every file is attempted; the exit code is 1
/// if any of them failed. No flags: a leading `-`-prefixed argument is a
/// usage error, not a file name.
pub(crate) fn touch(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    let rest = match take_flags(args, &[]) {
        Ok((_, rest)) => rest,
        Err(flag) => return usage_error(stderr, format_args!("touch: invalid option: {flag}")),
    };
    if rest.is_empty() {
        return usage_error(stderr, "touch: missing operand");
    }

    let mut writer = stderr.writer();
    let mut failed = false;
    for path in rest {
        let target = cwd.join(path);
        let result = if target.exists() {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        } else {
            std::fs::File::create(&target).map(|_| ())
        };
        if let Err(error) = result {
            failed = true;
            let _ = writeln!(writer, "touch: {path}: {error}");
        }
    }
    Flow::Next(if failed { 1 } else { 0 })
}

/// Splits `args` into every operand but the last (the sources) and the
/// last one (the destination); `None` when there are fewer than two
/// operands to split.
fn split_operands(args: &[String]) -> Option<(&[String], &str)> {
    if args.len() < 2 {
        return None;
    }
    let (sources, dst) = args.split_at(args.len() - 1);
    Some((sources, dst[0].as_str()))
}

/// Where one `src` lands under `dst`: joined onto it when `dst` is an
/// existing directory (`cp a.txt somedir` copies to `somedir/a.txt`),
/// otherwise `dst` itself.
fn resolve_target(dst: &Path, src: &Path) -> PathBuf {
    match src.file_name() {
        Some(name) if dst.is_dir() => dst.join(name),
        _ => dst.to_path_buf(),
    }
}

/// Recursively copies the directory `src` onto `dst`, creating `dst`
/// (and any of its own missing parents) first.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// `mv`'s rename-first, copy-and-delete-on-failure strategy, for a file
/// or a directory alike.
fn rename_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if src.is_dir() {
        copy_tree(src, dst)?;
        std::fs::remove_dir_all(src)
    } else {
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)
    }
}
