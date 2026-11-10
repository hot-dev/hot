// Opt-in phase timing for the compile/run pipeline.
//
// Set `DEBUG_TIMING=1` to print one line per pipeline phase to stderr:
//
//     DEBUG_TIMING=1 hot eval '1'
//
// The check is one cached boolean, so an unset variable costs a load and a
// branch per phase.

use std::sync::OnceLock;
use std::time::Instant;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("DEBUG_TIMING").is_some())
}

/// Times a phase from construction to drop.
pub(crate) struct Phase {
    name: &'static str,
    started: Option<Instant>,
}

/// Start timing `name`. Prints when the returned guard drops.
pub(crate) fn phase(name: &'static str) -> Phase {
    Phase {
        name,
        started: enabled().then(Instant::now),
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            eprintln!(
                "phase {:<28} {:>9.3} ms",
                self.name,
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
