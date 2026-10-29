---
description: "Control which network destinations Hot code can reach with ::hot::http and ::hot::tcp in a self-hosted installation."
---

# Outbound Network Access

Code you deploy to Hot reaches other systems with `::hot::http` and
`::hot::tcp`. In a worker or task worker with isolation on
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

Put the installation's own database, cache, ingress, and host addresses in
`network.deny-cidrs` whenever an allowlist could cover them. The runtime bundle
for Hot's single-machine AWS and Google Cloud stacks sets it to the bundle's
container network and, when the stack created it, the installation's network.

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

Alert deliveries (webhooks and Slack) follow `network.allow-cidrs`,
`network.allow-hosts`, and `network.deny-cidrs` too.
