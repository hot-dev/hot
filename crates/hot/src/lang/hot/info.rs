use crate::lang::hot::r#type::HotResult;
use crate::val::Val;

/// Return the Hot version and git SHA
///
/// Usage: `::hot::info/version`
///
/// Returns: Version { version: Str, git-sha: Str }
pub fn version(_args: &[Val]) -> HotResult<Val> {
    let version_info = crate::val!({
        "$type": "::hot::info/Version",
        "$val": {
            "version": crate::build_info::VERSION,
            "git-sha": crate::build_info::GIT_SHA
        }
    });

    HotResult::Ok(version_info)
}
