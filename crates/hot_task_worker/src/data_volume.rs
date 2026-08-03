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
//! The mount point is bind-mounted into the container. Cleanup normally
//! happens via the explicit async `cleanup()`; a volume dropped without it
//! (cancelled future, abort) hands its umount/remove sequence to a detached
//! thread from `Drop` — never inline on the dropping thread, where a hung
//! umount would pin a tokio runtime thread.
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use uuid::Uuid;

/// A wedged kernel unmount can park a cleanup thread forever. Bound the
/// number of such detached threads; excess cleanup is intentionally left for
/// startup recovery instead of exhausting the process/thread limit.
const MAX_DETACHED_CLEANUPS: usize = 16;
static ACTIVE_DETACHED_CLEANUPS: AtomicUsize = AtomicUsize::new(0);

fn try_reserve_detached_cleanup(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < limit).then_some(active + 1)
        })
        .is_ok()
}

struct DetachedCleanupSlot;

impl Drop for DetachedCleanupSlot {
    fn drop(&mut self) {
        ACTIVE_DETACHED_CLEANUPS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub struct DataVolume {
    mount_point: PathBuf,
    backing_file: PathBuf,
    /// True when backed by a real ext4 loop mount (Linux); false for plain directory fallback.
    is_loop_mount: bool,
    /// Set once `cleanup()` has run to completion. `Drop` then no-ops
    /// entirely — no thread is spawned and the umount/remove sequence is
    /// never re-run.
    cleaned: AtomicBool,
    /// Test-only observation point: records which thread actually executed
    /// the umount/remove sequence. Never written when the `cleaned` flag
    /// short-circuits.
    #[cfg(test)]
    pub(crate) drop_thread: Option<std::sync::Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>>,
    /// Test-only observation point: receives the `JoinHandle` of the
    /// detached cleanup thread spawned by `Drop`, so tests can join it and
    /// assert on its name. Never written when the `cleaned` flag
    /// short-circuits.
    #[cfg(test)]
    pub(crate) drop_join:
        Option<std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>>,
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
            cleaned: AtomicBool::new(false),
            #[cfg(test)]
            drop_thread: None,
            #[cfg(test)]
            drop_join: None,
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
        // Take ownership of cleanup as soon as the top-level directory
        // exists. Every error return and cancellation after this point drops
        // the guard and schedules removal of any partially-created resource.
        let volume = Self {
            mount_point,
            backing_file,
            is_loop_mount: true,
            cleaned: AtomicBool::new(false),
            #[cfg(test)]
            drop_thread: None,
            #[cfg(test)]
            drop_join: None,
        };
        tokio::fs::create_dir_all(&volume.mount_point)
            .await
            .map_err(|e| {
                DataVolumeError::Io(format!(
                    "create mount {}: {}",
                    volume.mount_point.display(),
                    e
                ))
            })?;

        // Create sparse file
        let size_bytes = size_mb * 1024 * 1024;
        let output = Self::killable_command("fallocate")
            .args([
                "-l",
                &size_bytes.to_string(),
                volume.backing_file.to_string_lossy().as_ref(),
            ])
            .output()
            .await;

        if !output.as_ref().is_ok_and(|output| output.status.success()) {
            // Fallback: truncate for systems without fallocate
            let output = match Self::killable_command("truncate")
                .args([
                    "-s",
                    &size_bytes.to_string(),
                    volume.backing_file.to_string_lossy().as_ref(),
                ])
                .output()
                .await
            {
                Ok(output) => output,
                Err(e) => return Err(DataVolumeError::Io(format!("truncate: {e}"))),
            };

            if !output.status.success() {
                return Err(DataVolumeError::Io(format!(
                    "failed to create backing file: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }

        // Format as ext4 (quiet, no journaling for perf)
        let output = match Self::killable_command("mkfs.ext4")
            .args([
                "-q",
                "-O",
                "^has_journal",
                "-F",
                volume.backing_file.to_string_lossy().as_ref(),
            ])
            .output()
            .await
        {
            Ok(output) => output,
            Err(e) => return Err(DataVolumeError::Format(e.to_string())),
        };

        if !output.status.success() {
            return Err(DataVolumeError::Format(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // Mount via loop device
        let output = match Self::killable_command("mount")
            .args([
                "-o",
                "loop,nosuid,nodev,noexec",
                volume.backing_file.to_string_lossy().as_ref(),
                volume.mount_point.to_string_lossy().as_ref(),
            ])
            .output()
            .await
        {
            Ok(output) => output,
            Err(e) => return Err(DataVolumeError::Mount(e.to_string())),
        };

        if !output.status.success() {
            return Err(DataVolumeError::Mount(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        // Make writable by container user (nobody = 65534)
        let output = match Self::killable_command("chown")
            .args(["65534:65534", volume.mount_point.to_string_lossy().as_ref()])
            .output()
            .await
        {
            Ok(output) => output,
            Err(e) => return Err(DataVolumeError::Io(format!("chown data volume: {e}"))),
        };

        if !output.status.success() {
            return Err(DataVolumeError::Io(format!(
                "chown data volume failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(volume)
    }

    fn killable_command(program: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        command.kill_on_drop(true);
        command
    }

    /// Get the host-side mount point path (for bind-mounting into containers).
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Explicitly clean up the volume.
    pub async fn cleanup(&self) {
        if self.is_loop_mount {
            let unmounted = Self::killable_command("umount")
                .arg(self.mount_point.to_string_lossy().to_string())
                .output()
                .await
                .is_ok_and(|output| output.status.success());
            if !unmounted {
                let _ = Self::killable_command("umount")
                    .args(["-l", self.mount_point.to_string_lossy().as_ref()])
                    .output()
                    .await;
            }
            let _ = tokio::fs::remove_file(&self.backing_file).await;
            if let Some(parent) = self.backing_file.parent() {
                let _ = tokio::fs::remove_dir_all(parent).await;
            }
        } else {
            let _ = tokio::fs::remove_dir_all(&self.mount_point).await;
        }
        // Only reached when every cleanup step ran (this future was not
        // cancelled by a caller's timeout): `Drop` must not repeat the
        // umount/remove sequence synchronously on whatever thread happens to
        // drop the value.
        self.cleaned.store(true, Ordering::Release);
    }

    /// Hand this volume's final cleanup to a detached thread and return
    /// immediately. Used when the bounded async `cleanup()` timed out and
    /// the caller wants the handoff to be explicit at the call site. `Drop`
    /// already detaches by default for un-cleaned volumes, so this is a thin
    /// wrapper: defuse `Drop`, then spawn the same detached cleanup thread.
    ///
    /// The returned handle exists for tests; production callers ignore it.
    pub fn drop_detached(mut self) -> Option<std::thread::JoinHandle<()>> {
        // Defuse Drop: the detached thread spawned below owns the cleanup
        // sequence now, and Drop must not spawn a second one.
        self.cleaned.store(true, Ordering::Release);
        let mount_point = std::mem::take(&mut self.mount_point);
        let backing_file = std::mem::take(&mut self.backing_file);
        #[cfg(test)]
        let drop_thread = self.drop_thread.take();
        Self::spawn_detached_cleanup(
            mount_point,
            backing_file,
            self.is_loop_mount,
            #[cfg(test)]
            drop_thread,
        )
    }

    /// Spawn a detached thread that runs the synchronous umount/remove
    /// sequence. Against a hung (D-state) mount that sequence blocks
    /// forever, so it must never run inline on a tokio worker thread. A
    /// dedicated `std::thread` is used instead of `spawn_blocking` on
    /// purpose — the blocking pool is bounded and shared (bundle
    /// extraction, blocking VM execution), so wedged umounts parked there
    /// would permanently consume its capacity.
    ///
    /// Returns `None` when the thread could not be spawned; the mount is
    /// leaked (with an error logged) rather than ever cleaned up inline.
    fn spawn_detached_cleanup(
        mount_point: PathBuf,
        backing_file: PathBuf,
        is_loop_mount: bool,
        #[cfg(test)] drop_thread: Option<
            std::sync::Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>,
        >,
    ) -> Option<std::thread::JoinHandle<()>> {
        let mount_display = mount_point.display().to_string();
        if !try_reserve_detached_cleanup(&ACTIVE_DETACHED_CLEANUPS, MAX_DETACHED_CLEANUPS) {
            tracing::error!(
                mount_point = %mount_display,
                active_cleanups = ACTIVE_DETACHED_CLEANUPS.load(Ordering::Acquire),
                max_cleanups = MAX_DETACHED_CLEANUPS,
                "Detached data-volume cleanup limit reached; leaking mount until worker restart"
            );
            return None;
        }
        let slot = DetachedCleanupSlot;
        match std::thread::Builder::new()
            .name("hot-datavol-drop".to_string())
            .spawn(move || {
                let _slot = slot;
                #[cfg(test)]
                if let Some(probe) = &drop_thread {
                    *probe.lock().unwrap() = Some(std::thread::current().id());
                }
                Self::cleanup_sync(is_loop_mount, &mount_point, &backing_file);
            }) {
            Ok(handle) => Some(handle),
            Err(e) => {
                // Leak the mount rather than risk a synchronously hung
                // umount on this thread.
                tracing::error!(
                    mount_point = %mount_display,
                    "Failed to spawn detached data-volume drop thread; leaking mount until worker restart: {e}"
                );
                None
            }
        }
    }

    /// The synchronous umount/remove sequence. Only ever executed on the
    /// dedicated `hot-datavol-drop` thread — never on the thread that drops
    /// the volume, where a hung umount would pin a tokio runtime thread.
    fn cleanup_sync(is_loop_mount: bool, mount_point: &Path, backing_file: &Path) {
        if is_loop_mount {
            let mount_str = mount_point.to_string_lossy().to_string();
            let unmounted = std::process::Command::new("umount")
                .arg(&mount_str)
                .output()
                .is_ok_and(|output| output.status.success());
            if !unmounted {
                let _ = std::process::Command::new("umount")
                    .args(["-l", &mount_str])
                    .output();
            }
            let _ = std::fs::remove_file(backing_file);
            if let Some(parent) = backing_file.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        } else {
            let _ = std::fs::remove_dir_all(mount_point);
        }
    }
}

impl Drop for DataVolume {
    /// Detach-by-default: an un-cleaned volume can be dropped from anywhere
    /// — e.g. a cancelled `process_container_task` future when its lease is
    /// lost, or any aborted task — and that is usually a tokio runtime
    /// thread. The umount sequence can hang indefinitely against a D-state
    /// mount, so `Drop` never runs it inline; it hands the taken-out paths
    /// to a detached `hot-datavol-drop` thread. Volumes whose `cleanup()`
    /// completed are a pure no-op (no thread spawned).
    fn drop(&mut self) {
        if self.cleaned.load(Ordering::Acquire) {
            return;
        }
        let mount_point = std::mem::take(&mut self.mount_point);
        let backing_file = std::mem::take(&mut self.backing_file);
        #[cfg(test)]
        let drop_thread = self.drop_thread.take();
        let handle = Self::spawn_detached_cleanup(
            mount_point,
            backing_file,
            self.is_loop_mount,
            #[cfg(test)]
            drop_thread,
        );
        #[cfg(test)]
        if let (Some(slot), Some(handle)) = (self.drop_join.take(), handle) {
            *slot.lock().unwrap() = Some(handle);
        }
        #[cfg(not(test))]
        let _ = handle;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Directory-backed volume (the non-loop-mount shape) rooted in the OS
    /// temp dir, with the drop-thread and join-handle probes installed.
    fn test_volume(label: &str) -> (PathBuf, DataVolume) {
        let mount_point = std::env::temp_dir().join(format!(
            "hot-datavol-test-{}-{}",
            label,
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&mount_point).unwrap();
        let volume = DataVolume {
            mount_point: mount_point.clone(),
            backing_file: mount_point.clone(),
            is_loop_mount: false,
            cleaned: AtomicBool::new(false),
            drop_thread: Some(std::sync::Arc::new(std::sync::Mutex::new(None))),
            drop_join: Some(std::sync::Arc::new(std::sync::Mutex::new(None))),
        };
        (mount_point, volume)
    }

    #[test]
    fn detached_cleanup_reservations_are_bounded() {
        let counter = AtomicUsize::new(0);
        assert!(try_reserve_detached_cleanup(&counter, 2));
        assert!(try_reserve_detached_cleanup(&counter, 2));
        assert!(!try_reserve_detached_cleanup(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
        counter.fetch_sub(1, Ordering::AcqRel);
        assert!(try_reserve_detached_cleanup(&counter, 2));
    }

    #[tokio::test]
    async fn completed_cleanup_defuses_drop() {
        let (mount_point, volume) = test_volume("defuse");
        let probe = volume.drop_thread.clone().unwrap();
        let join_slot = volume.drop_join.clone().unwrap();

        volume.cleanup().await;
        assert!(!mount_point.exists());

        // If Drop re-ran the cleanup sequence it would remove this recreated
        // directory — the double-umount the `cleaned` flag exists to stop.
        std::fs::create_dir_all(&mount_point).unwrap();
        drop(volume);
        assert!(
            join_slot.lock().unwrap().is_none(),
            "Drop must spawn no cleanup thread after cleanup() completed"
        );
        assert!(
            mount_point.exists(),
            "Drop must no-op after cleanup() ran to completion"
        );
        assert!(
            probe.lock().unwrap().is_none(),
            "Drop must not perform any cleanup work after cleanup() completed"
        );
        std::fs::remove_dir_all(&mount_point).unwrap();
    }

    #[test]
    fn uncleaned_drop_detaches_off_the_calling_thread() {
        let (mount_point, volume) = test_volume("drop");
        let probe = volume.drop_thread.clone().unwrap();
        let join_slot = volume.drop_join.clone().unwrap();
        let caller = std::thread::current().id();

        // A plain drop of an un-cleaned volume — the shape of a cancelled
        // future (lease-lost select, abort) — must hand the umount sequence
        // to a detached thread, never run it inline.
        drop(volume);

        let handle = join_slot
            .lock()
            .unwrap()
            .take()
            .expect("Drop of an un-cleaned volume must spawn a detached cleanup thread");
        assert_eq!(
            handle.thread().name(),
            Some("hot-datavol-drop"),
            "the detached cleanup thread must carry its diagnostic name"
        );
        handle.join().unwrap();

        let dropped_on = probe
            .lock()
            .unwrap()
            .expect("the detached thread must have run the cleanup sequence");
        assert_ne!(
            dropped_on, caller,
            "the umount sequence must never execute on the calling (runtime) thread"
        );
        assert!(
            !mount_point.exists(),
            "the detached drop must still perform the actual cleanup"
        );
    }

    #[test]
    fn detached_drop_runs_off_the_calling_thread() {
        let (mount_point, volume) = test_volume("detached");
        let probe = volume.drop_thread.clone().unwrap();
        let caller = std::thread::current().id();

        let handle = volume
            .drop_detached()
            .expect("detached drop thread must spawn");
        handle.join().unwrap();

        let dropped_on = probe
            .lock()
            .unwrap()
            .expect("the detached thread must have run Drop");
        assert_ne!(
            dropped_on, caller,
            "an un-cleaned volume must never run Drop on the calling (runtime) thread"
        );
        assert!(
            !mount_point.exists(),
            "the detached drop must still perform the actual cleanup"
        );
    }
}
