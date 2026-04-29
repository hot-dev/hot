//! Container/VM executor backends
//!
//! Provides pluggable backends for executing isolated workloads:
//! - **Docker**: Uses bollard (works on Mac, Linux, Windows)
//! - **Kata**: Uses Kata Containers with QEMU for microVM isolation (Linux with KVM only)

use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

/// Denied image patterns. Any image matching a prefix in this list is rejected.
/// Security-sensitive images that could escalate privileges or access host resources.
const DENIED_IMAGE_PREFIXES: &[&str] = &[
    "docker:", // Docker-in-Docker
    "docker.io/docker",
    "rancher/",
    "gcr.io/k8s-",
    "registry.k8s.io/",
    "quay.io/coreos/etcd",
];

/// Fully-denied image names (exact match).
const DENIED_IMAGES: &[&str] = &["docker", "docker:dind", "docker:latest", "privileged"];

/// Denied image tags that suggest elevated access requirements.
const DENIED_TAGS: &[&str] = &["dind", "privileged"];

/// Minimal capability set needed for writable package-install workloads.
const WRITABLE_ROOTFS_CAPS: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "SETGID",
    "SETUID",
    "NET_RAW",
    "SYS_CHROOT",
    "MKNOD",
];

/// Check whether an image is allowed under the denylist policy.
///
/// Everything is allowed by default except:
/// - Images matching denied prefixes (Docker-in-Docker, k8s infra, etc.)
/// - Images with denied tags (dind, privileged)
/// - Empty image names
pub fn is_image_allowed(image: &str) -> bool {
    if image.is_empty() {
        return false;
    }

    let image_lower = image.to_lowercase();

    // Check exact deny
    if DENIED_IMAGES.contains(&image_lower.as_str()) {
        return false;
    }

    // Check prefix deny
    for prefix in DENIED_IMAGE_PREFIXES {
        if image_lower.starts_with(prefix) {
            return false;
        }
    }

    // Check tag deny (after the last ':')
    if let Some(tag) = image_lower.rsplit(':').next()
        && DENIED_TAGS.contains(&tag)
    {
        return false;
    }

    true
}

/// Execution output from a container or microVM.
#[derive(Debug)]
pub struct ContainerOutput {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub container_id: String,
    pub timed_out: bool,
    pub oom_killed: bool,
}

/// Describe the exit code in human-readable terms when it indicates a signal kill.
/// Returns `None` for normal exit codes (0-128).
pub fn describe_exit_code(exit_code: i64) -> Option<&'static str> {
    match exit_code {
        137 => Some("killed (likely OOM — out of memory)"),
        139 => Some("crashed (segmentation fault)"),
        134 => Some("aborted (SIGABRT)"),
        135 => Some("crashed (bus error)"),
        _ => None,
    }
}

/// Timing breakdown for container execution.
#[derive(Debug, Default)]
pub struct ContainerTimings {
    pub slot_wait_ms: i64,
    pub image_pull_ms: i64,
    pub execution_ms: i64,
    pub logs_collect_ms: i64,
}

/// Backend selection for container execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Docker,
    #[cfg(all(target_os = "linux", feature = "kata"))]
    Kata,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Docker => write!(f, "docker"),
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata => write!(f, "kata"),
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "docker" | "" => Ok(Self::Docker),
            #[cfg(all(target_os = "linux", feature = "kata"))]
            "kata" | "firecracker" | "fc" => Ok(Self::Kata),
            #[cfg(not(all(target_os = "linux", feature = "kata")))]
            "kata" | "firecracker" | "fc" => {
                Err("Kata backend requires Linux with KVM and the 'kata' feature flag".to_string())
            }
            _ => Err(format!(
                "Unknown container backend: '{}'. Available: docker, kata",
                s
            )),
        }
    }
}

#[derive(Debug)]
pub enum ExecutorError {
    ImageNotAllowed(String),
    Connection(String),
    ImagePull(String),
    Create(String),
    Start(String),
    SlotTimeout(u64),
    ContainerNotFound(String),
    Other(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImageNotAllowed(img) => write!(
                f,
                "Image '{}' is not allowed by the container image policy",
                img
            ),
            Self::Connection(e) => write!(f, "Failed to connect to backend: {}", e),
            Self::ImagePull(e) => write!(f, "Failed to pull image: {}", e),
            Self::Create(e) => write!(f, "Failed to create container/VM: {}", e),
            Self::Start(e) => write!(f, "Failed to start container/VM: {}", e),
            Self::SlotTimeout(secs) => {
                write!(f, "Timed out waiting for execution slot ({}s)", secs)
            }
            Self::ContainerNotFound(id) => {
                write!(f, "Container not found (removed or never created): {}", id)
            }
            Self::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ExecutorError {}

pub type ExecutorResult<T> = Result<T, ExecutorError>;

/// Extra bind mounts and env vars to inject into the container.
#[derive(Debug, Default, Clone)]
pub struct ContainerExtras {
    /// Bind mount specifications: `["host:container:ro", ...]`
    pub binds: Vec<String>,
    /// Additional environment variables: `["KEY=value", ...]`
    pub extra_env: Vec<String>,
    /// When true, the root filesystem is writable (needed for `apk add` etc.)
    pub writable_rootfs: bool,
    /// Override the image ENTRYPOINT (e.g. `["sh"]` for images with non-shell entrypoints).
    pub entrypoint: Option<Vec<String>>,
    /// Host path to the data volume mount point (bind-mounted at `/data` inside the container).
    pub data_volume_path: Option<String>,
    /// When true, the container needs host network access for the file server transport
    /// (TCP on macOS where VirtioFS doesn't support Unix socket bind mounts).
    pub needs_host_network: bool,
}

/// VMM selection within the Kata backend.
#[cfg(all(target_os = "linux", feature = "kata"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KataVmm {
    /// QEMU — works in nested KVM (EC2). Uses kernel AF_VSOCK.
    #[default]
    Qemu,
    /// Firecracker via runtime-rs/Dragonball — requires bare metal (no nested KVM).
    /// Uses hybrid vsock (Unix domain sockets on host side).
    Firecracker,
}

#[cfg(all(target_os = "linux", feature = "kata"))]
impl std::fmt::Display for KataVmm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Qemu => write!(f, "qemu"),
            Self::Firecracker => write!(f, "firecracker"),
        }
    }
}

/// Vsock setup information passed to the pre-start hook, varies by VMM.
#[cfg(all(target_os = "linux", feature = "kata"))]
pub enum VsockSetup {
    /// QEMU: kernel AF_VSOCK — host binds VMADDR_CID_ANY:port.
    AfVsock,
    /// Firecracker: hybrid vsock UDS — host binds to `<path>_<port>`.
    HybridUds { path: std::path::PathBuf },
}

/// Async callback invoked after the VM/container task is created but before
/// it is started. Receives vsock setup info so the caller can start the
/// appropriate listener (AF_VSOCK for QEMU, UDS for Firecracker).
#[cfg(all(target_os = "linux", feature = "kata"))]
pub type PreStartHook = Box<
    dyn FnOnce(
            VsockSetup,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send,
>;

#[cfg(not(all(target_os = "linux", feature = "kata")))]
pub type PreStartHook = Box<
    dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send,
>;

/// Unified executor dispatching to Docker or Kata.
pub enum BoxExecutor {
    Docker(docker::DockerExecutor),
    #[cfg(all(target_os = "linux", feature = "kata"))]
    Kata(kata::KataExecutor),
}

#[cfg(all(target_os = "linux", feature = "kata"))]
const DEFAULT_CONTAINERD_SOCKET: &str = "/run/kata-containerd/containerd.sock";

impl BoxExecutor {
    pub async fn new(
        backend: Backend,
        max_concurrent: usize,
        slot_timeout_secs: u64,
        #[allow(unused_variables)] containerd_socket: Option<&str>,
        #[allow(unused_variables)] vmm: Option<&str>,
    ) -> ExecutorResult<Self> {
        match backend {
            Backend::Docker => {
                let executor = docker::DockerExecutor::new(max_concurrent, slot_timeout_secs)?;
                Ok(Self::Docker(executor))
            }
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Backend::Kata => {
                let socket = containerd_socket.unwrap_or(DEFAULT_CONTAINERD_SOCKET);
                let kata_vmm = match vmm.unwrap_or("qemu") {
                    "firecracker" | "fc" => KataVmm::Firecracker,
                    _ => KataVmm::Qemu,
                };
                let executor =
                    kata::KataExecutor::new(socket, max_concurrent, slot_timeout_secs, kata_vmm)
                        .await?;
                Ok(Self::Kata(executor))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_with_extras(
        &self,
        image: &str,
        cmd: Option<Vec<String>>,
        env: Option<Vec<String>>,
        timeout_secs: u64,
        trace_id: Option<&str>,
        limits: Option<&crate::box_limits::BoxLimits>,
        extras: Option<&ContainerExtras>,
        #[allow(unused_variables)] pre_start_hook: Option<PreStartHook>,
    ) -> ExecutorResult<(ContainerOutput, ContainerTimings)> {
        let mut timings = ContainerTimings::default();
        let result = match self {
            Self::Docker(e) => {
                e.execute_with_limits(
                    image,
                    cmd,
                    env,
                    timeout_secs,
                    &mut timings,
                    trace_id,
                    limits,
                    extras,
                )
                .await
            }
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(e) => {
                e.execute(
                    image,
                    cmd,
                    env,
                    timeout_secs,
                    &mut timings,
                    trace_id,
                    limits,
                    extras,
                    pre_start_hook,
                )
                .await
            }
        };
        result.map(|output| (output, timings))
    }

    /// Pull image, create container, start it, and return the container_id.
    /// The container continues running after this returns.
    /// Only supported for Docker backend.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_and_start(
        &self,
        image: &str,
        cmd: Option<Vec<String>>,
        env: Option<Vec<String>>,
        trace_id: Option<&str>,
        limits: Option<&crate::box_limits::BoxLimits>,
        extras: Option<&ContainerExtras>,
        timings: &mut ContainerTimings,
    ) -> ExecutorResult<String> {
        match self {
            Self::Docker(e) => {
                e.create_and_start(image, cmd, env, trace_id, limits, extras, timings)
                    .await
            }
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(_) => Err(ExecutorError::Other(
                "Phased execution not supported for Kata".into(),
            )),
        }
    }

    /// Check if a container is still running. Returns `Some(exit_code)` if stopped, `None` if running.
    pub async fn inspect_status(&self, container_id: &str) -> ExecutorResult<Option<i64>> {
        match self {
            Self::Docker(e) => e.inspect_status(container_id).await,
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(_) => Err(ExecutorError::Other(
                "inspect_status not supported for Kata".into(),
            )),
        }
    }

    /// Collect logs from a container (running or stopped).
    pub async fn collect_logs(&self, container_id: &str) -> ExecutorResult<(String, String)> {
        match self {
            Self::Docker(e) => e.collect_logs(container_id).await,
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(_) => Err(ExecutorError::Other(
                "collect_logs not supported for Kata".into(),
            )),
        }
    }

    /// Kill and remove a container.
    pub async fn kill_and_remove(&self, container_id: &str) {
        match self {
            Self::Docker(e) => e.kill_and_remove(container_id).await,
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(_) => {}
        }
    }

    /// Remove a stopped container.
    pub async fn remove_container(&self, container_id: &str) {
        match self {
            Self::Docker(e) => {
                e.remove_container(container_id).await;
            }
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(_) => {}
        }
    }

    pub fn backend(&self) -> Backend {
        match self {
            Self::Docker(_) => Backend::Docker,
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(_) => Backend::Kata,
        }
    }

    #[cfg(all(target_os = "linux", feature = "kata"))]
    pub fn kata_vmm(&self) -> Option<KataVmm> {
        match self {
            Self::Kata(e) => Some(e.vmm()),
            _ => None,
        }
    }

    /// Backend-specific listing of containers that are still registered with
    /// the executor's runtime (Docker daemon / kata-containerd) and were
    /// created by a `hot-task-worker`.
    ///
    /// For Docker, this enumerates containers labeled
    /// `hot.dev/managed-by=hot-task-worker`; the `hot.dev/task-id` label is
    /// returned when present.  For Kata, this enumerates every container in
    /// the `hot-box` namespace (kata-containerd is a host service shared
    /// across worker generations, so any container found here belongs to a
    /// previous worker) — see `KataExecutor::list_orphan_containers` for the
    /// exact semantics.
    #[cfg_attr(not(all(target_os = "linux", feature = "kata")), allow(dead_code))]
    pub async fn list_orphan_containers(&self) -> ExecutorResult<Vec<(String, Option<String>)>> {
        match self {
            Self::Docker(_) => {
                // The Docker adoption path uses bollard's typed
                // `list_containers` directly inside `adopt_orphaned_containers`
                // (it needs more than just id+label — it also wants state).
                // For symmetry the helper still exists, but returns empty for
                // Docker so Docker continues to use the existing typed path.
                Ok(Vec::new())
            }
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(e) => e.list_orphan_containers().await,
        }
    }

    /// Force-cleanup an orphan container previously returned by
    /// `list_orphan_containers`. Best-effort; never returns an error.
    #[cfg_attr(not(all(target_os = "linux", feature = "kata")), allow(dead_code))]
    pub async fn cleanup_orphan(&self, container_id: &str) {
        match self {
            Self::Docker(e) => {
                e.kill_and_remove(container_id).await;
            }
            #[cfg(all(target_os = "linux", feature = "kata"))]
            Self::Kata(e) => {
                e.cleanup_orphan(container_id).await;
            }
        }
    }
}

mod docker {
    use super::*;
    use crate::log_accumulator::LogAccumulator;
    use bollard::Docker;
    use bollard::models::{ContainerCreateBody, HostConfig};
    use bollard::query_parameters::{CreateImageOptionsBuilder, WaitContainerOptionsBuilder};
    use futures::stream::StreamExt;
    use std::collections::HashMap;

    pub struct DockerExecutor {
        pub(crate) docker: Docker,
        semaphore: Arc<Semaphore>,
        slot_timeout_secs: u64,
    }

    impl DockerExecutor {
        pub fn new(max_concurrent: usize, slot_timeout_secs: u64) -> ExecutorResult<Self> {
            let docker = Docker::connect_with_local_defaults()
                .map_err(|e| ExecutorError::Connection(e.to_string()))?;

            Ok(Self {
                docker,
                semaphore: Arc::new(Semaphore::new(max_concurrent)),
                slot_timeout_secs,
            })
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn execute_with_limits(
            &self,
            image: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            timeout_secs: u64,
            timings: &mut ContainerTimings,
            trace_id: Option<&str>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
        ) -> ExecutorResult<ContainerOutput> {
            if !is_image_allowed(image) {
                return Err(ExecutorError::ImageNotAllowed(image.to_string()));
            }

            let slot_start = Instant::now();
            let permit = match tokio::time::timeout(
                std::time::Duration::from_secs(self.slot_timeout_secs),
                self.semaphore.acquire(),
            )
            .await
            {
                Ok(Ok(permit)) => {
                    timings.slot_wait_ms = slot_start.elapsed().as_millis() as i64;
                    tracing::debug!(
                        trace_id = trace_id,
                        image = %image,
                        slot_wait_ms = timings.slot_wait_ms,
                        "box.slot.acquired"
                    );
                    permit
                }
                Ok(Err(_)) => {
                    return Err(ExecutorError::Other(
                        "Container semaphore closed unexpectedly".into(),
                    ));
                }
                Err(_) => {
                    return Err(ExecutorError::SlotTimeout(self.slot_timeout_secs));
                }
            };

            let result = self
                .execute_inner(
                    image,
                    cmd,
                    env,
                    timeout_secs,
                    timings,
                    trace_id,
                    limits,
                    extras,
                )
                .await;

            drop(permit);
            result
        }

        /// Pull image, create and start a container. Returns the container ID.
        #[allow(clippy::too_many_arguments)]
        pub async fn create_and_start(
            &self,
            image: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            trace_id: Option<&str>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
            timings: &mut ContainerTimings,
        ) -> ExecutorResult<String> {
            if !is_image_allowed(image) {
                return Err(ExecutorError::ImageNotAllowed(image.to_string()));
            }

            let pull_start = Instant::now();
            let create_image_options = CreateImageOptionsBuilder::default()
                .from_image(image)
                .build();
            let mut pull_stream = self
                .docker
                .create_image(Some(create_image_options), None, None);
            while let Some(pull_result) = pull_stream.next().await {
                if let Err(e) = pull_result {
                    return Err(ExecutorError::ImagePull(format!(
                        "Failed to pull image '{}': {}",
                        image, e
                    )));
                }
            }
            timings.image_pull_ms = pull_start.elapsed().as_millis() as i64;

            let memory_bytes =
                limits.map_or(512 * 1024 * 1024, |l| (l.memory_mb * 1024 * 1024) as i64);
            let cpu_quota = limits.map_or(50000, |l| l.cpu_quota as i64);
            let tmp_size = limits.map_or(500, |l| l.tmp_size_mb);
            let wants_network = limits.is_some_and(|l| l.network);
            let needs_host_network = extras.is_some_and(|e| e.needs_host_network);
            let network_mode = if wants_network || needs_host_network {
                "bridge"
            } else {
                "none"
            };

            let merged_env = {
                let mut all_env = env.unwrap_or_default();
                if let Some(ext) = extras {
                    all_env.extend(ext.extra_env.iter().cloned());
                }
                if all_env.is_empty() {
                    None
                } else {
                    Some(all_env)
                }
            };

            let binds = extras
                .filter(|ext| !ext.binds.is_empty())
                .map(|ext| ext.binds.clone());

            let mut labels = HashMap::new();
            labels.insert(
                "hot.dev/managed-by".to_string(),
                "hot-task-worker".to_string(),
            );
            if let Some(tid) = trace_id {
                labels.insert("hot.dev/task-id".to_string(), tid.to_string());
            }

            let writable = extras.is_some_and(|e| e.writable_rootfs);
            let entrypoint = extras.and_then(|e| e.entrypoint.clone());
            let config = ContainerCreateBody {
                image: Some(image.to_string()),
                cmd,
                entrypoint,
                env: merged_env,
                labels: Some(labels),
                host_config: Some(HostConfig {
                    network_mode: Some(network_mode.to_string()),
                    memory: Some(memory_bytes),
                    memory_swap: Some(memory_bytes),
                    cpu_quota: Some(cpu_quota),
                    pids_limit: Some(100i64),
                    readonly_rootfs: Some(!writable),
                    security_opt: Some(vec!["no-new-privileges".to_string()]),
                    cap_drop: Some(vec!["ALL".to_string()]),
                    cap_add: if writable {
                        Some(
                            WRITABLE_ROOTFS_CAPS
                                .iter()
                                .map(|cap| (*cap).to_string())
                                .collect(),
                        )
                    } else {
                        None
                    },
                    binds,
                    tmpfs: Some({
                        let mut map = HashMap::new();
                        map.insert("/tmp".to_string(), format!("size={}m", tmp_size));
                        map
                    }),
                    ..Default::default()
                }),
                user: if writable {
                    None
                } else {
                    Some("nobody".to_string())
                },
                ..Default::default()
            };

            let container = self
                .docker
                .create_container(None, config)
                .await
                .map_err(|e| ExecutorError::Create(e.to_string()))?;

            let container_id = container.id.clone();

            self.docker
                .start_container(&container_id, None)
                .await
                .map_err(|e| ExecutorError::Start(e.to_string()))?;

            tracing::info!(
                trace_id = trace_id,
                container_id = %container_id,
                image = %image,
                "box.container.started (phased)"
            );

            Ok(container_id)
        }

        /// Check container status. Returns `Some(exit_code)` if stopped, `None` if running.
        pub async fn inspect_status(&self, container_id: &str) -> ExecutorResult<Option<i64>> {
            let info = self
                .docker
                .inspect_container(container_id, None)
                .await
                .map_err(|e| {
                    if let bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    } = &e
                    {
                        ExecutorError::ContainerNotFound(container_id.to_string())
                    } else {
                        ExecutorError::Other(format!("inspect failed: {}", e))
                    }
                })?;

            let state = info
                .state
                .ok_or_else(|| ExecutorError::Other("Container has no state".into()))?;

            if state.running.unwrap_or(false) {
                Ok(None)
            } else {
                Ok(Some(state.exit_code.unwrap_or(-1)))
            }
        }

        /// Collect stdout and stderr from a container.
        pub async fn collect_logs(&self, container_id: &str) -> ExecutorResult<(String, String)> {
            let accumulator = LogAccumulator::from_docker(&self.docker, container_id);
            let (stdout, stderr) = accumulator.finalize().await;
            Ok((stdout, stderr))
        }

        /// Kill and remove a container.
        pub async fn kill_and_remove(&self, container_id: &str) {
            self.docker.kill_container(container_id, None).await.ok();
            self.docker.remove_container(container_id, None).await.ok();
        }

        /// Remove a stopped container.
        pub async fn remove_container(&self, container_id: &str) {
            self.docker.remove_container(container_id, None).await.ok();
        }

        #[allow(clippy::too_many_arguments)]
        async fn execute_inner(
            &self,
            image: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            timeout_secs: u64,
            timings: &mut ContainerTimings,
            trace_id: Option<&str>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
        ) -> ExecutorResult<ContainerOutput> {
            let pull_start = Instant::now();
            tracing::debug!(trace_id = trace_id, image = %image, "box.image.pulling");

            let create_image_options = CreateImageOptionsBuilder::default()
                .from_image(image)
                .build();

            let mut pull_stream = self
                .docker
                .create_image(Some(create_image_options), None, None);

            while let Some(pull_result) = pull_stream.next().await {
                if let Err(e) = pull_result {
                    return Err(ExecutorError::ImagePull(format!(
                        "Failed to pull image '{}': {}",
                        image, e
                    )));
                }
            }

            timings.image_pull_ms = pull_start.elapsed().as_millis() as i64;

            let memory_bytes =
                limits.map_or(512 * 1024 * 1024, |l| (l.memory_mb * 1024 * 1024) as i64);
            let cpu_quota = limits.map_or(50000, |l| l.cpu_quota as i64);
            let tmp_size = limits.map_or(500, |l| l.tmp_size_mb);
            let wants_network = limits.is_some_and(|l| l.network);
            let needs_host_network = extras.is_some_and(|e| e.needs_host_network);
            let network_mode = if wants_network || needs_host_network {
                "bridge".to_string()
            } else {
                "none".to_string()
            };

            // Merge extra env vars from ContainerExtras (e.g. HOTBOX_SOCKET)
            let merged_env = {
                let mut all_env = env.clone().unwrap_or_default();
                if let Some(ext) = extras {
                    all_env.extend(ext.extra_env.iter().cloned());
                }
                if all_env.is_empty() {
                    None
                } else {
                    Some(all_env)
                }
            };

            // Bind mounts from ContainerExtras (e.g. hotbox binary, socket)
            let binds = extras
                .filter(|ext| !ext.binds.is_empty())
                .map(|ext| ext.binds.clone());

            let mut labels = HashMap::new();
            labels.insert(
                "hot.dev/managed-by".to_string(),
                "hot-task-worker".to_string(),
            );
            if let Some(tid) = trace_id {
                labels.insert("hot.dev/task-id".to_string(), tid.to_string());
            }

            let writable = extras.is_some_and(|e| e.writable_rootfs);
            let entrypoint = extras.and_then(|e| e.entrypoint.clone());
            let config = ContainerCreateBody {
                image: Some(image.to_string()),
                cmd: cmd.clone(),
                entrypoint,
                env: merged_env,
                labels: Some(labels),
                host_config: Some(HostConfig {
                    network_mode: Some(network_mode),
                    memory: Some(memory_bytes),
                    memory_swap: Some(memory_bytes),
                    cpu_quota: Some(cpu_quota),
                    pids_limit: Some(100i64),
                    readonly_rootfs: Some(!writable),
                    security_opt: Some(vec!["no-new-privileges".to_string()]),
                    cap_drop: Some(vec!["ALL".to_string()]),
                    cap_add: if writable {
                        Some(
                            WRITABLE_ROOTFS_CAPS
                                .iter()
                                .map(|cap| (*cap).to_string())
                                .collect(),
                        )
                    } else {
                        None
                    },
                    binds,
                    tmpfs: Some({
                        let mut map = HashMap::new();
                        map.insert("/tmp".to_string(), format!("size={}m", tmp_size));
                        map
                    }),
                    ..Default::default()
                }),
                user: if writable {
                    None
                } else {
                    Some("nobody".to_string())
                },
                ..Default::default()
            };

            let container = self
                .docker
                .create_container(None, config)
                .await
                .map_err(|e| ExecutorError::Create(e.to_string()))?;

            let container_id = container.id.clone();

            let exec_start = Instant::now();
            self.docker
                .start_container(&container_id, None)
                .await
                .map_err(|e| ExecutorError::Start(e.to_string()))?;

            let accumulator = LogAccumulator::from_docker(&self.docker, &container_id);

            let wait_options = WaitContainerOptionsBuilder::default()
                .condition("not-running")
                .build();

            let docker_ref = &self.docker;
            let container_id_ref = &container_id;

            let wait_future = async {
                let mut wait_stream =
                    docker_ref.wait_container(container_id_ref, Some(wait_options));
                let mut exit_code = 0i64;
                while let Some(wait_result) = wait_stream.next().await {
                    if let Ok(result) = wait_result {
                        exit_code = result.status_code;
                    }
                }
                exit_code
            };

            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), wait_future)
                .await
            {
                Ok(exit_code) => {
                    timings.execution_ms = exec_start.elapsed().as_millis() as i64;

                    let logs_start = Instant::now();
                    let (stdout, stderr) = accumulator.finalize().await;
                    timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;

                    let oom_killed = if exit_code == 137 {
                        self.docker
                            .inspect_container(&container_id, None)
                            .await
                            .ok()
                            .and_then(|info| info.state)
                            .and_then(|state| state.oom_killed)
                            .unwrap_or(true) // assume OOM if inspect fails on 137
                    } else {
                        false
                    };

                    self.docker.remove_container(&container_id, None).await.ok();

                    tracing::info!(
                        trace_id = trace_id,
                        container_id = %container_id,
                        image = %image,
                        exit_code = exit_code,
                        oom_killed = oom_killed,
                        execution_ms = timings.execution_ms,
                        "box.container.completed"
                    );

                    Ok(ContainerOutput {
                        exit_code,
                        stdout,
                        stderr,
                        container_id,
                        timed_out: false,
                        oom_killed,
                    })
                }
                Err(_) => {
                    timings.execution_ms = exec_start.elapsed().as_millis() as i64;

                    let logs_start = Instant::now();
                    let (stdout, stderr) = accumulator.snapshot().await;
                    timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;

                    tracing::warn!(
                        trace_id = trace_id,
                        container_id = %container_id,
                        timeout_secs = timeout_secs,
                        "box.container.timeout"
                    );

                    self.docker.kill_container(&container_id, None).await.ok();
                    self.docker.remove_container(&container_id, None).await.ok();

                    Ok(ContainerOutput {
                        exit_code: -1,
                        stdout,
                        stderr,
                        container_id,
                        timed_out: true,
                        oom_killed: false,
                    })
                }
            }
        }
    }
}

/// Normalize a short image name to a fully qualified reference.
///
/// containerd/ctr (unlike Docker) doesn't auto-resolve short names.
/// `alpine:latest` must become `docker.io/library/alpine:latest`.
#[cfg(feature = "kata")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn normalize_image_ref(image: &str) -> String {
    if image.contains("://") {
        return image.to_string();
    }
    if let Some(slash_pos) = image.find('/') {
        let prefix = &image[..slash_pos];
        if prefix.contains('.') || prefix.contains(':') {
            return image.to_string();
        }
        return format!("docker.io/{image}");
    }
    format!("docker.io/library/{image}")
}

/// Compute the OCI chain ID from a list of diff IDs.
/// For a single layer, chain ID = diff ID.
/// For multiple layers: chain_id(L0..Ln) = sha256(chain_id(L0..Ln-1) + " " + diff_id(Ln))
#[cfg(feature = "kata")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn compute_chain_id(diff_ids: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut chain = diff_ids[0].clone();
    for diff_id in &diff_ids[1..] {
        let input = format!("{chain} {diff_id}");
        let hash = Sha256::digest(input.as_bytes());
        chain = format!("sha256:{hash:x}");
    }
    chain
}

#[cfg(all(target_os = "linux", feature = "kata"))]
mod kata {
    use super::*;
    use crate::log_accumulator::LogAccumulator;
    use containerd_client::tonic::Request;
    use containerd_client::tonic::transport::Channel;
    use containerd_client::{
        connect,
        services::v1::{
            Container, CreateContainerRequest, CreateTaskRequest, DeleteContainerRequest,
            DeleteTaskRequest, GetImageRequest, KillRequest, ListContainersRequest,
            ReadContentRequest, StartRequest, WaitRequest,
            containers_client::ContainersClient,
            content_client::ContentClient,
            images_client::ImagesClient,
            snapshots::{
                self, PrepareSnapshotRequest, RemoveSnapshotRequest,
                snapshots_client::SnapshotsClient,
            },
            tasks_client::TasksClient,
        },
        with_namespace,
    };

    const NAMESPACE: &str = "hot-box";
    const SNAPSHOTTER: &str = "devmapper";
    const KATA_IO_DIR: &str = "/tmp/hot-box-io";
    const KATA_VC_DIR: &str = "/run/vc/firecracker";

    pub struct KataExecutor {
        channel: Channel,
        semaphore: Arc<Semaphore>,
        slot_timeout_secs: u64,
        vmm: KataVmm,
    }

    impl KataExecutor {
        pub async fn new(
            socket_path: &str,
            max_concurrent: usize,
            slot_timeout_secs: u64,
            vmm: KataVmm,
        ) -> ExecutorResult<Self> {
            let channel = connect(socket_path)
                .await
                .map_err(|e| ExecutorError::Connection(e.to_string()))?;

            // Verify containerd + devmapper snapshotter are ready before accepting work.
            // The content store may not be initialized immediately after
            // kata-containerd starts, especially on fresh instances.
            let max_attempts = 10;
            for attempt in 1..=max_attempts {
                let mut snapshots = SnapshotsClient::new(channel.clone());
                let req = with_namespace!(
                    snapshots::ListSnapshotsRequest {
                        snapshotter: SNAPSHOTTER.to_string(),
                        ..Default::default()
                    },
                    NAMESPACE
                );
                match snapshots.list(req).await {
                    Ok(_) => {
                        tracing::info!("kata.containerd.ready after {attempt} attempt(s)");
                        break;
                    }
                    Err(e) if attempt == max_attempts => {
                        return Err(ExecutorError::Connection(format!(
                            "containerd snapshotter not ready after {max_attempts} attempts: {e}"
                        )));
                    }
                    Err(e) => {
                        tracing::warn!(
                            attempt,
                            error = %e,
                            "kata.containerd.snapshotter_not_ready, retrying..."
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                }
            }

            // Clean up stale CNI state from previous runs (crash recovery).
            crate::cni::cleanup_stale().await;

            // Ensure iptables MASQUERADE + FORWARD rules exist for the kata bridge.
            crate::cni::ensure_iptables().await;

            // Write a resolv.conf derived from the host's DNS config so Kata VMs
            // get correct DNS (the rootfs may have stale DNS from the build env).
            crate::cni::write_resolv_conf().await;

            tracing::info!(vmm = %vmm, "kata.executor.initialized");

            Ok(Self {
                channel,
                semaphore: Arc::new(Semaphore::new(max_concurrent)),
                slot_timeout_secs,
                vmm,
            })
        }

        pub fn vmm(&self) -> KataVmm {
            self.vmm
        }

        /// List every container currently registered in the `hot-box`
        /// namespace, returning each `(container_id, task_id_label)` pair.
        ///
        /// Used at worker startup to find containers leaked by a previous
        /// worker that died without cleaning up. The `task_id` is read from
        /// the `hot.dev/task-id` label set by `create_container`; older
        /// containers (predating that label) will report `None` and the
        /// caller can still force-cleanup the container.
        pub async fn list_orphan_containers(
            &self,
        ) -> ExecutorResult<Vec<(String, Option<String>)>> {
            let mut client = ContainersClient::new(self.channel.clone());
            let req = with_namespace!(ListContainersRequest { filters: vec![] }, NAMESPACE);
            let resp = client
                .list(req)
                .await
                .map_err(|e| ExecutorError::Other(format!("kata.list_containers: {e}")))?;

            let out = resp
                .into_inner()
                .containers
                .into_iter()
                .map(|c| {
                    let task_id = c.labels.get("hot.dev/task-id").cloned();
                    (c.id, task_id)
                })
                .collect();
            Ok(out)
        }

        /// Force-clean a single container in the `hot-box` namespace:
        /// best-effort kill its task, then delete the task record, the
        /// container record, the snapshot, and any leftover IO FIFOs.
        ///
        /// Safe to call on a container whose task has already exited or
        /// whose snapshot is missing — every step is best-effort and ignores
        /// "not found" errors.
        pub async fn cleanup_orphan(&self, container_id: &str) {
            let mut tasks = TasksClient::new(self.channel.clone());
            let mut containers = ContainersClient::new(self.channel.clone());
            let mut snapshots = SnapshotsClient::new(self.channel.clone());

            // Kill the task first if it's still alive — DeleteTask returns an
            // error on a still-running task in some containerd versions.
            let _ = self.kill_task(&mut tasks, container_id).await;

            let stdout_fifo = format!("{}/{}-stdout.fifo", KATA_IO_DIR, container_id);
            let stderr_fifo = format!("{}/{}-stderr.fifo", KATA_IO_DIR, container_id);

            self.cleanup(
                &mut tasks,
                &mut containers,
                &mut snapshots,
                container_id,
                Some((&stdout_fifo, &stderr_fifo)),
            )
            .await;
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn execute(
            &self,
            image: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            timeout_secs: u64,
            timings: &mut ContainerTimings,
            trace_id: Option<&str>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
            pre_start_hook: Option<PreStartHook>,
        ) -> ExecutorResult<ContainerOutput> {
            if !is_image_allowed(image) {
                return Err(ExecutorError::ImageNotAllowed(image.to_string()));
            }

            let slot_start = Instant::now();
            let permit = match tokio::time::timeout(
                std::time::Duration::from_secs(self.slot_timeout_secs),
                self.semaphore.acquire(),
            )
            .await
            {
                Ok(Ok(permit)) => {
                    timings.slot_wait_ms = slot_start.elapsed().as_millis() as i64;
                    tracing::debug!(
                        trace_id = trace_id,
                        image = %image,
                        slot_wait_ms = timings.slot_wait_ms,
                        "kata.slot.acquired"
                    );
                    permit
                }
                Ok(Err(_)) => {
                    return Err(ExecutorError::Other("Semaphore closed unexpectedly".into()));
                }
                Err(_) => {
                    return Err(ExecutorError::SlotTimeout(self.slot_timeout_secs));
                }
            };

            let result = self
                .execute_inner(
                    image,
                    cmd,
                    env,
                    timeout_secs,
                    timings,
                    trace_id,
                    limits,
                    extras,
                    pre_start_hook,
                )
                .await;

            drop(permit);
            result
        }

        #[allow(clippy::too_many_arguments)]
        async fn execute_inner(
            &self,
            image: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            timeout_secs: u64,
            timings: &mut ContainerTimings,
            trace_id: Option<&str>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
            pre_start_hook: Option<PreStartHook>,
        ) -> ExecutorResult<ContainerOutput> {
            let image = &normalize_image_ref(image);
            // Derive a short ID from the task_id (last 24 hex chars of UUID
            // v7, where most randomness lives). Used as both the containerd
            // container_id and the Kata sandbox ID so shim directories,
            // snapshots, FIFOs, and logs are all directly traceable to the
            // task. 24 chars keeps the vsock listener path at 99 chars, well
            // under the 107-char Unix socket limit (sun_path[108] - null).
            let container_id = {
                let s = trace_id
                    .and_then(|tid| uuid::Uuid::try_parse(tid).ok())
                    .unwrap_or_else(uuid::Uuid::new_v4)
                    .simple()
                    .to_string();
                s[s.len() - 24..].to_string()
            };

            let mut images = ImagesClient::new(self.channel.clone());
            let mut containers = ContainersClient::new(self.channel.clone());
            let mut tasks = TasksClient::new(self.channel.clone());
            let mut snapshots = SnapshotsClient::new(self.channel.clone());
            let mut content = ContentClient::new(self.channel.clone());

            let pull_start = Instant::now();
            tracing::debug!(trace_id = trace_id, image = %image, "kata.image.pulling");
            self.ensure_image(&mut images, image).await?;
            timings.image_pull_ms = pull_start.elapsed().as_millis() as i64;

            let (chain_id, image_env) = self
                .get_image_chain_id(&mut images, &mut content, image)
                .await?;
            tracing::debug!(
                trace_id = trace_id,
                image = %image,
                chain_id = %chain_id,
                "kata.image.chain_id"
            );

            let rootfs = self
                .prepare_snapshot(&mut snapshots, &container_id, &chain_id)
                .await?;

            tracing::debug!(
                trace_id = trace_id,
                container_id = %container_id,
                image = %image,
                "kata.container.creating"
            );

            // Set up CNI networking if the task requires internet access.
            // This creates a netns with a veth pair + bridge so Kata's
            // macvtap model can wire it into the VM.
            let needs_network = limits.is_some_and(|l| l.network);
            let mut netns_path = if needs_network {
                match crate::cni::setup(&container_id).await {
                    Ok(path) => {
                        tracing::info!(
                            trace_id = trace_id,
                            container_id = %container_id,
                            netns = %path,
                            "kata.cni.setup.ok"
                        );
                        Some(path)
                    }
                    Err(e) => {
                        tracing::warn!(
                            trace_id = trace_id,
                            container_id = %container_id,
                            error = %e,
                            "kata.cni.setup.failed — continuing without network"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Retry the full setup (snapshot → container → task) with a fresh
            // container ID on each attempt. The Kata Go shim has a known cgroup
            // cleanup bug where stale sandbox state from a failed attempt cannot
            // be removed because LoadResourceController and
            // NewSandboxResourceController use incompatible cgroup types. Using
            // a new ID sidesteps the stale state entirely.
            const MAX_SETUP_ATTEMPTS: u32 = 3;
            let setup_result: ExecutorResult<(String, String, String)> = async {
                let mut last_err = None;
                for attempt in 1..=MAX_SETUP_ATTEMPTS {
                    let cid = if attempt == 1 {
                        container_id.clone()
                    } else {
                        format!("{}{:x}", &container_id[..container_id.len() - 1], attempt)
                    };

                    let attempt_rootfs = if attempt == 1 {
                        rootfs.clone()
                    } else {
                        tracing::warn!(
                            container_id = %cid,
                            prev_container_id = %container_id,
                            attempt = attempt,
                            error = %last_err.as_ref().unwrap(),
                            "kata.setup_retry with fresh container ID"
                        );
                        self.prepare_snapshot(&mut snapshots, &cid, &chain_id)
                            .await?
                    };

                    self.create_container(
                        &mut containers,
                        &cid,
                        image,
                        cmd.clone(),
                        env.clone(),
                        limits,
                        extras,
                        netns_path.as_deref(),
                        &image_env,
                        trace_id,
                    )
                    .await?;

                    let fifos = self.create_io_fifos(&cid).await?;

                    match self
                        .create_task(&mut tasks, &cid, &attempt_rootfs, &fifos.0, &fifos.1)
                        .await
                    {
                        Ok(()) => {
                            return Ok((cid, fifos.0, fifos.1));
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            let is_transient = msg.contains("no such file or directory")
                                || msg.contains("not found");

                            self.cleanup(
                                &mut tasks,
                                &mut containers,
                                &mut snapshots,
                                &cid,
                                Some((&fifos.0, &fifos.1)),
                            )
                            .await;
                            for dir in &[
                                format!("/run/vc/sbs/{cid}"),
                                format!("/run/vc/vm/{cid}"),
                                format!("/run/kata-containers/shared/sandboxes/{cid}"),
                            ] {
                                let _ = tokio::fs::remove_dir_all(dir).await;
                            }

                            // When using macvtap networking, the failed Kata
                            // shim may leave a stale macvtap device attached
                            // to the veth in the netns (its cleanup often
                            // fails). Tear down and recreate the CNI netns so
                            // the next attempt gets a clean veth + macvtap.
                            if needs_network && netns_path.is_some() {
                                crate::cni::teardown(&container_id).await;
                                match crate::cni::setup(&container_id).await {
                                    Ok(path) => {
                                        tracing::info!(
                                            container_id = %container_id,
                                            attempt = attempt,
                                            netns = %path,
                                            "kata.cni.retry_setup.ok"
                                        );
                                        netns_path = Some(path);
                                    }
                                    Err(e2) => {
                                        tracing::warn!(
                                            container_id = %container_id,
                                            error = %e2,
                                            "kata.cni.retry_setup.failed"
                                        );
                                        netns_path = None;
                                    }
                                }
                            }

                            if is_transient && attempt < MAX_SETUP_ATTEMPTS {
                                let delay =
                                    std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                                tokio::time::sleep(delay).await;
                                last_err = Some(e);
                                continue;
                            }
                            return Err(e);
                        }
                    }
                }
                Err(last_err.unwrap())
            }
            .await;

            // Save the original container_id for CNI teardown — the retry
            // loop may produce a different cid for containerd, but the netns
            // is always keyed by the original one.
            let cni_container_id = container_id.clone();

            let (container_id, stdout_fifo, stderr_fifo) = match setup_result {
                Ok(result) => result,
                Err(e) => {
                    self.cleanup(
                        &mut tasks,
                        &mut containers,
                        &mut snapshots,
                        &container_id,
                        None,
                    )
                    .await;
                    if needs_network {
                        crate::cni::teardown(&cni_container_id).await;
                    }
                    return Err(e);
                }
            };

            if let Some(hook) = pre_start_hook {
                let vsock_setup = match self.vmm {
                    KataVmm::Qemu => VsockSetup::AfVsock,
                    KataVmm::Firecracker => {
                        let vsock_path = std::path::PathBuf::from(format!(
                            "{}/{}/root/kata.hvsock",
                            KATA_VC_DIR, container_id
                        ));
                        if let Some(parent) = vsock_path.parent() {
                            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                                ExecutorError::Other(format!(
                                    "failed to create vsock dir {}: {}",
                                    parent.display(),
                                    e
                                ))
                            })?;
                        }
                        VsockSetup::HybridUds { path: vsock_path }
                    }
                };

                hook(vsock_setup)
                    .await
                    .map_err(|e| ExecutorError::Other(format!("pre-start hook failed: {}", e)))?;

                if self.vmm == KataVmm::Firecracker {
                    let vsock_path = std::path::PathBuf::from(format!(
                        "{}/{}/root/kata.hvsock",
                        KATA_VC_DIR, container_id
                    ));
                    for attempt in 1..=50u32 {
                        if vsock_path.exists() {
                            break;
                        }
                        if attempt == 50 {
                            return Err(ExecutorError::Other(format!(
                                "VM vsock {} did not appear after 5s — VM may have failed to boot",
                                vsock_path.display()
                            )));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }

            // Open FIFO read side BEFORE starting the task. The containerd
            // shim opens the write side during create_task, so the blocking
            // File::open() calls in from_fifos resolve once create_task
            // completes. Starting the accumulator before start_task ensures
            // we capture all output from the first byte — otherwise data
            // written before our reader opens is lost (FIFOs don't buffer
            // without a reader).
            let accumulator = LogAccumulator::from_fifos(stdout_fifo.clone(), stderr_fifo.clone());

            self.start_task(&mut tasks, &container_id).await?;

            let exec_start = Instant::now();

            tracing::info!(
                trace_id = trace_id,
                container_id = %container_id,
                "kata.container.started"
            );

            let wait_result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                self.wait_task(&mut tasks, &container_id),
            )
            .await;

            let exit_code = match wait_result {
                Ok(Ok(code)) => {
                    timings.execution_ms = exec_start.elapsed().as_millis() as i64;
                    code
                }
                Ok(Err(e)) => {
                    self.cleanup(
                        &mut tasks,
                        &mut containers,
                        &mut snapshots,
                        &container_id,
                        Some((stdout_fifo.as_str(), stderr_fifo.as_str())),
                    )
                    .await;
                    if needs_network {
                        crate::cni::teardown(&cni_container_id).await;
                    }
                    return Err(e);
                }
                Err(_) => {
                    timings.execution_ms = exec_start.elapsed().as_millis() as i64;
                    let logs_start = Instant::now();
                    let (stdout, stderr) = accumulator.snapshot().await;
                    timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;

                    tracing::warn!(
                        trace_id = trace_id,
                        container_id = %container_id,
                        timeout_secs = timeout_secs,
                        "kata.container.timeout"
                    );
                    self.kill_task(&mut tasks, &container_id).await.ok();
                    self.cleanup(
                        &mut tasks,
                        &mut containers,
                        &mut snapshots,
                        &container_id,
                        Some((stdout_fifo.as_str(), stderr_fifo.as_str())),
                    )
                    .await;
                    if needs_network {
                        crate::cni::teardown(&cni_container_id).await;
                    }
                    return Ok(ContainerOutput {
                        exit_code: -1,
                        stdout,
                        stderr,
                        container_id,
                        timed_out: true,
                        oom_killed: false,
                    });
                }
            };

            let logs_start = Instant::now();
            let (stdout, stderr) = accumulator.finalize().await;
            timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;

            self.cleanup(
                &mut tasks,
                &mut containers,
                &mut snapshots,
                &container_id,
                Some((stdout_fifo.as_str(), stderr_fifo.as_str())),
            )
            .await;
            if needs_network {
                crate::cni::teardown(&cni_container_id).await;
            }

            let oom_killed = exit_code == 137;

            tracing::info!(
                trace_id = trace_id,
                container_id = %container_id,
                exit_code = exit_code,
                oom_killed = oom_killed,
                execution_ms = timings.execution_ms,
                "kata.container.completed"
            );

            Ok(ContainerOutput {
                exit_code,
                stdout,
                stderr,
                container_id,
                timed_out: false,
                oom_killed,
            })
        }

        async fn ensure_image(
            &self,
            _client: &mut ImagesClient<Channel>,
            image: &str,
        ) -> ExecutorResult<()> {
            // Always run ctr pull; it is idempotent and skips already-present
            // layers. Checking the image index via gRPC first is unreliable
            // because the index can exist while layer blobs are missing.
            let output = tokio::process::Command::new("ctr")
                .args([
                    "--address",
                    DEFAULT_CONTAINERD_SOCKET,
                    "-n",
                    NAMESPACE,
                    "images",
                    "pull",
                    "--snapshotter",
                    SNAPSHOTTER,
                    image,
                ])
                .output()
                .await
                .map_err(|e| ExecutorError::ImagePull(e.to_string()))?;

            if !output.status.success() {
                return Err(ExecutorError::ImagePull(
                    String::from_utf8_lossy(&output.stderr).to_string(),
                ));
            }

            Ok(())
        }

        /// Get the OCI chain ID for an image's topmost layer and the image's
        /// embedded environment variables (from `config.config.Env`).
        ///
        /// Reads the image manifest and config from the containerd content store,
        /// extracts the diff IDs, and computes the chain ID per the OCI spec.
        async fn get_image_chain_id(
            &self,
            images: &mut ImagesClient<Channel>,
            content: &mut ContentClient<Channel>,
            image: &str,
        ) -> ExecutorResult<(String, Vec<String>)> {
            let get_req = with_namespace!(
                GetImageRequest {
                    name: image.to_string(),
                },
                NAMESPACE
            );
            let image_resp = images
                .get(get_req)
                .await
                .map_err(|e| ExecutorError::Create(format!("get image: {e}")))?;
            let image_info = image_resp
                .into_inner()
                .image
                .ok_or_else(|| ExecutorError::Create("image not found in response".into()))?;
            let target = image_info
                .target
                .ok_or_else(|| ExecutorError::Create("image has no target descriptor".into()))?;

            let manifest_data = self.read_content(content, &target.digest).await?;
            let manifest: serde_json::Value = serde_json::from_slice(&manifest_data)
                .map_err(|e| ExecutorError::Create(format!("parse manifest: {e}")))?;

            // Handle manifest list (multi-platform images) vs single manifest
            let config_digest = if manifest.get("manifests").is_some() {
                let manifests = manifest["manifests"]
                    .as_array()
                    .ok_or_else(|| ExecutorError::Create("invalid manifest list".into()))?;
                let platform_manifest = manifests
                    .iter()
                    .find(|m| {
                        m["platform"]["architecture"].as_str() == Some("amd64")
                            && m["platform"]["os"].as_str() == Some("linux")
                    })
                    .or_else(|| manifests.first())
                    .ok_or_else(|| ExecutorError::Create("no suitable manifest found".into()))?;
                let inner_digest = platform_manifest["digest"]
                    .as_str()
                    .ok_or_else(|| ExecutorError::Create("manifest has no digest".into()))?;
                let inner_data = self.read_content(content, inner_digest).await?;
                let inner: serde_json::Value = serde_json::from_slice(&inner_data)
                    .map_err(|e| ExecutorError::Create(format!("parse platform manifest: {e}")))?;
                inner["config"]["digest"]
                    .as_str()
                    .ok_or_else(|| ExecutorError::Create("manifest has no config digest".into()))?
                    .to_string()
            } else {
                manifest["config"]["digest"]
                    .as_str()
                    .ok_or_else(|| ExecutorError::Create("manifest has no config digest".into()))?
                    .to_string()
            };

            let config_data = self.read_content(content, &config_digest).await?;
            let config: serde_json::Value = serde_json::from_slice(&config_data)
                .map_err(|e| ExecutorError::Create(format!("parse image config: {e}")))?;

            let diff_ids: Vec<String> = config["rootfs"]["diff_ids"]
                .as_array()
                .ok_or_else(|| ExecutorError::Create("config has no rootfs.diff_ids".into()))?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();

            if diff_ids.is_empty() {
                return Err(ExecutorError::Create("image has no layers".into()));
            }

            let image_env: Vec<String> = config["config"]["Env"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            Ok((compute_chain_id(&diff_ids), image_env))
        }

        async fn read_content(
            &self,
            client: &mut ContentClient<Channel>,
            digest: &str,
        ) -> ExecutorResult<Vec<u8>> {
            let req = with_namespace!(
                ReadContentRequest {
                    digest: digest.to_string(),
                    ..Default::default()
                },
                NAMESPACE
            );
            let response = client
                .read(req)
                .await
                .map_err(|e| ExecutorError::Create(format!("read content {digest}: {e}")))?;
            let mut data = Vec::new();
            let mut stream = response.into_inner();
            while let Some(chunk) = stream
                .message()
                .await
                .map_err(|e| ExecutorError::Create(format!("read content stream: {e}")))?
            {
                data.extend_from_slice(&chunk.data);
            }
            Ok(data)
        }

        /// Prepare a writable snapshot for the container, returning the rootfs mounts.
        async fn prepare_snapshot(
            &self,
            snapshots: &mut SnapshotsClient<Channel>,
            container_id: &str,
            parent_chain_id: &str,
        ) -> ExecutorResult<Vec<containerd_client::types::Mount>> {
            let req = with_namespace!(
                PrepareSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_string(),
                    key: container_id.to_string(),
                    parent: parent_chain_id.to_string(),
                    ..Default::default()
                },
                NAMESPACE
            );
            let resp = snapshots
                .prepare(req)
                .await
                .map_err(|e| ExecutorError::Create(format!("prepare snapshot: {e}")))?;
            Ok(resp.into_inner().mounts)
        }

        #[allow(clippy::too_many_arguments)]
        async fn create_container(
            &self,
            client: &mut ContainersClient<Channel>,
            id: &str,
            image: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
            netns_path: Option<&str>,
            image_env: &[String],
            task_id: Option<&str>,
        ) -> ExecutorResult<()> {
            let spec = self.build_spec(id, cmd, env, limits, extras, netns_path, image_env);
            let spec_json =
                serde_json::to_vec(&spec).map_err(|e| ExecutorError::Create(e.to_string()))?;

            // Tag the containerd container with `hot.dev/managed-by` and the
            // originating task UUID. Used by `list_orphan_containers` at
            // worker startup so a fresh worker can clean up containers left
            // behind by a previous worker that died abruptly.
            let mut labels = std::collections::HashMap::new();
            labels.insert(
                "hot.dev/managed-by".to_string(),
                "hot-task-worker".to_string(),
            );
            if let Some(tid) = task_id {
                labels.insert("hot.dev/task-id".to_string(), tid.to_string());
            }

            let container = Container {
                id: id.to_string(),
                labels,
                image: image.to_string(),
                snapshotter: SNAPSHOTTER.to_string(),
                snapshot_key: id.to_string(),
                runtime: Some(containerd_client::services::v1::container::Runtime {
                    name: match self.vmm {
                        KataVmm::Qemu => "io.containerd.kata.v2",
                        KataVmm::Firecracker => "io.containerd.kata-fc.v2",
                    }
                    .to_string(),
                    options: None,
                }),
                spec: Some(prost_types::Any {
                    type_url: "types.containerd.io/opencontainers/runtime-spec/1/Spec".to_string(),
                    value: spec_json,
                }),
                ..Default::default()
            };

            let req = with_namespace!(
                CreateContainerRequest {
                    container: Some(container),
                },
                NAMESPACE
            );

            client
                .create(req)
                .await
                .map_err(|e| ExecutorError::Create(e.to_string()))?;

            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        fn build_spec(
            &self,
            _container_id: &str,
            cmd: Option<Vec<String>>,
            env: Option<Vec<String>>,
            limits: Option<&crate::box_limits::BoxLimits>,
            extras: Option<&ContainerExtras>,
            netns_path: Option<&str>,
            image_env: &[String],
        ) -> serde_json::Value {
            // Start with the image's embedded ENV, then overlay user/extras env.
            // User-provided vars win over image vars for the same key.
            let mut env_vec: Vec<String> = image_env.to_vec();
            if let Some(user_env) = env {
                for var in &user_env {
                    if let Some(key) = var.split('=').next() {
                        env_vec.retain(|e| e.split('=').next() != Some(key));
                    }
                }
                env_vec.extend(user_env);
            }
            if let Some(ext) = extras {
                for var in &ext.extra_env {
                    if let Some(key) = var.split('=').next() {
                        env_vec.retain(|e| e.split('=').next() != Some(key));
                    }
                }
                env_vec.extend(ext.extra_env.iter().cloned());
                if !ext.binds.is_empty() {
                    tracing::warn!(
                        bind_count = ext.binds.len(),
                        "Kata: ignoring host bind mounts (not supported for microVMs)"
                    );
                }
            }
            if !env_vec.iter().any(|v| v.starts_with("PATH=")) {
                env_vec.push(
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                );
            }

            let memory_bytes = limits.map_or(
                (crate::box_limits::BoxLimits::DEFAULT_MEMORY_MB * 1024 * 1024) as i64,
                |l| (l.memory_mb * 1024 * 1024) as i64,
            );
            let cpu_quota = limits.map_or(
                crate::box_limits::BoxLimits::DEFAULT_CPU_QUOTA as i64,
                |l| l.cpu_quota as i64,
            );
            let tmp_size_mb = limits
                .map_or(crate::box_limits::BoxLimits::DEFAULT_TMP_SIZE_MB, |l| {
                    l.tmp_size_mb
                });

            let writable = extras.is_some_and(|e| e.writable_rootfs);

            // Match Docker's security model: writable containers run as root
            // with capabilities (needed for apk add, apt install, etc.);
            // read-only containers run as nobody with all caps dropped.
            let (uid, gid) = if writable { (0, 0) } else { (65534, 65534) };

            let capabilities = if writable {
                serde_json::json!({
                    "bounding": ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER",
                                 "CAP_FSETID", "CAP_SETGID", "CAP_SETUID",
                                 "CAP_NET_BIND_SERVICE", "CAP_NET_RAW",
                                 "CAP_SYS_CHROOT", "CAP_MKNOD", "CAP_KILL"],
                    "effective": ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER",
                                  "CAP_FSETID", "CAP_SETGID", "CAP_SETUID",
                                  "CAP_NET_BIND_SERVICE", "CAP_NET_RAW",
                                  "CAP_SYS_CHROOT", "CAP_MKNOD", "CAP_KILL"],
                    "inheritable": [],
                    "permitted": ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER",
                                  "CAP_FSETID", "CAP_SETGID", "CAP_SETUID",
                                  "CAP_NET_BIND_SERVICE", "CAP_NET_RAW",
                                  "CAP_SYS_CHROOT", "CAP_MKNOD", "CAP_KILL"],
                    "ambient": []
                })
            } else {
                serde_json::json!({
                    "bounding": [],
                    "effective": [],
                    "inheritable": [],
                    "permitted": [],
                    "ambient": []
                })
            };

            let mut namespaces = vec![
                serde_json::json!({ "type": "pid" }),
                serde_json::json!({ "type": "ipc" }),
                serde_json::json!({ "type": "uts" }),
                serde_json::json!({ "type": "mount" }),
            ];
            if let Some(path) = netns_path {
                namespaces.push(serde_json::json!({ "type": "network", "path": path }));
            } else {
                namespaces.push(serde_json::json!({ "type": "network" }));
            }

            let mut mounts = vec![
                serde_json::json!({
                    "destination": "/proc",
                    "type": "proc",
                    "source": "proc",
                    "options": ["nosuid", "nodev", "noexec"]
                }),
                serde_json::json!({
                    "destination": "/dev",
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
                }),
                serde_json::json!({
                    "destination": "/dev/pts",
                    "type": "devpts",
                    "source": "devpts",
                    "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]
                }),
                serde_json::json!({
                    "destination": "/dev/shm",
                    "type": "tmpfs",
                    "source": "shm",
                    "options": ["nosuid", "noexec", "nodev", "mode=1777",
                                format!("size={}k", if writable { 262144 } else { 65536 })]
                }),
                serde_json::json!({
                    "destination": "/sys",
                    "type": "sysfs",
                    "source": "sysfs",
                    "options": ["nosuid", "noexec", "nodev", "ro"]
                }),
                serde_json::json!({
                    "destination": "/tmp",
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": ["nosuid", "nodev", format!("size={}m", tmp_size_mb)]
                }),
                serde_json::json!({
                    "destination": "/etc/resolv.conf",
                    "type": "bind",
                    "source": crate::cni::RESOLV_CONF_HOST_PATH,
                    "options": ["rbind", "ro"]
                }),
            ];

            if let Some(ext) = extras
                && let Some(ref dvp) = ext.data_volume_path
            {
                mounts.push(serde_json::json!({
                    "destination": "/data",
                    "type": "bind",
                    "source": dvp,
                    "options": ["rbind", "rw"]
                }));
            }

            mounts.push(serde_json::json!({
                "destination": "/usr/local/bin/hotbox",
                "type": "bind",
                "source": "/usr/local/bin/hotbox",
                "options": ["rbind", "ro"]
            }));

            serde_json::json!({
                "ociVersion": "1.0.0",
                "process": {
                    "terminal": false,
                    "user": { "uid": uid, "gid": gid },
                    "args": cmd.unwrap_or_else(|| vec!["/bin/sh".to_string()]),
                    "env": env_vec,
                    "cwd": "/",
                    "capabilities": capabilities,
                    "noNewPrivileges": !writable
                },
                "root": {
                    "path": "rootfs",
                    "readonly": false
                },
                "linux": {
                    "resources": {
                        "memory": { "limit": memory_bytes },
                        "cpu": { "quota": cpu_quota, "period": 100000 },
                        "pids": { "limit": if writable { 512 } else { 100 } }
                    },
                    "namespaces": namespaces
                },
                "mounts": mounts
            })
        }

        async fn create_task(
            &self,
            client: &mut TasksClient<Channel>,
            container_id: &str,
            rootfs: &[containerd_client::types::Mount],
            stdout_fifo: &str,
            stderr_fifo: &str,
        ) -> ExecutorResult<()> {
            let req = with_namespace!(
                CreateTaskRequest {
                    container_id: container_id.to_string(),
                    rootfs: rootfs.to_vec(),
                    stdout: stdout_fifo.to_string(),
                    stderr: stderr_fifo.to_string(),
                    ..Default::default()
                },
                NAMESPACE
            );

            client
                .create(req)
                .await
                .map_err(|e| ExecutorError::Start(e.to_string()))?;

            Ok(())
        }

        async fn start_task(
            &self,
            client: &mut TasksClient<Channel>,
            container_id: &str,
        ) -> ExecutorResult<()> {
            let req = with_namespace!(
                StartRequest {
                    container_id: container_id.to_string(),
                    ..Default::default()
                },
                NAMESPACE
            );

            client
                .start(req)
                .await
                .map_err(|e| ExecutorError::Start(e.to_string()))?;

            Ok(())
        }

        async fn wait_task(
            &self,
            client: &mut TasksClient<Channel>,
            container_id: &str,
        ) -> ExecutorResult<i64> {
            let req = with_namespace!(
                WaitRequest {
                    container_id: container_id.to_string(),
                    ..Default::default()
                },
                NAMESPACE
            );

            let response = client
                .wait(req)
                .await
                .map_err(|e| ExecutorError::Other(e.to_string()))?;

            Ok(response.into_inner().exit_status as i64)
        }

        async fn kill_task(
            &self,
            client: &mut TasksClient<Channel>,
            container_id: &str,
        ) -> ExecutorResult<()> {
            let req = with_namespace!(
                KillRequest {
                    container_id: container_id.to_string(),
                    signal: 9,
                    ..Default::default()
                },
                NAMESPACE
            );

            client.kill(req).await.ok();
            Ok(())
        }

        async fn cleanup(
            &self,
            tasks: &mut TasksClient<Channel>,
            containers: &mut ContainersClient<Channel>,
            snapshots: &mut SnapshotsClient<Channel>,
            container_id: &str,
            io_fifos: Option<(&str, &str)>,
        ) {
            let task_req = with_namespace!(
                DeleteTaskRequest {
                    container_id: container_id.to_string(),
                },
                NAMESPACE
            );
            tasks.delete(task_req).await.ok();

            let container_req = with_namespace!(
                DeleteContainerRequest {
                    id: container_id.to_string(),
                },
                NAMESPACE
            );
            containers.delete(container_req).await.ok();

            let snap_req = with_namespace!(
                RemoveSnapshotRequest {
                    snapshotter: SNAPSHOTTER.to_string(),
                    key: container_id.to_string(),
                },
                NAMESPACE
            );
            snapshots.remove(snap_req).await.ok();

            if let Some((stdout_fifo, stderr_fifo)) = io_fifos {
                let _ = tokio::fs::remove_file(stdout_fifo).await;
                let _ = tokio::fs::remove_file(stderr_fifo).await;
            }
        }

        async fn create_io_fifos(&self, container_id: &str) -> ExecutorResult<(String, String)> {
            tokio::fs::create_dir_all(KATA_IO_DIR)
                .await
                .map_err(|e| ExecutorError::Create(format!("failed to create io dir: {}", e)))?;

            let stdout_fifo = format!("{}/{}-stdout.fifo", KATA_IO_DIR, container_id);
            let stderr_fifo = format!("{}/{}-stderr.fifo", KATA_IO_DIR, container_id);

            for path in [&stdout_fifo, &stderr_fifo] {
                let _ = tokio::fs::remove_file(path).await;

                let status = tokio::process::Command::new("mkfifo")
                    .arg(path)
                    .status()
                    .await
                    .map_err(|e| {
                        ExecutorError::Create(format!("failed to run mkfifo for {}: {}", path, e))
                    })?;

                if !status.success() {
                    return Err(ExecutorError::Create(format!(
                        "mkfifo failed for {} with status {}",
                        path, status
                    )));
                }
            }

            Ok((stdout_fifo, stderr_fifo))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_denylist_allows_common_images() {
        assert!(is_image_allowed("alpine:latest"));
        assert!(is_image_allowed("alpine:3.21"));
        assert!(is_image_allowed("ubuntu:22.04"));
        assert!(is_image_allowed("ubuntu:24.04"));
        assert!(is_image_allowed("python:3.12-alpine"));
        assert!(is_image_allowed("node:22-alpine"));
        assert!(is_image_allowed("debian:bookworm-slim"));
        assert!(is_image_allowed("nginx:latest"));
        assert!(is_image_allowed("redis:7-alpine"));
        assert!(is_image_allowed("ffmpeg:latest"));
        assert!(is_image_allowed("ghcr.io/user/custom-image:v1"));
    }

    #[test]
    fn test_image_denylist_blocks_dangerous_images() {
        assert!(!is_image_allowed("docker:dind"));
        assert!(!is_image_allowed("docker:latest"));
        assert!(!is_image_allowed("docker"));
        assert!(!is_image_allowed("docker.io/docker:latest"));
        assert!(!is_image_allowed("rancher/rancher:latest"));
        assert!(!is_image_allowed("registry.k8s.io/kube-apiserver:v1.29"));
        assert!(!is_image_allowed("some-image:dind"));
        assert!(!is_image_allowed("some-image:privileged"));
        assert!(!is_image_allowed(""));
    }

    #[test]
    fn test_backend_from_str() {
        assert_eq!("docker".parse::<Backend>().unwrap(), Backend::Docker);
        assert_eq!("Docker".parse::<Backend>().unwrap(), Backend::Docker);
        assert_eq!("".parse::<Backend>().unwrap(), Backend::Docker);
        assert!("invalid".parse::<Backend>().is_err());
    }

    #[test]
    fn test_backend_display() {
        assert_eq!(Backend::Docker.to_string(), "docker");
    }
}

#[cfg(all(test, feature = "kata"))]
mod kata_tests {
    use super::normalize_image_ref;

    #[test]
    fn test_normalize_image_ref() {
        assert_eq!(
            normalize_image_ref("alpine:latest"),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(normalize_image_ref("alpine"), "docker.io/library/alpine");
        assert_eq!(
            normalize_image_ref("ubuntu:22.04"),
            "docker.io/library/ubuntu:22.04"
        );
        assert_eq!(
            normalize_image_ref("python:3.12-alpine"),
            "docker.io/library/python:3.12-alpine"
        );
        assert_eq!(
            normalize_image_ref("user/repo:v1"),
            "docker.io/user/repo:v1"
        );
        assert_eq!(
            normalize_image_ref("myorg/myimage"),
            "docker.io/myorg/myimage"
        );
        // Already fully qualified -- unchanged
        assert_eq!(
            normalize_image_ref("docker.io/library/alpine:latest"),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(
            normalize_image_ref("ghcr.io/user/image:v1"),
            "ghcr.io/user/image:v1"
        );
        assert_eq!(
            normalize_image_ref("registry.example.com/image:tag"),
            "registry.example.com/image:tag"
        );
        assert_eq!(
            normalize_image_ref("localhost:5000/image:tag"),
            "localhost:5000/image:tag"
        );
    }

    #[test]
    fn test_compute_chain_id_single_layer() {
        let diff_ids = vec!["sha256:aaa".to_string()];
        assert_eq!(super::compute_chain_id(&diff_ids), "sha256:aaa");
    }

    #[test]
    fn test_compute_chain_id_multi_layer() {
        let diff_ids = vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()];
        let result = super::compute_chain_id(&diff_ids);
        assert!(result.starts_with("sha256:"));
        assert_ne!(result, "sha256:aaa");
    }
}
