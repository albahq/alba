//! The grouped renderer: each beam's output is held back and printed as
//! one block, under a header, the moment that beam finishes.
//!
//! The default when stdout is not a terminal — a log file or a CI job
//! record is read after the fact, where one contiguous block per beam is
//! far easier to follow than the interleaved live view.

use std::collections::HashMap;
use std::io;

use alba_engine::RunEvent;

use super::{LineSink, Renderer, format_duration, print_summary, status_label};

pub struct GroupedRenderer {
    /// Lines seen so far, per beam that has not finished yet. Only ever
    /// looked up and removed by a known id, never iterated in the normal
    /// path: the flush order is the order beams finish in (a real event
    /// order the engine produced), not this map's. The one exception is
    /// the orphan drain in the `RunFinished` arm, which sorts by id
    /// precisely so it does not leak this map's iteration order either.
    buffers: HashMap<String, Vec<String>>,
    out: LineSink<io::Stdout>,
    err: LineSink<io::Stderr>,
}

impl GroupedRenderer {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            out: LineSink::stdout(),
            err: LineSink::stderr(),
        }
    }

    /// Prints `header`, then everything buffered for `id`, and forgets the
    /// beam.
    fn flush(&mut self, id: &str, header: &str) {
        self.out.line(header);
        // A beam cancelled before it ever started has no buffer: its
        // header alone is the whole group.
        for line in self.buffers.remove(id).unwrap_or_default() {
            self.out.line(&line);
        }
    }
}

impl Renderer for GroupedRenderer {
    fn handle(&mut self, event: &RunEvent) {
        match event {
            // A beam that produces no output at all still gets a group,
            // so the header is printed for every beam that ran rather
            // than only for the talkative ones.
            RunEvent::BeamStarted { id } => {
                self.buffers.entry(id.0.clone()).or_default();
            }
            RunEvent::BeamOutput { id, line } => {
                self.buffers
                    .entry(id.0.clone())
                    .or_default()
                    .push(line.text.clone());
            }
            RunEvent::BeamFinished {
                id,
                status,
                duration,
            } => {
                let header = format!(
                    "\u{2500}\u{2500} {} ({}, {}) \u{2500}\u{2500}",
                    id.0,
                    format_duration(*duration),
                    status_label(status)
                );
                self.flush(&id.0, &header);
            }
            RunEvent::RunFinished { summary } => {
                // Every beam the engine starts is also finished, so this
                // normally drains nothing. It exists so that buffering can
                // never turn into *losing* output: a beam whose
                // `BeamFinished` went missing would otherwise have
                // everything it printed silently discarded, a far worse
                // failure than an oddly labelled group. Sorted by id so
                // the fallback order is stable rather than the map's.
                let mut orphans: Vec<String> = self.buffers.keys().cloned().collect();
                orphans.sort();
                for id in orphans {
                    let header = format!("\u{2500}\u{2500} {id} (unfinished) \u{2500}\u{2500}");
                    self.flush(&id, &header);
                }
                print_summary(&mut self.err, summary);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::RunSummary;
    use alba_executors::{OutputLine, Stream};

    fn output(id: &str, text: &str) -> RunEvent {
        RunEvent::BeamOutput {
            id: BeamId(id.to_string()),
            line: OutputLine {
                stream: Stream::Stdout,
                text: text.to_string(),
            },
        }
    }

    /// Buffered output must never be lost, even for a beam whose
    /// `BeamFinished` never arrives. Asserted on the buffers rather than
    /// on captured stdout, which a unit test cannot intercept.
    #[test]
    fn the_final_event_drains_any_beam_left_unfinished() {
        let mut renderer = GroupedRenderer::new();

        renderer.handle(&output("orphan", "would be lost"));
        assert!(renderer.buffers.contains_key("orphan"));

        renderer.handle(&RunEvent::RunFinished {
            summary: RunSummary::default(),
        });

        assert!(
            renderer.buffers.is_empty(),
            "a beam left unfinished must still be flushed, not dropped"
        );
    }
}
