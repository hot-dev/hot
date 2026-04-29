// Test to verify that run:start and run:stop events are properly emitted

#[cfg(test)]
mod tests {
    use super::super::{EngineEvent, EngineEventEmitter};
    use crate::lang::engine::Engine;
    use crate::lang::event::ExecutionContext;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// Test emitter that captures events for verification
    struct TestEmitter {
        events: Arc<Mutex<Vec<EngineEvent>>>,
    }

    impl TestEmitter {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn get_run_events(&self) -> Vec<EngineEvent> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.event_type.starts_with("run:"))
                .cloned()
                .collect()
        }
    }

    impl EngineEventEmitter for TestEmitter {
        fn emit(&self, event: EngineEvent) {
            println!(
                "Captured event: {} - {:?}",
                event.event_type, event.event_data
            );
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn test_run_events_are_emitted() {
        // Create a test emitter to capture events
        let test_emitter = Arc::new(TestEmitter::new());

        // Create execution context
        let execution_context = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(), // stream_id
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );

        // Create engine with emitter
        let engine = Engine::new_with_emitter(test_emitter.clone(), execution_context);

        // Execute some simple Hot code
        let result = engine.eval_code("x 42", &[], &[], None, None);

        match result {
            Ok(_) => {
                println!("Code execution successful");
            }
            Err(e) => {
                println!("Code execution failed: {}", e);
                // Continue with test even if execution fails
            }
        }

        // Check if run events were captured
        let run_events = test_emitter.get_run_events();
        println!("Captured {} run events", run_events.len());

        for (i, event) in run_events.iter().enumerate() {
            println!(
                "Run Event {}: {} at {}",
                i, event.event_type, event.event_time
            );
        }

        // We should have at least run:start and run:stop events
        let start_events: Vec<_> = run_events
            .iter()
            .filter(|e| e.event_type == "run:start")
            .collect();
        let stop_events: Vec<_> = run_events
            .iter()
            .filter(|e| e.event_type == "run:stop")
            .collect();

        println!(
            "Start events: {}, Stop events: {}",
            start_events.len(),
            stop_events.len()
        );

        // Verify we have at least one start and one stop event
        assert!(
            !start_events.is_empty(),
            "Expected at least one run:start event, got {}",
            start_events.len()
        );
        assert!(
            !stop_events.is_empty(),
            "Expected at least one run:stop event, got {}",
            stop_events.len()
        );

        // Verify the events have the same run_id
        if let (Some(start), Some(stop)) = (start_events.first(), stop_events.first()) {
            assert_eq!(
                start.execution_context.run_id, stop.execution_context.run_id,
                "Start and stop events should have the same run_id"
            );
        }

        println!("✅ Run events test passed!");
    }

    #[test]
    fn test_multiple_executions_emit_separate_run_events() {
        // Create a test emitter to capture events
        let test_emitter = Arc::new(TestEmitter::new());

        // Create execution context
        let execution_context = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(), // stream_id
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );

        // Create engine with emitter
        let engine = Engine::new_with_emitter(test_emitter.clone(), execution_context);

        // Execute multiple pieces of code
        let _ = engine.eval_code("x 1", &[], &[], None, None);
        let _ = engine.eval_code("y 2", &[], &[], None, None);
        let _ = engine.eval_code("z 3", &[], &[], None, None);

        // Check if run events were captured for each execution
        let run_events = test_emitter.get_run_events();
        println!(
            "Captured {} run events from multiple executions",
            run_events.len()
        );

        let start_events: Vec<_> = run_events
            .iter()
            .filter(|e| e.event_type == "run:start")
            .collect();
        let stop_events: Vec<_> = run_events
            .iter()
            .filter(|e| e.event_type == "run:stop")
            .collect();

        println!(
            "Start events: {}, Stop events: {}",
            start_events.len(),
            stop_events.len()
        );

        // We should have multiple start and stop events (one pair per execution)
        assert!(
            start_events.len() >= 3,
            "Expected at least 3 run:start events, got {}",
            start_events.len()
        );
        assert!(
            stop_events.len() >= 3,
            "Expected at least 3 run:stop events, got {}",
            stop_events.len()
        );

        println!("✅ Multiple executions run events test passed!");
    }
}
