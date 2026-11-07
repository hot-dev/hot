---
description: "Control which network destinations Hot code can reach with ::hot::http, ::hot::tcp, and ::hot::ws in a self-hosted installation."
---

# Outbound Network Access

Code you deploy to Hot reaches other systems with `::hot::http`,
`::hot::tcp`, and `::hot::ws`. In a worker or task worker with isolation on
(`hot.engine.isolation`, the default for those services), those calls can
reach public internet addresses only. Each destination is resolved and checked
immediately before connecting, and redirects are checked again, so DNS changes
and redirects cannot lead to a private address.

Self-hosted production workers and task workers refuse to start with isolation
off (`HOT_ENV=production` with the `self-host` product experience).

## Reaching private services

To let deployed code call a service inside your network, allow its addresses
or hostnames:

```hot
hot.network.allow-cidrs ["10.20.0.0/16", "192.168.5.10"]
hot.network.allow-hosts ["api.corp.example", "*.svc.internal"]
```

| Setting | Environment variable | Meaning |
|---------|----------------------|---------|
| `network.allow-cidrs` | `HOT_NETWORK_ALLOW_CIDRS` | Private addresses or CIDR ranges code may reach |
| `network.allow-hosts` | `HOT_NETWORK_ALLOW_HOSTS` | Hostnames that may resolve to private addresses; `*.domain` matches any subdomain |
| `network.deny-cidrs` | `HOT_NETWORK_DENY_CIDRS` | Addresses code may never reach, even when an allowlist covers them |

Environment variables take comma-separated lists. All three are empty by
default.

`network.allow-cidrs` and `network.deny-cidrs` take IP addresses and CIDR
ranges with a prefix length. A worker or task worker refuses to start on any
other form, such as a netmask (`10.0.0.0/255.0.0.0`). An IPv4 range written in
IPv6 form means the IPv4 range it carries: `::ffff:10.68.0.0/112` is
`10.68.0.0/16`, and `::ffff:0:0/96` or `::/96` is all of IPv4. That covers
`::ffff:a.b.c.d`, `::a.b.c.d`, and `64:ff9b::a.b.c.d` with a prefix of 96 or
longer.

Put the installation's own database, cache, ingress, and host addresses in
`network.deny-cidrs` whenever an allowlist could cover them. On
[Hot's self-hosted AWS and Google Cloud stacks](https://github.com/hot-dev/hot/blob/main/infra/README.md),
the runtime bundle adds some of them for workers, task workers, and box guests.
Your own entries in `settings.HOT_NETWORK_DENY_CIDRS` stay, and the bundle adds its container
network (`172.30.0.0/24`), the Kata guest bridge (`10.88.0.0/16`), and the
addresses of the data services and of the host it runs on.

A data service's address is only where it is now, so the bundle also denies the
networks its standbys and replicas can be on:
`data.postgres.endpoint.deny_cidrs` and `data.valkey.endpoint.deny_cidrs` in the
installation's configuration. These apply to workers, task workers, and box
guests, even when an allowlist covers them. Each entry is an IPv4 or IPv6 network
with a prefix narrower than `/0` and no host bits (`10.68.4.0/24`), and a plan
refuses them on a local service. For a service the stack runs, the stack adds
entries itself:

- **AWS:** each data subnet's range, for RDS and for ElastiCache. Anything else in
  those subnets becomes unreachable from user code too, so on an existing network,
  give the services subnets of their own.
- **Google Cloud, Cloud SQL:** on a network the stack created, the private services
  range it reserves. On an existing network it cannot look that range up, so list
  it in `data.postgres.endpoint.deny_cidrs` if an allowlist could cover it
  (`gcloud compute addresses list --global --filter=purpose=VPC_PEERING`). That also
  blocks any other Cloud SQL instance in the range, so the choice is yours; without
  the entry, only the database's own address is denied.
- **Google Cloud, Memorystore:** each Private Service Connect address of the
  instance.

For an existing service, in either cloud, only your own entries are used, because
the stack does not know where it runs. Without them, only the addresses it has
resolved to are denied.

On a network the stack created, the bundle also denies the network's whole range,
which covers the hosts and load balancers in it. On a network the stack did not
create (`network.mode` `existing`), it denies only the addresses and entries above.
If an allowlist could cover the rest, add them to
`settings.HOT_NETWORK_DENY_CIDRS` yourself: your load balancers, and on a scalable
installation the ranges of the other hosts.

The addresses the bundle denies can also change. A data service that moves inside
`deny_cidrs`, or inside the range of a network the stack created, as a managed
database or cache can when it fails over, restarts nothing but the box guests'
firewall baseline, which leaves running boxes alone. When a data service gets an
address outside those networks, which for an existing service with no entries can
be any new address, or when the host gets a new address on a network the stack did
not create, the host agent redeploys the same release on its own. That recreates
the workers and task workers whose deny list changed, restarts the scheduler, and
recreates the ingress on hosts that have one, which briefly interrupts requests to
them. Earlier data service addresses stay denied, but a host's earlier addresses
do not.
If the redeploy fails, the agent reports the failure and waits for
[`deploy-now`](https://github.com/hot-dev/hot/blob/main/infra/README.md#operating-an-installation),
or on a scalable Google Cloud installation for a
[replacement of the role's instances](https://github.com/hot-dev/hot/blob/main/infra/README.md#upgrading), and Hot keeps running.

## Egress proxy

To audit outbound traffic or allow it by domain, send it through your own
HTTP proxy:

```hot
hot.network.egress-proxy "http://proxy.internal:3128"
```

The address is `http://` or `https://`, with no path. Percent-encode any `/`,
`?`, `#`, or `\` in its credentials. Otherwise Hot refuses the address, and the
worker and task worker exit at startup, or it connects to the wrong host.
`HOT_NETWORK_EGRESS_PROXY` sets it from the environment; supply it as
`HOT_NETWORK_EGRESS_PROXY_FILE` if the address carries credentials. On Hot's
self-hosted stacks, put an address with credentials in the operator secret
bundle as `HOT_NETWORK_EGRESS_PROXY`, and the bundle sets the `_FILE` variable
itself. A new entry reaches the worker and task worker only when the release is
rendered again. Until then they don't use it, and `restart-service` of either is
refused, naming it. After adding or removing the entry, run `deploy-now`, which renders
the release again to mount or unmount it. To rotate the value later, update it and run
`restart-service` for the worker and the task worker. On a scalable Google Cloud
installation, where the `selfhost.py` helper sends neither,
[replace those groups' instances](https://github.com/hot-dev/hot/blob/main/infra/README.md#rotating-secrets-and-certificates)
instead. A deploy, `deploy-now`, and `restart-service` of the worker or task worker
refuse a malformed entry before anything stops. A reboot delivers it unchecked, and
those services then exit on it. `settings` refuses an address with credentials and
any `_FILE` name. An address without credentials can stay in `settings`.

With a proxy set, `::hot::http` requests and alert deliveries go through it,
with or without isolation, and the proxy resolves destination names. Hot never
sees the address behind a name, so `network.deny-cidrs`, `network.allow-hosts`,
and the public-only rule do not apply to a hostname. The proxy has to enforce
them itself. It must refuse the destinations in
[Always blocked](#always-blocked), everything in `network.deny-cidrs`, including
what the runtime bundle adds, and any private range your code should not reach.
Otherwise a name that resolves to your database reaches your database.

Under isolation, Hot still checks addresses written directly in a URL against
`network.allow-cidrs`, `network.deny-cidrs`, and the
[Always blocked](#always-blocked) list. Without isolation, user code has no
other limit: only the proxy's rules apply. `::hot::tcp` and `::hot::ws`
connections are refused either way, since they could not pass through the
proxy. Hot's own connections to its database, cache, storage, and secret store
never use the proxy.

Only `network.egress-proxy` sends this traffic through a proxy. `HTTP_PROXY`,
`HTTPS_PROXY`, and `ALL_PROXY` in the worker's environment are ignored for
user-code HTTP and alert deliveries whenever destinations are restricted: with
isolation on, or for alert deliveries without `network.allow-private`. Those
connections go direct, because such a proxy would resolve names again, past
these checks.

## Always blocked

No setting makes these reachable from deployed code:

- loopback and unspecified addresses (`127.0.0.0/8`, `::1`, `0.0.0.0`)
- link-local addresses (`169.254.0.0/16`, `fe80::/10`), which include the
  cloud metadata services and the ECS task metadata endpoint
- the AWS and Google Cloud metadata services over IPv6 (`fd00:ec2::254`,
  `fd20:ce::254`)
- multicast, broadcast, and reserved ranges

IPv4 addresses written in IPv6 form, such as `::ffff:169.254.169.254`, are
checked as the IPv4 address they carry.

`network.allow-private = true`, the earlier all-or-nothing setting, now allows
every private address but no longer reaches anything in this list. Prefer the
explicit allowlists.

Alert deliveries (webhooks, Slack, and PagerDuty) follow `network.allow-cidrs`,
`network.allow-hosts`, and `network.deny-cidrs` too.

## Box tasks

[Box](/docs/box) guests on the Kata backend follow the same fixed deny set as
other code. With no egress proxy set, they also follow the same
`network.allow-cidrs` and `network.deny-cidrs` rules. Guests are IPv4-only, so
IPv6 entries do not apply to them, and `network.allow-hosts` does not apply
because guests connect by address. The Docker backend, which `hot dev` uses,
applies none of these rules: its guests use plain bridge networking.

Guests resolve names through public resolvers (`1.1.1.1` and `8.8.8.8`) by
default. To use your own, set `box.guest-dns` (`HOT_BOX_GUEST_DNS`) to their
IPv4 addresses; guests can reach them on port 53 only, even when they are
private. The task worker refuses to start on a resolver in `0.0.0.0/8`,
`127.0.0.0/8`, `169.254.0.0/16`, `10.88.0.0/16`, `224.0.0.0/4`, or
`240.0.0.0/4`, and a deploy on Hot's self-hosted stacks refuses one too. That
rules out the link-local VPC resolvers (`169.254.169.253` on AWS and
`169.254.169.254` on Google Cloud). Use the VPC's base address plus two on AWS,
or a Cloud DNS inbound forwarding address on Google Cloud.

### With an egress proxy

With `network.egress-proxy` set, guests on the Kata backend can connect only to
the proxy and their resolvers. `network.allow-cidrs` opens nothing for them, and
every other connection fails: the task's own firewall rules refuse it, and on
Hot's host images the host's firewall drops it first, so it times out. Tasks
with `network: "none"` are unaffected.

Guests do not use the proxy on their own and never get its credentials; a box
task sets `HTTP_PROXY` and `HTTPS_PROXY` in its env to use it:

```hot
task ::box/start(BoxConf({
  image: "alpine:3",
  cmd: ["wget", "-qO-", "https://example.com"],
  env: {
    HTTP_PROXY: "http://10.20.0.5:3128",
    HTTPS_PROXY: "http://10.20.0.5:3128",
    http_proxy: "http://10.20.0.5:3128",
    https_proxy: "http://10.20.0.5:3128",
  },
}))
```

Some tools read only the lowercase names, and some runtimes ignore these
variables.

- **Credentials.** Credentials supplied through
  `HOT_NETWORK_EGRESS_PROXY_FILE` stay with the worker and task worker. Guest
  connections reach the proxy from the machine's own address, so the proxy
  authorizes by that source address, or the task supplies its own credentials
  in the URL.
- **The proxy's address.** The proxy needs an IPv4 address outside the
  always-blocked ranges and the guest network `10.88.0.0/16`. Without one, box
  tasks with network access fail with `Failed to start container` and the task
  page's infrastructure failure banner. The reason, `egress proxy <host> has no
  IPv4 address box guests can reach`, is only in the task worker's log, on the
  lines `kata.cni.setup.failed` and `Container task failed`.
- **The proxy's name.** Guests resolve the name in `HTTP_PROXY` with their own
  resolvers, and public resolvers cannot resolve private names. Use the proxy's
  IP address, as above, or set `box.guest-dns`.
- **A proxy whose address changes.** The task worker looks the proxy up for each
  task. On Hot's host images, the host's firewall looks it up every 5 minutes,
  so a new address can take that long to work. While the name does not resolve,
  the firewall keeps the addresses already in place and running tasks keep their
  rules, but new box tasks with network access fail in the same way, with
  `failed to resolve egress proxy <host>` on the same log lines.
- **DNS.** Guest DNS does not pass through the proxy. Unless `box.guest-dns` is
  set, guests can query `1.1.1.1` and `8.8.8.8` directly, and the task worker
  logs a warning at startup. To audit DNS, set `box.guest-dns` to resolvers you
  log.

A task worker on the Docker backend doesn't apply `network.egress-proxy`, so its
guests would connect directly, past the proxy. With Hot Box on, it refuses to start
with a proxy set. Set `HOT_BOX_BACKEND=kata`, which needs Linux with KVM and a Hot
built with Kata support, or unset the proxy. With `HOT_ENV` unset or `development`,
you can also turn Hot Box off with `HOT_BOX_ENABLED=false`: the task worker then
starts, runs no box tasks, and its code tasks use the proxy. The same refusal applies
to `hot dev` and `hot test`, which default to Docker: their task worker logs the error
and doesn't run, while the other services keep running, so tasks stay queued.

With `HOT_ENV` set to `production` or `staging`, a task worker refuses the Docker
backend whether Hot Box is on or off and whether or not a proxy is set, so it needs
`HOT_BOX_BACKEND=kata`.

### Image registries

With isolation on, `::hot::box/start` checks the image's registry as well. Any
registry other than Docker Hub must resolve to addresses your code may reach, or
the start fails with `image registry '<name>' is not reachable`. A private
registry, including ECR or Artifact Registry behind a private endpoint, needs
`network.allow-hosts` or `network.allow-cidrs` to cover it. That entry also opens
those addresses to `::hot::http`, `::hot::tcp`, and `::hot::ws`, and
`network.allow-cidrs` opens them to box guests. On a network one of
[Hot's self-hosted stacks](https://github.com/hot-dev/hot/blob/main/infra/README.md)
created, an address inside its range stays denied, whatever is allowed, so the
endpoint has to be outside that range.

This check reads the name in the image reference when the task starts. The pull
itself runs in the installation's container runtime, not in your code. It does
not use `network.egress-proxy`, it resolves the name again, and it follows the
registry's redirects, which nothing checks against the deny list. A registry your
code names can therefore steer the pull's requests to other addresses, including
private ones. A Docker Hub image can do the same without a redirect: its manifest
can give a layer other addresses, in the `urls` of that layer's descriptor, and
containerd tries those before the registry. Whether Docker Hub accepts such an
image has not been checked. Either way the pull's timing can tell the image's
author something about what answered, and so can the failure message, which for
a Docker Hub image still tells a refusal apart from a dropped connection.

So that a failed pull says nothing about what answered there, every pull failure
from a registry other than Docker Hub reads the same, on every installation,
whether or not isolation is on:

```text
Could not pull image '<image>'. Check the image name and tag, that its registry is reachable from this worker, and, for a private image, its registry-auth
```

The reason is only in the worker log, on the line `Container task failed`. That
covers a wrong `registry-auth` token, which is no longer named in the message, and
a Kata worker whose containerd lacks the Transfer service. These failures are not
marked as infrastructure failures, so the task page shows no such banner even when
the worker is at fault. Timing is not hidden: the task result's `image-pull-ms`
still says how long the pull took. Docker Hub images keep their specific messages,
such as an image that was not found or a pull that was rate-limited.

## Box network compatibility

The worker isolation update removes the `NET_RAW` capability from boxes.
Programs that require raw sockets, including some implementations of `ping`,
will fail with a permission error. Ordinary HTTP, TCP, and UDP clients remain
available subject to the configured network policy. Images that use raw-socket
`ping` for a health check should use a TCP or HTTP check instead. Linux ping
sockets may still work when the image and kernel permit them.

Kata hosts must install `nftables`; both the bundled host setup and production
AMI setup install it. Bridge traffic permits only IPv4 and ARP from each guest,
with its assigned MAC and source IP. VLAN-tagged frames are rejected. The pinned
[CNI bridge v1.9.1](https://github.com/containernetworking/plugins/blob/v1.9.1/plugins/main/bridge/bridge.go)
supports the MAC spoof check and bridge port isolation used by the worker.

## Box command compatibility

Hot 3.0.5 runs Kata boxes the way the Docker backend runs them. Two things
change for images that set an `ENTRYPOINT` or a `USER`:

- A box's `cmd`, or the shell command its `script` becomes, is passed to the
  image `ENTRYPOINT` as arguments, as `docker run` does. Before, Kata ran it in
  place of the entrypoint. An image whose entrypoint is not a shell, such as
  `["python"]`, now receives `sh -c ...` as arguments to that program.
- A writable box runs as the image `USER`. Before, Kata ran it as root. A box
  that installs packages with `apk add` or `apt-get` in an image with a non-root
  `USER` now fails with a permission error. Read-only boxes are unchanged.

To keep the old command behavior, set `entrypoint: [""]` on the box. It
clears the image entrypoint, so `cmd` or `script` runs on its own. For a box
that needs root in an image with a non-root `USER`, build a derived image that
sets `USER root`, or run the install step in an image without one. An image
whose `USER` names an account missing from its `/etc/passwd` now fails as an
image error its owner can fix, not as an infrastructure failure.
