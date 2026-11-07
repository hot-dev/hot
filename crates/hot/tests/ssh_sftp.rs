use hot::lang::hot::internal_ssh::*;
use hot::lang::hot::r#type::HotResult;
use hot::lang::runtime::vm::VirtualMachine;
use hot::val::Val;
use russh::keys::HashAlg;
use russh::server::{self, Auth, Session, VerifiedPublicKey};
use russh_sftp::protocol::{Attrs, Data, Handle as FileHandle, Name, Status, StatusCode};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn field<'a>(value: &'a Val, key: &str) -> Option<&'a Val> {
    if let Val::Map(m) = value {
        m.get(&Val::from(key))
            .or_else(|| m.get(&Val::from("$val")).and_then(|v| field(v, key)))
    } else {
        None
    }
}

fn new_vm(conf: Val) -> VirtualMachine {
    VirtualMachine::new(
        Arc::new(Default::default()),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Some(conf),
    )
}

fn ok(result: HotResult<Val>) -> Val {
    match result {
        HotResult::Ok(v) => v,
        HotResult::Err(e) => panic!("unexpected error: {e:?}"),
    }
}

fn error(result: HotResult<Val>) -> String {
    match result {
        HotResult::Err(e) => format!("{e:?}"),
        HotResult::Ok(v) => panic!("unexpected success: {v:?}"),
    }
}

#[derive(Default)]
struct Fs {
    files: HashMap<String, Vec<u8>>,
    dirs: HashSet<String>,
    open: usize,
}

struct TestSsh {
    channels: HashMap<russh::ChannelId, russh::Channel<server::Msg>>,
    fs: Arc<parking_lot::Mutex<Fs>>,
}

impl server::Handler for TestSsh {
    type Error = russh::Error;
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "test" && password == "secret" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        _key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(if user == "test" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: russh::ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let channel = self.channels.remove(&id).unwrap();
        match name {
            "sftp" => {
                session.channel_success(id)?;
                let fs = TestSftp {
                    fs: self.fs.clone(),
                    handles: HashMap::new(),
                    listed: false,
                    next: 0,
                };
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), fs));
            }
            "echo" => {
                session.channel_success(id)?;
                tokio::spawn(async move {
                    let (mut reader, mut writer) = tokio::io::split(channel.into_stream());
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                });
            }
            _ => {
                session.channel_failure(id)?;
            }
        }
        Ok(())
    }
}

struct TestSftp {
    fs: Arc<parking_lot::Mutex<Fs>>,
    handles: HashMap<String, String>,
    listed: bool,
    next: usize,
}

fn status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}

impl TestSftp {
    fn attrs(&self, path: &str) -> Result<FileAttributes, StatusCode> {
        if path == "/denied" {
            return Err(StatusCode::PermissionDenied);
        }
        let fs = self.fs.lock();
        if let Some(data) = fs.files.get(path) {
            Ok(FileAttributes {
                size: Some(data.len() as u64),
                permissions: Some(0o100600),
                ..Default::default()
            })
        } else if fs.dirs.contains(path) || path == "/" {
            Ok(FileAttributes {
                permissions: Some(0o040700),
                ..Default::default()
            })
        } else {
            Err(StatusCode::NoSuchFile)
        }
    }
}

impl russh_sftp::server::Handler for TestSftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<FileHandle, StatusCode> {
        let mut fs = self.fs.lock();
        if !fs.files.contains_key(&filename) && !flags.contains(OpenFlags::CREATE) {
            return Err(StatusCode::NoSuchFile);
        }
        let data = fs.files.entry(filename.clone()).or_default();
        if flags.contains(OpenFlags::TRUNCATE) {
            data.clear();
        }
        self.next += 1;
        let handle = format!("file-{}", self.next);
        self.handles.insert(handle.clone(), filename);
        fs.open += 1;
        Ok(FileHandle { id, handle })
    }
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        if self.handles.remove(&handle).is_some() {
            self.fs.lock().open -= 1;
        }
        Ok(status(id))
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        let fs = self.fs.lock();
        let data = fs
            .files
            .get(self.handles.get(&handle).ok_or(StatusCode::Failure)?)
            .ok_or(StatusCode::NoSuchFile)?;
        if offset as usize >= data.len() {
            return Err(StatusCode::Eof);
        }
        let end = (offset as usize + len as usize).min(data.len());
        Ok(Data {
            id,
            data: data[offset as usize..end].to_vec(),
        })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let mut fs = self.fs.lock();
        let out = fs
            .files
            .get_mut(self.handles.get(&handle).ok_or(StatusCode::Failure)?)
            .ok_or(StatusCode::NoSuchFile)?;
        let end = offset as usize + data.len();
        if out.len() < end {
            out.resize(end, 0);
        }
        out[offset as usize..end].copy_from_slice(&data);
        Ok(status(id))
    }
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        Ok(Attrs {
            id,
            attrs: self.attrs(&path)?,
        })
    }
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        self.stat(id, path).await
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        Ok(Attrs {
            id,
            attrs: self.attrs(self.handles.get(&handle).ok_or(StatusCode::Failure)?)?,
        })
    }
    async fn rename(&mut self, id: u32, old: String, new: String) -> Result<Status, StatusCode> {
        let mut fs = self.fs.lock();
        let data = fs.files.remove(&old).ok_or(StatusCode::NoSuchFile)?;
        fs.files.insert(new, data);
        Ok(status(id))
    }
    async fn remove(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        self.fs
            .lock()
            .files
            .remove(&path)
            .ok_or(StatusCode::NoSuchFile)?;
        Ok(status(id))
    }
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        self.fs.lock().dirs.insert(path);
        Ok(status(id))
    }
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        self.fs.lock().dirs.remove(&path);
        Ok(status(id))
    }
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![russh_sftp::protocol::File::dummy(path)],
        })
    }
    async fn opendir(&mut self, id: u32, path: String) -> Result<FileHandle, StatusCode> {
        self.listed = false;
        Ok(FileHandle { id, handle: path })
    }
    async fn readdir(&mut self, id: u32, _handle: String) -> Result<Name, StatusCode> {
        if self.listed {
            return Err(StatusCode::Eof);
        }
        self.listed = true;
        let files = self
            .fs
            .lock()
            .files
            .iter()
            .map(|(path, data)| {
                russh_sftp::protocol::File::new(
                    path.trim_start_matches('/'),
                    FileAttributes {
                        size: Some(data.len() as u64),
                        permissions: Some(0o100600),
                        ..Default::default()
                    },
                )
            })
            .collect();
        Ok(Name { id, files })
    }
}

async fn server() -> (
    u16,
    String,
    Arc<parking_lot::Mutex<Fs>>,
    tokio::task::JoinHandle<()>,
) {
    let key =
        russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
    let pin = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let fs = Arc::new(parking_lot::Mutex::new(Fs::default()));
    let files = fs.clone();
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let config = config.clone();
            let handler = TestSsh {
                channels: HashMap::new(),
                fs: files.clone(),
            };
            tokio::spawn(async move {
                if let Ok(session) = server::run_stream(config, socket, handler).await {
                    let _ = session.await;
                }
            });
        }
    });
    (port, pin, fs, task)
}

fn options(port: u16, pin: &str) -> Val {
    hot::val!({"host": "127.0.0.1", "port": i64::from(port), "user": "test", "password": "secret", "host-key": pin, "timeout": 5000})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_policy_and_channels() {
    let (port, pin, _, server) = server().await;
    tokio::task::spawn_blocking(move || {
        let mut vm = new_vm(Val::map_empty());
        let opts = options(port, &pin);
        let conn = ok(connect(&mut vm, std::slice::from_ref(&opts)));
        assert!(conn.is_type("::ssh/Connection"));
        assert_eq!(
            ok(is_open(&mut vm, std::slice::from_ref(&conn))),
            Val::Bool(true)
        );
        assert!(
            error(open_subsystem(
                &mut vm,
                &[conn.clone(), Val::from("missing")]
            ))
            .contains("rejected")
        );
        let channel = ok(open_subsystem(&mut vm, &[conn.clone(), Val::from("echo")]));
        assert!(channel.is_type("::ssh/Channel"));
        let data = Val::bytes(vec![0, 255, 42, 0]);
        assert_eq!(
            ok(write(&mut vm, &[channel.clone(), data.clone()])),
            Val::Int(4)
        );
        assert_eq!(
            ok(read_exact(&mut vm, &[channel.clone(), Val::Int(4)])),
            data
        );
        ok(close(&mut vm, std::slice::from_ref(&channel)));
        ok(close(&mut vm, std::slice::from_ref(&channel)));
        assert!(error(write(&mut vm, &[channel, Val::from("no")])).contains("closed"));
        ok(close(&mut vm, std::slice::from_ref(&conn)));
        ok(close(&mut vm, &[conn]));

        let mut wrong = opts.clone();
        if let Val::Map(m) = &mut wrong {
            Arc::make_mut(m).insert(Val::from("password"), Val::from("wrong"));
        }
        assert!(error(connect(&mut vm, &[wrong])).contains("authentication rejected"));
        let mut wrong = opts.clone();
        if let Val::Map(m) = &mut wrong {
            Arc::make_mut(m).insert(
                Val::from("host-key"),
                Val::from("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            );
        }
        assert!(error(connect(&mut vm, &[wrong])).contains("host-key"));

        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let mut key_opts = opts.clone();
        if let Val::Map(m) = &mut key_opts {
            let m = Arc::make_mut(m);
            m.shift_remove(&Val::from("password"));
            m.insert(
                Val::from("private-key"),
                Val::from(
                    key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                        .unwrap()
                        .to_string(),
                ),
            );
        }
        let conn = ok(connect(&mut vm, std::slice::from_ref(&key_opts)));
        ok(close(&mut vm, &[conn]));

        let encrypted = key.encrypt(&mut rand::rng(), "test-passphrase").unwrap();
        if let Val::Map(m) = &mut key_opts {
            let m = Arc::make_mut(m);
            m.insert(
                Val::from("private-key"),
                Val::from(
                    encrypted
                        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                        .unwrap()
                        .to_string(),
                ),
            );
            m.insert(Val::from("passphrase"), Val::from("test-passphrase"));
        }
        let conn = ok(connect(&mut vm, &[key_opts]));
        ok(close(&mut vm, &[conn]));

        let mut isolated = new_vm(hot::val!({"engine": {"isolation": true}}));
        assert!(error(connect(&mut isolated, std::slice::from_ref(&opts))).contains("blocked"));
        let mut proxied =
            new_vm(hot::val!({"network": {"egress-proxy": "http://proxy.invalid:3128"}}));
        assert!(error(connect(&mut proxied, std::slice::from_ref(&opts))).contains("egress proxy"));
        let conn = ok(connect(&mut vm, std::slice::from_ref(&opts)));
        vm.set_external_cancel(Arc::new(AtomicBool::new(true)));
        assert!(error(connect(&mut vm, &[opts])).contains("cancelled"));
        ok(close(&mut vm, &[conn]));
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sftp_binary_transfers_files_and_lifecycle() {
    let (port, pin, fs, server) = server().await;
    tokio::task::spawn_blocking(move || {
        let mut vm = new_vm(Val::map_empty());
        let opts = options(port, &pin);
        let s = ok(sftp_connect(&mut vm, std::slice::from_ref(&opts)));
        assert!(s.is_type("::sftp/Session"));
        let path = Val::from("/data.bin");
        let bytes = Val::bytes((0..200_000).map(|i| (i % 256) as u8).collect());
        assert_eq!(
            ok(sftp_write_file(
                &mut vm,
                &[s.clone(), path.clone(), bytes.clone()]
            )),
            Val::Int(200_000)
        );
        assert_eq!(
            ok(sftp_read_file(&mut vm, &[s.clone(), path.clone()])),
            bytes
        );
        assert!(
            error(sftp_read_file(
                &mut vm,
                &[s.clone(), path.clone(), Val::Int(10)]
            ))
            .contains("limit")
        );
        assert_eq!(
            fs.lock().open,
            0,
            "whole-file operations close handles on success and size errors"
        );
        let attrs = ok(sftp_stat(&mut vm, &[s.clone(), path.clone()]));
        assert!(attrs.is_type("::sftp/Attributes"));
        assert_eq!(field(&attrs, "size"), Some(&Val::Int(200_000)));
        assert_eq!(field(&attrs, "kind"), Some(&Val::from("file")));
        let f = ok(sftp_open(
            &mut vm,
            &[s.clone(), path.clone(), Val::from("read")],
        ));
        assert!(f.is_type("::sftp/File"));
        assert_eq!(
            ok(sftp_seek(&mut vm, &[f.clone(), Val::Int(199_998)])),
            Val::Int(199_998)
        );
        assert_eq!(
            ok(sftp_read(&mut vm, &[f.clone(), Val::Int(32)])),
            Val::bytes(vec![(199_998 % 256) as u8, (199_999 % 256) as u8])
        );
        assert_eq!(
            ok(sftp_read(&mut vm, &[f.clone(), Val::Int(32)])),
            Val::Null
        );
        ok(sftp_close(&mut vm, std::slice::from_ref(&f)));
        ok(sftp_close(&mut vm, std::slice::from_ref(&f)));
        assert!(error(sftp_read(&mut vm, &[f, Val::Int(1)])).contains("closed"));
        assert_eq!(
            ok(sftp_exists(&mut vm, &[s.clone(), Val::from("/missing")])),
            Val::Bool(false)
        );
        assert!(
            error(sftp_exists(&mut vm, &[s.clone(), Val::from("/denied")])).contains("Permission")
        );
        let entries = ok(sftp_list(&mut vm, &[s.clone(), Val::from("/")]));
        assert!(matches!(entries, Val::Vec(v) if v.len() == 1 && v[0].is_type("::sftp/Entry")));
        ok(sftp_mkdir(&mut vm, &[s.clone(), Val::from("/dir")]));
        ok(sftp_rename(
            &mut vm,
            &[s.clone(), path, Val::from("/dir/moved")],
        ));
        ok(sftp_remove(&mut vm, &[s.clone(), Val::from("/dir/moved")]));
        ok(sftp_rmdir(&mut vm, &[s.clone(), Val::from("/dir")]));
        ok(sftp_close(&mut vm, std::slice::from_ref(&s)));
        ok(sftp_close(&mut vm, std::slice::from_ref(&s)));
        assert!(error(sftp_stat(&mut vm, &[s, Val::from("/")])).contains("closed"));

        let conn = ok(connect(&mut vm, &[opts]));
        let shared = ok(sftp_start(&mut vm, std::slice::from_ref(&conn)));
        ok(sftp_close(&mut vm, &[shared]));
        assert_eq!(
            ok(is_open(&mut vm, std::slice::from_ref(&conn))),
            Val::Bool(true)
        );
        ok(close(&mut vm, &[conn]));
    })
    .await
    .unwrap();
    server.abort();
}

#[test]
fn invalid_arguments_and_metadata_are_rejected() {
    let mut vm = new_vm(Val::map_empty());
    assert!(error(connect(&mut vm, &[])).contains("options"));
    assert!(
        error(connect(
            &mut vm,
            &[hot::val!({"host": "host", "user": "user"})]
        ))
        .contains("host-key")
    );
    assert!(error(sftp_start(&mut vm, &[hot::val!({"id": "forged"})])).contains("live"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshakes_and_inflight_cancellation_are_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stalled = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(socket);
    });
    tokio::task::spawn_blocking(move || {
        let mut vm = new_vm(Val::map_empty());
        let mut opts = options(port, "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        if let Val::Map(m) = &mut opts {
            Arc::make_mut(m).insert(Val::from("timeout"), Val::Int(100));
        }
        let start = std::time::Instant::now();
        assert!(error(connect(&mut vm, &[opts])).contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    })
    .await
    .unwrap();
    stalled.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inflight_reads_are_cancelled_and_close_still_works() {
    let (port, pin, _, server) = server().await;
    tokio::task::spawn_blocking(move || {
        let mut vm = new_vm(Val::map_empty());
        let connection = ok(connect(&mut vm, &[options(port, &pin)]));
        let channel = ok(open_subsystem(
            &mut vm,
            &[connection.clone(), Val::from("echo")],
        ));
        let token = Arc::new(AtomicBool::new(false));
        vm.set_external_cancel(token.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.store(true, Ordering::Relaxed);
        });
        assert!(error(read_exact(&mut vm, &[channel.clone(), Val::Int(1)])).contains("cancelled"));
        assert_eq!(
            ok(is_open(&mut vm, std::slice::from_ref(&channel))),
            Val::Bool(false)
        );
        ok(close(&mut vm, &[channel]));
        ok(close(&mut vm, &[connection]));
    })
    .await
    .unwrap();
    server.abort();
}
