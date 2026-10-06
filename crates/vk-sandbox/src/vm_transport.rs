//! Host-to-VM transports (13 §4): how the host reaches the in-VM `vibeke sandbox bridge`
//! (egress, per-pane brokers, preview ports). Preference is vsock, then virtio-serial, then the
//! provider's exec channel, then SSH; `transport = "auto"` walks that chain and records why each
//! step failed, an explicit transport never silently downgrades.
//!
//! The byte stream a [`Dialer`] returns is the same stream the container box link multiplexes
//! (`vk_remote::boxlink`); this module only gets the stream. Dialers here:
//! - [`ExecDialer`]: spawn the provider's exec command running the bridge and use its stdio;
//! - [`VsockDialer`]: `AF_VSOCK` connect (Linux hosts; other hosts report it unsupported);
//! - [`FakeDialer`]: an in-process "VM agent" over a socket pair, for tests.
//!
//! Virtio-serial and SSH have no dialer yet (the providers do not expose either to Vibeke), so
//! they are chain members that report unsupported.

use crate::vm::{VmBackend, VmTransportKind};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Best transport first.
pub const PREFERENCE: &[VmTransportKind] = &[
    VmTransportKind::Vsock,
    VmTransportKind::VirtioSerial,
    VmTransportKind::Exec,
    VmTransportKind::Ssh,
];

pub fn parse(s: &str) -> Result<Option<VmTransportKind>, String> {
    Ok(match s {
        "auto" | "" => None,
        "vsock" => Some(VmTransportKind::Vsock),
        "virtio-serial" | "virtio_serial" => Some(VmTransportKind::VirtioSerial),
        "exec" => Some(VmTransportKind::Exec),
        "ssh" => Some(VmTransportKind::Ssh),
        o => {
            return Err(format!(
                "unknown vm transport `{o}` (auto, vsock, virtio-serial, exec, ssh)"
            ));
        }
    })
}

/// The transports to try, in order: `offered` by the VM, restricted to `wanted` when explicit.
pub fn plan(
    offered: &[VmTransportKind],
    wanted: Option<VmTransportKind>,
) -> Result<Vec<VmTransportKind>, String> {
    match wanted {
        Some(w) if offered.contains(&w) => Ok(vec![w]),
        Some(w) => Err(format!(
            "this VM does not offer the {} transport (it offers: {})",
            w.as_str(),
            if offered.is_empty() {
                "none".to_string()
            } else {
                offered
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )),
        None => {
            let v: Vec<VmTransportKind> = PREFERENCE
                .iter()
                .copied()
                .filter(|k| offered.contains(k))
                .collect();
            if v.is_empty() {
                Err("this VM offers no transport".into())
            } else {
                Ok(v)
            }
        }
    }
}

pub trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

pub trait Dialer: Send + Sync {
    fn kind(&self) -> VmTransportKind;
    fn dial(&self, vm: &str) -> io::Result<Box<dyn Duplex>>;
}

/// Why a step of the chain was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub kind: VmTransportKind,
    pub error: String,
}

/// A connected link: the transport that won, the stream, and the steps that failed first.
pub type Connected = (VmTransportKind, Box<dyn Duplex>, Vec<Attempt>);

/// Dial `vm` along `chain` (see [`plan`]); the first dialer that connects wins. The attempts
/// that failed come back with the connection, or all of them as the error text.
pub fn connect(
    vm: &str,
    chain: &[VmTransportKind],
    dialers: &[&dyn Dialer],
) -> Result<Connected, String> {
    let mut failed = vec![];
    for k in chain {
        let Some(d) = dialers.iter().find(|d| d.kind() == *k) else {
            failed.push(Attempt {
                kind: *k,
                error: "no dialer for this transport".into(),
            });
            continue;
        };
        match d.dial(vm) {
            Ok(s) => return Ok((*k, s, failed)),
            Err(e) => failed.push(Attempt {
                kind: *k,
                error: e.to_string(),
            }),
        }
    }
    Err(failed
        .iter()
        .map(|a| format!("{}: {}", a.kind.as_str(), a.error))
        .collect::<Vec<_>>()
        .join("; "))
}

/// The provider's exec channel running the in-VM bridge: stdin/stdout of the spawned command.
pub struct ExecDialer {
    pub backend: Arc<dyn VmBackend>,
    /// The command run inside the VM (`/vibeke/bin/vibeke sandbox bridge`).
    pub bridge: Vec<String>,
}

struct ChildDuplex {
    child: Child,
}

impl Read for ChildDuplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.child
            .stdout
            .as_mut()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .read(buf)
    }
}
impl Write for ChildDuplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.child
            .stdin
            .as_mut()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.child
            .stdin
            .as_mut()
            .map(|s| s.flush())
            .unwrap_or(Ok(()))
    }
}
impl Drop for ChildDuplex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Dialer for ExecDialer {
    fn kind(&self) -> VmTransportKind {
        VmTransportKind::Exec
    }
    fn dial(&self, vm: &str) -> io::Result<Box<dyn Duplex>> {
        let argv = self.backend.exec_argv(vm, &self.bridge, None, &[]);
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Box::new(ChildDuplex { child }))
    }
}

/// `AF_VSOCK` (Linux host, guest CID and port).
pub struct VsockDialer {
    pub cid: u32,
    pub port: u32,
}

impl Dialer for VsockDialer {
    fn kind(&self) -> VmTransportKind {
        VmTransportKind::Vsock
    }
    #[cfg(target_os = "linux")]
    fn dial(&self, _vm: &str) -> io::Result<Box<dyn Duplex>> {
        use std::os::fd::{FromRawFd, OwnedFd};
        // SAFETY: plain socket/connect calls with a fully initialised sockaddr_vm; the fd is
        // owned by the returned `OwnedFd` on success and closed on error.
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let owned = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_cid = self.cid;
            addr.svm_port = self.port;
            let r = libc::connect(
                fd,
                (&addr as *const libc::sockaddr_vm).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Box::new(std::fs::File::from(owned)))
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn dial(&self, _vm: &str) -> io::Result<Box<dyn Duplex>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AF_VSOCK from the host needs Linux",
        ))
    }
}

/// A transport that is a chain member without an implementation.
pub struct Unsupported(pub VmTransportKind, pub &'static str);
impl Dialer for Unsupported {
    fn kind(&self) -> VmTransportKind {
        self.0
    }
    fn dial(&self, _vm: &str) -> io::Result<Box<dyn Duplex>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, self.1))
    }
}

/// The in-process "VM agent": for every dial it runs `agent` on the guest end of a socket pair
/// in a thread. Failures can be injected.
pub struct FakeDialer {
    pub kind: VmTransportKind,
    agent: Arc<dyn Fn(UnixStream) + Send + Sync>,
    fail: Mutex<u32>,
    pub dials: Mutex<Vec<String>>,
}

impl FakeDialer {
    pub fn new(
        kind: VmTransportKind,
        agent: impl Fn(UnixStream) + Send + Sync + 'static,
    ) -> FakeDialer {
        FakeDialer {
            kind,
            agent: Arc::new(agent),
            fail: Mutex::new(0),
            dials: Mutex::new(vec![]),
        }
    }
    /// Refuse the next `n` dials.
    pub fn fail_next(&self, n: u32) {
        *self.fail.lock().unwrap() = n;
    }
}

impl Dialer for FakeDialer {
    fn kind(&self) -> VmTransportKind {
        self.kind
    }
    fn dial(&self, vm: &str) -> io::Result<Box<dyn Duplex>> {
        self.dials.lock().unwrap().push(vm.to_string());
        {
            let mut f = self.fail.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "agent not listening",
                ));
            }
        }
        let (host, guest) = UnixStream::pair()?;
        let agent = self.agent.clone();
        std::thread::spawn(move || agent(guest));
        Ok(Box::new(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{FakeVmBackend, VmSpec};

    fn echo_agent(mut s: UnixStream) {
        let mut buf = [0u8; 256];
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut out = b"echo:".to_vec();
                    out.extend_from_slice(&buf[..n]);
                    if s.write_all(&out).is_err() {
                        return;
                    }
                }
            }
        }
    }

    #[test]
    fn parsing_and_planning() {
        assert_eq!(parse("auto").unwrap(), None);
        assert_eq!(parse("vsock").unwrap(), Some(VmTransportKind::Vsock));
        assert_eq!(
            parse("virtio_serial").unwrap(),
            Some(VmTransportKind::VirtioSerial)
        );
        assert!(parse("carrier-pigeon").is_err());
        use VmTransportKind::*;
        // auto walks the preference order, not the order the VM listed them in
        assert_eq!(
            plan(&[Ssh, Exec, Vsock], None).unwrap(),
            vec![Vsock, Exec, Ssh]
        );
        assert_eq!(plan(&[Exec], None).unwrap(), vec![Exec]);
        assert!(plan(&[], None).is_err());
        // explicit never downgrades
        assert_eq!(plan(&[Vsock, Exec], Some(Exec)).unwrap(), vec![Exec]);
        let e = plan(&[Exec], Some(Vsock)).unwrap_err();
        assert!(
            e.contains("does not offer the vsock transport") && e.contains("exec"),
            "{e}"
        );
    }

    #[test]
    fn a_dialed_stream_reaches_the_agent() {
        let d = FakeDialer::new(VmTransportKind::Vsock, echo_agent);
        let (k, mut s, failed) = connect("vm1", &[VmTransportKind::Vsock], &[&d]).unwrap();
        assert_eq!(k, VmTransportKind::Vsock);
        assert!(failed.is_empty());
        s.write_all(b"hello").unwrap();
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"echo:hello");
        assert_eq!(d.dials.lock().unwrap().as_slice(), ["vm1"]);
    }

    #[test]
    fn auto_falls_back_down_the_chain_and_reports_why() {
        let vsock = FakeDialer::new(VmTransportKind::Vsock, echo_agent);
        vsock.fail_next(1);
        let exec = FakeDialer::new(VmTransportKind::Exec, echo_agent);
        let serial = Unsupported(
            VmTransportKind::VirtioSerial,
            "no virtio-serial channel is exposed",
        );
        let chain = plan(
            &[
                VmTransportKind::Exec,
                VmTransportKind::VirtioSerial,
                VmTransportKind::Vsock,
            ],
            None,
        )
        .unwrap();
        let (k, _s, failed) = connect("vm1", &chain, &[&vsock, &serial, &exec]).unwrap();
        assert_eq!(k, VmTransportKind::Exec);
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].kind, VmTransportKind::Vsock);
        assert!(failed[0].error.contains("agent not listening"));
        assert_eq!(failed[1].kind, VmTransportKind::VirtioSerial);
        // Everything failing gives one readable error.
        vsock.fail_next(1);
        exec.fail_next(1);
        let e = connect("vm1", &chain, &[&vsock, &serial, &exec])
            .err()
            .unwrap();
        assert!(e.contains("vsock: agent not listening"), "{e}");
        assert!(e.contains("exec: agent not listening"), "{e}");
        assert!(e.contains("virtio-serial: no virtio-serial channel"), "{e}");
        // A chain member without a dialer is reported, not skipped silently.
        let e = connect("vm1", &[VmTransportKind::Ssh], &[&exec])
            .err()
            .unwrap();
        assert!(e.contains("ssh: no dialer"), "{e}");
    }

    #[test]
    fn exec_dialer_runs_the_bridge_through_the_backend() {
        let d = tempfile::tempdir().unwrap();
        let b = Arc::new(FakeVmBackend::new(d.path()));
        b.create(&VmSpec::new("vm1")).unwrap();
        b.start("vm1").unwrap();
        // `cat` stands in for the in-VM bridge: it echoes what it is sent.
        let ex = ExecDialer {
            backend: b.clone(),
            bridge: vec!["cat".into()],
        };
        assert_eq!(ex.kind(), VmTransportKind::Exec);
        let (k, mut s, _) = connect("vm1", &[VmTransportKind::Exec], &[&ex]).unwrap();
        assert_eq!(k, VmTransportKind::Exec);
        s.write_all(b"ping\n").unwrap();
        s.flush().unwrap();
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping\n");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn vsock_is_unsupported_off_linux() {
        let v = VsockDialer { cid: 3, port: 1024 };
        assert_eq!(v.kind(), VmTransportKind::Vsock);
        assert!(v.dial("x").is_err());
    }
}
