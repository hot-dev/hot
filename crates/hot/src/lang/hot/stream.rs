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

    // Convert payload to JSON
    let payload_json: serde_json::Value = payload.into();

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
        // Publish stream data (use Handle::block_on since VM runs in spawn_blocking context)
        let publisher_clone = publisher.clone();
        tokio::runtime::Handle::current().block_on(async {
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
