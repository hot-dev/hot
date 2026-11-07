//! Runtime-owned SSH/SFTP clients. Public APIs live in hot.dev/ssh and hot.dev/sftp.
//! Connect an already policy-checked socket: russh must never resolve a host itself.

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::outbound::DestinationPolicy;
use crate::val::{Val, as_fields};
use indexmap::IndexMap;
use russh::client;
use russh::keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh_sftp::client::{SftpSession, fs::File};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use std::any::Any;
use std::borrow::Cow;
use std::future::Future;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};

const DEFAULT_TIMEOUT: u64 = 30_000;
const MAX_BYTES: i64 = 64 * 1024 * 1024;

#[derive(Default)]
struct Control {
    closed: AtomicBool,
    notify: Notify,
}

impl Control {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn stopped(&self) {
        loop {
            let notified = self.notify.notified();
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct Client {
    fingerprint: String,
    // The key the server offered, recorded so a pin mismatch can name it.
    presented: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // CA certificates need their own validation policy; a raw-key pin is not a CA pin.
        if key.certificate().is_some() {
            return Ok(false);
        }
        let key = key.public_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let matched = fingerprint == self.fingerprint;
        if !matched && let Ok(mut presented) = self.presented.lock() {
            *presented = Some(format!("{} {fingerprint}", key.algorithm()));
        }
        Ok(matched)
    }
}

struct Connection {
    id: String,
    session: Mutex<Option<client::Handle<Client>>>,
    control: Control,
    timeout: Duration,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Channels retain the connection, so its last owner is also the last channel owner.
        if let Some(session) = self.session.get_mut().take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    session.disconnect(russh::Disconnect::ByApplication, "", ""),
                )
                .await;
            });
        }
    }
}

type Stream = russh::ChannelStream<client::Msg>;

struct Channel {
    id: String,
    connection: Arc<Connection>,
    reader: Mutex<Option<tokio::io::ReadHalf<Stream>>>,
    writer: Mutex<Option<tokio::io::WriteHalf<Stream>>>,
    control: Control,
}

struct Sftp {
    id: String,
    connection: Arc<Connection>,
    session: SftpSession,
    owns_connection: bool,
    control: Control,
}

struct RemoteFile {
    id: String,
    sftp: Arc<Sftp>,
    file: Mutex<Option<File>>,
    control: Control,
}

trait Resource: Send + Sync + 'static {
    const NAME: &'static str;
    const FIELD: &'static str;
    const TYPE: &'static str;
    fn id(&self) -> &str;
}

macro_rules! resource {
    ($ty:ty, $name:literal, $field:literal, $record:literal) => {
        impl Resource for $ty {
            const NAME: &'static str = $name;
            const FIELD: &'static str = $field;
            const TYPE: &'static str = $record;
            fn id(&self) -> &str {
                &self.id
            }
        }
    };
}
resource!(Connection, "SshConnection", "$ssh", "::ssh/Connection");
resource!(Channel, "SshChannel", "$ssh-channel", "::ssh/Channel");
resource!(Sftp, "SftpSession", "$sftp", "::sftp/Session");
resource!(RemoteFile, "SftpFile", "$sftp-file", "::sftp/File");

struct Handle<T: Resource>(Arc<T>);

impl<T: Resource> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}<{}>", T::NAME, self.0.id())
    }
}

impl<T: Resource> crate::val::ValBox for Handle<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
    fn clone_box(&self) -> Box<dyn crate::val::ValBox> {
        Box::new(Self(self.0.clone()))
    }
    fn equals(&self, other: &dyn crate::val::ValBox) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|o| Arc::ptr_eq(&self.0, &o.0))
    }
    fn hash(&self, _state: &mut dyn Hasher) {}
    fn to_string(&self) -> String {
        format!("{:?}", self)
    }
    fn compare(&self, _other: &dyn crate::val::ValBox) -> Option<std::cmp::Ordering> {
        None
    }
    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        // An identifier is safe in call records, but cannot restore a live socket.
        Ok(serde_json::json!({"$type": T::NAME, "id": self.0.id()}))
    }
    fn type_name(&self) -> &'static str {
        T::NAME
    }
}

fn resource_fields<T: Resource>(inner: Arc<T>) -> IndexMap<Val, Val> {
    let mut m = IndexMap::new();
    m.insert(Val::from("id"), Val::from(inner.id()));
    m.insert(Val::from(T::FIELD), Val::Box(Box::new(Handle(inner))));
    m
}

fn resource_val<T: Resource>(inner: Arc<T>) -> Val {
    Val::constructed(T::TYPE, resource_fields(inner))
}

fn record<const N: usize>(name: &str, fields: [(&str, Val); N]) -> Val {
    Val::constructed(
        name,
        fields.into_iter().map(|(k, v)| (Val::from(k), v)).collect(),
    )
}

fn extract<T: Resource>(val: &Val) -> Result<Arc<T>, String> {
    match field(val, T::FIELD) {
        Some(Val::Box(b)) => b
            .as_any()
            .downcast_ref::<Handle<T>>()
            .map(|h| h.0.clone())
            .ok_or_else(|| format!("invalid {} handle", T::NAME)),
        _ => Err(format!("expected a live {} handle", T::NAME)),
    }
}

fn field<'a>(val: &'a Val, key: &str) -> Option<&'a Val> {
    match as_fields(val) {
        Val::Map(m) => m.get(&Val::from(key)).filter(|v| !matches!(v, Val::Null)),
        _ => None,
    }
}

fn string<'a>(val: &'a Val, key: &str) -> Result<Option<&'a str>, String> {
    match field(val, key) {
        Some(Val::Str(s)) => Ok(Some(s.as_ref())),
        None => Ok(None),
        _ => Err(format!("{key} must be a Str")),
    }
}

fn required<'a>(val: &'a Val, key: &str) -> Result<&'a str, String> {
    string(val, key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

fn integer(val: &Val, key: &str, default: i64, min: i64, max: i64) -> Result<i64, String> {
    match field(val, key) {
        None => Ok(default),
        Some(Val::Int(n)) if (min..=max).contains(n) => Ok(*n),
        _ => Err(format!("{key} must be an Int between {min} and {max}")),
    }
}

fn arg_str(args: &[Val], index: usize) -> Result<&str, String> {
    match args.get(index) {
        Some(Val::Str(s)) if !s.contains('\0') => Ok(s),
        _ => Err(format!(
            "argument {} must be a Str without NUL bytes",
            index + 1
        )),
    }
}

fn arg_bytes(args: &[Val], index: usize) -> Result<&[u8], String> {
    match args.get(index) {
        Some(Val::Bytes(b)) => Ok(b),
        Some(Val::Str(s)) => Ok(s.as_bytes()),
        _ => Err(format!("argument {} must be Bytes or Str", index + 1)),
    }
}

fn bounded_size(args: &[Val], index: usize) -> Result<usize, String> {
    match args.get(index) {
        Some(Val::Int(n)) if (1..=MAX_BYTES).contains(n) => Ok(*n as usize),
        _ => Err(format!("size must be an Int between 1 and {MAX_BYTES}")),
    }
}

fn result(name: &str, result: Result<Val, String>) -> HotResult<Val> {
    match result {
        Ok(v) => HotResult::Ok(v),
        Err(e) => HotResult::Err(Val::from(format!("{name}: {e}"))),
    }
}

fn run<T>(
    vm: &VirtualMachine,
    timeout: Duration,
    controls: &[&Control],
    fut: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let cancelled = vm.external_cancel_token();
    tokio::runtime::Handle::current().block_on(async {
        let cancel = async {
            loop {
                if cancelled.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) { return; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let stopped = async {
            if controls.is_empty() { std::future::pending::<()>().await; }
            futures::future::select_all(controls.iter().map(|c| Box::pin(c.stopped()))).await;
        };
        tokio::select! {
            biased;
            _ = cancel => Err("operation cancelled".into()),
            _ = stopped => Err("connection or handle is closed".into()),
            r = tokio::time::timeout(timeout, fut) => r.map_err(|_| "operation timed out".to_string())?,
        }
    })
}

// Cleanup must still run after the run/task's cancellation token has fired.
fn cleanup<T>(
    timeout: Duration,
    fut: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::runtime::Handle::current().block_on(async {
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| "close timed out".to_string())?
    })
}

struct HostKeyPin {
    fingerprint: String,
    /// Host-key algorithms to offer. A full public key restricts negotiation to its own type,
    /// so a server that also holds other key types presents the pinned one.
    algorithms: Option<Vec<Algorithm>>,
}

fn host_key_pin(pin: &str) -> Result<HostKeyPin, String> {
    if pin.starts_with("SHA256:") {
        let parsed: russh::keys::ssh_key::Fingerprint = pin
            .parse()
            .map_err(|_| "invalid SHA256 host-key fingerprint")?;
        return Ok(HostKeyPin {
            fingerprint: parsed.to_string(),
            algorithms: None,
        });
    }
    let key = PublicKey::from_openssh(pin)
        .map_err(|_| "host-key must be an OpenSSH public key or SHA256 fingerprint")?;
    let algorithms: Vec<Algorithm> = match key.algorithm() {
        // An RSA key signs with any of the RSA hash variants; keep russh's order.
        Algorithm::Rsa { .. } => russh::Preferred::DEFAULT
            .key
            .iter()
            .filter(|a| matches!(a, Algorithm::Rsa { .. }))
            .cloned()
            .collect(),
        alg if russh::Preferred::DEFAULT.key.contains(&alg) => vec![alg],
        alg => return Err(format!("host-key type {alg} is not supported")),
    };
    Ok(HostKeyPin {
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        algorithms: Some(algorithms),
    })
}

fn connect_inner(vm: &mut VirtualMachine, options: &Val) -> Result<(Arc<Connection>, Val), String> {
    if !matches!(as_fields(options), Val::Map(_)) {
        return Err("options must be a Map".into());
    }
    for key in ["password", "private-key", "passphrase"] {
        if let Some(value) = field(options, key) {
            super::ctx::register_secret(vm, value);
        }
    }
    let host = required(options, "host")?;
    let user = required(options, "user")?;
    let HostKeyPin {
        fingerprint: pin,
        algorithms,
    } = host_key_pin(required(options, "host-key")?)?;
    let port = integer(options, "port", 22, 1, 65535)?;
    let timeout =
        Duration::from_millis(
            integer(options, "timeout", DEFAULT_TIMEOUT as i64, 1, 86_400_000)? as u64,
        );
    let password = string(options, "password")?;
    let key = string(options, "private-key")?;
    let passphrase = string(options, "passphrase")?;
    if password.is_some() == key.is_some() {
        return Err("supply exactly one of password or private-key".into());
    }
    if passphrase.is_some() && key.is_none() {
        return Err("passphrase requires private-key".into());
    }
    let key = key
        .map(|k| {
            if k.len() > 1024 * 1024 {
                return Err("private-key exceeds 1 MiB".into());
            }
            russh::keys::decode_secret_key(k, passphrase).map_err(|_| {
                "cannot decode private-key (unsupported format or incorrect passphrase)".to_string()
            })
        })
        .transpose()?;
    let policy = DestinationPolicy::from_vm(vm);
    if policy.egress_proxy().is_some() {
        return Err("SSH is unavailable when outbound traffic must use the egress proxy".into());
    }
    let session = run(vm, timeout, &[], async {
        // Include DNS resolution, negotiation and authentication in the same deadline.
        let addrs = policy.resolve_host(host, port as u16).await?;
        let socket = tokio::net::TcpStream::connect(addrs.as_slice())
            .await
            .map_err(|e| e.to_string())?;
        socket.set_nodelay(true).map_err(|e| e.to_string())?;
        let mut config = client::Config {
            inactivity_timeout: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        if let Some(algorithms) = algorithms {
            config.preferred.key = Cow::Owned(algorithms);
        }
        let presented = Arc::new(std::sync::Mutex::new(None));
        let mut session = client::connect_stream(
            Arc::new(config),
            socket,
            Client {
                fingerprint: pin.clone(),
                presented: presented.clone(),
            },
        )
        .await
        .map_err(|e| match presented.lock().ok().and_then(|p| p.clone()) {
            Some(key) => {
                format!("host-key verification failed: server presented {key}, expected {pin}")
            }
            None => format!("SSH negotiation or host-key verification failed: {e}"),
        })?;
        let authenticated = match key {
            Some(key) => {
                let hash = session
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| e.to_string())?
                    .flatten();
                session
                    .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
            }
            None => {
                session
                    .authenticate_password(user, password.expect("validated authentication choice"))
                    .await
            }
        }
        .map_err(|e| format!("authentication failed: {e}"))?;
        if !authenticated.success() {
            return Err("authentication rejected".into());
        }
        Ok(session)
    })?;
    let connection = Arc::new(Connection {
        id: uuid::Uuid::new_v4().to_string(),
        session: Mutex::new(Some(session)),
        control: Control::default(),
        timeout,
    });
    let mut fields = resource_fields(connection.clone());
    for (key, v) in [
        ("host", Val::from(host)),
        ("user", Val::from(user)),
        ("port", Val::Int(port)),
        ("host-key", Val::from(pin)),
    ] {
        fields.insert(Val::from(key), v);
    }
    Ok((connection, Val::constructed(Connection::TYPE, fields)))
}

pub fn connect(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "ssh/connect",
        (|| {
            if args.len() != 1 {
                return Err("expected options".into());
            }
            connect_inner(vm, &args[0]).map(|(_, v)| v)
        })(),
    )
}

async fn subsystem(
    connection: &Connection,
    name: &str,
) -> Result<russh::Channel<client::Msg>, String> {
    let mut session = connection.session.lock().await;
    let session = session.as_mut().ok_or("connection is closed")?;
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .request_subsystem(true, name)
        .await
        .map_err(|e| e.to_string())?;
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Success) => return Ok(channel),
            Some(russh::ChannelMsg::Failure | russh::ChannelMsg::Close) | None => {
                return Err("subsystem request rejected".into());
            }
            _ => {}
        }
    }
}

pub fn open_subsystem(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "ssh/open-subsystem",
        (|| {
            if args.len() != 2 {
                return Err("expected connection and subsystem name".into());
            }
            let connection = extract::<Connection>(&args[0])?;
            let channel = run(
                vm,
                connection.timeout,
                &[&connection.control],
                subsystem(&connection, arg_str(args, 1)?),
            )?;
            let (reader, writer) = tokio::io::split(channel.into_stream());
            Ok(resource_val(Arc::new(Channel {
                id: uuid::Uuid::new_v4().to_string(),
                connection,
                reader: Mutex::new(Some(reader)),
                writer: Mutex::new(Some(writer)),
                control: Control::default(),
            })))
        })(),
    )
}

fn channel_read(vm: &mut VirtualMachine, args: &[Val], exact: bool) -> HotResult<Val> {
    result(
        "ssh/read",
        (|| {
            if args.len() != 2 {
                return Err("expected channel and size".into());
            }
            let channel = extract::<Channel>(&args[0])?;
            let size = bounded_size(args, 1)?;
            let r = run(
                vm,
                channel.connection.timeout,
                &[&channel.control, &channel.connection.control],
                async {
                    let mut guard = channel.reader.lock().await;
                    let reader = guard.as_mut().ok_or("channel is closed")?;
                    let mut bytes = vec![0; size];
                    if exact {
                        reader
                            .read_exact(&mut bytes)
                            .await
                            .map_err(|e| e.to_string())?;
                    } else {
                        let n = reader.read(&mut bytes).await.map_err(|e| e.to_string())?;
                        if n == 0 {
                            return Ok(Val::Null);
                        }
                        bytes.truncate(n);
                    }
                    Ok(Val::bytes(bytes))
                },
            );
            // An interrupted exact read may have consumed part of the frame.
            if exact && r.is_err() {
                channel.control.close();
            }
            r
        })(),
    )
}
pub fn read(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    channel_read(vm, args, false)
}
pub fn read_exact(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    channel_read(vm, args, true)
}

pub fn write(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "ssh/write",
        (|| {
            if args.len() != 2 {
                return Err("expected channel and data".into());
            }
            let channel = extract::<Channel>(&args[0])?;
            let bytes = arg_bytes(args, 1)?;
            let r = run(
                vm,
                channel.connection.timeout,
                &[&channel.control, &channel.connection.control],
                async {
                    let mut guard = channel.writer.lock().await;
                    let writer = guard.as_mut().ok_or("channel is closed")?;
                    writer.write_all(bytes).await.map_err(|e| e.to_string())?;
                    writer.flush().await.map_err(|e| e.to_string())?;
                    Ok(Val::Int(bytes.len() as i64))
                },
            );
            if r.is_err() {
                channel.control.close();
            }
            r
        })(),
    )
}

async fn disconnect(connection: &Connection) -> Result<(), String> {
    connection.control.close();
    if let Some(session) = connection.session.lock().await.take() {
        session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn close(_vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "ssh/close",
        (|| {
            if args.len() != 1 {
                return Err("expected connection or channel".into());
            }
            if let Ok(c) = extract::<Connection>(&args[0]) {
                cleanup(c.timeout, disconnect(&c))?;
            } else {
                let c = extract::<Channel>(&args[0])?;
                c.control.close();
                cleanup(c.connection.timeout, async {
                    c.reader.lock().await.take();
                    if let Some(mut writer) = c.writer.lock().await.take() {
                        writer.shutdown().await.map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })?;
            }
            Ok(Val::Bool(true))
        })(),
    )
}

pub fn is_open(_vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "ssh/is-open",
        (|| {
            if args.len() != 1 {
                return Err("expected connection or channel".into());
            }
            let open = if let Ok(c) = extract::<Connection>(&args[0]) {
                !c.control.closed.load(Ordering::Acquire)
                    && c.session
                        .try_lock()
                        .map(|s| s.as_ref().is_some_and(|s| !s.is_closed()))
                        .unwrap_or(true)
            } else {
                let c = extract::<Channel>(&args[0])?;
                !c.control.closed.load(Ordering::Acquire)
                    && !c.connection.control.closed.load(Ordering::Acquire)
            };
            Ok(Val::Bool(open))
        })(),
    )
}

fn start_sftp(
    vm: &VirtualMachine,
    connection: Arc<Connection>,
    owns_connection: bool,
) -> Result<Val, String> {
    let session = run(vm, connection.timeout, &[&connection.control], async {
        let channel = subsystem(&connection, "sftp").await?;
        SftpSession::new_with_config(
            channel.into_stream(),
            russh_sftp::client::Config {
                request_timeout_secs: connection.timeout.as_secs().max(1),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())
    })?;
    Ok(resource_val(Arc::new(Sftp {
        id: uuid::Uuid::new_v4().to_string(),
        connection,
        session,
        owns_connection,
        control: Control::default(),
    })))
}

pub fn sftp_connect(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "sftp/connect",
        (|| {
            if args.len() != 1 {
                return Err("expected options".into());
            }
            let (connection, _) = connect_inner(vm, &args[0])?;
            start_sftp(vm, connection, true)
        })(),
    )
}

pub fn sftp_start(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "sftp/start",
        (|| {
            if args.len() != 1 {
                return Err("expected SSH connection".into());
            }
            start_sftp(vm, extract(&args[0])?, false)
        })(),
    )
}

fn attributes(a: &FileAttributes) -> Result<Val, String> {
    let kind = match a.permissions.map(|p| p & 0o170000) {
        Some(0o100000) => "file",
        Some(0o040000) => "directory",
        Some(0o120000) => "symlink",
        None => "unknown",
        _ => "other",
    };
    let size = a
        .size
        .map(i64::try_from)
        .transpose()
        .map_err(|_| "file size exceeds Hot Int range")?;
    Ok(record(
        "::sftp/Attributes",
        [
            ("size", size.map(Val::Int).unwrap_or(Val::Null)),
            (
                "uid",
                a.uid.map(|n| Val::Int(n.into())).unwrap_or(Val::Null),
            ),
            (
                "gid",
                a.gid.map(|n| Val::Int(n.into())).unwrap_or(Val::Null),
            ),
            (
                "permissions",
                a.permissions
                    .map(|n| Val::Int(n.into()))
                    .unwrap_or(Val::Null),
            ),
            (
                "accessed",
                a.atime.map(|n| Val::Int(n.into())).unwrap_or(Val::Null),
            ),
            (
                "modified",
                a.mtime.map(|n| Val::Int(n.into())).unwrap_or(Val::Null),
            ),
            ("kind", Val::from(kind)),
        ],
    ))
}

#[derive(Clone, Copy)]
enum SftpOp {
    Stat,
    Lstat,
    List,
    Exists,
    Realpath,
    Readlink,
    Mkdir,
    Rmdir,
    Remove,
    Rename,
    Symlink,
    ReadFile,
    WriteFile,
    Open,
}

fn sftp_op(vm: &mut VirtualMachine, args: &[Val], op: SftpOp) -> HotResult<Val> {
    result(
        "sftp",
        (|| {
            let expected = match op {
                SftpOp::Rename | SftpOp::Symlink | SftpOp::WriteFile | SftpOp::Open => 3,
                _ => 2,
            };
            if args.len() != expected && !(matches!(op, SftpOp::ReadFile) && args.len() == 3) {
                return Err(format!("expected {expected} arguments"));
            }
            let sftp = extract::<Sftp>(&args[0])?;
            let path = arg_str(args, 1)?;
            run(
                vm,
                sftp.connection.timeout,
                &[&sftp.control, &sftp.connection.control],
                async {
                    let s = &sftp.session;
                    match op {
                        SftpOp::Stat => {
                            attributes(&s.metadata(path).await.map_err(|e| e.to_string())?)
                        }
                        SftpOp::Lstat => {
                            attributes(&s.symlink_metadata(path).await.map_err(|e| e.to_string())?)
                        }
                        SftpOp::Exists => Ok(Val::Bool(
                            s.try_exists(path).await.map_err(|e| e.to_string())?,
                        )),
                        SftpOp::Realpath => s
                            .canonicalize(path)
                            .await
                            .map(Val::from)
                            .map_err(|e| e.to_string()),
                        SftpOp::Readlink => s
                            .read_link(path)
                            .await
                            .map(Val::from)
                            .map_err(|e| e.to_string()),
                        SftpOp::List => {
                            let entries = s.read_dir(path).await.map_err(|e| e.to_string())?;
                            let mut out = Vec::new();
                            for e in entries {
                                out.push(record(
                                    "::sftp/Entry",
                                    [
                                        ("name", Val::from(e.file_name())),
                                        ("path", Val::from(e.path())),
                                        ("attributes", attributes(&e.metadata())?),
                                    ],
                                ));
                            }
                            Ok(Val::vec(out))
                        }
                        SftpOp::ReadFile => {
                            let max = if args.len() == 3 {
                                bounded_size(args, 2)?
                            } else {
                                MAX_BYTES as usize
                            };
                            let mut file = s.open(path).await.map_err(|e| e.to_string())?;
                            let mut out = Vec::new();
                            let read = (&mut file).take(max as u64 + 1).read_to_end(&mut out).await;
                            let close = file.close().await;
                            read.map_err(|e| e.to_string())?;
                            close.map_err(|e| e.to_string())?;
                            if out.len() > max {
                                return Err("file exceeds read-file limit; use open/read for chunked access".into());
                            }
                            Ok(Val::bytes(out))
                        }
                        SftpOp::WriteFile => {
                            let data = arg_bytes(args, 2)?;
                            let mut file = s.create(path).await.map_err(|e| e.to_string())?;
                            let write = file.write_all(data).await;
                            let close = file.close().await;
                            write.map_err(|e| e.to_string())?;
                            close.map_err(|e| e.to_string())?;
                            Ok(Val::Int(data.len() as i64))
                        }
                        SftpOp::Open => {
                            let flags = match arg_str(args, 2)? {
                                "read" => OpenFlags::READ,
                                "write" => {
                                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
                                }
                                "append" => {
                                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND
                                }
                                "read-write" => {
                                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE
                                }
                                _ => {
                                    return Err(
                                        "mode must be read, write, append, or read-write".into()
                                    );
                                }
                            };
                            let file = s
                                .open_with_flags(path, flags)
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok(resource_val(Arc::new(RemoteFile {
                                id: uuid::Uuid::new_v4().to_string(),
                                sftp: sftp.clone(),
                                file: Mutex::new(Some(file)),
                                control: Control::default(),
                            })))
                        }
                        op => {
                            match op {
                                SftpOp::Mkdir => s.create_dir(path).await,
                                SftpOp::Rmdir => s.remove_dir(path).await,
                                SftpOp::Remove => s.remove_file(path).await,
                                SftpOp::Rename => s.rename(path, arg_str(args, 2)?).await,
                                // OpenSSH's v3 wire order is target, link-path; russh-sftp
                                // retains the draft's misleading field names internally.
                                SftpOp::Symlink => s.symlink(path, arg_str(args, 2)?).await,
                                _ => unreachable!(),
                            }
                            .map_err(|e| e.to_string())?;
                            Ok(Val::Bool(true))
                        }
                    }
                },
            )
        })(),
    )
}

macro_rules! sftp_ops {
    ($($name:ident => $op:ident),* $(,)?) => {$(
        pub fn $name(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> { sftp_op(vm, args, SftpOp::$op) }
    )*};
}
sftp_ops! {
    sftp_stat => Stat, sftp_lstat => Lstat, sftp_list => List, sftp_exists => Exists,
    sftp_realpath => Realpath, sftp_readlink => Readlink, sftp_mkdir => Mkdir,
    sftp_rmdir => Rmdir, sftp_remove => Remove, sftp_rename => Rename,
    sftp_symlink => Symlink, sftp_read_file => ReadFile, sftp_write_file => WriteFile, sftp_open => Open,
}

#[derive(Clone, Copy)]
enum FileOp {
    Read,
    Write,
    Seek,
}

fn file_op(vm: &mut VirtualMachine, args: &[Val], op: FileOp) -> HotResult<Val> {
    result(
        "sftp/file",
        (|| {
            if args.len() != 2 {
                return Err("expected file and size, data, or offset".into());
            }
            let f = extract::<RemoteFile>(&args[0])?;
            let r = run(
                vm,
                f.sftp.connection.timeout,
                &[&f.control, &f.sftp.control, &f.sftp.connection.control],
                async {
                    let mut guard = f.file.lock().await;
                    let file = guard.as_mut().ok_or("file is closed")?;
                    match op {
                        FileOp::Read => {
                            let mut out = vec![0; bounded_size(args, 1)?];
                            let n = file.read(&mut out).await.map_err(|e| e.to_string())?;
                            if n == 0 {
                                return Ok(Val::Null);
                            }
                            out.truncate(n);
                            Ok(Val::bytes(out))
                        }
                        FileOp::Write => {
                            let data = arg_bytes(args, 1)?;
                            file.write_all(data).await.map_err(|e| e.to_string())?;
                            file.flush().await.map_err(|e| e.to_string())?;
                            Ok(Val::Int(data.len() as i64))
                        }
                        FileOp::Seek => {
                            let offset = match &args[1] {
                                Val::Int(n) if *n >= 0 => *n as u64,
                                _ => return Err("offset must be a nonnegative Int".into()),
                            };
                            let pos = file
                                .seek(std::io::SeekFrom::Start(offset))
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok(Val::Int(
                                i64::try_from(pos).map_err(|_| "offset exceeds Hot Int range")?,
                            ))
                        }
                    }
                },
            );
            // A cancelled write/seek may have advanced the cursor. Don't allow reuse.
            if r.is_err() {
                f.control.close();
            }
            r
        })(),
    )
}

pub fn sftp_read(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    file_op(vm, args, FileOp::Read)
}
pub fn sftp_write(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    file_op(vm, args, FileOp::Write)
}
pub fn sftp_seek(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    file_op(vm, args, FileOp::Seek)
}

pub fn sftp_close(_vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    result(
        "sftp/close",
        (|| {
            if args.len() != 1 {
                return Err("expected session or file".into());
            }
            if let Ok(s) = extract::<Sftp>(&args[0]) {
                s.control.close();
                cleanup(s.connection.timeout, async {
                    let close = s.session.close().await.map_err(|e| e.to_string());
                    if s.owns_connection {
                        disconnect(&s.connection).await?;
                    }
                    close
                })?;
            } else {
                let f = extract::<RemoteFile>(&args[0])?;
                f.control.close();
                cleanup(f.sftp.connection.timeout, async {
                    if let Some(file) = f.file.lock().await.take() {
                        file.close().await.map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })?;
            }
            Ok(Val::Bool(true))
        })(),
    )
}
