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
const IP6TABLES_BIN: &str = "/usr/sbin/ip6tables";
const BRIDGE_NAME: &str = "kata-br0";
const SUBNET: &str = "10.88.0.0/16";
const TASK_CHAIN_PREFIX: &str = "HKT-";

/// Destinations that are never public internet. This includes cloud metadata,
/// host/VPC address space, carrier NAT, multicast, and other Kata guests.
const BLOCKED_DESTINATIONS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

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
pub async fn ensure_iptables() -> Result<(), String> {
    // The per-task chains below only see same-bridge (guest-to-guest) traffic
    // when the kernel passes bridged frames through iptables. Docker usually
    // enables this as a side effect; enforce it so isolation never depends on
    // that.
    ensure_bridge_netfilter().await?;
    ensure_ipv6_blocked().await?;

    // Remove the legacy bridge-wide allows. They bypass per-task policy and
    // one of them explicitly allowed guest-to-guest traffic.
    for rule in [
        vec![
            "-D",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "!",
            "-o",
            BRIDGE_NAME,
            "-j",
            "ACCEPT",
        ],
        vec![
            "-D",
            "FORWARD",
            "-i",
            BRIDGE_NAME,
            "-o",
            BRIDGE_NAME,
            "-j",
            "ACCEPT",
        ],
    ] {
        while iptables_rule_exists(
            &std::iter::once("-C")
                .chain(std::iter::once("FORWARD"))
                .chain(rule.iter().skip(2).copied())
                .collect::<Vec<_>>(),
        )
        .await
        {
            run_iptables(&rule).await?;
        }
    }

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
        run_iptables(&[
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
        .await?;
        tracing::debug!("cni.iptables: added MASQUERADE rule for {SUBNET}");
    }

    // FORWARD: only allow replies here. Each task gets a source-specific
    // public-egress chain in setup(); no bridge-wide outbound or peer allow.
    let forward_rules: &[&[&str]] = &[&[
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
    ]];
    let check_rules: &[&[&str]] = &[&[
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
    ]];
    for (check, add) in check_rules.iter().zip(forward_rules.iter()) {
        if !iptables_rule_exists(check).await {
            run_iptables(add).await?;
        }
    }
    tracing::debug!("cni.iptables: FORWARD rules ensured for {BRIDGE_NAME}");
    Ok(())
}

/// Write a resolv.conf on the HOST for Kata VMs to bind-mount.
pub async fn write_resolv_conf() -> Result<(), String> {
    // VPC resolvers are private/link-local destinations and therefore
    // intentionally blocked. Public resolvers preserve DNS without punching
    // a hole to the VPC or metadata network.
    let content = "nameserver 1.1.1.1\nnameserver 8.8.8.8\noptions ndots:0\n";

    let status = host_command("mkdir", &["-p", "/var/lib/kata"])
        .status()
        .await
        .map_err(|e| format!("failed to create guest DNS directory: {e}"))?;
    if !status.success() {
        return Err(format!("failed to create guest DNS directory: {status}"));
    }

    // Write to host filesystem via nsenter.
    let output = host_command("tee", &[RESOLV_CONF_HOST_PATH])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match output {
        Ok(child) => match write_stdin_and_wait(child, content).await {
            Ok(o) if o.status.success() => {
                tracing::debug!(path = RESOLV_CONF_HOST_PATH, "cni.resolv: written");
                Ok(())
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Err(format!("failed to write guest resolv.conf: {stderr}"))
            }
            Err(e) => Err(format!("failed to write guest resolv.conf: {e}")),
        },
        Err(e) => Err(format!("failed to spawn guest resolv.conf writer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_chains_are_stable_and_isolated() {
        let first = task_chain("0123456789abcdef01234567");
        let second = task_chain("0123456789abcdef89abcdef");
        assert_eq!(first, task_chain("0123456789abcdef01234567"));
        assert_ne!(first, second);
        assert!(first.len() <= 28);
    }

    #[test]
    fn blocked_ranges_cover_guest_and_metadata_networks() {
        assert!(BLOCKED_DESTINATIONS.contains(&"10.0.0.0/8"));
        assert!(BLOCKED_DESTINATIONS.contains(&"169.254.0.0/16"));
        assert!(BLOCKED_DESTINATIONS.contains(&"100.64.0.0/10"));
        assert!(BLOCKED_DESTINATIONS.contains(&"224.0.0.0/4"));
    }
}

fn task_chain(container_id: &str) -> String {
    let suffix: String = container_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .rev()
        .take(24)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{TASK_CHAIN_PREFIX}{suffix}")
}

async fn guest_ipv4(container_id: &str) -> Result<String, String> {
    let output = host_command(
        IP_BIN,
        &[
            "netns",
            "exec",
            container_id,
            IP_BIN,
            "-4",
            "-o",
            "addr",
            "show",
            "dev",
            "eth0",
        ],
    )
    .output()
    .await
    .map_err(|e| format!("failed to inspect guest address: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect guest address: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|part| part.contains('/') && part.bytes().any(|b| b == b'.'))
        .and_then(|cidr| cidr.split('/').next())
        .map(str::to_string)
        .ok_or_else(|| "CNI guest has no IPv4 address".to_string())
}

async fn install_task_policy(container_id: &str) -> Result<(), String> {
    let chain = task_chain(container_id);
    let source = format!("{}/32", guest_ipv4(container_id).await?);

    // Rebuild the task chain so retries are idempotent and cannot retain a
    // partially-installed allow policy.
    remove_task_policy(container_id).await;
    run_iptables(&["-N", &chain]).await?;
    for destination in BLOCKED_DESTINATIONS {
        run_iptables(&["-A", &chain, "-d", destination, "-j", "REJECT"]).await?;
    }
    run_iptables(&["-A", &chain, "-j", "ACCEPT"]).await?;
    run_iptables(&[
        "-I",
        "FORWARD",
        "1",
        "-i",
        BRIDGE_NAME,
        "-s",
        &source,
        "-j",
        &chain,
    ])
    .await?;
    Ok(())
}

async fn remove_task_policy(container_id: &str) {
    let chain = task_chain(container_id);
    remove_policy_chain(&chain).await;
}

async fn remove_policy_chain(chain: &str) {
    // Delete every jump to this chain (including a partial/retried setup).
    loop {
        let output = host_command(IPTABLES_BIN, &["-S", "FORWARD"])
            .output()
            .await;
        let Some(rule) = output.ok().filter(|o| o.status.success()).and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|line| line.split_whitespace().any(|part| part == chain))
                .map(str::to_string)
        }) else {
            break;
        };
        let mut args: Vec<&str> = rule.split_whitespace().collect();
        if args.first() == Some(&"-A") {
            args[0] = "-D";
        }
        if run_iptables(&args).await.is_err() {
            break;
        }
    }
    let _ = run_iptables(&["-F", &chain]).await;
    let _ = run_iptables(&["-X", &chain]).await;
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

async fn ip6tables_rule_exists(args: &[&str]) -> bool {
    match host_command(IP6TABLES_BIN, args).output().await {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

async fn run_ip6tables(args: &[&str]) -> Result<(), String> {
    let output = host_command(IP6TABLES_BIN, args)
        .output()
        .await
        .map_err(|e| format!("ip6tables spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(args = ?args, stderr = %stderr, "cni.ip6tables: rule failed");
        return Err(format!("ip6tables failed: {stderr}"));
    }
    Ok(())
}

/// Force bridged (same-L2) frames through iptables/ip6tables. Without
/// `bridge-nf-call-*`, guest-to-guest traffic on kata-br0 is switched below
/// the FORWARD chain and every per-task policy is bypassed.
async fn ensure_bridge_netfilter() -> Result<(), String> {
    // Best-effort: the module may be built in or already loaded; the sysctl
    // checks below are what actually decide pass/fail.
    let _ = host_command("/usr/sbin/modprobe", &["br_netfilter"])
        .output()
        .await;

    for key in [
        "net.bridge.bridge-nf-call-iptables=1",
        "net.bridge.bridge-nf-call-ip6tables=1",
    ] {
        let output = host_command("/usr/sbin/sysctl", &["-w", key])
            .output()
            .await
            .map_err(|e| format!("failed to run sysctl {key}: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "failed to enable {key} (required for guest isolation): {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(())
}

/// Guests are IPv4-only (host-local IPAM, v4 NAT). Drop all IPv6 on the
/// bridge so self-assigned link-local addresses cannot bypass the v4-only
/// per-task policy, and drop bridge traffic addressed to the host itself.
async fn ensure_ipv6_blocked() -> Result<(), String> {
    // IPv4: guests route through the host but have no business talking TO it.
    if !iptables_rule_exists(&["-C", "INPUT", "-i", BRIDGE_NAME, "-j", "DROP"]).await {
        run_iptables(&["-I", "INPUT", "1", "-i", BRIDGE_NAME, "-j", "DROP"]).await?;
    }

    let ipv6_enabled = host_command("test", &["-e", "/proc/net/if_inet6"])
        .status()
        .await
        .map_err(|e| format!("failed to probe host IPv6 support: {e}"))?
        .success();
    if !ipv6_enabled {
        return Ok(());
    }

    for chain in ["FORWARD", "INPUT"] {
        for direction in ["-i", "-o"] {
            // INPUT has no -o match.
            if chain == "INPUT" && direction == "-o" {
                continue;
            }
            if !ip6tables_rule_exists(&["-C", chain, direction, BRIDGE_NAME, "-j", "DROP"]).await {
                run_ip6tables(&["-I", chain, "1", direction, BRIDGE_NAME, "-j", "DROP"]).await?;
            }
        }
    }
    Ok(())
}

/// Create a network namespace and run CNI ADD to set up bridge networking.
/// Returns the netns path on success.
pub async fn setup(container_id: &str) -> Result<String, String> {
    let ns_path = netns_path(container_id);

    // Make retries/restarts idempotent.
    teardown(container_id).await;

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

    if let Err(e) = install_task_policy(container_id).await {
        teardown(container_id).await;
        return Err(format!("failed to install public-only CNI policy: {e}"));
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

    // Rules are independent of netns existence and must also be removed after
    // partial setup or a previous crash.
    remove_task_policy(container_id).await;

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
    let rules = host_command(IPTABLES_BIN, &["-S"])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .unwrap_or_default();
    for chain in rules.lines().filter_map(|line| {
        line.strip_prefix("-N ")
            .filter(|name| name.starts_with(TASK_CHAIN_PREFIX))
    }) {
        remove_policy_chain(chain).await;
    }

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

        tracing::debug!(netns = name, "cni.cleanup: removing stale netns");
        teardown(name).await;
        count += 1;
    }

    if count > 0 {
        tracing::debug!(count, "cni.cleanup: removed stale netns entries");
    } else {
        tracing::debug!("cni.cleanup: no stale netns entries found");
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
