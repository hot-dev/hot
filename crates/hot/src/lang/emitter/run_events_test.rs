// Test to verify that run:start and run:stop events are properly emitted

#[cfg(test)]
mod tests {
    use super::super::{EngineEvent, EngineEventEmitter};
    use crate::lang::engine::Engine;
    use crate::lang::event::ExecutionContext;
    use crate::val::Val;
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

        fn get_events(&self) -> Vec<EngineEvent> {
            self.events.lock().unwrap().clone()
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

    #[test]
    fn test_compiled_lazy_args_emit_hot_data_repr_without_vm_instructions() {
        let test_emitter = Arc::new(TestEmitter::new());
        let execution_context = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );
        let engine = Engine::new_with_emitter(test_emitter.clone(), execution_context);

        let code = r#"
choose fn (lazy thenv: Any, lazy elsev: Any): Any {
    thenv
}

a "then"
b "else"
x choose(a, b)
"#;

        engine
            .eval_code(code, &[], &[], None, None)
            .expect("compiled Hot code should run");

        let events = test_emitter.get_events();
        let args = events
            .iter()
            .filter(|event| event.event_type == "call:start")
            .filter_map(|event| map_get(&event.event_data, "args"))
            .find(|args| contains_boxed_lambda(args))
            .expect("expected emitted call args to include a lazy lambda");

        let raw_json = serde_json::to_string(args).unwrap();
        assert!(raw_json.contains("\"$box\""));
        assert!(raw_json.contains("instructions"));

        let display = args.to_string();
        assert!(!display.contains("instructions"));
        assert!(!display.contains("register_count"));

        let normalized_json = serde_json::to_string(&args.to_hot_data_repr()).unwrap();
        assert!(!normalized_json.contains("\"$box\""));
        assert!(!normalized_json.contains("instructions"));
        assert!(!normalized_json.contains("register_count"));
        assert!(normalized_json.contains("\"$type\":\"::hot::type/Fn\""));
        assert!(normalized_json.contains("\"captures\""));
    }

    fn map_get<'a>(val: &'a Val, key: &str) -> Option<&'a Val> {
        if let Val::Map(map) = val {
            map.get(&Val::from(key))
        } else {
            None
        }
    }

    fn contains_boxed_lambda(val: &Val) -> bool {
        match val {
            Val::Box(boxed) => boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some(),
            Val::Vec(items) => items.iter().any(contains_boxed_lambda),
            Val::Map(map) => map
                .iter()
                .any(|(key, value)| contains_boxed_lambda(key) || contains_boxed_lambda(value)),
            _ => false,
        }
    }
}
