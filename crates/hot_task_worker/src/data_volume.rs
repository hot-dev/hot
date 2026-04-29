//! Writable volume for container `/data/` directory.
//!
//! **Linux (production):** Creates a sparse file, formats it as ext4, and
//! loop-mounts it to a temporary directory with size enforcement.
//!
//! **Non-Linux (dev):** Creates a plain directory at
//! `.hot/box/data/{task_id}-{nonce}` for Docker bind-mount. No size
//! enforcement, but provides the same `/data/` path inside the container
//! for dev/prod parity.
//!
//! The mount point is bind-mounted into the container. Cleanup happens on
//! `Drop`.
//!
//! ## Per-invocation isolation
//!
//! Every call to `create` appends a fresh random nonce to the directory
//! name. The same `task_id` can be invoked multiple times concurrently
//! (queue redelivery, retries, scheduler firing the same job twice in
//! flight, etc.); without the nonce, sibling invocations would share the
//! same bind-mount path and the first one to finish would yank the
//! directory out from under the others via `cleanup`/`Drop`, producing
//! Docker errors like:
//!
//! ```text
//! failed to fulfil mount request: open /host_mnt/.../<task_id>:
//! no such file or directory
//! ```
//!
//! The nonce keeps each invocation's `/data/` fully independent.

use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug)]
pub struct DataVolume {
    mount_point: PathBuf,
    backing_file: PathBuf,
    /// True when backed by a real ext4 loop mount (Linux); false for plain directory fallback.
    is_loop_mount: bool,
}

impl DataVolume {
    /// Create a new disk-backed volume with the specified size.
    ///
    /// - Allocates a sparse file of `size_mb` megabytes
    /// - Formats it as ext4
    /// - Loop-mounts it to a unique directory under `base_dir`
    ///
    /// Requires Linux (fallocate, mkfs.ext4, mount -o loop).
    pub async fn create(
        base_dir: &Path,
        task_id: &str,
        size_mb: u64,
    ) -> Result<Self, DataVolumeError> {
        std::cfg_select! {
            target_os = "linux" => {
                Self::create_linux(base_dir, task_id, size_mb).await
            }
            _ => {
                let _ = (base_dir, size_mb);
                Self::create_fallback(task_id).await
            }
        }
    }

    /// Non-Linux fallback: plain directory at
    /// `.hot/box/data/{task_id}-{nonce}`. Uses an absolute path so Docker
    /// treats it as a bind-mount, not a named volume. The nonce isolates
    /// concurrent invocations of the same task_id (see module docs).
    #[cfg(not(target_os = "linux"))]
    async fn create_fallback(task_id: &str) -> Result<Self, DataVolumeError> {
        let dir_name = format!("{}-{}", task_id, Uuid::new_v4().simple());
        let rel_dir = PathBuf::from(".hot/box/data").join(dir_name);
        tokio::fs::create_dir_all(&rel_dir)
            .await
            .map_err(|e| DataVolumeError::Io(format!("create dir {}: {}", rel_dir.display(), e)))?;

        let vol_dir = rel_dir.canonicalize().map_err(|e| {
            DataVolumeError::Io(format!("canonicalize {}: {}", rel_dir.display(), e))
        })?;

        tracing::debug!(path = %vol_dir.display(), "Using plain directory for /data/ (non-Linux fallback)");

        Ok(Self {
            mount_point: vol_dir.clone(),
            backing_file: vol_dir,
            is_loop_mount: false,
        })
    }

    #[cfg(target_os = "linux")]
    async fn create_linux(
        base_dir: &Path,
        task_id: &str,
        size_mb: u64,
    ) -> Result<Self, DataVolumeError> {
        // Nonce isolates concurrent invocations of the same task_id —
        // otherwise sibling invocations would clobber each other's
        // backing file and mount point. See module docs.
        let vol_dir = base_dir.join(format!("hot-data-{}-{}", task_id, Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&vol_dir)
            .await
            .map_err(|e| DataVolumeError::Io(format!("create dir {}: {}", vol_dir.display(), e)))?;

        let backing_file = vol_dir.join("data.img");
        let mount_point = vol_dir.join("mnt");
        tokio::fs::create_dir_all(&mount_point).await.map_err(|e| {
            DataVolumeError::Io(format!("create mount {}: {}", mount_point.display(), e))
        })?;

        // Create sparse file
        let size_bytes = size_mb * 1024 * 1024;
        let output = tokio::process::Command::new("fallocate")
            .args([
                "-l",
                &size_bytes.to_string(),
                backing_file.to_string_lossy().as_ref(),
            ])
            .output()
            .await
            .map_err(|e| DataVolumeError::Io(format!("fallocate: {}", e)))?;

        if !output.status.success() {
            // Fallback: truncate for systems without fallocate
            let output = tokio::process::Command::new("truncate")
                .args([
                    "-s",
                    &size_bytes.to_string(),
                    backing_file.to_string_lossy().as_ref(),
                ])
                .output()
                .await
                .map_err(|e| DataVolumeError::Io(format!("truncate: {}", e)))?;

            if !output.status.success() {
                return Err(DataVolumeError::Io(format!(
                    "failed to create backing file: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }

        // Format as ext4 (quiet, no journaling for perf)
        let output = tokio::process::Command::new("mkfs.ext4")
            .args([
                "-q",
                "-O",
                "^has_journal",
                "-F",
                backing_file.to_string_lossy().as_ref(),
            ])
            .output()
            .await
            .map_err(|e| DataVolumeError::Format(e.to_string()))?;

        if !output.status.success() {
            return Err(DataVolumeError::Format(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // Mount via loop device
        let output = tokio::process::Command::new("mount")
            .args([
                "-o",
                "loop,nosuid,nodev,noexec",
                backing_file.to_string_lossy().as_ref(),
                mount_point.to_string_lossy().as_ref(),
            ])
            .output()
            .await
            .map_err(|e| DataVolumeError::Mount(e.to_string()))?;

        if !output.status.success() {
            return Err(DataVolumeError::Mount(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // Make writable by container user (nobody = 65534)
        let output = tokio::process::Command::new("chown")
            .args(["65534:65534", mount_point.to_string_lossy().as_ref()])
            .output()
            .await
            .ok();

        if let Some(out) = output
            && !out.status.success()
        {
            tracing::warn!(
                "chown on data volume mount point failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        Ok(Self {
            mount_point,
            backing_file,
            is_loop_mount: true,
        })
    }

    /// Get the host-side mount point path (for bind-mounting into containers).
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Explicitly clean up the volume.
    pub async fn cleanup(&self) {
        if self.is_loop_mount {
            let _ = tokio::process::Command::new("umount")
                .arg(self.mount_point.to_string_lossy().to_string())
                .output()
                .await;
            let _ = tokio::fs::remove_file(&self.backing_file).await;
            if let Some(parent) = self.backing_file.parent() {
                let _ = tokio::fs::remove_dir_all(parent).await;
            }
        } else {
            let _ = tokio::fs::remove_dir_all(&self.mount_point).await;
        }
    }
}

impl Drop for DataVolume {
    fn drop(&mut self) {
        if self.is_loop_mount {
            let mount_str = self.mount_point.to_string_lossy().to_string();
            let _ = std::process::Command::new("umount")
                .arg(&mount_str)
                .output();
            let _ = std::fs::remove_file(&self.backing_file);
            if let Some(parent) = self.backing_file.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        } else {
            let _ = std::fs::remove_dir_all(&self.mount_point);
        }
    }
}

#[derive(Debug)]
pub enum DataVolumeError {
    Io(String),
    #[cfg(target_os = "linux")]
    Format(String),
    #[cfg(target_os = "linux")]
    Mount(String),
}

impl std::fmt::Display for DataVolumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "Data volume I/O error: {}", e),
            #[cfg(target_os = "linux")]
            Self::Format(e) => write!(f, "Data volume format error: {}", e),
            #[cfg(target_os = "linux")]
            Self::Mount(e) => write!(f, "Data volume mount error: {}", e),
        }
    }
}

impl std::error::Error for DataVolumeError {}
