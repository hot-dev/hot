# ssh

SSH client sessions and subsystem byte streams backed by Rust's `russh` library.
Requires a Hot runtime built with the SSH/SFTP bindings in this change. The
manifest minimum matches the current checkout (3.0.4); older binaries without
these bindings cannot load the package.

```hot
hot.project.my-app.deps {
    "hot.dev/ssh": "0.1.0"
}
```

```hot
::ssh ::ssh
::ctx ::hot::ctx

connection ::ssh/connect({
    host: "files.example.com",
    user: "integration",
    host-key: ::ctx/get("files.host.key"),
    private-key: ::ctx/get("files.private.key")
})

channel ::ssh/open-subsystem(connection, "my-subsystem")
::ssh/write(channel, "request")
reply ::ssh/read(channel, 65536)
::ssh/close(channel)
::ssh/close(connection)
```

For file transfers use `hot.dev/sftp`, which can start on an existing connection.

## Connection settings

Supply `host`, `user`, `host-key`, and exactly one of `password` or `private-key`.
`private-key` is the key's contents, not a local file path. `passphrase` unlocks
an encrypted key. Credentials are registered for masking in run records.

`host-key` is an OpenSSH public key or a `SHA256:...` fingerprint obtained through
a trusted channel. Unknown keys and changed keys are rejected. Host certificates
and automatic trust-on-first-use are not supported.

Prefer the full public key (for example a line from `ssh-keyscan`, without the
host name). Hot then asks the server for that key type, so the pin works even
when the server also has other host keys. A fingerprint cannot say which type
to ask for: Hot asks for Ed25519, then ECDSA, then RSA, and the fingerprint must
match the first type the server has. A mismatch error names the key the server
presented.

`port` defaults to 22. `timeout` is a positive millisecond deadline for each
operation, default 30000; connecting includes DNS, negotiation and authentication.
An inactive connection is disconnected after five minutes.

Outbound connections follow Hot's network allow/deny rules and are refused when
the installation requires an HTTP egress proxy.

## Channels and lifecycle

`read` returns up to the requested byte count, or null at EOF. `read-exact` either
returns all bytes or fails. Read sizes must be between 1 and 64 MiB. Reads and
writes have independent locks; use only one reader per channel to preserve
message boundaries. A failed exact read or interrupted write invalidates that
channel. `is-open` checks local state and does not probe the peer.

Close handles explicitly. Closing the connection invalidates all its channels
and SFTP sessions. Repeated closes are safe. Handles are scoped to the runtime;
their serialized IDs cannot restore connections. Reconnect after task restore.

The initial API provides subsystem channels. Shells, command execution, terminal
allocation, forwarding, agents, and SSH servers are outside this package's first
release.

## License

Apache-2.0. See [LICENSE](LICENSE).
