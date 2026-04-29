/// CNI (Container Network Interface) helpers for Kata container networking.
///
/// Manages network namespace lifecycle and invokes the CNI bridge plugin to
/// provide outbound internet access from Kata microVMs. The flow is:
///   1. `setup(container_id)` — create a netns + call CNI ADD (bridge + IPAM)
///   2. Pass the netns path in the OCI spec so Kata's macvtap model wires it
///      into the VM via a macvtap device
///   3. `teardown(container_id)` — call CNI DEL + remove the netns
///
/// All operations run via `nsenter -t 1 -m -n` so they execute in the HOST's
/// mount and network namespaces. This is required when the task worker itself
/// runs inside an ECS container — netns bind-mounts and bridge/iptables rules
/// must be visible to the host where Kata's containerd shim operates.
const CNI_BIN_DIR: &str = "/opt/cni/bin";
const NETNS_DIR: &str = "/var/run/netns";
const IP_BIN: &str = "/usr/sbin/ip";
const IPTABLES_BIN: &str = "/usr/sbin/iptables";
const BRIDGE_NAME: &str = "kata-br0";
const SUBNET: &str = "10.88.0.0/16";

/// Path on the HOST where we write a resolv.conf for Kata VMs to use.
/// The guest rootfs may contain stale DNS (e.g. Azure DNS from a CI build),
/// so we generate a correct one from the host's actual DNS config.
pub const RESOLV_CONF_HOST_PATH: &str = "/var/lib/kata/resolv.conf";

/// Prefix for commands that must run in the host's mount + network namespaces.
/// With `pidMode: "host"` on the ECS task, PID 1 is the host init process.
const NSENTER: &[&str] = &["nsenter", "-t", "1", "-m", "-n", "--"];

fn bridge_config() -> &'static str {
    // ipMasq is false — we manage iptables rules ourselves in ensure_iptables()
    // because the bridge plugin's ipMasq silently fails when run through nsenter
    // (nf_tables backend / PATH incompatibility).
    r#"{
  "cniVersion": "1.0.0",
  "name": "kata-bridge",
  "type": "bridge",
  "bridge": "kata-br0",
  "isGateway": true,
  "ipMasq": false,
  "ipam": {
    "type": "host-local",
    "subnet": "10.88.0.0/16",
    "routes": [{ "dst": "0.0.0.0/0" }]
  }
}"#
}

pub fn netns_path(container_id: &str) -> String {
    format!("{}/{}", NETNS_DIR, container_id)
}

/// Build a Command that runs via nsenter in the host's mount+network namespace.
fn host_command(program: &str, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(NSENTER[0]);
    cmd.args(&NSENTER[1..]);
    cmd.arg(program);
    cmd.args(args);
    cmd
}

/// Ensure iptables rules exist for kata-br0 MASQUERADE and FORWARD.
/// Idempotent — checks before adding. Called once on startup and is a
/// prerequisite for any container to have outbound internet access.
pub async fn ensure_iptables() {
    // NAT: MASQUERADE traffic from the CNI subnet to the outside world.
    if !iptables_rule_exists(&[
        "-t",
        "nat",
        "-C",
        "POSTROUTING",
        "-s",
        SUBNET,
        "!",
        "-d",
        SUBNET,
        "-j",
        "MASQUERADE",
    ])
    .await
    {
        let _ = run_iptables(&[
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            SUBNET,
            "!",
            "-d",
            SUBNET,
            "-j",
            "MASQUERADE",
        ])
        .await;
        tracing::info!("cni.iptables: added MASQUERADE rule for {SUBNET}");
    }

    // FORWARD: allow outbound traffic from kata-br0 (policy is DROP by default
    // because Docker sets it).
    let forward_rules: &[&[&str]] = &[
        &[
            "-I",
            "FORWARD",
            "-o",
            BRIDGE_NAME,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ],
        &[
            "-I",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "!",
            "-o",
            BRIDGE_NAME,
            "-j",
            "ACCEPT",
        ],
        &[
            "-I",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "-o",
            BRIDGE_NAME,
            "-j",
            "ACCEPT",
        ],
    ];
    let check_rules: &[&[&str]] = &[
        &[
            "-C",
            "FORWARD",
            "-o",
            BRIDGE_NAME,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ],
        &[
            "-C",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "!",
            "-o",
            BRIDGE_NAME,
            "-j",
            "ACCEPT",
        ],
        &[
            "-C",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "-o",
            BRIDGE_NAME,
            "-j",
            "ACCEPT",
        ],
    ];
    for (check, add) in check_rules.iter().zip(forward_rules.iter()) {
        if !iptables_rule_exists(check).await {
            let _ = run_iptables(add).await;
        }
    }
    tracing::info!("cni.iptables: FORWARD rules ensured for {BRIDGE_NAME}");
}

/// Write a resolv.conf on the HOST for Kata VMs to bind-mount.
/// Reads the host's /etc/resolv.conf (via nsenter) to pick up VPC DNS.
/// Falls back to reading the task worker container's /etc/resolv.conf.
pub async fn write_resolv_conf() {
    // Try reading the host's resolv.conf first.
    let content = match host_command("cat", &["/etc/resolv.conf"]).output().await {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            if s.contains("nameserver") {
                Some(s)
            } else {
                None
            }
        }
        _ => None,
    };

    // Fall back to the task worker container's own resolv.conf.
    let content = match content {
        Some(c) => c,
        None => match tokio::fs::read_to_string("/etc/resolv.conf").await {
            Ok(s) if s.contains("nameserver") => s,
            _ => {
                tracing::warn!("cni.resolv: could not read any resolv.conf, using fallback");
                "nameserver 169.254.169.253\noptions ndots:0\n".to_string()
            }
        },
    };

    // Write to host filesystem via nsenter.
    let output = host_command("tee", &[RESOLV_CONF_HOST_PATH])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match output {
        Ok(child) => match write_stdin_and_wait(child, &content).await {
            Ok(o) if o.status.success() => {
                tracing::info!(path = RESOLV_CONF_HOST_PATH, "cni.resolv: written");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(stderr = %stderr, "cni.resolv: tee failed");
            }
            Err(e) => tracing::warn!(error = %e, "cni.resolv: write failed"),
        },
        Err(e) => tracing::warn!(error = %e, "cni.resolv: spawn failed"),
    }
}

async fn iptables_rule_exists(args: &[&str]) -> bool {
    match host_command(IPTABLES_BIN, args).output().await {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

async fn run_iptables(args: &[&str]) -> Result<(), String> {
    let output = host_command(IPTABLES_BIN, args)
        .output()
        .await
        .map_err(|e| format!("iptables spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(args = ?args, stderr = %stderr, "cni.iptables: rule failed");
        return Err(format!("iptables failed: {stderr}"));
    }
    Ok(())
}

/// Create a network namespace and run CNI ADD to set up bridge networking.
/// Returns the netns path on success.
pub async fn setup(container_id: &str) -> Result<String, String> {
    let ns_path = netns_path(container_id);

    // Ensure the netns directory exists on the host.
    let status = host_command("mkdir", &["-p", NETNS_DIR])
        .status()
        .await
        .map_err(|e| format!("failed to create {NETNS_DIR}: {e}"))?;

    if !status.success() {
        return Err(format!("mkdir -p {NETNS_DIR} failed: {status}"));
    }

    // Create the netns in the host's mount namespace so Kata can see it.
    let status = host_command(IP_BIN, &["netns", "add", container_id])
        .status()
        .await
        .map_err(|e| format!("failed to run `{IP_BIN} netns add`: {e}"))?;

    if !status.success() {
        return Err(format!(
            "`{IP_BIN} netns add {container_id}` failed: {status}"
        ));
    }

    // Invoke the CNI bridge plugin with ADD in the host namespace.
    let bridge_bin = format!("{CNI_BIN_DIR}/bridge");
    let output = tokio::process::Command::new(NSENTER[0])
        .args(&NSENTER[1..])
        .arg(&bridge_bin)
        .env("CNI_COMMAND", "ADD")
        .env("CNI_CONTAINERID", container_id)
        .env("CNI_NETNS", &ns_path)
        .env("CNI_IFNAME", "eth0")
        .env("CNI_PATH", CNI_BIN_DIR)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn CNI bridge plugin: {e}"))?;

    let output = write_stdin_and_wait(output, bridge_config()).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = host_command(IP_BIN, &["netns", "del", container_id])
            .status()
            .await;
        return Err(format!("CNI bridge ADD failed: {stderr}"));
    }

    tracing::debug!(
        container_id = container_id,
        netns = %ns_path,
        "cni.setup.complete"
    );

    Ok(ns_path)
}

/// Run CNI DEL and remove the network namespace. Best-effort; errors are logged
/// but not propagated since this runs during cleanup.
pub async fn teardown(container_id: &str) {
    let ns_path = netns_path(container_id);

    // Check if the netns file exists (via host namespace).
    let check = host_command("test", &["-e", &ns_path]).status().await;
    match check {
        Ok(s) if s.success() => {}
        _ => return,
    }

    // CNI DEL — idempotent by spec.
    let bridge_bin = format!("{CNI_BIN_DIR}/bridge");
    let del_result = tokio::process::Command::new(NSENTER[0])
        .args(&NSENTER[1..])
        .arg(&bridge_bin)
        .env("CNI_COMMAND", "DEL")
        .env("CNI_CONTAINERID", container_id)
        .env("CNI_NETNS", &ns_path)
        .env("CNI_IFNAME", "eth0")
        .env("CNI_PATH", CNI_BIN_DIR)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match del_result {
        Ok(child) => {
            if let Err(e) = write_stdin_and_wait(child, bridge_config()).await {
                tracing::warn!(container_id = container_id, error = %e, "cni.del.failed");
            }
        }
        Err(e) => {
            tracing::warn!(container_id = container_id, error = %e, "cni.del.spawn_failed");
        }
    }

    // Remove the netns.
    let _ = host_command(IP_BIN, &["netns", "del", container_id])
        .status()
        .await;

    tracing::debug!(container_id = container_id, "cni.teardown.complete");
}

/// Remove all stale network namespaces and their CNI state on startup.
/// Called once when the Kata executor initializes to clean up after a
/// previous crash or ungraceful shutdown.
pub async fn cleanup_stale() {
    let output = host_command(IP_BIN, &["netns", "list"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    let entries = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => {
            tracing::warn!(error = %e, "cni.cleanup: failed to list netns");
            return;
        }
    };

    let mut count = 0u32;
    for line in entries.lines() {
        // `ip netns list` outputs lines like "name (id: N)" — take the first word.
        let name = line.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            continue;
        }

        // Only clean up entries that look like hex container IDs (24 hex chars)
        // to avoid removing other system netns entries.
        let is_container_id = name.len() == 24 && name.chars().all(|c| c.is_ascii_hexdigit());
        let is_cni_test = name.starts_with("cnitest-");
        if !is_container_id && !is_cni_test {
            continue;
        }

        tracing::info!(netns = name, "cni.cleanup: removing stale netns");
        teardown(name).await;
        count += 1;
    }

    if count > 0 {
        tracing::info!(count, "cni.cleanup: removed stale netns entries");
    } else {
        tracing::info!("cni.cleanup: no stale netns entries found");
    }
}

async fn write_stdin_and_wait(
    mut child: tokio::process::Child,
    input: &str,
) -> Result<std::process::Output, String> {
    use tokio::io::AsyncWriteExt;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| format!("failed to write to CNI stdin: {e}"))?;
        drop(stdin);
    }

    child
        .wait_with_output()
        .await
        .map_err(|e| format!("failed to wait for CNI plugin: {e}"))
}
