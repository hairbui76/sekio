//! Cold-start instrumentation. A spacebar previewer lives or dies on
//! time-to-first-paint (ROADMAP target: <100 ms), so the milestones are
//! measured rather than assumed. Enabled with `--timing`; a no-op otherwise.

use std::time::Instant;

#[derive(Clone, Copy)]
pub struct Timing {
    start: Instant,
    enabled: bool,
}

impl Timing {
    pub fn start(enabled: bool) -> Self {
        Self {
            start: Instant::now(),
            enabled,
        }
    }

    /// Milliseconds since process start.
    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Log a milestone as "<label>: N.N ms since start".
    pub fn log(&self, label: &str) {
        if self.enabled {
            eprintln!("[sekio-gui] {label}: {:.1} ms", self.elapsed_ms());
        }
    }
}
