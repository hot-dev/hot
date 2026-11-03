//! `task.status-path`: how much of this worker's capacity is in use. The
//! self-host agent reads it to scale task hosts and to retire idle ones.

use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn seconds_since_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// When this worker last ran a task. The slots in use are a sample from each
/// write, which a short task can fall between, so they can read idle on a
/// worker that never is.
#[derive(Debug, Default)]
pub struct TaskActivity {
    running: AtomicUsize,
    last_finished_at: AtomicU64,
}

/// A task in progress, from [`TaskActivity::start`] until dropped.
#[must_use]
pub struct RunningTask<'a>(&'a TaskActivity);

impl TaskActivity {
    pub const fn new() -> Self {
        Self {
            running: AtomicUsize::new(0),
            last_finished_at: AtomicU64::new(0),
        }
    }

    pub fn start(&self) -> RunningTask<'_> {
        self.running.fetch_add(1, Ordering::SeqCst);
        RunningTask(self)
    }

    /// Seconds since the epoch when a task last ran: now while one does, and 0
    /// if none has since the worker started.
    pub fn last_task_at(&self) -> u64 {
        if self.running.load(Ordering::SeqCst) > 0 {
            return seconds_since_epoch();
        }
        self.last_finished_at.load(Ordering::SeqCst)
    }
}

impl Drop for RunningTask<'_> {
    fn drop(&mut self) {
        self.0
            .last_finished_at
            .fetch_max(seconds_since_epoch(), Ordering::SeqCst);
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A snapshot of the worker's slots and container memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotStatus {
    pub code_slots: usize,
    pub code_busy: usize,
    pub container_slots: usize,
    pub container_busy: usize,
    pub memory_mb: u64,
    pub memory_used_mb: u64,
    pub last_task_at: u64,
}

impl SlotStatus {
    /// The file's JSON, stamped with the current time in seconds since the
    /// epoch so a reader can tell a stopped worker's stale file.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "time": seconds_since_epoch(),
            "code": {"slots": self.code_slots, "busy": self.code_busy},
            "containers": {"slots": self.container_slots, "busy": self.container_busy},
            "memory_mb": self.memory_mb,
            "memory_used_mb": self.memory_used_mb,
            "last_task_at": self.last_task_at,
        })
    }
}

/// Replace `path` with `status`, through a temporary file so a reader never
/// sees a partial write.
pub fn write(path: &Path, status: &SlotStatus) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, status.to_json().to_string())?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_file_reports_each_pool_and_memory() {
        let directory = std::env::temp_dir().join(format!("hot-slots-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("task-slots.json");
        let status = SlotStatus {
            code_slots: 500,
            code_busy: 3,
            container_slots: 8,
            container_busy: 2,
            memory_mb: 8192,
            memory_used_mb: 1024,
            last_task_at: 1_700_000_123,
        };

        write(&path, &status).unwrap();
        write(&path, &status).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["code"],
            serde_json::json!({"slots": 500, "busy": 3})
        );
        assert_eq!(
            written["containers"],
            serde_json::json!({"slots": 8, "busy": 2})
        );
        assert_eq!(written["memory_mb"], 8192);
        assert_eq!(written["memory_used_mb"], 1024);
        assert_eq!(written["last_task_at"], 1_700_000_123);
        assert!(written["time"].as_u64().unwrap() > 1_700_000_000);
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_task_between_two_writes_still_shows() {
        let activity = TaskActivity::new();
        assert_eq!(activity.last_task_at(), 0, "none has run yet");

        let before = seconds_since_epoch();
        let first = activity.start();
        let second = activity.start();
        assert!(activity.last_task_at() >= before, "running now");
        drop(first);
        assert!(activity.last_task_at() >= before, "one is still running");
        drop(second);

        // Nothing is running, and the slots would read idle; the task is on
        // record for whichever write comes next.
        let finished = activity.last_task_at();
        assert!(finished >= before && finished <= seconds_since_epoch());
        assert_eq!(activity.running.load(Ordering::SeqCst), 0);
    }
}
