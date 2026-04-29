// Run-specific functions: info
//
// fail(), cancel(), and exit() have moved to exec.rs

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;

/// Return information about the current run execution context
pub fn info(vm: &mut crate::lang::runtime::vm::VirtualMachine, _args: &[Val]) -> HotResult<Val> {
    fn run_type_string(run_type_id: i16) -> &'static str {
        match run_type_id {
            1 => "call",
            2 => "event",
            3 => "schedule",
            4 => "run",
            5 => "eval",
            6 => "repl",
            _ => "unknown",
        }
    }

    fn uuid_to_val(uuid: Option<uuid::Uuid>) -> Val {
        match uuid {
            Some(id) => Val::from(id.to_string()),
            None => Val::Null,
        }
    }

    fn string_to_val(s: Option<String>) -> Val {
        match s {
            Some(s) => Val::from(s),
            None => Val::Null,
        }
    }

    let start_time = vm.get_run_start_time();
    let start_time_instant = crate::val!({
        "$type": "::hot::time/Instant",
        "$val": {
            "epochNanoseconds": start_time.timestamp_nanos_opt().unwrap_or(0)
        }
    });

    if let Some(ctx) = vm.get_execution_context() {
        let run = crate::val!({
            "$type": "::hot::run/Run",
            "$val": {
                "id": ctx.run_id.to_string(),
                "type": run_type_string(ctx.run_type_id),
                "status": "running",
                "start-time": start_time_instant.clone(),
                "retry-attempt": ctx.retry_attempt as i64,
                "origin-run-id": uuid_to_val(ctx.origin_run_id)
            }
        });

        let stream = crate::val!({
            "$type": "::hot::stream/Stream",
            "$val": {
                "id": ctx.stream_id.to_string()
            }
        });

        let build = crate::val!({
            "$type": "::hot::info/Build",
            "$val": {
                "id": uuid_to_val(ctx.build_id),
                "hash": string_to_val(ctx.build_hash.clone())
            }
        });

        let project = crate::val!({
            "$type": "::hot::info/Project",
            "$val": {
                "id": uuid_to_val(ctx.project_id),
                "name": string_to_val(ctx.project_name.clone())
            }
        });

        let env = crate::val!({
            "$type": "::hot::info/Env",
            "$val": {
                "id": uuid_to_val(ctx.env_id),
                "name": string_to_val(ctx.env_name.clone())
            }
        });

        let user = crate::val!({
            "$type": "::hot::info/User",
            "$val": {
                "id": uuid_to_val(ctx.user_id)
            }
        });

        let org = crate::val!({
            "$type": "::hot::info/Org",
            "$val": {
                "id": uuid_to_val(ctx.org_id),
                "slug": string_to_val(ctx.org_slug.clone())
            }
        });

        let run_info = crate::val!({
            "$type": "::hot::run/RunInfo",
            "$val": {
                "run": run,
                "stream": stream,
                "build": build,
                "project": project,
                "env": env,
                "user": user,
                "org": org
            }
        });

        HotResult::Ok(run_info)
    } else {
        let run = crate::val!({
            "$type": "::hot::run/Run",
            "$val": {
                "id": Val::Null,
                "type": "cli",
                "status": "running",
                "start-time": start_time_instant,
                "retry-attempt": 0i64,
                "max-retries": 0i64,
                "retry-delay": 0i64,
                "origin-run-id": Val::Null
            }
        });

        let stream = crate::val!({
            "$type": "::hot::stream/Stream",
            "$val": {
                "id": Val::Null
            }
        });

        let build = crate::val!({
            "$type": "::hot::info/Build",
            "$val": {
                "id": Val::Null,
                "hash": Val::Null
            }
        });

        let project = crate::val!({
            "$type": "::hot::info/Project",
            "$val": {
                "id": Val::Null,
                "name": Val::Null
            }
        });

        let env = crate::val!({
            "$type": "::hot::info/Env",
            "$val": {
                "id": Val::Null,
                "name": Val::Null
            }
        });

        let user = crate::val!({
            "$type": "::hot::info/User",
            "$val": {
                "id": Val::Null
            }
        });

        let org = crate::val!({
            "$type": "::hot::info/Org",
            "$val": {
                "id": Val::Null,
                "slug": Val::Null
            }
        });

        let run_info = crate::val!({
            "$type": "::hot::run/RunInfo",
            "$val": {
                "run": run,
                "stream": stream,
                "build": build,
                "project": project,
                "env": env,
                "user": user,
                "org": org
            }
        });

        HotResult::Ok(run_info)
    }
}

#[cfg(test)]
mod tests {
    include!("run_test.rs");
}
