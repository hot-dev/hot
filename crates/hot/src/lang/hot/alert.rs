//! Alert publishing functions for Hot.
//!
//! Provides the `::hot::alert/alert` function to publish alerts from Hot code.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;

/// Publish an alert (VM-aware version)
///
/// Usage: `alert channel data`
///
/// Arguments:
/// - channel: String representing the alert channel (e.g., "payment:failed")
/// - data: Map containing additional alert data
///
/// Returns: Result<Map, Error> - alert info on success, error on failure
///
/// Alerts are a Hot Cloud feature. In local dev this is a no-op: it logs
/// the alert channel at info level and returns success with a non-published
/// status so user code continues without error.
pub fn alert(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::alert/alert expects 1-2 arguments (channel, data?), got {}",
            args.len()
        )));
    }

    // Extract channel argument - must be string
    let channel: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        _ => {
            return HotResult::Err(Val::from(format!(
                "::hot::alert/alert: channel must be string, got: {:?}",
                args[0]
            )));
        }
    };

    // Extract data argument - default to empty map
    let data: serde_json::Value = if args.len() > 1 {
        match serde_json::to_value(&args[1]) {
            Ok(json) => json,
            Err(e) => {
                return HotResult::Err(Val::from(format!(
                    "::hot::alert/alert: failed to serialize data: {}",
                    e
                )));
            }
        }
    } else {
        serde_json::json!({})
    };

    // Get execution context from VM
    let execution_context = match vm.get_execution_context() {
        Some(ctx) => ctx,
        None => {
            return HotResult::Err(Val::from(
                "::hot::alert/alert: no execution context configured in VM".to_string(),
            ));
        }
    };

    // Get org_id and env_id from execution context
    let env_id = match execution_context.env_id {
        Some(id) => id,
        None => {
            // No env_id — log locally and return success (e.g., local dev without profile)
            tracing::info!(
                "::hot::alert/alert: channel='{}' (no env_id in execution context, alert not persisted)",
                channel
            );
            let mut result = indexmap::IndexMap::new();
            result.insert(Val::from("channel"), Val::from(channel.as_str()));
            result.insert(Val::from("status"), Val::from("no_context"));
            return HotResult::Ok(Val::ok(Val::Map(Box::new(result))));
        }
    };

    let org_id = match execution_context.org_id {
        Some(id) => id,
        None => {
            // No org_id — log locally and return success (e.g., local dev without profile)
            tracing::info!(
                "::hot::alert/alert: channel='{}' (no org_id in execution context, alert not persisted)",
                channel
            );
            let mut result = indexmap::IndexMap::new();
            result.insert(Val::from("channel"), Val::from(channel.as_str()));
            result.insert(Val::from("status"), Val::from("no_context"));
            return HotResult::Ok(Val::ok(Val::Map(Box::new(result))));
        }
    };

    // Get database pool from VM
    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => {
            // In local dev without database, just log and return success
            tracing::info!(
                "::hot::alert/alert: channel='{}' (no database configured, alert not persisted)",
                channel
            );
            let mut result = indexmap::IndexMap::new();
            result.insert(Val::from("channel"), Val::from(channel.as_str()));
            result.insert(Val::from("status"), Val::from("no_database"));
            return HotResult::Ok(Val::ok(Val::Map(Box::new(result))));
        }
    };

    // Publish the alert using the database function
    // We need to run this asynchronously - use block_on since we're in sync context
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            crate::db::alert::publish_alert(&db, &org_id, &env_id, &channel, &data).await
        })
    });

    match result {
        Ok(alert) => {
            tracing::info!(
                "::hot::alert/alert: published alert '{}' (id: {})",
                channel,
                alert.alert_id
            );

            let mut result = indexmap::IndexMap::new();
            result.insert(Val::from("alert_id"), Val::from(alert.alert_id.to_string()));
            result.insert(Val::from("channel"), Val::from(alert.channel.as_str()));
            result.insert(Val::from("status"), Val::from("published"));
            HotResult::Ok(Val::ok(Val::Map(Box::new(result))))
        }
        Err(e) => {
            tracing::error!("::hot::alert/alert: failed to publish alert: {}", e);
            HotResult::Err(Val::from(format!(
                "::hot::alert/alert: failed to publish alert: {}",
                e
            )))
        }
    }
}
