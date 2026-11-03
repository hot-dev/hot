//! Extracted bundles on a worker's local disk.
//!
//! A worker unpacks each bundle build it runs into `<root>/build-<id>` and
//! reuses the directory for later runs. The directory is only a cache of build
//! storage: the next run of an evicted build extracts it again. Kept forever
//! they fill the disk. A self-hosted installation keeps them on its data disk
//! and clears them only when the service starts, so a long-lived worker relies
//! on eviction.
//!
//! [`BundleExtractions::sweep`] removes what the database says no deployment
//! needs. A run can outlive its build's deployment, so runs hold a
//! [`BundleLease`]. A lease also holds a shared OS lock on `build-<id>.lease`,
//! so it counts for every worker process on the root: several can share one, as
//! standalone services in a project directory do. A process that kept a path to
//! an extraction another evicted finds it missing and extracts it again.

use crate::db::{Build, BuildError, DatabasePool};
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How often a worker sweeps. Also the longest a replaced build stays on disk
/// once nothing uses it.
pub const EVICTION_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// An evicted directory is renamed to this prefix before it is removed, so no
/// run can see it half deleted. A worker stopped mid-removal leaves one behind.
const EVICTING_PREFIX: &str = ".evicting-";

type Leases = Arc<Mutex<HashMap<Uuid, usize>>>;

/// The directory one worker process extracts bundles into.
#[derive(Clone)]
pub struct BundleExtractions {
    root: PathBuf,
    leases: Leases,
}

/// Keeps a build's extraction on disk until dropped.
#[must_use]
pub struct BundleLease {
    leases: Leases,
    build_id: Uuid,
    /// Holds the shared lock other processes' sweeps respect. Without it, as on
    /// a filesystem with no locks, the lease still counts in this process.
    _lock: Option<File>,
}

impl Drop for BundleLease {
    fn drop(&mut self) {
        let mut leases = self.leases.lock();
        if let Some(count) = leases.get_mut(&self.build_id) {
            *count -= 1;
            if *count == 0 {
                leases.remove(&self.build_id);
            }
        }
    }
}

impl BundleExtractions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            leases: Arc::default(),
        }
    }

    /// Where `build_id` is extracted.
    pub fn extract_dir(&self, build_id: &Uuid) -> PathBuf {
        self.root.join(format!("build-{}", build_id.simple()))
    }

    /// Take this before looking for the build's extraction. The extraction
    /// then either stays until the lease is dropped, or is already gone and
    /// the caller extracts it again.
    pub fn lease(&self, build_id: Uuid) -> BundleLease {
        // Counted first, and not under this mutex while waiting for the file
        // lock: an eviction that holds the file lock takes the mutex to see
        // this lease and back off.
        *self.leases.lock().entry(build_id).or_insert(0) += 1;
        let lock = self
            .lock_lease_file(&build_id)
            .inspect_err(|error| {
                tracing::debug!(%build_id, %error, "Cannot lock a bundle extraction's lease file");
            })
            .ok();
        BundleLease {
            leases: Arc::clone(&self.leases),
            build_id,
            _lock: lock,
        }
    }

    fn lease_path(&self, build_id: &Uuid) -> PathBuf {
        self.extract_dir(build_id).with_extension("lease")
    }

    /// A shared lock on the build's lease file. Blocks only while an eviction
    /// renames the extraction, and leaves the caller to find it gone.
    fn lock_lease_file(&self, build_id: &Uuid) -> std::io::Result<File> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.lease_path(build_id);
        loop {
            let file = open_lock_file(&path)?;
            file.lock_shared()?;
            // An eviction unlinks the file: one locked after that keeps nothing
            // on disk, since the next sweep locks the path's new file.
            if is_file_at_path(&file, &path) {
                return Ok(file);
            }
        }
    }

    /// Remove every extraction that no lease holds and no deployment needs,
    /// calling `on_evicted` for each build while no lease on it can start.
    /// An extraction whose build can't be read now stays for the next sweep.
    pub async fn sweep<F>(&self, db: &DatabasePool, on_evicted: F)
    where
        F: Fn(Uuid) + Clone + Send + 'static,
    {
        let extractions = self.clone();
        let extracted = tokio::task::spawn_blocking(move || {
            extractions.finish_interrupted_evictions();
            extractions.extracted_builds()
        })
        .await
        .unwrap_or_default();

        for build_id in extracted {
            if self.leases.lock().contains_key(&build_id) {
                continue;
            }
            match Build::get_build(db, &build_id).await {
                Ok(build) if deployment_needs(&build) => continue,
                Ok(_) | Err(BuildError::NotFound) => {}
                Err(error) => {
                    tracing::warn!(%build_id, %error, "Keeping a bundle extraction: cannot read its build");
                    continue;
                }
            }
            let extractions = self.clone();
            let on_evicted = on_evicted.clone();
            match tokio::task::spawn_blocking(move || {
                extractions.evict(build_id, on_evicted, remove_evicted)
            })
            .await
            {
                Ok(Ok(true)) => tracing::debug!(%build_id, "Removed a bundle extraction"),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%build_id, %error, "Failed to remove a bundle extraction");
                }
                Err(error) => {
                    tracing::warn!(%build_id, %error, "Bundle extraction removal stopped");
                }
            }
        }
    }

    /// The builds with an extraction, a lock file, or a lease file under the
    /// root. Anything else there (temp dirs, other locks) is not ours to remove.
    fn extracted_builds(&self) -> BTreeSet<Uuid> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return BTreeSet::new();
        };
        entries
            .flatten()
            .filter_map(|entry| build_of_entry(entry.file_name().to_str()?))
            .collect()
    }

    fn finish_interrupted_evictions(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(EVICTING_PREFIX)
            {
                remove_evicted(&entry.path());
            }
        }
    }

    /// Whether the extraction was removed. `Ok(false)` means something is using
    /// it: a lease here or in another process, or the extraction lock a worker
    /// holds while it writes. `remove` deletes the directory once it is renamed.
    fn evict(
        &self,
        build_id: Uuid,
        on_evicted: impl Fn(Uuid),
        remove: impl FnOnce(&Path),
    ) -> std::io::Result<bool> {
        let dir = self.extract_dir(&build_id);
        let lock_path = dir.with_extension("lock");
        let mut lock = fd_lock::RwLock::new(open_lock_file(&lock_path)?);
        let Ok(extraction) = lock.try_write() else {
            return Ok(false);
        };
        let lease_path = self.lease_path(&build_id);
        let lease_lock = open_lock_file(&lease_path)?;
        match lease_lock.try_lock() {
            Ok(()) if is_file_at_path(&lease_lock, &lease_path) => {}
            // Another sweep unlinked it just now, and the next one will see.
            Ok(()) | Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error),
        }

        let evicting = self
            .root
            .join(format!("{EVICTING_PREFIX}{}", Uuid::new_v4().simple()));
        {
            let leases = self.leases.lock();
            if leases.contains_key(&build_id) {
                return Ok(false);
            }
            match std::fs::rename(&dir, &evicting) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            on_evicted(build_id);
        }
        // Unlinked before unlocked, so a lease waiting on this lock moves on to
        // a new file rather than holding the old one. The extraction lock goes
        // first and before the removal, which can take a while: a run that found
        // it busy would extract without it, and the next holder would clear that
        // extraction as abandoned. The removal needs no lock: no run can see the
        // renamed directory, and a later sweep finishes it after a crash.
        let _ = std::fs::remove_file(&lock_path);
        let _ = std::fs::remove_file(&lease_path);
        drop(lease_lock);
        drop(extraction);
        remove(&evicting);
        Ok(true)
    }
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Whether `file` is still the file at `path`.
#[cfg(unix)]
fn is_file_at_path(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(open), Ok(current)) => open.dev() == current.dev() && open.ino() == current.ino(),
        _ => false,
    }
}

/// Elsewhere, an open file can't be unlinked out from under its path.
#[cfg(not(unix))]
fn is_file_at_path(_file: &File, _path: &Path) -> bool {
    true
}

/// Whether a deployment runs this build, or is about to. A newer deployment
/// supersedes a candidate, and activating one clears `deployed` on the build it
/// replaces, whose status stays `ready`.
fn deployment_needs(build: &Build) -> bool {
    build.deployed
        || matches!(
            build.runtime_status.as_str(),
            Build::RUNTIME_STATUS_PENDING | Build::RUNTIME_STATUS_LOADING
        )
}

fn build_of_entry(name: &str) -> Option<Uuid> {
    let id = name.strip_prefix("build-")?;
    let id = id
        .strip_suffix(".lock")
        .or_else(|| id.strip_suffix(".lease"))
        .unwrap_or(id);
    let build_id = Uuid::try_parse(id).ok()?;
    (build_id.simple().to_string() == id).then_some(build_id)
}

fn remove_evicted(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "Failed to remove an evicted bundle extraction");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    struct Fixture {
        _directory: tempfile::TempDir,
        extractions: BundleExtractions,
        db: DatabasePool,
        user_id: Uuid,
        project_id: Uuid,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let extractions = BundleExtractions::new(directory.path());
            let db = test_db().await;
            let user_id = Uuid::now_v7();
            let org_id = Uuid::now_v7();
            let env_id = Uuid::now_v7();
            let project_id = Uuid::now_v7();
            crate::db::Org::insert_org(
                &db,
                &org_id,
                "Extraction test",
                &org_id.to_string(),
                "organization",
                &user_id,
            )
            .await
            .unwrap();
            crate::db::Env::insert_env(&db, &env_id, &org_id, "test-env", &user_id)
                .await
                .unwrap();
            crate::db::Project::insert_project(&db, &project_id, &env_id, "test", &user_id)
                .await
                .unwrap();
            Self {
                _directory: directory,
                extractions,
                db,
                user_id,
                project_id,
            }
        }

        async fn build(&self) -> Uuid {
            let build_id = Uuid::now_v7();
            Build::insert_build(
                &self.db,
                &build_id,
                &self.project_id,
                "test-hash",
                1024,
                Build::BUILD_TYPE_BUNDLE,
                &self.user_id,
            )
            .await
            .unwrap();
            self.extract(build_id);
            build_id
        }

        async fn deployed(&self) -> Uuid {
            let build_id = self.build().await;
            Build::deploy_build(&self.db, &build_id, &self.user_id)
                .await
                .unwrap();
            build_id
        }

        async fn replaced(&self) -> Uuid {
            let build_id = self.deployed().await;
            self.deployed().await;
            build_id
        }

        fn extract(&self, build_id: Uuid) {
            let dir = self.extractions.extract_dir(&build_id);
            std::fs::create_dir_all(dir.join("hot/src")).unwrap();
            std::fs::write(dir.join("manifest.hot"), "{}").unwrap();
            std::fs::write(dir.with_extension("lock"), "").unwrap();
        }

        fn extracted(&self, build_id: Uuid) -> bool {
            self.extractions.extract_dir(&build_id).exists()
        }

        async fn sweep(&self) -> Vec<Uuid> {
            let evicted = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&evicted);
            self.extractions
                .sweep(&self.db, move |build_id| recorded.lock().push(build_id))
                .await;
            let mut evicted = evicted.lock().clone();
            evicted.sort();
            evicted
        }
    }

    #[tokio::test]
    async fn sweep_removes_only_what_no_deployment_needs() {
        let fixture = Fixture::new().await;
        let replaced = fixture.replaced().await;
        let current = Build::get_deployed_build_by_project(&fixture.db, &fixture.project_id)
            .await
            .unwrap()
            .unwrap()
            .build_id;
        fixture.extract(current);
        let superseded = fixture.build().await;
        Build::request_bundle_deployment(&fixture.db, &superseded, &fixture.user_id)
            .await
            .unwrap();
        let candidate = fixture.build().await;
        Build::request_bundle_deployment(&fixture.db, &candidate, &fixture.user_id)
            .await
            .unwrap();
        let deleted = Uuid::now_v7();
        fixture.extract(deleted);

        let mut expected = vec![replaced, superseded, deleted];
        expected.sort();
        assert_eq!(fixture.sweep().await, expected);

        for build_id in [replaced, superseded, deleted] {
            assert!(!fixture.extracted(build_id));
            let lock = fixture
                .extractions
                .extract_dir(&build_id)
                .with_extension("lock");
            assert!(!lock.exists());
        }
        for build_id in [current, candidate] {
            assert!(fixture.extracted(build_id));
        }
    }

    #[tokio::test]
    async fn a_lease_keeps_an_extraction_until_the_last_is_dropped() {
        let fixture = Fixture::new().await;
        let replaced = fixture.replaced().await;
        let first = fixture.extractions.lease(replaced);
        let second = fixture.extractions.lease(replaced);

        assert!(fixture.sweep().await.is_empty());
        drop(first);
        assert!(fixture.sweep().await.is_empty());
        assert!(fixture.extracted(replaced));

        drop(second);
        assert_eq!(fixture.sweep().await, vec![replaced]);
        assert!(!fixture.extracted(replaced));
    }

    #[tokio::test]
    async fn a_lease_in_another_process_keeps_an_extraction() {
        let fixture = Fixture::new().await;
        let replaced = fixture.replaced().await;
        // Another process's locks and lease count, on their own open files.
        let other_process = BundleExtractions::new(fixture._directory.path());
        let lease = other_process.lease(replaced);

        assert!(fixture.sweep().await.is_empty());
        assert!(fixture.extracted(replaced));

        drop(lease);
        assert_eq!(fixture.sweep().await, vec![replaced]);
        assert!(!fixture.extracted(replaced));
        let path = fixture.extractions.lease_path(&replaced);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_lease_taken_during_an_eviction_locks_the_next_file() {
        let fixture = Fixture::new().await;
        let build_id = Uuid::now_v7();
        let path = fixture.extractions.lease_path(&build_id);
        // An eviction in progress, holding the file a new lease opens first.
        let evicting = open_lock_file(&path).unwrap();
        evicting.lock().unwrap();

        let other_process = BundleExtractions::new(fixture._directory.path());
        let (leased_tx, leased_rx) = std::sync::mpsc::channel();
        let leasing = std::thread::spawn(move || {
            let lease = other_process.lease(build_id);
            leased_tx.send(()).unwrap();
            lease
        });
        assert!(
            leased_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a lease waits for the eviction"
        );
        std::fs::remove_file(&path).unwrap();
        drop(evicting);
        leased_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let lease = leasing.join().unwrap();

        // The file a later sweep will lock is the one the lease holds.
        let next_sweep = open_lock_file(&path).unwrap();
        assert!(matches!(
            next_sweep.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(lease);
        next_sweep.try_lock().unwrap();
    }

    #[tokio::test]
    async fn a_run_arriving_during_the_removal_holds_a_real_extraction_lock() {
        let fixture = Fixture::new().await;
        let build_id = Uuid::now_v7();
        fixture.extract(build_id);
        let lock_path = fixture
            .extractions
            .extract_dir(&build_id)
            .with_extension("lock");

        let mut arriving = None;
        let evicted = fixture.extractions.evict(
            build_id,
            |_| {},
            |evicting| {
                let file = open_lock_file(&lock_path).unwrap();
                file.try_lock().unwrap();
                arriving = Some(file);
                remove_evicted(evicting);
            },
        );
        assert!(evicted.unwrap());

        // A later process finds the same file, so it can't take this run's
        // extraction for an abandoned one and clear it.
        let arriving = arriving.unwrap();
        assert!(is_file_at_path(&arriving, &lock_path));
        let mut later = fd_lock::RwLock::new(open_lock_file(&lock_path).unwrap());
        assert!(later.try_write().is_err());
    }

    #[tokio::test]
    async fn the_removal_holds_no_lock() {
        let fixture = Fixture::new().await;
        let build_id = Uuid::now_v7();
        fixture.extract(build_id);
        let dir = fixture.extractions.extract_dir(&build_id);
        // Opened before the eviction unlinks them, as by a process just arriving.
        let extraction = open_lock_file(&dir.with_extension("lock")).unwrap();
        let lease = open_lock_file(&fixture.extractions.lease_path(&build_id)).unwrap();

        let evicted = fixture.extractions.evict(
            build_id,
            |_| {},
            |evicting| {
                extraction.try_lock().unwrap();
                lease.try_lock().unwrap();
                remove_evicted(evicting);
            },
        );
        assert!(evicted.unwrap());
    }

    #[tokio::test]
    async fn a_lease_waiting_on_an_eviction_finds_the_old_lock_file_gone() {
        let fixture = Fixture::new().await;
        let build_id = Uuid::now_v7();
        fixture.extract(build_id);
        let lock_path = fixture
            .extractions
            .extract_dir(&build_id)
            .with_extension("lock");
        let other_process = BundleExtractions::new(fixture._directory.path());
        let (leased_tx, leased_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let evicted = fixture.extractions.evict(
                build_id,
                |_| {
                    scope.spawn(|| {
                        let _lease = other_process.lease(build_id);
                        leased_tx.send(lock_path.exists()).unwrap();
                    });
                    assert!(
                        leased_rx.recv_timeout(Duration::from_millis(100)).is_err(),
                        "a lease waits for the eviction"
                    );
                },
                remove_evicted,
            );
            assert!(evicted.unwrap());
        });

        assert!(!leased_rx.recv().unwrap());
    }

    #[tokio::test]
    async fn sweep_removes_a_lease_file_that_outlived_its_extraction() {
        let fixture = Fixture::new().await;
        let deleted = Uuid::now_v7();
        drop(fixture.extractions.lease(deleted));
        let path = fixture.extractions.lease_path(&deleted);
        assert!(path.exists());

        assert_eq!(fixture.sweep().await, vec![deleted]);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn an_extraction_in_progress_is_left_alone() {
        let fixture = Fixture::new().await;
        let replaced = fixture.replaced().await;
        let lock_path = fixture
            .extractions
            .extract_dir(&replaced)
            .with_extension("lock");
        let mut lock = fd_lock::RwLock::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .unwrap(),
        );
        let guard = lock.try_write().unwrap();

        assert!(fixture.sweep().await.is_empty());
        assert!(fixture.extracted(replaced));

        drop(guard);
        assert_eq!(fixture.sweep().await, vec![replaced]);
    }

    #[tokio::test]
    async fn an_extraction_stays_when_its_build_cannot_be_read() {
        let fixture = Fixture::new().await;
        let build_id = Uuid::now_v7();
        fixture.extract(build_id);
        let DatabasePool::Sqlite(pool) = &fixture.db else {
            unreachable!("test_db returns SQLite");
        };
        pool.close().await;

        assert!(fixture.sweep().await.is_empty());
        assert!(fixture.extracted(build_id));
    }

    #[tokio::test]
    async fn sweep_only_touches_its_own_entries() {
        let fixture = Fixture::new().await;
        let root = fixture._directory.path();
        let hyphenated = format!("build-{}", Uuid::now_v7());
        for name in [".tmp-build-1", "build-not-a-uuid", hyphenated.as_str()] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("queue-session.lock"), "").unwrap();
        let interrupted = root.join(format!("{EVICTING_PREFIX}abc"));
        std::fs::create_dir_all(interrupted.join("hot/src")).unwrap();

        assert!(fixture.sweep().await.is_empty());

        assert!(!interrupted.exists());
        for name in [
            ".tmp-build-1",
            "build-not-a-uuid",
            hyphenated.as_str(),
            "queue-session.lock",
        ] {
            assert!(root.join(name).exists(), "{name}");
        }
    }

    #[tokio::test]
    async fn sweeping_a_missing_root_does_nothing() {
        let fixture = Fixture::new().await;
        let extractions = BundleExtractions::new(fixture._directory.path().join("missing"));
        extractions.sweep(&fixture.db, |_| {}).await;
        assert!(!fixture._directory.path().join("missing").exists());
    }
}
