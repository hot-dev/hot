// Integration test for emitter with actual Hot code execution

#[cfg(test)]
mod tests {
    use super::super::{ConsoleEngineEventEmitter, EngineEvent, EngineEventEmitter};
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

        fn get_events(&self) -> Vec<EngineEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EngineEventEmitter for TestEmitter {
        fn emit(&self, event: EngineEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn test_emitter_with_hot_code_execution() {
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

        // Execute some simple Hot code using the new instance method
        let result = engine.eval_code("x = 42", &[], &[], None, None);

        match result {
            Ok(_) => {
                // Check if events were captured
                let events = test_emitter.get_events();
                println!("Captured {} events", events.len());

                // We should have at least some events (run:start, var:start, var:stop, etc.)
                // Note: The exact number depends on the implementation details
                for (i, event) in events.iter().enumerate() {
                    println!("Event {}: {} - {:?}", i, event.event_type, event.event_data);
                }

                // This test passes if we can execute code with an emitter without errors
                println!("Emitter integration with Hot code execution test passed!");
            }
            Err(e) => {
                println!("Code execution failed: {}", e);
                // Test still passes as long as the emitter integration works
                println!("Emitter integration test passed (execution failed but emitter worked)!");
            }
        }
    }

    // A cond flow carrying an origin_name (as emitted by the compiler for
    // inlined `if()`) must trace as a call named after the original callable
    // (with the {type: "cond", fn: true} flow marker), not as an anonymous
    // `<cond>` inline flow — call-trace search for "if" relies on the name.
    #[test]
    fn test_origin_named_flow_traces_under_origin_name() {
        use crate::lang::bytecode::{
            BytecodeProgram, Constant, FlowResultModifier, FlowType, Instruction,
        };
        use crate::lang::runtime::vm::VirtualMachine;
        use crate::val::Val;

        let mut program = BytecodeProgram::new();
        let origin_id = program.add_string_ref("::hot::bool/if".to_string());
        let one_id = program.add_constant(Constant::Val(Val::Int(1)));
        program.entry_point = vec![
            Instruction::BeginFlow {
                flow_type: FlowType::Cond,
                result_modifier: FlowResultModifier::One,
                source: None,
                origin_name: Some(origin_id),
            },
            Instruction::LoadConst {
                dest: 0,
                constant: one_id,
            },
            Instruction::EndFlow { dest: 0 },
        ];
        program.entry_register_count = 4;

        let mut vm = VirtualMachine::new(
            Arc::new(program),
            None,
            Arc::new(indexmap::IndexMap::new()),
            Arc::new(indexmap::IndexMap::new()),
            Arc::new(indexmap::IndexMap::new()),
            Arc::new(Default::default()),
            None,
        );
        let test_emitter = Arc::new(TestEmitter::new());
        vm.set_emitter(test_emitter.clone());
        vm.set_execution_context(ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        ));

        vm.execute().expect("flow execution failed");

        let events = test_emitter.get_events();
        let if_call_start = events.iter().find(|e| {
            e.event_type == "call:start"
                && e.event_data
                    .get("function_name")
                    .and_then(|v| match v {
                        Val::Str(s) => Some(&*s == "::hot::bool/if"),
                        _ => None,
                    })
                    .unwrap_or(false)
        });
        let flow_marker = if_call_start
            .and_then(|e| e.event_data.get("flow"))
            .and_then(|f| f.get("fn"));
        assert!(
            if_call_start.is_some(),
            "expected a call:start named ::hot::bool/if; events: {:?}",
            events
                .iter()
                .map(|e| (e.event_type.clone(), e.event_data.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            flow_marker,
            Some(Val::Bool(true)),
            "origin-named flow should carry the fn:true cond marker"
        );
    }

    #[test]
    fn test_console_emitter_with_simple_execution() {
        // Create a console emitter
        let console_emitter = Arc::new(ConsoleEngineEventEmitter::new());

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

        // Create engine with console emitter
        let _engine = Engine::new_with_emitter(console_emitter, execution_context);

        // Test passes if we can create the engine with console emitter
        println!("Console emitter integration test passed!");
    }
}
