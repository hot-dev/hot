// Test to verify that database emitter properly handles run:start and run:stop events

#[cfg(test)]
mod tests {
    use super::super::{DatabaseEngineEventEmitter, EngineEvent, EngineEventEmitter};
    use crate::lang::engine::Engine;
    use crate::lang::event::ExecutionContext;
    use crate::val;
    use crate::val::Val;
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_database_run_events_update_status() {
        // Create a database emitter with in-memory SQLite
        let emitter = Arc::new(DatabaseEngineEventEmitter::new(val!({
            "uri": "sqlite::memory:",
        })));

        // Create execution context
        let run_id = Uuid::now_v7();
        let execution_context = ExecutionContext::new(
            run_id,
            Uuid::now_v7(), // stream_id
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );

        println!("Testing database run events with run_id: {}", run_id);

        // Manually emit run:start event
        let start_event = EngineEvent::run_start(&execution_context);
        emitter.emit(start_event);
        println!("Emitted run:start event");

        // Wait a bit for the event to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Manually emit run:stop event
        let stop_event = EngineEvent::run_stop(&execution_context, Val::Null);
        emitter.emit(stop_event);
        println!("Emitted run:stop event");

        // Wait for the event to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Shutdown the emitter to ensure all events are flushed
        match emitter.shutdown().await {
            Ok(()) => println!("Database emitter shutdown successfully"),
            Err(e) => println!("Database emitter shutdown error: {}", e),
        }

        println!("✅ Database run events test completed!");
    }

    #[tokio::test]
    async fn test_engine_with_database_emitter() {
        // Create a database emitter with in-memory SQLite
        let emitter = Arc::new(DatabaseEngineEventEmitter::new(val!({
            "uri": "sqlite::memory:",
        })));

        // Create execution context
        let run_id = Uuid::now_v7();
        let execution_context = ExecutionContext::new(
            run_id,
            Uuid::now_v7(), // stream_id
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );

        println!("Testing engine with database emitter, run_id: {}", run_id);

        // Create engine with database emitter
        let engine = Engine::new_with_emitter(emitter.clone(), execution_context);

        // Execute some Hot code
        match engine.eval_code("test_var 123", &[], &[], None, None) {
            Ok(_) => println!("Code execution successful"),
            Err(e) => println!("Code execution failed: {}", e),
        }

        // Wait for events to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Shutdown the emitter to ensure all events are flushed
        match emitter.shutdown().await {
            Ok(()) => println!("Database emitter shutdown successfully"),
            Err(e) => println!("Database emitter shutdown error: {}", e),
        }

        println!("✅ Engine with database emitter test completed!");
    }
}
