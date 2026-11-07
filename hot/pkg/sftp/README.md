# sftp

SFTP v3 file transfers and directory operations backed by `russh` and
`russh-sftp`. Bulk I/O uses native request pipelining. Requires a Hot runtime
built with the SSH/SFTP bindings in this change. The manifest minimum matches
the current checkout (3.0.4); older binaries without these bindings cannot load
the package.

```hot
hot.project.my-app.deps {
    "hot.dev/sftp": "0.1.0"
}
```

```hot
::sftp ::sftp
::ctx ::hot::ctx

session ::sftp/connect({
    host: "files.example.com",
    user: "integration",
    host-key: ::ctx/get("files.host.key"),
    private-key: ::ctx/get("files.private.key")
})

entries ::sftp/list(session, "/incoming")
data ::sftp/read-file(session, "/incoming/report.csv")
::sftp/write-file(session, "/outgoing/report.csv", data)
::sftp/rename(session, "/outgoing/report.csv", "/outgoing/ready.csv")
::sftp/close(session)
```

Use a trusted OpenSSH public key or SHA256 fingerprint for `host-key`. A full
public key also selects which of the server's host keys is used; see the
`hot.dev/ssh` README.
Connection options are shared with `hot.dev/ssh`: `port` defaults to 22;
`timeout` defaults to 30000 milliseconds. Supply either `private-key` contents
(with optional `passphrase`) or `password`. Hot's outbound network policy applies.

## Operations

| Functions | Behavior |
| --- | --- |
| `connect(options)`, `start(ssh-connection)` | Open SFTP on a new or shared SSH connection |
| `list(session, path)` | Directory entries with `name`, `path`, and `attributes`; buffers the directory listing |
| `stat`, `lstat` | Attributes with optional size, uid, gid, permissions, accessed/modified Unix seconds, and kind |
| `exists`, `realpath`, `readlink` | Existence, canonical path, and symbolic-link target |
| `mkdir`, `rmdir`, `remove` | Create a directory, remove an empty directory, or remove a file/link |
| `rename(session, old, new)`, `symlink(session, target, link)` | Rename or create a symbolic link |
| `read-file(session, path[, max])` | Read Bytes up to a limit; default and maximum 64 MiB |
| `write-file(session, path, data)` | Create/truncate, write Bytes or UTF-8 Str, acknowledge and close; return byte count |
| `open(session, path[, mode])` | Open a file: `read` (default), `write`, `append`, or `read-write` |
| `read(file, max)`, `write(file, data)`, `seek(file, offset)` | Chunk I/O and absolute cursor positioning |
| `close(file-or-session)` | Release resources; safe to repeat |

Paths are remote POSIX paths. `exists` returns false for a missing path; permission
and connection errors remain Err. Rename replacement behavior follows the
server's SFTP v3 implementation. Writes acknowledge server acceptance, without
requesting an additional durable filesystem sync.

## Large files and errors

Use `open` and repeated `read`/`write` calls for large files. Each read is limited
to 64 MiB and may return fewer bytes than requested; only null indicates EOF.
`write` flushes outstanding requests before returning. A failed file operation
invalidates its cursor handle; close it and reopen as appropriate. Failed or
cancelled writes may have partially modified the remote file. Upload to a
temporary remote path and rename when the application needs a publish step.

Operations on one file handle serialize cursor access. Separate files may be
used in parallel on one session. Close files before closing their session.
`connect` owns and closes its SSH connection; `start` leaves the caller's shared
SSH connection open. Runtime handles cannot be restored from checkpoints.

## Development checks

From the repository root, run the hermetic native tests and the optional local
OpenSSH interoperability test (requires `sshd` and `ssh-keygen`):

```sh
cargo test -p hot --test ssh_sftp
python3 scripts/test-sftp-openssh.py --hot-bin target/debug/hot
```

The OpenSSH check creates temporary keys, files, and a loopback server, and cleans
them up when it exits.

## License

Apache-2.0. See [LICENSE](LICENSE).
