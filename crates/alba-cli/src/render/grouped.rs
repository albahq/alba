//! The grouped renderer: each beam's output is held back and printed as
//! one block, under a header, the moment that beam finishes.
//!
//! The default when stdout is not a terminal — a log file or a CI job
//! record is read after the fact, where one contiguous block per beam is
//! far easier to follow than the interleaved live view.

use std::collections::HashMap;

use alba_engine::RunEvent;

use super::{Renderer, format_duration, print_summary, status_label};

#[derive(Default)]
pub struct GroupedRenderer {
    /// Lines seen so far, per beam that has not finished yet. Only ever
    /// looked up and removed by a known id, never iterated: the flush
    /// order is the order beams finish in (a real event order the engine
    /// produced), not this map's.
    buffers: HashMap<String, Vec<String>>,
}

impl GroupedRenderer {
    pub fn new() -> Self {
        Self::default()
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
                println!(
                    "\u{2500}\u{2500} {} ({}, {}) \u{2500}\u{2500}",
                    id.0,
                    format_duration(*duration),
                    status_label(status)
                );
                // A beam cancelled before it ever started has no buffer:
                // its header alone is the whole group.
                for line in self.buffers.remove(&id.0).unwrap_or_default() {
                    println!("{line}");
                }
            }
            RunEvent::RunFinished { summary } => print_summary(summary),
        }
    }
}
