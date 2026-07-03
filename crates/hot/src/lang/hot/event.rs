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

                    publisher.publish(ctx, event);

                    // Build the EventInfo return value
                    let event_time_instant = crate::val!({
                        "$type": "::hot::time/Instant",
                        "$val": {
                            "epochNanoseconds": event_time.timestamp_nanos_opt().unwrap_or(0)
                        }
                    });

                    let event_detail = crate::val!({
                        "$type": "::hot::event/EventDetail",
                        "$val": {
                            "id": event_id.to_string(),
                            "type": event_type_str,
                            "time": event_time_instant
                        }
                    });

                    let stream = crate::val!({
                        "$type": "::hot::stream/Stream",
                        "$val": {
                            "id": event_stream_id.to_string()
                        }
                    });

                    // Helper to convert Option<String> to Val (null if None)
                    let env_name_val = match event_env_name {
                        Some(s) => Val::from(s),
                        None => Val::Null,
                    };

                    let env = crate::val!({
                        "$type": "::hot::info/Env",
                        "$val": {
                            "id": event_env_id.to_string(),
                            "name": env_name_val
                        }
                    });

                    let event_info = crate::val!({
                        "$type": "::hot::event/EventInfo",
                        "$val": {
                            "event": event_detail,
                            "stream": stream,
                            "env": env
                        }
                    });

                    HotResult::Ok(Val::ok(event_info))
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

/// Listen for events
pub fn listen(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::event/listen", args, 1);

    // Mock event listener
    let mut result_map = indexmap::IndexMap::new();
    result_map.insert(Val::from("listening"), Val::Bool(true));
    result_map.insert(Val::from("event_type"), args[0].clone());

    HotResult::Ok(Val::Map(Box::new(result_map)))
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

    HotResult::Ok(Val::Map(Box::new(event_map)))
}
