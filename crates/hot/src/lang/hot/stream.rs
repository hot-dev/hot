//! Stream module - user-emitted data during run execution
//!
//! This module provides the `::hot::stream/data` function for emitting
//! streaming data during run execution (partial results, SSE events, progress, etc.)

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::stream::StreamPublisher; // Import the trait for the publish method
use crate::val::Val;
use crate::validate_args;
use uuid::Uuid;

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

/// Emit data to the current stream
///
/// This function emits typed data associated with the current run's stream.
/// The data is published immediately to Redis Streams for real-time SSE delivery.
/// Stream data is ephemeral and NOT persisted to the database.
///
/// # Arguments
/// * `data_type` - A string identifying the type of data (e.g., "http:sse:event", "progress", "llm:token")
/// * `payload` - The actual data payload (any Hot value)
///
/// # Returns
/// * `null` on success
/// * Error if no run context is available
pub fn data(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::stream/data", args, 2);

    // Get the data type
    let data_type: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::stream/data: data_type must be a string".to_string(),
            ));
        }
    };

    // Get the payload (can be any value)
    let payload = &args[1];

    // Get execution context from the VM
    let execution_context = match vm.get_execution_context() {
        Some(ctx) => ctx.clone(),
        None => {
            return HotResult::Err(err_val(
                "::hot::stream/data: no execution context available (not running in a run)"
                    .to_string(),
            ));
        }
    };

    let run_id = execution_context.run_id;
    let stream_id = execution_context.stream_id;
    let env_id = execution_context.env_id;

    // Validate we have the required IDs
    if stream_id == Uuid::nil() {
        return HotResult::Err(err_val(
            "::hot::stream/data: no stream_id in execution context".to_string(),
        ));
    }

    // Generate a new UUIDv7 for this stream data (provides ordering)
    let stream_data_id = Uuid::now_v7();

    crate::validate_record_expansion!("::hot::stream/data", std::slice::from_ref(payload));
    // Convert payload to JSON, with registered secrets masked: a stream's
    // payload is meant for its consumer, but a run's secret isn't.
    let payload = match vm.get_execution_context() {
        Some(ctx) => {
            crate::lang::masking::RecordMask::new(&ctx.secret_values, &ctx.secret_value_hashes)
                .mask_delivered(payload.to_hot_data_repr())
        }
        None => payload.clone(),
    };
    let payload_json: serde_json::Value = (&payload).into();

    // Create the stream event for pub/sub (real-time delivery)
    let stream_event = crate::stream::StreamEvent::StreamData {
        stream_data_id,
        run_id,
        env_id,
        stream_id,
        data_type: data_type.clone(),
        payload: payload_json.clone(),
    };

    // Publish to stream pub/sub for real-time SSE delivery via Redis Streams
    // This is the only delivery mechanism - stream_data is ephemeral and not persisted to database
    if let Some(publisher) = vm.get_stream_publisher() {
        tracing::debug!(
            "::hot::stream/data: Publishing type='{}' run_id={} stream_id={}",
            data_type,
            run_id,
            stream_id
        );
        // Publish stream data (use Handle::block_on since VM runs in spawn_blocking context).
        // Wait for any in-flight asynchronous `run:start` publication first so
        // stream subscribers always receive `run:start` before the first chunk.
        let publisher_clone = publisher.clone();
        let emitter = vm.get_emitter();
        tokio::runtime::Handle::current().block_on(async {
            if let Some(emitter) = emitter {
                emitter.wait_run_start_published().await;
            }
            if let Err(e) = publisher_clone.publish(stream_event).await {
                tracing::warn!("Failed to publish stream data: {}", e);
            }
        });
    } else {
        tracing::warn!(
            "::hot::stream/data: No stream publisher available - is this running outside of a worker?"
        );
    }

    HotResult::Ok(Val::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Compiler;
    use crate::lang::compiler::core_registry::CoreVariableRegistry;
    use crate::lang::emitter::{EngineEvent, EngineEventEmitter};
    use crate::lang::event::ExecutionContext;
    use crate::lang::parser::parse_hot;
    use crate::stream::{
        StreamEvent, StreamNext, StreamPubSub, StreamPubSubType, StreamSubscriberFactory,
    };
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_data_waits_for_in_flight_run_start_publication() {
        struct GatedStartEmitter {
            ready: tokio::sync::watch::Receiver<bool>,
        }
        impl EngineEventEmitter for GatedStartEmitter {
            fn emit(&self, _event: EngineEvent) {}
            fn wait_run_start_published(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
                let mut rx = self.ready.clone();
                Box::pin(async move {
                    let _ = rx.wait_for(|done| *done).await;
                })
            }
        }

        let pubsub = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());
        let (run_id, env_id, stream_id) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let mut sub = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);

        let vm_pubsub = Arc::clone(&pubsub);
        let vm_task = tokio::task::spawn_blocking(move || {
            let mut program = parse_hot("::test ns").expect("test source should parse");
            let mut compiler = Compiler::new();
            compiler
                .compile_program_unchecked(&mut program)
                .expect("test source should compile");
            let mut vm = VirtualMachine::new(
                compiler.get_program_arc(),
                None,
                compiler.get_function_mapping_arc(),
                compiler.get_core_functions_arc(),
                compiler.get_type_implementations_arc(),
                Arc::new(CoreVariableRegistry::new()),
                None,
            );
            vm.set_execution_context(ExecutionContext::new(
                run_id,
                stream_id,
                crate::db::run::RunType::Call.as_id(),
                Some(env_id),
                None,
                None,
                None,
            ));
            vm.set_stream_publisher(vm_pubsub);
            vm.set_emitter(Arc::new(GatedStartEmitter { ready: ready_rx }));

            let res = data(&mut vm, &[Val::from("chunk"), Val::from(1i64)]);
            assert!(matches!(res, HotResult::Ok(Val::Null)), "{res:?}");
        });

        // While run:start is still in-flight, stream:data must not overtake it.
        let early = tokio::time::timeout(std::time::Duration::from_millis(40), sub.next()).await;
        assert!(early.is_err(), "stream:data published early: {early:?}");

        // Publish RunStart and release waiters, mimicking RunStartPublisher.
        pubsub
            .publish(StreamEvent::RunStart {
                run_id,
                env_id,
                stream_id,
                event_id: None,
            })
            .await
            .unwrap();
        let _ = ready_tx.send(true);
        vm_task.await.unwrap();

        let first = sub.next().await;
        assert!(
            matches!(first, StreamNext::Event(StreamEvent::RunStart { run_id: got, .. }) if got == run_id),
            "{first:?}"
        );
        let second = sub.next().await;
        assert!(
            matches!(&second, StreamNext::Event(StreamEvent::StreamData { run_id: got, data_type, .. }) if *got == run_id && data_type == "chunk"),
            "{second:?}"
        );
    }
}
