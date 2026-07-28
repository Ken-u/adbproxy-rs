//! Peer identity helpers for multi-user mode.
//!
//! - Agent identity: `SO_PEERCRED` on AF_UNIX (Linux) / `getpeereid` (macOS)
//! - ADB TCP client UID: `NETLINK_SOCK_DIAG` / `inet_diag` (Linux only)

use std::io;
use std::net::SocketAddr;

use crate::tenant::Uid;

/// Credentials of a connected local peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCred {
    pub uid: Uid,
    pub pid: u32,
    pub gid: u32,
}

/// Whether multi-user shared-5037 mode is supported on this platform.
pub fn multi_user_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Human-readable reason when multi-user mode is unavailable.
pub fn multi_user_unsupported_reason() -> String {
    if cfg!(target_os = "linux") {
        String::new()
    } else if cfg!(target_os = "macos") {
        "macOS does not support shared :5037 multi-user mode: no portable \
         unprivileged API to resolve the owner UID of an accepted loopback TCP peer. \
         Use one port per user with ADB_SERVER_SOCKET=tcp:127.0.0.1:<port>."
            .into()
    } else if cfg!(windows) {
        "Windows does not support shared :5037 multi-user mode yet. \
         Use one port per user with ADB_SERVER_SOCKET=tcp:127.0.0.1:<port>."
            .into()
    } else {
        "shared :5037 multi-user mode is only supported on Linux (same network namespace). \
         Use one port per user or an isolated network namespace."
            .into()
    }
}

/// Reject non-loopback ADB clients in multi-user mode.
pub fn is_loopback(addr: SocketAddr) -> bool {
    match addr {
        SocketAddr::V4(v4) => v4.ip().is_loopback(),
        SocketAddr::V6(v6) => {
            v6.ip().is_loopback() || v6.ip().to_ipv4_mapped().map(|v| v.is_loopback()) == Some(true)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::mem;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream as StdUnixStream;

    use tokio::net::UnixStream;

    /// Read `SO_PEERCRED` from an accepted Unix-domain connection.
    pub fn peer_cred_unix(stream: &UnixStream) -> io::Result<PeerCred> {
        let fd = stream.as_raw_fd();
        unsafe { peer_cred_fd(fd) }
    }

    /// Same for a std UnixStream (used during accept handshake if needed).
    #[allow(dead_code)]
    pub fn peer_cred_std(stream: &StdUnixStream) -> io::Result<PeerCred> {
        unsafe { peer_cred_fd(stream.as_raw_fd()) }
    }

    unsafe fn peer_cred_fd(fd: libc::c_int) -> io::Result<PeerCred> {
        let mut cred: libc::ucred = mem::zeroed();
        let mut len = mem::size_of_val(&cred) as libc::socklen_t;
        let rc = libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PeerCred {
            uid: cred.uid,
            pid: cred.pid as u32,
            gid: cred.gid,
        })
    }

    /// Resolve the UID of the *client* side of a local TCP connection using
    /// `NETLINK_SOCK_DIAG` / `inet_diag`.
    ///
    /// `peer` is the remote address of the accepted server socket (i.e. the
    /// client's bind address:port). We match the client-side tuple, not the
    /// listening socket.
    pub fn tcp_peer_uid(local: SocketAddr, peer: SocketAddr) -> io::Result<Uid> {
        // On the accepted socket: local = daemon's 5037 side, peer = client.
        // inet_diag matches sockets by their (src, dst) as seen by the owner.
        // For the client socket: src = peer, dst = local.
        query_inet_diag_uid(peer, local)
    }

    fn query_inet_diag_uid(src: SocketAddr, dst: SocketAddr) -> io::Result<Uid> {
        // Build inet_diag request over NETLINK_SOCK_DIAG.
        // See sock_diag(7) and include/uapi/linux/inet_diag.h.
        const SOCK_DIAG_BY_FAMILY: u16 = 20;
        const TCPF_ESTABLISHED: u32 = 1 << 1;
        const INET_DIAG_INFO: u8 = 2;

        let family: u8 = match (src, dst) {
            (SocketAddr::V4(_), SocketAddr::V4(_)) => libc::AF_INET as u8,
            (SocketAddr::V6(_), SocketAddr::V6(_)) => libc::AF_INET6 as u8,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "src/dst address family mismatch",
                ));
            }
        };

        let nl = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_SOCK_DIAG,
            )
        };
        if nl < 0 {
            return Err(io::Error::last_os_error());
        }

        let result = (|| {
            // nlmsghdr + inet_diag_req_v2
            #[repr(C)]
            struct InetDiagReqV2 {
                sdiag_family: u8,
                sdiag_protocol: u8,
                idiag_ext: u8,
                pad: u8,
                idiag_states: u32,
                id: InetDiagSockId,
            }
            #[repr(C)]
            struct InetDiagSockId {
                idiag_sport: u16,
                idiag_dport: u16,
                idiag_src: [u32; 4],
                idiag_dst: [u32; 4],
                idiag_if: u32,
                idiag_cookie: [u32; 2],
            }

            let mut src_bytes = [0u32; 4];
            let mut dst_bytes = [0u32; 4];
            let (sport, dport) = fill_addrs(src, dst, &mut src_bytes, &mut dst_bytes)?;

            let req = InetDiagReqV2 {
                sdiag_family: family,
                sdiag_protocol: libc::IPPROTO_TCP as u8,
                idiag_ext: INET_DIAG_INFO,
                pad: 0,
                idiag_states: TCPF_ESTABLISHED | (1 << 2) | (1 << 3), // ESTABLISHED, SYN_SENT, SYN_RECV
                id: InetDiagSockId {
                    idiag_sport: sport,
                    idiag_dport: dport,
                    idiag_src: src_bytes,
                    idiag_dst: dst_bytes,
                    idiag_if: 0,
                    idiag_cookie: [u32::MAX, u32::MAX], // INET_DIAG_NOCOOKIE
                },
            };

            #[repr(C)]
            struct NlMsgHdr {
                nlmsg_len: u32,
                nlmsg_type: u16,
                nlmsg_flags: u16,
                nlmsg_seq: u32,
                nlmsg_pid: u32,
            }

            let payload_len = mem::size_of::<InetDiagReqV2>();
            let nlmsg_len = (mem::size_of::<NlMsgHdr>() + payload_len) as u32;
            let hdr = NlMsgHdr {
                nlmsg_len,
                nlmsg_type: SOCK_DIAG_BY_FAMILY,
                nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16,
                nlmsg_seq: 1,
                nlmsg_pid: 0,
            };

            let mut msg = Vec::with_capacity(nlmsg_len as usize);
            unsafe {
                msg.set_len(nlmsg_len as usize);
                std::ptr::copy_nonoverlapping(
                    &hdr as *const _ as *const u8,
                    msg.as_mut_ptr(),
                    mem::size_of::<NlMsgHdr>(),
                );
                std::ptr::copy_nonoverlapping(
                    &req as *const _ as *const u8,
                    msg.as_mut_ptr().add(mem::size_of::<NlMsgHdr>()),
                    payload_len,
                );
            }

            let sent = unsafe {
                libc::send(
                    nl,
                    msg.as_ptr() as *const libc::c_void,
                    msg.len(),
                    0,
                )
            };
            if sent < 0 {
                return Err(io::Error::last_os_error());
            }

            let mut buf = vec![0u8; 8192];
            let n = unsafe {
                libc::recv(
                    nl,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let n = n as usize;

            // Parse nlmsg; look for inet_diag_msg with matching ports.
            parse_inet_diag_uid(&buf[..n], sport, dport, &src_bytes, &dst_bytes, family)
        })();

        unsafe {
            libc::close(nl);
        }
        result
    }

    fn fill_addrs(
        src: SocketAddr,
        dst: SocketAddr,
        src_out: &mut [u32; 4],
        dst_out: &mut [u32; 4],
    ) -> io::Result<(u16, u16)> {
        match (src, dst) {
            (SocketAddr::V4(s), SocketAddr::V4(d)) => {
                src_out[0] = u32::from_ne_bytes(s.ip().octets());
                dst_out[0] = u32::from_ne_bytes(d.ip().octets());
                Ok((s.port().to_be(), d.port().to_be()))
            }
            (SocketAddr::V6(s), SocketAddr::V6(d)) => {
                let sb = s.ip().octets();
                let db = d.ip().octets();
                for i in 0..4 {
                    src_out[i] = u32::from_ne_bytes(sb[i * 4..i * 4 + 4].try_into().unwrap());
                    dst_out[i] = u32::from_ne_bytes(db[i * 4..i * 4 + 4].try_into().unwrap());
                }
                Ok((s.port().to_be(), d.port().to_be()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "address family mismatch",
            )),
        }
    }

    fn parse_inet_diag_uid(
        buf: &[u8],
        sport: u16,
        dport: u16,
        src: &[u32; 4],
        dst: &[u32; 4],
        family: u8,
    ) -> io::Result<Uid> {
        #[repr(C)]
        struct NlMsgHdr {
            nlmsg_len: u32,
            nlmsg_type: u16,
            nlmsg_flags: u16,
            nlmsg_seq: u32,
            nlmsg_pid: u32,
        }
        #[repr(C)]
        struct InetDiagMsg {
            idiag_family: u8,
            idiag_state: u8,
            idiag_timer: u8,
            idiag_retrans: u8,
            id: InetDiagSockId,
            idiag_expires: u32,
            idiag_rqueue: u32,
            idiag_wqueue: u32,
            idiag_uid: u32,
            idiag_inode: u32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct InetDiagSockId {
            idiag_sport: u16,
            idiag_dport: u16,
            idiag_src: [u32; 4],
            idiag_dst: [u32; 4],
            idiag_if: u32,
            idiag_cookie: [u32; 2],
        }

        let mut offset = 0usize;
        let mut matches: Vec<Uid> = Vec::new();

        while offset + mem::size_of::<NlMsgHdr>() <= buf.len() {
            let hdr = unsafe { &*(buf.as_ptr().add(offset) as *const NlMsgHdr) };
            let len = hdr.nlmsg_len as usize;
            if len < mem::size_of::<NlMsgHdr>() || offset + len > buf.len() {
                break;
            }
            if hdr.nlmsg_type == libc::NLMSG_DONE as u16 {
                break;
            }
            if hdr.nlmsg_type == libc::NLMSG_ERROR as u16 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "netlink sock_diag error",
                ));
            }

            let payload_off = offset + mem::size_of::<NlMsgHdr>();
            if payload_off + mem::size_of::<InetDiagMsg>() <= offset + len {
                let msg = unsafe { &*(buf.as_ptr().add(payload_off) as *const InetDiagMsg) };
                if msg.idiag_family == family
                    && msg.id.idiag_sport == sport
                    && msg.id.idiag_dport == dport
                    && addrs_equal(&msg.id.idiag_src, src, family)
                    && addrs_equal(&msg.id.idiag_dst, dst, family)
                {
                    matches.push(msg.idiag_uid);
                }
            }

            // NLMSG_ALIGN(len)
            offset += (len + 3) & !3;
        }

        match matches.as_slice() {
            [uid] => Ok(*uid),
            [] => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no matching TCP socket for peer UID lookup",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                "ambiguous TCP peer UID (multiple matches)",
            )),
        }
    }

    fn addrs_equal(a: &[u32; 4], b: &[u32; 4], family: u8) -> bool {
        if family == libc::AF_INET as u8 {
            a[0] == b[0]
        } else {
            a == b
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{peer_cred_unix, tcp_peer_uid};

#[cfg(all(unix, not(target_os = "linux")))]
pub fn peer_cred_unix(_stream: &tokio::net::UnixStream) -> io::Result<PeerCred> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        multi_user_unsupported_reason(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn tcp_peer_uid(_local: SocketAddr, _peer: SocketAddr) -> io::Result<Uid> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        multi_user_unsupported_reason(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1:5037".parse().unwrap()));
        assert!(is_loopback("[::1]:5037".parse().unwrap()));
        assert!(!is_loopback("192.168.1.1:5037".parse().unwrap()));
    }
}
