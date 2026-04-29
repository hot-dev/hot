// Test module for emitter functionality

#[cfg(test)]
mod tests {
    use super::super::ConsoleEngineEventEmitter;
    use crate::lang::bytecode::{SourceLocation, VariableMetadata};
    use crate::lang::engine::Engine;
    use crate::lang::event::ExecutionContext;
    use crate::val::Val;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn test_console_emitter_integration() {
        // Create a console emitter
        let emitter = Arc::new(ConsoleEngineEventEmitter::new());

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
        let _engine = Engine::new_with_emitter(emitter, execution_context);

        // Test passes if it compiles and creates successfully
        println!("Emitter integration test passed!");
    }

    #[test]
    fn test_variable_metadata_creation() {
        let metadata = VariableMetadata {
            name: "test_var".to_string(),
            namespace: "::hot::test".to_string(),
            static_scope: Some("::hot::test".to_string()),
            meta: Some(Val::from("test_meta")),
            source: Some(SourceLocation {
                file: Some("test.hot".to_string()),
                line: 10,
                column: 5,
                position: 100,
                length: 8,
            }),
        };

        assert_eq!(metadata.name, "test_var");
        assert_eq!(metadata.namespace, "::hot::test");
        assert!(metadata.source.is_some());
    }

    #[tokio::test]
    async fn test_database_emitter_integration() {
        use super::super::DatabaseEngineEventEmitter;
        use crate::val;

        // Create a database emitter with in-memory SQLite
        let emitter = Arc::new(DatabaseEngineEventEmitter::new(val!({
            "uri": "sqlite::memory:",
        })));

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
        let _engine = Engine::new_with_emitter(emitter, execution_context);

        // Test passes if it compiles and creates successfully
        println!("Database emitter integration test passed!");
    }
}
