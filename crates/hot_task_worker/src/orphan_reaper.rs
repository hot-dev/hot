//! Orphan reaper for leaked Kata containerd shims and QEMU VMs.
//!
//! When a previous task-worker is killed abruptly (OOM-killer, kernel panic,
//! `kill -9`, ECS hard-stop after stopTimeout, ...) any Kata containers it
//! launched leave their `containerd-shim-kata-v2` and `qemu-system-*`
//! processes running. These get re-parented to PID 1 on the host. Each
//! shim+VM pair holds memory (the VM's guest RAM is fully allocated), so a
//! few generations of crashed workers will OOM the host into a death spiral.
//!
//! At startup we walk `/proc` (visible because the task definition sets
//! `pidMode: host`) and SIGKILL any obvious orphans. Safety guards:
//! - We only consider processes whose parent PID is 1 (truly orphaned —
//!   never touch a process still owned by a sibling worker or containerd).
//! - We only consider a small, hard-coded allowlist of process names
//!   (`containerd-shim-kata-v2`, `qemu-system-*`).
//! - We require the process to be at least `MIN_AGE_SECS` old, so a
//!   currently-running worker that briefly orphans children during a
//!   reparent race won't be hit.

use std::time::Duration;

const MIN_AGE_SECS: u64 = 60;
/// Wall-clock ceiling on the entire reaper pass. /proc walks are cheap but
/// we don't want a wedged kernel to block worker startup.
const REAPER_TIMEOUT: Duration = Duration::from_secs(30);

/// Process names we are willing to SIGKILL when found orphaned (PPid=1).
fn is_target_comm(comm: &str) -> bool {
    matches!(comm, "containerd-shim" | "containerd-shim-kata-v2") || comm.starts_with("qemu-system")
}

/// Walks /proc and SIGKILLs any orphaned Kata shim or QEMU VM. Best-effort,
/// always returns. Logs a summary so we can spot recurring leaks in
/// CloudWatch.
///
/// No-op on non-Linux platforms: the reaper depends on procfs (`/proc`) and
/// Kata containers, neither of which exist on macOS / Windows. Without this
/// guard, local dev on a Mac produces a misleading "cannot read /proc" warn
/// on every worker startup.
pub async fn reap_orphan_kata_processes() {
    if !cfg!(target_os = "linux") {
        return;
    }

    if tokio::time::timeout(REAPER_TIMEOUT, reap_inner())
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = REAPER_TIMEOUT.as_secs(),
            "orphan_reaper timed out"
        );
    }
}

async fn reap_inner() {
    let entries = match tokio::fs::read_dir("/proc").await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "orphan_reaper: cannot read /proc");
            return;
        }
    };

    let pids = collect_pids(entries).await;
    if pids.is_empty() {
        return;
    }

    let mut killed = 0u32;
    let mut skipped_young = 0u32;
    let mut skipped_attached = 0u32;
    let our_pid = std::process::id();

    for pid in pids {
        if pid == our_pid {
            continue;
        }

        let comm = match tokio::fs::read_to_string(format!("/proc/{}/comm", pid)).await {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };
        if !is_target_comm(&comm) {
            continue;
        }

        let (ppid, age_secs) = match read_ppid_and_age(pid).await {
            Some(t) => t,
            None => continue,
        };

        if ppid != 1 {
            skipped_attached += 1;
            continue;
        }
        if age_secs < MIN_AGE_SECS {
            skipped_young += 1;
            continue;
        }

        match kill_pid(pid) {
            Ok(_) => {
                tracing::warn!(
                    pid,
                    comm = %comm,
                    age_secs,
                    "orphan_reaper: SIGKILLed orphan kata/qemu process from a dead worker"
                );
                killed += 1;
            }
            Err(e) => {
                tracing::warn!(pid, comm = %comm, error = %e, "orphan_reaper: kill failed");
            }
        }
    }

    if killed > 0 || skipped_attached > 0 || skipped_young > 0 {
        tracing::info!(
            killed,
            skipped_young,
            skipped_attached,
            "orphan_reaper: pass complete"
        );
    } else {
        tracing::info!("orphan_reaper: no orphan kata/qemu processes found");
    }
}

async fn collect_pids(mut entries: tokio::fs::ReadDir) -> Vec<u32> {
    let mut out = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(name) = entry.file_name().to_str()
            && let Ok(pid) = name.parse::<u32>()
        {
            out.push(pid);
        }
    }
    out
}

async fn read_ppid_and_age(pid: u32) -> Option<(u32, u64)> {
    let status = tokio::fs::read_to_string(format!("/proc/{}/status", pid))
        .await
        .ok()?;
    let mut ppid: Option<u32> = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            ppid = rest.trim().parse().ok();
            break;
        }
    }
    let ppid = ppid?;

    // /proc/<pid>/stat field 22 is starttime in clock ticks since boot.
    // Combined with /proc/uptime we can compute the process's age in seconds.
    let stat = tokio::fs::read_to_string(format!("/proc/{}/stat", pid))
        .await
        .ok()?;
    // The 2nd field (comm) is parenthesized and may contain spaces, so split
    // on the LAST ')' to separate it from the rest.
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Index 19 in fields == field 22 in /proc/[pid]/stat (counting from 1
    // and skipping the first 2 fields we consumed via the rsplit).
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;

    let uptime = tokio::fs::read_to_string("/proc/uptime").await.ok()?;
    let uptime_secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let ticks_per_sec = clock_ticks_per_second();
    let starttime_secs = starttime_ticks as f64 / ticks_per_sec as f64;
    let age_secs = (uptime_secs - starttime_secs).max(0.0) as u64;

    Some((ppid, age_secs))
}

std::cfg_select! {
    target_os = "linux" => {
        fn clock_ticks_per_second() -> u64 {
            // SAFETY: sysconf with `_SC_CLK_TCK` is a thread-safe POSIX query that
            // returns a constant for the running kernel.
            let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if v <= 0 { 100 } else { v as u64 }
        }

        fn kill_pid(pid: u32) -> std::io::Result<()> {
            let r = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if r == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }
    _ => {
        fn clock_ticks_per_second() -> u64 {
            100
        }

        fn kill_pid(_pid: u32) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "orphan_reaper only supported on Linux",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_comm_matches_expected_binaries() {
        assert!(is_target_comm("containerd-shim-kata-v2"));
        assert!(is_target_comm("containerd-shim"));
        assert!(is_target_comm("qemu-system-x86_64"));
        assert!(is_target_comm("qemu-system-aarch64"));
        assert!(!is_target_comm("hot"));
        assert!(!is_target_comm("dockerd"));
        assert!(!is_target_comm(""));
    }

    #[test]
    fn clock_ticks_per_second_is_positive() {
        assert!(clock_ticks_per_second() > 0);
    }
}
