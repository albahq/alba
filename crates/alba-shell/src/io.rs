//! Where a command's three standard streams come from and go to.
//!
//! One [`CommandIo`] describes a single command's stdin/stdout/stderr,
//! whatever they happen to be: the run's output channel, an in-memory
//! capture buffer (command substitution), a redirected file, or one end
//! of an `os_pipe` connecting two pipeline stages. Both dispatch paths
//! consume it — `spawn.rs` turns each target into a `std::process::Stdio`
//! ([`OutTarget::into_stdio`]) and drains the child's handle back into
//! it ([`OutTarget::forward`]), builtins turn theirs into a blocking
//! [`std::io::Write`] ([`OutTarget::writer`]).
//!
//! Ownership is the whole point of the module: a pipe's write end must
//! reach exactly one owner and be dropped as soon as that owner is done,
//! or the reader on the other side never observes EOF. Every target here
//! is therefore move-only, and cloning is explicit ([`OutTarget::try_clone`],
//! which `2>&1` and the per-command defaults both need).

use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::{ShellOutputLine, ShellStream};

/// Where one command's output stream goes.
pub(crate) enum OutTarget {
    /// The run's output channel, one [`ShellOutputLine`] per line, all
    /// tagged `stream`. `2>&1` works by cloning the stdout target — tag
    /// included — over the stderr one, which is exactly what makes the
    /// redirected lines arrive tagged `Stdout`.
    Lines {
        tx: UnboundedSender<ShellOutputLine>,
        stream: ShellStream,
    },
    /// An in-memory buffer collecting raw bytes, for command
    /// substitution (see `interp::execute_captured`).
    Capture(Arc<Mutex<Vec<u8>>>),
    /// A file opened by a `>`, `>>`, `2>` or `2>>` redirect.
    File(std::fs::File),
    /// The write end of the pipe joining this pipeline stage to the next.
    Pipe(os_pipe::PipeWriter),
}

/// Where one command's input comes from.
pub(crate) enum InTarget {
    /// No input: an immediate EOF. The default for every command that
    /// neither redirects stdin nor follows another pipeline stage.
    Null,
    /// A file opened by a `<` redirect.
    File(std::fs::File),
    /// The read end of the pipe joining this pipeline stage to the
    /// previous one.
    Pipe(os_pipe::PipeReader),
}

/// A single command's three standard streams.
pub(crate) struct CommandIo {
    pub stdin: InTarget,
    pub stdout: OutTarget,
    pub stderr: OutTarget,
}

impl CommandIo {
    /// The default io for a command whose stdout goes to `stdout`
    /// (the enclosing level's stdout: the run's channel, or a capture
    /// buffer) and whose stderr always reaches the run's channel.
    pub(crate) fn defaults(
        stdout: &OutTarget,
        output: &UnboundedSender<ShellOutputLine>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            stdin: InTarget::Null,
            stdout: stdout.try_clone()?,
            stderr: OutTarget::Lines {
                tx: output.clone(),
                stream: ShellStream::Stderr,
            },
        })
    }

    /// Whether using these streams can block the calling thread. A file
    /// or a pipe can — a full pipe blocks its writer until the reader
    /// drains, an empty one blocks its reader until the writer produces
    /// — so a builtin holding any of the three belongs in
    /// `tokio::task::spawn_blocking`; the channel and the capture buffer
    /// never do, and `Null` yields EOF immediately.
    ///
    /// Stdin counts even though no builtin reads it yet: the moment one
    /// does, a last pipeline stage would have a non-blocking stdout and
    /// a blocking stdin, and running it inline would park a runtime
    /// thread on the pipe feeding it — quite possibly the very thread
    /// its own producer needs.
    pub(crate) fn can_block(&self) -> bool {
        self.stdin.can_block() || self.stdout.can_block() || self.stderr.can_block()
    }
}

impl OutTarget {
    /// A second, independent handle on the same destination. Needed by
    /// `2>&1` (stderr becomes a clone of the *current* stdout) and by
    /// [`CommandIo::defaults`]. Fallible because duplicating a file
    /// descriptor is.
    pub(crate) fn try_clone(&self) -> std::io::Result<Self> {
        Ok(match self {
            Self::Lines { tx, stream } => Self::Lines {
                tx: tx.clone(),
                stream: *stream,
            },
            Self::Capture(buffer) => Self::Capture(buffer.clone()),
            Self::File(file) => Self::File(file.try_clone()?),
            Self::Pipe(writer) => Self::Pipe(writer.try_clone()?),
        })
    }

    fn can_block(&self) -> bool {
        matches!(self, Self::File(_) | Self::Pipe(_))
    }

    /// Turns this target into something a child process can be spawned
    /// with. A file or a pipeline neighbour's pipe is a file descriptor
    /// already, so it is handed straight over and nothing comes back.
    /// The channel and the capture buffer are not, so the child gets
    /// `Stdio::piped()` and this target comes back as the sink the
    /// caller must [`forward`](Self::forward) the child's handle into.
    ///
    /// Deliberately *tokio's* pipe rather than one made here: only a
    /// handle tokio created can be read with cancel-safe async io on
    /// both unix and windows, and only an async reader can be aborted
    /// when `spawn::run_external`'s drain gives up. An `os_pipe` read
    /// would have to happen on a blocking thread that nothing can
    /// interrupt, which would pin that thread — and, since dropping a
    /// runtime waits for its blocking tasks, the whole host process —
    /// for as long as a stray descendant held the write end open.
    ///
    /// The returned `Stdio` may own this process's only remaining handle
    /// on a pipe's write end (the `File`/`Pipe` arms), so the caller
    /// must drop it — by dropping the `Command` it was given to — right
    /// after spawning.
    pub(crate) fn into_stdio(self) -> (Stdio, Option<Self>) {
        match self {
            Self::File(file) => (Stdio::from(file), None),
            Self::Pipe(writer) => (Stdio::from(writer), None),
            sink @ (Self::Lines { .. } | Self::Capture(_)) => (Stdio::piped(), Some(sink)),
        }
    }

    /// Drains `reader` — a child's piped stdout or stderr — into this
    /// target, to EOF or to the first error.
    ///
    /// Only ever called on the two targets [`into_stdio`](Self::into_stdio)
    /// hands back, whose writes never block: sending on an unbounded
    /// channel and appending to a buffer both return immediately, so
    /// doing them from an async task is safe. Reading, the part that
    /// does wait, is asynchronous and therefore abortable.
    pub(crate) async fn forward(self, mut reader: impl AsyncRead + Unpin) {
        let mut writer = self.writer();
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if writer.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                }
            }
        }
    }

    /// A blocking writer onto this target, for builtins (and for
    /// [`forward`](Self::forward)). Writing a `Lines` target emits one
    /// [`ShellOutputLine`] per `\n`, with a final unterminated line
    /// flushed on drop.
    pub(crate) fn writer(self) -> Box<dyn Write + Send> {
        match self {
            Self::Lines { tx, stream } => Box::new(LineWriter {
                tx,
                stream,
                pending: Vec::new(),
            }),
            Self::Capture(buffer) => Box::new(CaptureWriter { buffer }),
            Self::File(file) => Box::new(file),
            Self::Pipe(writer) => Box::new(writer),
        }
    }
}

impl InTarget {
    pub(crate) fn into_stdio(self) -> Stdio {
        match self {
            Self::Null => Stdio::null(),
            Self::File(file) => Stdio::from(file),
            Self::Pipe(reader) => Stdio::from(reader),
        }
    }

    fn can_block(&self) -> bool {
        matches!(self, Self::File(_) | Self::Pipe(_))
    }

    /// A blocking reader onto this target, for a builtin that reads its
    /// stdin (`cat`). `Null` yields immediate EOF; a file or a
    /// pipeline neighbour's pipe reads exactly as any other [`Read`]
    /// would.
    pub(crate) fn reader(self) -> Box<dyn Read + Send> {
        match self {
            Self::Null => Box::new(std::io::empty()),
            Self::File(file) => Box::new(file),
            Self::Pipe(reader) => Box::new(reader),
        }
    }
}

/// Splits everything written to it into lines and sends one
/// [`ShellOutputLine`] per line, stripping a `\r` that immediately
/// precedes the `\n` (the CRLF a windows child produces). Whatever is
/// left unterminated when the writer is dropped is emitted as a final
/// line, so the last line of a command that does not end its output with
/// a newline is never lost.
struct LineWriter {
    tx: UnboundedSender<ShellOutputLine>,
    stream: ShellStream,
    pending: Vec<u8>,
}

impl LineWriter {
    fn emit(&self, bytes: &[u8]) {
        let bytes = match bytes.last() {
            Some(b'\r') => &bytes[..bytes.len() - 1],
            _ => bytes,
        };
        // If the receiver has been dropped nobody is listening anymore;
        // keep accepting writes regardless, so a command is never
        // blocked on output no one wants.
        let _ = self.tx.send(ShellOutputLine {
            stream: self.stream,
            text: String::from_utf8_lossy(bytes).into_owned(),
        });
    }
}

impl Write for LineWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(data);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.emit(&line[..end]);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.emit(&pending);
        }
    }
}

/// Appends every byte written to the shared capture buffer. Unlike
/// [`LineWriter`] nothing is split or trimmed: command substitution
/// needs the raw bytes, so that interior newlines survive while only the
/// trailing ones are stripped (in `expand.rs`).
struct CaptureWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buffer
            .lock()
            .expect("capture buffer poisoned")
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut writer = OutTarget::Lines {
                tx,
                stream: ShellStream::Stdout,
            }
            .writer();
            for chunk in chunks {
                writer.write_all(chunk).unwrap();
            }
        }
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line.text);
        }
        lines
    }

    #[test]
    fn splits_whole_lines() {
        assert_eq!(collect(&[b"first\nsecond\n"]), vec!["first", "second"]);
    }

    #[test]
    fn strips_a_cr_before_the_newline() {
        assert_eq!(collect(&[b"hello\r\nworld\r\n"]), vec!["hello", "world"]);
    }

    #[test]
    fn a_lone_cr_not_followed_by_newline_is_preserved() {
        assert_eq!(collect(&[b"a\rb\n"]), vec!["a\rb"]);
    }

    #[test]
    fn flushes_a_final_unterminated_line_on_drop() {
        assert_eq!(collect(&[b"first\nsecond"]), vec!["first", "second"]);
    }

    #[test]
    fn reassembles_a_line_split_across_writes() {
        assert_eq!(collect(&[b"he", b"ll", b"o\n"]), vec!["hello"]);
    }

    #[test]
    fn writing_nothing_emits_nothing() {
        assert_eq!(collect(&[]), Vec::<String>::new());
    }

    #[test]
    fn handles_a_very_long_line() {
        let long_line = "x".repeat(200_000);
        let mut data = long_line.clone().into_bytes();
        data.push(b'\n');
        assert_eq!(collect(&[&data]), vec![long_line]);
    }

    #[test]
    fn a_capture_writer_keeps_every_byte_verbatim() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        {
            let mut writer = OutTarget::Capture(buffer.clone()).writer();
            writer.write_all(b"one\ntwo").unwrap();
        }
        assert_eq!(&*buffer.lock().unwrap(), b"one\ntwo");
    }
}
