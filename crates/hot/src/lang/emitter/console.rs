use super::{EngineEvent, EngineEventEmitter};

pub struct ConsoleEngineEventEmitter;

impl ConsoleEngineEventEmitter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConsoleEngineEventEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineEventEmitter for ConsoleEngineEventEmitter {
    fn emit(&self, event: EngineEvent) {
        println!(
            "[{}] {:<15} {} - {}",
            event.event_id,
            event.event_type,
            event.event_time.format("%H:%M:%S%.6f"),
            event.event_data
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::event::ExecutionContext;
    use crate::val::Val;
    use uuid::Uuid;

    #[test]
    fn test_console_event_emitter() {
        let event_emitter = ConsoleEngineEventEmitter::new();
        let run_id = Uuid::now_v7();

        let stream_id = Uuid::now_v7();
        let execution_context = ExecutionContext::new(
            run_id,
            stream_id,
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );

        // Test run events
        let run_start_event = EngineEvent::run_start(&execution_context);
        event_emitter.emit(run_start_event);

        let run_stop_event = EngineEvent::run_stop(&execution_context, Val::Null);
        event_emitter.emit(run_stop_event);
    }
}
