//! `task.status-path`: how much of this worker's capacity is in use. The
//! self-host agent reads it to scale task hosts and to retire idle ones.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A snapshot of the worker's slots and container memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotStatus {
    pub code_slots: usize,
    pub code_busy: usize,
    pub container_slots: usize,
    pub container_busy: usize,
    pub memory_mb: u64,
    pub memory_used_mb: u64,
}

impl SlotStatus {
    /// The file's JSON, stamped with the current time in seconds since the
    /// epoch so a reader can tell a stopped worker's stale file.
    pub fn to_json(&self) -> serde_json::Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        serde_json::json!({
            "time": now,
            "code": {"slots": self.code_slots, "busy": self.code_busy},
            "containers": {"slots": self.container_slots, "busy": self.container_busy},
            "memory_mb": self.memory_mb,
            "memory_used_mb": self.memory_used_mb,
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
        assert!(written["time"].as_u64().unwrap() > 1_700_000_000);
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
