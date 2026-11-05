use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use crate::validate_args_range;

/// Send an event (VM-aware version)
///
/// Usage: `::hot::event/send event-type event-data`
///
/// Arguments:
/// - event-type: String representing the type of event
/// - event-data: Any value representing the event data
///
/// Returns: Result<EventInfo, Error> - EventInfo with event details, stream, and env
pub fn send_event(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if let Err(error) = vm.require_work_admission() {
        return HotResult::Err(Val::from(error));
    }
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::event/send expects exactly 2 arguments (event-type, event-data), got {}",
            args.len()
        )));
    }

    // Extract event_type argument - must be string
    let event_type: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        _ => {
            return HotResult::Err(Val::from(format!(
                "::hot::event/send: event-type must be string, got: {:?}",
                args[0]
            )));
        }
    };

    // A `hot test` run keeps the event for `::hot::test/deliver`, which runs
    // its handlers inside the test, instead of publishing it.
    if crate::lang::hot::test::record_sent(&event_type, &args[1]) {
        let ctx = vm.get_execution_context().as_ref();
        let time_nanos = crate::lang::hot::time::now_epoch_nanos().unwrap_or_default();
        return HotResult::Ok(Val::ok(event_info(
            uuid::Uuid::now_v7(),
            event_type,
            time_nanos as i64,
            ctx.map(|ctx| ctx.stream_id).unwrap_or_default(),
            ctx.and_then(|ctx| ctx.env_id),
            ctx.and_then(|ctx| ctx.env_name.clone()),
        )));
    }

    // Extract event_data argument - can be any value
    let event_data = args[1].clone();

    tracing::debug!(
        "send_event: event_type='{}', has_publisher={}, has_context={}",
        event_type,
        vm.get_event_publisher().is_some(),
        vm.get_execution_context().is_some()
    );

    // Get the event publisher from the VM
    let event_publisher = vm.get_event_publisher();

    match event_publisher {
        Some(publisher) => {
            // Get the execution context from the VM
            let execution_context = vm.get_execution_context();

            match execution_context {
                Some(ctx) => {
                    // Create the event - Event::new takes env_id but ExecutionContext has it
                    let env_id = match ctx.env_id {
                        Some(id) => id,
                        None => {
                            return HotResult::Err(Val::from(
                                "::hot::event/send: no env_id in execution context".to_string(),
                            ));
                        }
                    };

                    // Flush the current run's pending emitter writes before publishing so
                    // this run's run:start is in the database before any child events are
                    // queued (the child run references this run as origin).
                    //
                    // The flush is scoped to this run and bounded by a timeout: a global,
                    // unbounded flush can wedge the VM thread (and the whole serial event
                    // stream behind it) for minutes when the database writer is backlogged.
                    // On timeout or failure we continue anyway - the FK retry logic in
                    // write_run_start handles the rare out-of-order case.
                    const EMITTER_FLUSH_TIMEOUT: std::time::Duration =
                        std::time::Duration::from_secs(5);
                    if let Some(emitter) = vm.get_emitter() {
                        let run_id = ctx.run_id;
                        let flush_result = tokio::runtime::Handle::current().block_on(async {
                            tokio::time::timeout(EMITTER_FLUSH_TIMEOUT, emitter.flush_run(run_id))
                                .await
                        });
                        match flush_result {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    "send_event: failed to flush emitter before publishing: {}",
                                    e
                                );
                            }
                            Err(_) => {
                                tracing::warn!(
                                    run_id = %run_id,
                                    "send_event: emitter flush timed out after {:?} (database \
                                     writer backlog); publishing anyway",
                                    EMITTER_FLUSH_TIMEOUT
                                );
                            }
                        }
                    }

                    // Get stream_id and project context to propagate through the execution chain
                    let stream_id = ctx.stream_id;
                    let event = crate::lang::event::Event::new_with_project(
                        env_id,
                        stream_id,
                        event_type.clone(),
                        event_data,
                        ctx.project_id,
                        ctx.project_name.clone(),
                    );

                    // Publish the event with context
                    tracing::debug!(
                        "send_event: publishing event type='{}' to queue (event_id={}, target_project={:?})",
                        event.event_type,
                        event.event_id,
                        event.target_project_name
                    );

                    // Capture event info before publishing (publish consumes ctx)
                    let event_id = event.event_id;
                    let event_type_str = event.event_type.clone();
                    let event_time = event.event_time;
                    let event_stream_id = event.stream_id;
                    let event_env_id = env_id;
                    let event_env_name = ctx.env_name.clone();

                    if let Err(e) = publisher.publish_checked(&ctx.for_sent_event(), event) {
                        return HotResult::Err(Val::from(format!(
                            "::hot::event/send: failed to store event: {}",
                            e
                        )));
                    }

                    HotResult::Ok(Val::ok(event_info(
                        event_id,
                        event_type_str,
                        event_time.timestamp_nanos_opt().unwrap_or(0),
                        event_stream_id,
                        Some(event_env_id),
                        event_env_name,
                    )))
                }
                None => {
                    // No execution context configured
                    HotResult::Err(Val::from(
                        "::hot::event/send: no execution context configured in VM".to_string(),
                    ))
                }
            }
        }
        None => {
            // No event publisher configured
            HotResult::Err(Val::from(
                "::hot::event/send: no event publisher configured in VM".to_string(),
            ))
        }
    }
}

/// The EventInfo `send` returns: the event's id, type and time, its stream,
/// and its environment.
fn event_info(
    event_id: uuid::Uuid,
    event_type: String,
    time_nanos: i64,
    stream_id: uuid::Uuid,
    env_id: Option<uuid::Uuid>,
    env_name: Option<String>,
) -> Val {
    let event_time_instant = crate::val!({
        "$type": "::hot::time/Instant",
        "$val": {
            "epochNanoseconds": time_nanos
        }
    });

    let event_detail = crate::val!({
        "$type": "::hot::event/EventDetail",
        "$val": {
            "id": event_id.to_string(),
            "type": event_type,
            "time": event_time_instant
        }
    });

    let stream = crate::val!({
        "$type": "::hot::stream/Stream",
        "$val": {
            "id": stream_id.to_string()
        }
    });

    // Helper to convert Option<String> to Val (null if None)
    let env_name_val = match env_name {
        Some(s) => Val::from(s),
        None => Val::Null,
    };
    let env_id_val = match env_id {
        Some(id) => Val::from(id.to_string()),
        None => Val::Null,
    };

    let env = crate::val!({
        "$type": "::hot::info/Env",
        "$val": {
            "id": env_id_val,
            "name": env_name_val
        }
    });

    crate::val!({
        "$type": "::hot::event/EventInfo",
        "$val": {
            "event": event_detail,
            "stream": stream,
            "env": env
        }
    })
}

/// Listen for events
pub fn listen(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::event/listen", args, 1);

    // Mock event listener
    let mut result_map = indexmap::IndexMap::new();
    result_map.insert(Val::from("listening"), Val::Bool(true));
    result_map.insert(Val::from("event_type"), args[0].clone());

    HotResult::Ok(Val::map(result_map))
}

/// Create an event
pub fn create_event(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::event/create-event", args, 1, 3);

    let mut event_map = indexmap::IndexMap::new();
    event_map.insert(Val::from("type"), args[0].clone());

    if args.len() > 1 {
        event_map.insert(Val::from("data"), args[1].clone());
    }

    if args.len() > 2 {
        event_map.insert(Val::from("metadata"), args[2].clone());
    }

    HotResult::Ok(Val::map(event_map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::compiler::Compiler;
    use crate::lang::compiler::core_registry::CoreVariableRegistry;
    use crate::lang::event::{Event, EventPublisher, ExecutionContext};
    use crate::lang::parser::parse_hot;
    use crate::lang::runtime::vm::VirtualMachine;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    struct RecordingPublisher {
        contexts: Mutex<Vec<ExecutionContext>>,
    }

    impl EventPublisher for RecordingPublisher {
        fn publish(&self, ctx: &ExecutionContext, _event: Event) {
            self.contexts.lock().unwrap().push(ctx.clone());
        }
    }

    /// The context `send` publishes an event with, from a run of `sender`.
    fn context_of_event_sent_by(sender: ExecutionContext) -> ExecutionContext {
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
        let publisher = Arc::new(RecordingPublisher::default());
        vm.set_execution_context(sender);
        vm.set_event_publisher(publisher.clone());

        let sent = send_event(&mut vm, &[Val::from("order:paid"), Val::Null]);
        assert!(matches!(sent, HotResult::Ok(_)), "send failed: {sent:?}");

        publisher.contexts.lock().unwrap().remove(0)
    }

    fn sender_context() -> ExecutionContext {
        ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            crate::db::run::RunType::Event.as_id(),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
        )
    }

    /// Hot counts a run's retries from the attempt it starts at, so a handler
    /// woken by a retry's last attempt would get no retries of its own.
    #[test]
    fn a_sent_event_starts_its_handlers_at_attempt_zero() {
        let mut retried = sender_context().with_retry_attempt(2);
        retried.max_retries = 3;
        retried.retry_delay_ms = 1000;

        let sent = context_of_event_sent_by(retried);

        assert_eq!(sent.retry_attempt, 0);
        assert_eq!(sent.max_retries, 0);
        assert_eq!(sent.retry_delay_ms, 0);
    }

    #[test]
    fn a_sent_event_keeps_the_lineage_of_the_run_that_sent_it() {
        let mut retried = sender_context().with_retry_attempt(2);
        let origin_run_id = Uuid::now_v7();
        retried.origin_run_id = Some(origin_run_id);
        let sender_run_id = retried.run_id;
        let stream_id = retried.stream_id;

        let sent = context_of_event_sent_by(retried);

        assert_eq!(sent.origin_run_id, Some(origin_run_id));
        assert_eq!(sent.run_id, sender_run_id);
        assert_eq!(sent.stream_id, stream_id);
    }
}
