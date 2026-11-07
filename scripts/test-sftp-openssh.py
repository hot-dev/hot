#!/usr/bin/env python3
"""Exercise the Hot SSH/SFTP packages against a temporary local OpenSSH server."""

import argparse
import getpass
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hot-bin", required=True, type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    sshd = shutil.which("sshd") or "/usr/sbin/sshd"
    keygen = shutil.which("ssh-keygen")
    if not Path(sshd).is_file() or not keygen:
        parser.error("OpenSSH sshd and ssh-keygen are required")
    with tempfile.TemporaryDirectory(prefix="hot-sftp-") as directory:
        tmp = Path(directory).resolve()
        for name in ("host", "client"):
            subprocess.run([keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(tmp / name)], check=True)
        pin = subprocess.check_output([keygen, "-lf", str(tmp / "host.pub"), "-E", "sha256"], text=True).split()[1]
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        conf = tmp / "sshd_config"
        conf.write_text(f"""Port {port}
ListenAddress 127.0.0.1
HostKey {tmp / 'host'}
PidFile {tmp / 'sshd.pid'}
AuthorizedKeysFile {tmp / 'client.pub'}
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowUsers {getpass.getuser()}
Subsystem sftp internal-sftp
""")
        home = tmp / "home"
        home.mkdir()
        (home / "pkg").symlink_to(root / "hot/pkg", target_is_directory=True)
        (home / "resources").symlink_to(root / "resources", target_is_directory=True)
        src = tmp / "src"
        src.mkdir()
        (tmp / "hot.hot").write_text(f"""::hot::conf ns
hot.set.project "sftp-integration"
hot.project.sftp-integration.src.paths ["{root / 'hot/pkg/ssh/src'}", "{root / 'hot/pkg/sftp/src'}"]
hot.project.sftp-integration.test.paths ["{src}"]
hot.project.sftp-integration.test.capture true
hot.project.sftp-integration.deps {{}}
""")
        (src / "integration.hot").write_text('''::sftp::integration ns
meta ["test"]

::env ::hot::env

test-openssh-lifecycle
meta ["test"] fn () {
    options {
        host: "127.0.0.1",
        port: Int(::env/get("SFTP_TEST_PORT", "")),
        user: ::env/get("SFTP_TEST_USER", ""),
        host-key: ::env/get("SFTP_TEST_PIN", ""),
        private-key: ::env/get("SFTP_TEST_KEY", ""),
        timeout: 5000
    }
    base ::env/get("SFTP_TEST_ROOT", "")
    session ::sftp/connect(options)
    assert(is-type(session, ::sftp/Session))
    dir `${base}/files`
    path `${dir}/binary`
    link `${dir}/link`
    moved `${dir}/moved`
    ::sftp/mkdir(session, dir)
    data Bytes([0, 255, 128, 42, 0])
    assert-eq(5, ::sftp/write-file(session, path, data))
    assert-eq(data, ::sftp/read-file(session, path))
    assert(is-err(::sftp/read-file(session, path, 2)))
    attributes ::sftp/stat(session, path)
    assert(is-type(attributes, ::sftp/Attributes))
    assert-eq("file", attributes.kind)
    file ::sftp/open(session, path, "append")
    assert(is-type(file, ::sftp/File))
    assert-eq(1, ::sftp/write(file, Bytes([7])))
    ::sftp/close(file)
    assert-eq(concat(data, Bytes([7])), ::sftp/read-file(session, path))
    file ::sftp/open(session, path, "read-write")
    ::sftp/seek(file, 3)
    assert-eq(2, ::sftp/write(file, Bytes([9, 10])))
    ::sftp/seek(file, 3)
    assert-eq(Bytes([9, 10, 7]), ::sftp/read(file, 3))
    assert(is-null(::sftp/read(file, 1)))
    ::sftp/close(file)
    ::sftp/close(file)
    ::sftp/symlink(session, path, link)
    assert-eq(path, ::sftp/readlink(session, link))
    assert-eq("symlink", ::sftp/lstat(session, link).kind)
    assert-eq("file", ::sftp/stat(session, link).kind)
    entries ::sftp/list(session, dir)
    assert-eq(2, length(entries))
    assert(is-type(first(entries), ::sftp/Entry))
    assert-eq(dir, ::sftp/realpath(session, dir))
    assert(not(::sftp/exists(session, `${dir}/missing`)))
    ::sftp/rename(session, path, moved)
    ::sftp/remove(session, link)
    ::sftp/remove(session, moved)
    ::sftp/rmdir(session, dir)
    ::sftp/close(session)
    ::sftp/close(session)
    assert(is-err(::sftp/stat(session, base)))

    connection ::ssh/connect(options)
    assert(is-type(connection, ::ssh/Connection))
    shared ::sftp/start(connection)
    ::sftp/close(shared)
    assert(::ssh/is-open(connection))
    ::ssh/close(connection)
    assert(not(::ssh/is-open(connection)))
}
''')
        env = os.environ.copy()
        env.update({"HOT_HOME": str(home), "SFTP_TEST_PORT": str(port), "SFTP_TEST_USER": getpass.getuser(), "SFTP_TEST_PIN": pin, "SFTP_TEST_KEY": (tmp / "client").read_text(), "SFTP_TEST_ROOT": str(tmp)})
        with (tmp / "sshd.log").open("w+") as log:
            server = subprocess.Popen([sshd, "-D", "-e", "-f", str(conf)], stdout=log, stderr=log)
            try:
                for _ in range(100):
                    if server.poll() is not None:
                        log.seek(0)
                        raise RuntimeError("sshd failed to start: " + log.read())
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        time.sleep(0.05)
                else:
                    raise RuntimeError("sshd did not become ready")
                completed = subprocess.run([str(args.hot_bin.resolve()), "test", "--emitter.type", "none", "--db.uri", ""], cwd=tmp, env=env)
                if completed.returncode:
                    log.seek(0)
                    print(log.read())
                return completed.returncode
            finally:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    raise SystemExit(main())
