//! Writable volume for container `/data/` directory.
//!
//! **Linux (production):** Allocates a file, formats it as ext4, and
//! loop-mounts it to a temporary directory with size enforcement. The file
//! keeps its blocks for the box's lifetime, so a box can't write more than the
//! disk holds: that disk may be PostgreSQL's and Valkey's too. On tmpfs the file
//! stays sparse: see `ensure_disk_holds`.
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
        .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
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

/// Whether a failed `fallocate` leaves `truncate` to make the backing file: only
/// where fallocate can't work at all, because the tool is missing or the
/// filesystem doesn't support it. Any other failure, no space above all, is
/// returned. A sparse file there would promise the box a `/data` the disk can't
/// hold, and its writes would fill a disk PostgreSQL and Valkey may share.
#[cfg(any(target_os = "linux", test))]
fn fall_back_from_fallocate(
    fallocate: &std::io::Result<std::process::Output>,
) -> Result<(), DataVolumeError> {
    let failure = match fallocate {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Operation not supported") {
                return Ok(());
            }
            stderr.trim().to_string()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => error.to_string(),
    };
    Err(DataVolumeError::Io(format!(
        "could not allocate the backing file: {failure}"
    )))
}

/// Refuse a box larger than what the disk has free for unprivileged writers.
/// `available_bytes` leaves out the blocks ext4 keeps for root. `fallocate` runs
/// as root and can take those too, and then PostgreSQL, which doesn't run as
/// root, finds the disk full.
///
/// A real disk must hold all that admission counts for the box, not just its
/// volume: the image and writable rootfs come next, from a thin pool in a sparse
/// file on the same disk, and a box that only just fits would leave PostgreSQL
/// next to nothing for as long as it runs.
///
/// tmpfs is exempt, and its backing file stays sparse. Its blocks are memory,
/// which the box would hold at its full size for its whole lifetime, and its
/// writes can't reach a data disk. A box that outgrows it gets write errors in
/// its own `/data`.
#[cfg(any(target_os = "linux", test))]
fn ensure_disk_holds(
    available_bytes: u64,
    size_mb: u64,
    on_tmpfs: bool,
) -> Result<(), DataVolumeError> {
    const MB: u64 = 1024 * 1024;
    if on_tmpfs {
        return Ok(());
    }
    let required_mb = crate::resource_budget::disk_admission_mb(size_mb);
    if available_bytes < required_mb.saturating_mul(MB) {
        return Err(DataVolumeError::Io(format!(
            "the disk has {} MB available, less than the {} MB a box with a {} MB /data may use",
            available_bytes / MB,
            required_mb,
            size_mb
        )));
    }
    Ok(())
}

/// `mkfs.ext4` options for a box's `/data`, with no journal for speed. By default
/// `mkfs.ext4` discards a regular file's blocks, which punches holes in the file
/// and gives its allocation back to the disk, so `nodiscard` keeps them. tmpfs
/// has no allocation to keep: see `ensure_disk_holds`.
#[cfg(any(target_os = "linux", test))]
fn mkfs_ext4_options(on_tmpfs: bool) -> &'static [&'static str] {
    if on_tmpfs {
        &["-q", "-O", "^has_journal", "-F"]
    } else {
        &["-q", "-O", "^has_journal", "-E", "nodiscard", "-F"]
    }
}

/// What the filesystem under a new volume offers it.
#[cfg(target_os = "linux")]
struct VolumeDisk {
    available_bytes: u64,
    on_tmpfs: bool,
}

#[cfg(target_os = "linux")]
fn volume_disk(path: &Path) -> std::io::Result<VolumeDisk> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated, and `statfs` fills `stat` when it returns 0.
    let stat = unsafe {
        if libc::statfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        stat.assume_init()
    };
    Ok(VolumeDisk {
        available_bytes: (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
        on_tmpfs: stat.f_type as u64 == libc::TMPFS_MAGIC as u64,
    })
}

/// Held from the free-space check through the allocation, so two boxes can't both
/// pass the check on the same free space.
#[cfg(target_os = "linux")]
static VOLUME_ALLOCATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    /// - Refuses a box the disk doesn't have room for
    /// - Allocates a file of `size_mb` megabytes (sparse only on tmpfs and where
    ///   fallocate can't work)
    /// - Formats it as ext4, keeping the file's blocks
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

        let size_bytes = size_mb * 1024 * 1024;
        let allocating = VOLUME_ALLOCATION.lock().await;
        let disk = tokio::task::spawn_blocking(move || volume_disk(&vol_dir))
            .await
            .map_err(std::io::Error::other)
            .flatten()
            .map_err(|e| DataVolumeError::Io(format!("check free space: {e}")))?;
        ensure_disk_holds(disk.available_bytes, size_mb, disk.on_tmpfs)?;

        let allocated = !disk.on_tmpfs && volume.allocate_backing_file(size_bytes).await?;
        if !allocated {
            // Sparse: on tmpfs, and for systems without fallocate
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

        drop(allocating);

        // Format as ext4
        let output = match Self::killable_command("mkfs.ext4")
            .args(mkfs_ext4_options(disk.on_tmpfs))
            .arg(&volume.backing_file)
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

    /// `fallocate` the backing file. False where that can't work at all, which
    /// leaves `truncate` to make the file: see `fall_back_from_fallocate`.
    #[cfg(target_os = "linux")]
    async fn allocate_backing_file(&self, size_bytes: u64) -> Result<bool, DataVolumeError> {
        // In English for fall_back_from_fallocate.
        let output = Self::killable_command("fallocate")
            .env("LC_ALL", "C")
            .args([
                "-l",
                &size_bytes.to_string(),
                self.backing_file.to_string_lossy().as_ref(),
            ])
            .output()
            .await;

        if output.as_ref().is_ok_and(|output| output.status.success()) {
            return Ok(true);
        }
        if let Err(error) = fall_back_from_fallocate(&output) {
            // A fallocate that runs out of space keeps what it got, and
            // removal on drop waits for a cleanup thread, if it gets one.
            let _ = tokio::fs::remove_file(&self.backing_file).await;
            return Err(error);
        }
        Ok(false)
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

    fn fallocate_failed(stderr: &str) -> std::io::Result<std::process::Output> {
        Ok(std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn only_a_fallocate_that_cannot_work_falls_back_to_a_sparse_file() {
        let unsupported = fallocate_failed("fallocate: fallocate failed: Operation not supported");
        assert!(fall_back_from_fallocate(&unsupported).is_ok());
        let missing = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(fall_back_from_fallocate(&missing).is_ok());

        let no_space = fallocate_failed("fallocate: fallocate failed: No space left on device\n");
        let error = fall_back_from_fallocate(&no_space).unwrap_err().to_string();
        assert!(error.contains("No space left on device"), "{error}");
        // It couldn't start, which says nothing about what the disk can hold.
        let spawn_failed = Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(fall_back_from_fallocate(&spawn_failed).is_err());
    }

    #[test]
    fn a_volume_must_fit_in_what_unprivileged_writers_have_free() {
        const MB: u64 = 1024 * 1024;
        // A real disk holds what admission counts for the box: room to fit the
        // volume alone isn't enough.
        assert!(ensure_disk_holds(8448 * MB, 5120, false).is_ok());
        assert!(ensure_disk_holds(5120 * MB, 5120, false).is_err());
        let error = ensure_disk_holds(8448 * MB - 1, 5120, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("8447 MB available"), "{error}");
        assert!(error.contains("the 8448 MB a box"), "{error}");
        assert!(error.contains("a 5120 MB /data"), "{error}");

        // tmpfs gets a sparse file, so its free space says nothing about the box.
        assert!(ensure_disk_holds(0, 5120, true).is_ok());
    }

    #[test]
    fn only_a_real_disk_has_mkfs_keep_the_file_blocks() {
        let keeps_blocks = |options: &[&str]| options.windows(2).any(|o| o == ["-E", "nodiscard"]);
        assert!(keeps_blocks(mkfs_ext4_options(false)));
        assert!(!keeps_blocks(mkfs_ext4_options(true)));
        for on_tmpfs in [false, true] {
            assert!(
                mkfs_ext4_options(on_tmpfs)
                    .windows(2)
                    .any(|o| o == ["-O", "^has_journal"])
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn volume_disk_reads_the_filesystem_under_a_path() {
        let disk = volume_disk(&std::env::temp_dir()).unwrap();
        assert!(disk.available_bytes > 0);
        assert!(volume_disk(Path::new("/nonexistent/hot-data-volume")).is_err());
    }

    /// Run as root where loop mounts work, with `/var/tmp` not on tmpfs and 3 GB free:
    /// `cargo test -p hot_task_worker a_volume_keeps_its_allocation -- --ignored`
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires root and loop devices"]
    async fn a_volume_keeps_its_allocation_and_one_the_disk_cannot_hold_is_refused() {
        use std::os::unix::fs::MetadataExt;
        const MB: u64 = 1024 * 1024;

        let base = Path::new("/var/tmp").join(format!("hot-datavol-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).unwrap();

        let volume = DataVolume::create(&base, "keeps", 64).await.unwrap();
        let allocated = std::fs::metadata(&volume.backing_file).unwrap().blocks() * 512;
        assert!(
            allocated >= 64 * MB,
            "mkfs gave the allocation back: {allocated} bytes of {}",
            64 * MB
        );

        // Fits in what is left after the first, with no room for its rootfs.
        let too_big = volume_disk(&base).unwrap().available_bytes / MB - 1;
        let error = DataVolume::create(&base, "refused", too_big)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("MB available"), "{error}");

        volume.cleanup().await;
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A 600 MB tmpfs in a new directory, for the root-only tmpfs tests.
    #[cfg(target_os = "linux")]
    fn mount_test_tmpfs() -> PathBuf {
        let base = Path::new("/var/tmp").join(format!("hot-datavol-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).unwrap();
        let mounted = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "-o", "size=600m", "tmpfs"])
            .arg(&base)
            .status()
            .unwrap();
        assert!(mounted.success());
        assert!(volume_disk(&base).unwrap().on_tmpfs);
        base
    }

    #[cfg(target_os = "linux")]
    fn unmount_test_tmpfs(base: &Path) {
        let unmounted = std::process::Command::new("umount")
            .arg(base)
            .status()
            .unwrap();
        assert!(unmounted.success());
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Run as root where loop mounts work:
    /// `cargo test -p hot_task_worker on_tmpfs -- --ignored`
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires root and loop devices"]
    async fn boxes_started_together_on_tmpfs_each_get_a_volume() {
        let base = mount_test_tmpfs();

        // Together they're more than the tmpfs has.
        let (first, second) = tokio::join!(
            DataVolume::create(&base, "first", 400),
            DataVolume::create(&base, "second", 400),
        );
        let (first, second) = (first.unwrap(), second.unwrap());

        first.cleanup().await;
        second.cleanup().await;
        unmount_test_tmpfs(&base);
    }

    /// Run as root where loop mounts work:
    /// `cargo test -p hot_task_worker on_tmpfs -- --ignored`
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires root and loop devices"]
    async fn a_box_larger_than_the_free_space_on_tmpfs_gets_a_sparse_volume() {
        use std::os::unix::fs::MetadataExt;
        const MB: u64 = 1024 * 1024;

        let base = mount_test_tmpfs();

        let volume = DataVolume::create(&base, "larger", 1024).await.unwrap();
        let metadata = std::fs::metadata(&volume.backing_file).unwrap();
        assert_eq!(metadata.len(), 1024 * MB);
        let allocated = metadata.blocks() * 512;
        assert!(
            allocated < 300 * MB,
            "the tmpfs holds {allocated} bytes of a sparse file"
        );

        std::fs::write(volume.mount_point().join("written"), b"data").unwrap();

        volume.cleanup().await;
        unmount_test_tmpfs(&base);
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
