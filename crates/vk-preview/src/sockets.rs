//! TCP socket ownership (06 B2 listener discovery, B3.4 SOCKS peer check).
//!
//! macOS: libproc `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`.
//! Linux: `/proc/<pid>/fd` socket inodes joined with `/proc/net/tcp{,6}`.
//! Both only look at the pids they are given, so the cost is bounded by the process tree.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A TCP socket in LISTEN state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Listener {
    pub pid: u32,
    pub port: u16,
    pub addr: IpAddr,
}

/// Loopback or wildcard binds are reachable through `127.0.0.1`/`::1` (06 B2); a bind to a
/// specific LAN address is not a local dev server we can route to.
pub fn is_local_bind(addr: &IpAddr) -> bool {
    addr.is_loopback() || addr.is_unspecified()
}

/// LISTEN sockets owned by `pids` (all addresses; filter with [`is_local_bind`]).
pub fn listeners(pids: &[u32]) -> Vec<Listener> {
    let mut v = imp::listeners(pids);
    v.sort_by_key(|l| (l.port, l.pid));
    v.dedup();
    v
}

/// Which of `pids` owns the client end of a loopback TCP connection whose local port is
/// `client_port` and remote port is `server_port` (the SOCKS peer check, 06 B3.4).
pub fn owner_of_connection(pids: &[u32], client_port: u16, server_port: u16) -> Option<u32> {
    imp::owner_of_connection(pids, client_port, server_port)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    const SOCKINFO_TCP: i32 = 2;
    const TSI_S_LISTEN: i32 = 1;
    const INI_IPV4: u8 = 0x1;
    const INI_IPV6: u8 = 0x2;

    // Layouts from xnu `bsd/sys/proc_info.h` (stable ABI used by lsof).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VinfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: u32,
        vst_gid: u32,
        vst_times: [i64; 8],
        vst_size: i64,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SockbufInfo {
        sbi_cc: u32,
        sbi_hiwat: u32,
        sbi_mbcnt: u32,
        sbi_mbmax: u32,
        sbi_lowat: u32,
        sbi_flags: i16,
        sbi_timeo: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct In6Ext {
        in6_hlim: u8,
        in6_cksum: i32,
        in6_ifindex: u16,
        in6_hops: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InSockInfo {
        insi_fport: i32,
        insi_lport: i32,
        insi_gencnt: u64,
        insi_flags: u32,
        insi_flow: u32,
        insi_vflag: u8,
        insi_ip_ttl: u8,
        rfu_1: u32,
        insi_faddr: [u32; 4],
        insi_laddr: [u32; 4],
        insi_v4: u8,
        insi_v6: In6Ext,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TcpSockInfo {
        tcpsi_ini: InSockInfo,
        tcpsi_state: i32,
        tcpsi_timer: [i32; 4],
        tcpsi_mss: i32,
        tcpsi_flags: u32,
        rfu_1: u32,
        tcpsi_tp: u64,
    }

    /// The kernel union is 528 bytes (`un_sockinfo`); a larger buffer is accepted.
    #[repr(C)]
    #[derive(Clone, Copy)]
    union ProtoUnion {
        tcp: TcpSockInfo,
        pad: [u64; 128],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SocketInfo {
        soi_stat: VinfoStat,
        soi_so: u64,
        soi_pcb: u64,
        soi_type: i32,
        soi_protocol: i32,
        soi_family: i32,
        soi_options: i16,
        soi_linger: i16,
        soi_state: i16,
        soi_qlen: i16,
        soi_incqlen: i16,
        soi_qlimit: i16,
        soi_timeo: i16,
        soi_error: u16,
        soi_oobmark: u32,
        soi_rcv: SockbufInfo,
        soi_snd: SockbufInfo,
        soi_kind: i32,
        rfu_1: u32,
        soi_proto: ProtoUnion,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SocketFdInfo {
        pfi: ProcFileInfo,
        psi: SocketInfo,
    }

    const _: () = assert!(std::mem::size_of::<InSockInfo>() == 80);
    const _: () = assert!(std::mem::size_of::<TcpSockInfo>() == 120);
    const _: () = assert!(std::mem::size_of::<VinfoStat>() == 136);
    const _: () = assert!(std::mem::offset_of!(SocketInfo, soi_proto) == 240);

    struct TcpSock {
        state: i32,
        lport: u16,
        fport: u16,
        laddr: IpAddr,
    }

    fn socket_fds(pid: u32) -> Vec<i32> {
        // SAFETY: a null buffer asks for the required size.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        if n <= 0 {
            return vec![];
        }
        let each = std::mem::size_of::<libc::proc_fdinfo>();
        // Room for fds opened between the two calls.
        let cap = n as usize / each + 32;
        let mut buf: Vec<libc::proc_fdinfo> = Vec::with_capacity(cap);
        // SAFETY: buffer of `cap` proc_fdinfo entries; the kernel writes at most that many bytes.
        let got = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDLISTFDS,
                0,
                buf.as_mut_ptr().cast(),
                (cap * each) as libc::c_int,
            )
        };
        if got <= 0 {
            return vec![];
        }
        // SAFETY: the kernel initialised `got` bytes of whole entries.
        unsafe { buf.set_len((got as usize / each).min(cap)) };
        buf.into_iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
            .map(|f| f.proc_fd)
            .collect()
    }

    fn tcp_sock(pid: u32, fd: i32) -> Option<TcpSock> {
        // SAFETY: plain-old-data out-buffer, zero is a valid bit pattern.
        let mut info: SocketFdInfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<SocketFdInfo>() as libc::c_int;
        // SAFETY: `info` is `size` bytes; the kernel writes at most sizeof(socket_fdinfo).
        let r = unsafe {
            libc::proc_pidfdinfo(
                pid as libc::c_int,
                fd,
                PROC_PIDFDSOCKETINFO,
                (&mut info as *mut SocketFdInfo).cast(),
                size,
            )
        };
        if r <= 0 || info.psi.soi_kind != SOCKINFO_TCP {
            return None;
        }
        // SAFETY: soi_kind == SOCKINFO_TCP selects the tcp member.
        let tcp = unsafe { info.psi.soi_proto.tcp };
        let ini = tcp.tcpsi_ini;
        let bytes: [u8; 16] = {
            let mut b = [0u8; 16];
            for (i, w) in ini.insi_laddr.iter().enumerate() {
                b[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
            }
            b
        };
        let laddr = if ini.insi_vflag & INI_IPV4 != 0 && ini.insi_vflag & INI_IPV6 == 0 {
            IpAddr::V4(Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]))
        } else {
            let v6 = Ipv6Addr::from(bytes);
            v6.to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(v6))
        };
        Some(TcpSock {
            state: tcp.tcpsi_state,
            lport: u16::from_be(ini.insi_lport as u16),
            fport: u16::from_be(ini.insi_fport as u16),
            laddr,
        })
    }

    pub fn listeners(pids: &[u32]) -> Vec<Listener> {
        let mut out = Vec::new();
        for &pid in pids {
            for fd in socket_fds(pid) {
                if let Some(s) = tcp_sock(pid, fd)
                    && s.state == TSI_S_LISTEN
                {
                    out.push(Listener {
                        pid,
                        port: s.lport,
                        addr: s.laddr,
                    });
                }
            }
        }
        out
    }

    pub fn owner_of_connection(pids: &[u32], client_port: u16, server_port: u16) -> Option<u32> {
        for &pid in pids {
            for fd in socket_fds(pid) {
                if let Some(s) = tcp_sock(pid, fd)
                    && s.state != TSI_S_LISTEN
                    && s.lport == client_port
                    && s.fport == server_port
                    && s.laddr.is_loopback()
                {
                    return Some(pid);
                }
            }
        }
        None
    }
}

/// One row of `/proc/net/tcp{,6}`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TcpRow {
    pub local: (IpAddr, u16),
    pub remote: (IpAddr, u16),
    pub state: u8,
    pub inode: u64,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const TCP_LISTEN: u8 = 0x0A;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_addr(s: &str) -> Option<(IpAddr, u16)> {
    let (a, p) = s.split_once(':')?;
    let port = u16::from_str_radix(p, 16).ok()?;
    let ip = match a.len() {
        8 => {
            let w = u32::from_str_radix(a, 16).ok()?;
            IpAddr::V4(Ipv4Addr::from(w.to_ne_bytes()))
        }
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                let w = u32::from_str_radix(&a[i * 8..i * 8 + 8], 16).ok()?;
                b[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
            }
            let v6 = Ipv6Addr::from(b);
            v6.to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(v6))
        }
        _ => return None,
    };
    Some((ip, port))
}

/// Parse `/proc/net/tcp` or `/proc/net/tcp6` text.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_proc_net_tcp(text: &str) -> Vec<TcpRow> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some(TcpRow {
                local: parse_addr(f.get(1)?)?,
                remote: parse_addr(f.get(2)?)?,
                state: u8::from_str_radix(f.get(3)?, 16).ok()?,
                inode: f.get(9)?.parse().ok()?,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn socket_inodes(pid: u32) -> HashSet<u64> {
        let mut out = HashSet::new();
        if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for e in rd.flatten() {
                if let Ok(t) = std::fs::read_link(e.path())
                    && let Some(n) = t
                        .to_str()
                        .and_then(|s| s.strip_prefix("socket:["))
                        .and_then(|s| s.strip_suffix(']'))
                        .and_then(|s| s.parse().ok())
                {
                    out.insert(n);
                }
            }
        }
        out
    }

    fn rows() -> Vec<TcpRow> {
        let mut v = Vec::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(t) = std::fs::read_to_string(f) {
                v.extend(parse_proc_net_tcp(&t));
            }
        }
        v
    }

    pub fn listeners(pids: &[u32]) -> Vec<Listener> {
        let owners: HashMap<u64, u32> = pids
            .iter()
            .flat_map(|&p| socket_inodes(p).into_iter().map(move |i| (i, p)))
            .collect();
        if owners.is_empty() {
            return vec![];
        }
        rows()
            .into_iter()
            .filter(|r| r.state == TCP_LISTEN)
            .filter_map(|r| {
                owners.get(&r.inode).map(|&pid| Listener {
                    pid,
                    port: r.local.1,
                    addr: r.local.0,
                })
            })
            .collect()
    }

    pub fn owner_of_connection(pids: &[u32], client_port: u16, server_port: u16) -> Option<u32> {
        let inodes: HashSet<u64> = rows()
            .into_iter()
            .filter(|r| {
                r.state != TCP_LISTEN
                    && r.local.1 == client_port
                    && r.remote.1 == server_port
                    && r.local.0.is_loopback()
            })
            .map(|r| r.inode)
            .filter(|i| *i != 0)
            .collect();
        if inodes.is_empty() {
            return None;
        }
        pids.iter()
            .copied()
            .find(|&p| !socket_inodes(p).is_disjoint(&inodes))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;
    pub fn listeners(_: &[u32]) -> Vec<Listener> {
        vec![]
    }
    pub fn owner_of_connection(_: &[u32], _: u16, _: u16) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_net_tcp() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1111 1 0000000000000000 100 0 0 10 0
   2: 0100007F:C350 0100007F:1435 01 00000000:00000000 00:00000000 00000000  1000        0 5555 1 0000000000000000 20 4 30 10 -1
";
        let r = parse_proc_net_tcp(v4);
        assert_eq!(r.len(), 3);
        if cfg!(target_endian = "little") {
            assert_eq!(r[0].local, (IpAddr::V4(Ipv4Addr::LOCALHOST), 5173));
            assert_eq!(r[2].remote, (IpAddr::V4(Ipv4Addr::LOCALHOST), 5173));
        }
        assert_eq!(r[0].state, TCP_LISTEN);
        assert_eq!(r[0].inode, 41234);
        assert_eq!(r[1].local, (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 22));
        assert_eq!(r[2].local.1, 50000);
        let v6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 778 1 0000000000000000 100 0 0 10 0
";
        let r = parse_proc_net_tcp(v6);
        if cfg!(target_endian = "little") {
            assert_eq!(r[0].local, (IpAddr::V6(Ipv6Addr::LOCALHOST), 3000));
        }
        assert_eq!(r[1].local, (IpAddr::V6(Ipv6Addr::UNSPECIFIED), 8080));
        assert!(is_local_bind(&r[1].local.0));
        assert!(!is_local_bind(&"192.168.1.4".parse().unwrap()));
    }

    #[test]
    fn finds_own_listener_and_connection() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id();
        let found = listeners(&[me]);
        assert!(
            found
                .iter()
                .any(|x| x.port == port && x.pid == me && x.addr.is_loopback()),
            "{found:?}"
        );
        let c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let cport = c.local_addr().unwrap().port();
        assert_eq!(owner_of_connection(&[me], cport, port), Some(me));
        assert_eq!(
            owner_of_connection(&[me], cport, port.wrapping_add(1)),
            None
        );
        // pid 1 (launchd/init) owns no such socket (and is not ours to inspect).
        assert_eq!(owner_of_connection(&[1], cport, port), None);
    }
}
