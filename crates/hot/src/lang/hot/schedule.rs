//! Schedule functions for creating and cancelling dynamic schedules
//!
//! These functions are used by the hot:schedule:new and hot:schedule:cancel
//! event handlers to create and cancel one-time or recurring schedules.

use crate::db::{Schedule, parse_schedule_expression};
use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use uuid::Uuid;

/// Create a new schedule dynamically (called by hot:schedule:new event handler)
///
/// Arguments:
/// - fn_name: String - fully qualified function name (e.g., "::myapp::orders/process")
/// - args: Vec<Any> - arguments to pass to the function
/// - schedule: String - schedule expression (datetime, duration, or cron)
///
/// Returns: String - the schedule_id of the created schedule
pub fn create_schedule(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 3 {
        return HotResult::Err(Val::from(format!(
            "::hot::schedule/create expects 3 arguments (fn, args, schedule), got {}",
            args.len()
        )));
    }

    // Extract function name
    let fn_name: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        Val::Map(m) => {
            // Handle Fn type wrapper
            if let Some(Val::Str(type_name)) = m.get(&Val::from("$type"))
                && &**type_name == "::hot::type/Fn"
            {
                if let Some(Val::Str(qualified_name)) = m.get(&Val::from("$val")) {
                    (**qualified_name).to_owned()
                } else {
                    return HotResult::Err(Val::from(
                        "Fn value missing string $val for named function".to_string(),
                    ));
                }
            } else {
                return HotResult::Err(Val::from(
                    "Expected function reference or string, got Map".to_string(),
                ));
            }
        }
        Val::Box(b) => {
            if let Some(fr) = b
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                fr.name().to_string()
            } else {
                return HotResult::Err(Val::from(
                    "Expected function reference or string".to_string(),
                ));
            }
        }
        other => {
            return HotResult::Err(Val::from(format!(
                "Expected function reference or string for fn, got: {:?}",
                other
            )));
        }
    };

    // Parse namespace and var from function name
    let (ns, var) = if let Some((ns_part, var_part)) = fn_name.rsplit_once('/') {
        (ns_part.to_string(), var_part.to_string())
    } else {
        return HotResult::Err(Val::from(format!(
            "Invalid function name format '{}', expected ::namespace/function",
            fn_name
        )));
    };

    // Extract args (should be a Vec)
    let fn_args = match &args[1] {
        Val::Vec(v) => v.clone(),
        Val::Null => Vec::new(),
        other => {
            return HotResult::Err(Val::from(format!(
                "Expected Vec for args, got: {:?}",
                other
            )));
        }
    };

    // Extract and parse schedule expression
    let schedule_expr = match &args[2] {
        Val::Str(s) => s.clone(),
        // Also support DateTime values (convert to ISO string)
        Val::Map(m) => {
            if let Some(Val::Str(type_name)) = m.get(&Val::from("$type"))
                && &**type_name == "::hot::type/DateTime"
            {
                if let Some(Val::Str(dt_str)) = m.get(&Val::from("$val")) {
                    dt_str.clone()
                } else {
                    return HotResult::Err(Val::from(
                        "DateTime value missing string $val".to_string(),
                    ));
                }
            } else {
                return HotResult::Err(Val::from(format!(
                    "Expected string or DateTime for schedule, got Map: {:?}",
                    m
                )));
            }
        }
        other => {
            return HotResult::Err(Val::from(format!(
                "Expected string or DateTime for schedule, got: {:?}",
                other
            )));
        }
    };

    // Parse the schedule expression
    let schedule_type = match parse_schedule_expression(&schedule_expr) {
        Ok(st) => st,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "Invalid schedule expression '{}': {}",
                schedule_expr, e
            )));
        }
    };

    // Get execution context to find build_id
    let ctx = match vm.get_execution_context() {
        Some(ctx) => ctx.clone(),
        None => {
            return HotResult::Err(Val::from(
                "No execution context available - cannot create schedule".to_string(),
            ));
        }
    };

    let build_id = match ctx.build_id {
        Some(id) => id,
        None => {
            return HotResult::Err(Val::from(
                "No build_id in execution context - cannot create schedule".to_string(),
            ));
        }
    };

    // Get database pool
    let db = match vm.get_database_pool() {
        Some(db) => (*db).clone(),
        None => {
            return HotResult::Err(Val::from(
                "No database connection available - cannot create schedule".to_string(),
            ));
        }
    };

    // Resolve org-scoped schedule policy for dynamic schedules. Resolve the org
    // and its features in a single block_on so the VM thread bridges to async
    // once instead of twice.
    let (org_id, policy) = match tokio::runtime::Handle::current().block_on(async {
        let build = crate::db::Build::get_build(&db, &build_id)
            .await
            .map_err(|e| e.to_string())?;
        let project = crate::db::Project::get_project(&db, &build.project_id)
            .await
            .map_err(|e| e.to_string())?;
        let env = crate::db::Env::get_env(&db, &project.env_id)
            .await
            .map_err(|e| e.to_string())?;
        let org_id = env.org_id;
        let features = crate::db::Features::resolve_for_org(&db, &org_id).await;
        Ok::<_, String>((org_id, features))
    }) {
        Ok((org_id, features)) => (
            Some(org_id),
            crate::db::SchedulePolicy::from_conf(vm.get_conf()).with_features(&features),
        ),
        Err(e) => {
            tracing::warn!(
                "Unable to resolve org for dynamic schedule policy on build {}: {}",
                build_id,
                e
            );
            (None, crate::db::SchedulePolicy::from_conf(vm.get_conf()))
        }
    };

    // Create the schedule
    let schedule_id = Uuid::now_v7();
    let args_json: Option<serde_json::Value> = if fn_args.is_empty() {
        None
    } else {
        Some(
            serde_json::to_value(Val::Vec(fn_args.clone()).to_hot_data_repr())
                .unwrap_or(serde_json::Value::Null),
        )
    };

    // Insert the schedule synchronously (use Handle::block_on since VM runs in spawn_blocking)
    let result = tokio::runtime::Handle::current().block_on(async {
        Schedule::insert_dynamic_schedule(
            &db,
            &schedule_id,
            &build_id,
            &schedule_type,
            org_id.as_ref(),
            policy,
            &ns,
            &var,
            None, // meta
            args_json.as_ref(),
        )
        .await
    });

    match result {
        Ok(()) => {
            tracing::info!(
                "Created dynamic schedule {}: {}:{} @ {:?}",
                schedule_id,
                ns,
                var,
                schedule_type
            );
            HotResult::Ok(Val::from(schedule_id.to_string()))
        }
        Err(e) => HotResult::Err(Val::from(format!("Failed to create schedule: {}", e))),
    }
}

/// Cancel a schedule by ID or function name
///
/// Arguments (one of):
/// - schedule_id: String - UUID of the schedule to cancel
/// - fn_name: String - fully qualified function name to cancel all schedules for
///
/// Returns: Bool - true if at least one schedule was cancelled
pub fn cancel_schedule(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::schedule/cancel expects 1 argument (data map with schedule-id or fn), got {}",
            args.len()
        )));
    }

    // Extract data map
    let data = match &args[0] {
        Val::Map(m) => m,
        other => {
            return HotResult::Err(Val::from(format!(
                "Expected Map for cancel data, got: {:?}",
                other
            )));
        }
    };

    // Get database pool
    let db = match vm.get_database_pool() {
        Some(db) => (*db).clone(),
        None => {
            return HotResult::Err(Val::from(
                "No database connection available - cannot cancel schedule".to_string(),
            ));
        }
    };

    // Check for schedule-id
    if let Some(Val::Str(schedule_id_str)) = data.get(&Val::from("schedule-id")) {
        let schedule_id = match Uuid::parse_str(schedule_id_str) {
            Ok(id) => id,
            Err(e) => {
                return HotResult::Err(Val::from(format!(
                    "Invalid schedule-id '{}': {}",
                    schedule_id_str, e
                )));
            }
        };

        let result = tokio::runtime::Handle::current()
            .block_on(async { Schedule::cancel_schedule(&db, &schedule_id).await });

        return match result {
            Ok(cancelled) => {
                if cancelled {
                    tracing::info!("Cancelled schedule {}", schedule_id);
                }
                HotResult::Ok(Val::Bool(cancelled))
            }
            Err(e) => HotResult::Err(Val::from(format!("Failed to cancel schedule: {}", e))),
        };
    }

    // Check for fn (function name)
    if let Some(fn_val) = data.get(&Val::from("fn")) {
        let fn_name = match fn_val {
            Val::Str(s) => s.clone(),
            Val::Map(m) => {
                if let Some(Val::Str(type_name)) = m.get(&Val::from("$type"))
                    && &**type_name == "::hot::type/Fn"
                {
                    if let Some(Val::Str(qualified_name)) = m.get(&Val::from("$val")) {
                        qualified_name.clone()
                    } else {
                        return HotResult::Err(Val::from(
                            "Fn value missing string $val".to_string(),
                        ));
                    }
                } else {
                    return HotResult::Err(Val::from("Invalid Fn type in cancel data"));
                }
            }
            other => {
                return HotResult::Err(Val::from(format!(
                    "Expected string or Fn for fn, got: {:?}",
                    other
                )));
            }
        };

        // Parse namespace and var
        let (ns, var) = if let Some((ns_part, var_part)) = fn_name.rsplit_once('/') {
            (ns_part.to_string(), var_part.to_string())
        } else {
            return HotResult::Err(Val::from(format!(
                "Invalid function name format '{}', expected ::namespace/function",
                fn_name
            )));
        };

        // Get build_id from context
        let ctx = match vm.get_execution_context() {
            Some(ctx) => ctx.clone(),
            None => {
                return HotResult::Err(Val::from(
                    "No execution context available - cannot cancel schedule by function"
                        .to_string(),
                ));
            }
        };

        let build_id = match ctx.build_id {
            Some(id) => id,
            None => {
                return HotResult::Err(Val::from(
                    "No build_id in execution context - cannot cancel schedule by function"
                        .to_string(),
                ));
            }
        };

        let result = tokio::runtime::Handle::current().block_on(async {
            Schedule::cancel_schedules_by_function(&db, &build_id, &ns, &var).await
        });

        return match result {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("Cancelled {} schedule(s) for {}:{}", count, ns, var);
                }
                HotResult::Ok(Val::Bool(count > 0))
            }
            Err(e) => HotResult::Err(Val::from(format!(
                "Failed to cancel schedules by function: {}",
                e
            ))),
        };
    }

    HotResult::Err(Val::from(
        "cancel expects data with either 'schedule-id' or 'fn' key".to_string(),
    ))
}
