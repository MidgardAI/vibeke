//! Holder protocol (07 §4). Kept small and stable: the server must be able to talk to
//! holders started by the previous major version.
//!
//! Postcard is positional, so the compatibility rules are strict: variants and struct fields
//! are only ever appended (never reordered or removed), and a frame the older peer must decode
//! never gains a field. Versions:
//!
//! - `holder/1`: PTY mode only.
//! - `holder/2`: pipe mode (01 §1.2): the child gets stdin/stdout/stderr pipes instead of a
//!   PTY. Appended for it: [`Stream::Stdin`] (the holder journals what it wrote to the child's
//!   stdin, so a restarted server can tell which protocol requests it already answered),
//!   [`MarkerKind::Stream`] (ring-internal, never sent), [`InputStatus::Unconfirmed`]
//!   (server-local, never sent by a holder) and [`SpawnSpec::mode`] (read only by a holder of
//!   the same build). PTY-mode traffic is byte-identical to holder/1, so this server attaches
//!   to holder/1 holders unchanged (`holder_compat_tests`). A pipe-mode holder refuses a
//!   server whose `proto_max` is below [`PROTO_PIPE`]: it could not decode `Stdin`.

use serde::{Deserialize, Serialize};

pub const PROTO: u32 = 2;
pub const PROTO_MIN: u32 = 1;
/// Lowest protocol a server must speak to attach to a pipe-mode holder.
pub const PROTO_PIPE: u32 = 2;

/// Size of the dedupe window for input ids (01 §1.2).
pub const INPUT_DEDUPE_WINDOW: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Pty,
    /// Headless harness over stdio (01 §1.2, 04 §3.3): no PTY, no terminal queries answered,
    /// output journaled per stream.
    Pipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Pty,
    Stdout,
    Stderr,
    /// Pipe mode: bytes the holder wrote to the child's stdin, journaled in write order
    /// (holder/2). A restarted server replays them to learn which requests it already answered.
    Stdin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sig {
    Int,
    Term,
    Hup,
    Kill,
    Winch,
    Cont,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SigTarget {
    FgPgrp,
    Child,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingInfo {
    pub start_offset: u64,
    pub end_offset: u64,
    pub capacity: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerKind {
    Resize {
        cols: u16,
        rows: u16,
        px_w: u16,
        px_h: u16,
    },
    InputWritten {
        input_id: u64,
    },
    ServerDetached,
    ServerAttached {
        epoch: u64,
    },
    /// Pipe mode: the bytes from this offset on belong to `stream` (holder/2). Kept in the
    /// ring only; replay turns it into the `stream` of the `Output` frames it covers.
    Stream {
        stream: Stream,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputStatus {
    /// Every byte of this input has been written to the PTY master (01 §1.2). Sent only
    /// after the write completed, not when the bytes were queued.
    Written,
    /// The id is in the dedupe window and its bytes were already written; nothing was
    /// written again. (A resend of an id whose bytes are still being written gets no
    /// immediate reply; it is acked `Written` once the original write completes.)
    Duplicate,
    /// The child had already exited (PTY closed) when the input arrived; nothing written.
    ChildExited,
    /// The input was accepted but writing it to the PTY failed part-way (EIO: the child
    /// and every slave fd are gone). Some prefix of the bytes may have reached the PTY, so
    /// the client must treat it as unconfirmed. Added after holder/1 shipped as the last
    /// variant so older servers' decoders stay aligned for every other status.
    Failed,
    /// Server-local, never sent by a holder: no ack arrived (timeout, or the holder was lost
    /// with the input in flight), so the input may or may not have reached the child. Reported
    /// to clients as `input_unconfirmed` and never replayed automatically (01 §1.2).
    Unconfirmed,
}

/// A terminal query the holder could not answer itself because it depends on screen
/// state; queued while no server is attached and handed over on attach (01 §1.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedQuery {
    pub offset: u64,
    pub age_ms: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcStatus {
    pub child_pid: u32,
    pub fg_pgid: Option<u32>,
    pub fg_cmdline: Vec<String>,
    pub fg_exe: Option<String>,
    pub fg_cwd: Option<String>,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// Server → holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToHolder {
    Hello {
        proto_min: u32,
        proto_max: u32,
        server_pid: u32,
        server_boot_id: String,
    },
    Acquire {
        epoch: u64,
        server_pid: u32,
        hmac: Vec<u8>,
    },
    Attach {
        epoch: u64,
        from_offset: u64,
    },
    Input {
        epoch: u64,
        input_id: u64,
        bytes: Vec<u8>,
    },
    Resize {
        epoch: u64,
        cols: u16,
        rows: u16,
        px_w: u16,
        px_h: u16,
    },
    Signal {
        epoch: u64,
        sig: Sig,
        target: SigTarget,
    },
    StatusQuery,
    AckExit {
        epoch: u64,
    },
    Checkpoint {
        epoch: u64,
        offset: u64,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

/// Holder → server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromHolder {
    HelloOk {
        proto: u32,
        holder_version: String,
        pane_id: String,
        mode: Mode,
        child_pid: u32,
        started_at_ms: i64,
        ring: RingInfo,
        last_checkpoint: Option<u64>,
        nonce: Vec<u8>,
        epoch: u64,
    },
    Acquired {
        epoch: u64,
    },
    Rejected {
        reason: String,
    },
    Output {
        offset: u64,
        stream: Stream,
        bytes: Vec<u8>,
        replay: bool,
    },
    /// The ring overflowed past the requested offset; replay starts at `available_from`.
    Gap {
        requested: u64,
        available_from: u64,
    },
    /// End of the replayed region; live output follows.
    ReplayDone {
        offset: u64,
        queued_queries: Vec<QueuedQuery>,
    },
    Marker {
        offset: u64,
        kind: MarkerKind,
    },
    InputAck {
        input_id: u64,
        offset_at_write: u64,
        status: InputStatus,
    },
    Status(ProcStatus),
    ChildExited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    /// The journal has accumulated half a ring of bytes since the last checkpoint (01 §1.2).
    CheckpointWanted {
        offset: u64,
    },
    /// Foreground process group changed (detectors re-run, 04 §5.2).
    FgChanged,
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

/// HMAC-SHA256(holder_key, nonce ‖ epoch) — computed by callers with their crypto crate;
/// this helper only defines the message layout so both sides agree.
pub fn acquire_message(nonce: &[u8], epoch: u64) -> Vec<u8> {
    let mut m = nonce.to_vec();
    m.extend_from_slice(&epoch.to_le_bytes());
    m
}

/// Spawn parameters handed to `vibeke hold` through a 0600 file that the holder reads and
/// deletes, so neither the holder key nor the env appear in argv (07 §4, 09 §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub pane_id: String,
    pub socket: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub key: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    pub ring_bytes: u64,
    /// PTY (default) or pipe mode (holder/2). The spec file is written and read by the same
    /// build (`vibeke hold` is the server's own binary), so appending is safe.
    #[serde(default)]
    pub mode: Mode,
}

/// N-1 compatibility (01 §1.2: "the server talks to N-1 holders"): the holder/1 types as they
/// shipped, frozen here. Every frame a holder/1 holder sends must encode byte-for-byte like
/// the current type (so this server decodes it identically), and every frame this server
/// sends to a PTY-mode holder must be decodable by holder/1. A protocol change that breaks
/// either direction fails here and needs a major bump with a negotiated fallback.
#[cfg(test)]
mod holder_compat_tests {
    use super::*;

    mod v1 {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
        pub enum Mode {
            Pty,
            Pipe,
        }
        #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
        pub enum Stream {
            Pty,
            Stdout,
            Stderr,
        }
        #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
        pub enum Sig {
            Int,
            Term,
            Hup,
            Kill,
            Winch,
            Cont,
            Stop,
        }
        #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
        pub enum SigTarget {
            FgPgrp,
            Child,
        }
        #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
        pub struct RingInfo {
            pub start_offset: u64,
            pub end_offset: u64,
            pub capacity: u64,
        }
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum MarkerKind {
            Resize {
                cols: u16,
                rows: u16,
                px_w: u16,
                px_h: u16,
            },
            InputWritten {
                input_id: u64,
            },
            ServerDetached,
            ServerAttached {
                epoch: u64,
            },
        }
        #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
        pub enum InputStatus {
            Written,
            Duplicate,
            ChildExited,
            Failed,
        }
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct QueuedQuery {
            pub offset: u64,
            pub age_ms: u64,
            pub bytes: Vec<u8>,
        }
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct ProcStatus {
            pub child_pid: u32,
            pub fg_pgid: Option<u32>,
            pub fg_cmdline: Vec<String>,
            pub fg_exe: Option<String>,
            pub fg_cwd: Option<String>,
            pub exited: bool,
            pub exit_code: Option<i32>,
            pub signal: Option<i32>,
        }
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum ToHolder {
            Hello {
                proto_min: u32,
                proto_max: u32,
                server_pid: u32,
                server_boot_id: String,
            },
            Acquire {
                epoch: u64,
                server_pid: u32,
                hmac: Vec<u8>,
            },
            Attach {
                epoch: u64,
                from_offset: u64,
            },
            Input {
                epoch: u64,
                input_id: u64,
                bytes: Vec<u8>,
            },
            Resize {
                epoch: u64,
                cols: u16,
                rows: u16,
                px_w: u16,
                px_h: u16,
            },
            Signal {
                epoch: u64,
                sig: Sig,
                target: SigTarget,
            },
            StatusQuery,
            AckExit {
                epoch: u64,
            },
            Checkpoint {
                epoch: u64,
                offset: u64,
            },
            Ping {
                nonce: u64,
            },
            Pong {
                nonce: u64,
            },
        }
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum FromHolder {
            HelloOk {
                proto: u32,
                holder_version: String,
                pane_id: String,
                mode: Mode,
                child_pid: u32,
                started_at_ms: i64,
                ring: RingInfo,
                last_checkpoint: Option<u64>,
                nonce: Vec<u8>,
                epoch: u64,
            },
            Acquired {
                epoch: u64,
            },
            Rejected {
                reason: String,
            },
            Output {
                offset: u64,
                stream: Stream,
                bytes: Vec<u8>,
                replay: bool,
            },
            Gap {
                requested: u64,
                available_from: u64,
            },
            ReplayDone {
                offset: u64,
                queued_queries: Vec<QueuedQuery>,
            },
            Marker {
                offset: u64,
                kind: MarkerKind,
            },
            InputAck {
                input_id: u64,
                offset_at_write: u64,
                status: InputStatus,
            },
            Status(ProcStatus),
            ChildExited {
                exit_code: Option<i32>,
                signal: Option<i32>,
            },
            CheckpointWanted {
                offset: u64,
            },
            FgChanged,
            Ping {
                nonce: u64,
            },
            Pong {
                nonce: u64,
            },
        }
    }

    fn bytes<T: Serialize>(v: &T) -> Vec<u8> {
        postcard::to_stdvec(v).unwrap()
    }

    /// Pairs (current, holder/1) of every frame a holder/1 holder can send.
    fn from_holder_pairs() -> Vec<(FromHolder, v1::FromHolder)> {
        let ring = RingInfo {
            start_offset: 3,
            end_offset: 900,
            capacity: 1 << 24,
        };
        let ring1 = v1::RingInfo {
            start_offset: 3,
            end_offset: 900,
            capacity: 1 << 24,
        };
        let st = ProcStatus {
            child_pid: 42,
            fg_pgid: Some(43),
            fg_cmdline: vec!["claude".into(), "--resume".into()],
            fg_exe: Some("/bin/claude".into()),
            fg_cwd: None,
            exited: true,
            exit_code: Some(1),
            signal: None,
        };
        let st1 = v1::ProcStatus {
            child_pid: 42,
            fg_pgid: Some(43),
            fg_cmdline: vec!["claude".into(), "--resume".into()],
            fg_exe: Some("/bin/claude".into()),
            fg_cwd: None,
            exited: true,
            exit_code: Some(1),
            signal: None,
        };
        let mut v = vec![
            (
                FromHolder::HelloOk {
                    proto: 1,
                    holder_version: "0.4.0".into(),
                    pane_id: "01P".into(),
                    mode: Mode::Pty,
                    child_pid: 7,
                    started_at_ms: -5,
                    ring,
                    last_checkpoint: Some(77),
                    nonce: vec![1, 2, 3],
                    epoch: 9,
                },
                v1::FromHolder::HelloOk {
                    proto: 1,
                    holder_version: "0.4.0".into(),
                    pane_id: "01P".into(),
                    mode: v1::Mode::Pty,
                    child_pid: 7,
                    started_at_ms: -5,
                    ring: ring1,
                    last_checkpoint: Some(77),
                    nonce: vec![1, 2, 3],
                    epoch: 9,
                },
            ),
            (
                FromHolder::Acquired { epoch: 10 },
                v1::FromHolder::Acquired { epoch: 10 },
            ),
            (
                FromHolder::Rejected { reason: "x".into() },
                v1::FromHolder::Rejected { reason: "x".into() },
            ),
            (
                FromHolder::Gap {
                    requested: 1,
                    available_from: 2,
                },
                v1::FromHolder::Gap {
                    requested: 1,
                    available_from: 2,
                },
            ),
            (
                FromHolder::ReplayDone {
                    offset: 5,
                    queued_queries: vec![QueuedQuery {
                        offset: 4,
                        age_ms: 20,
                        bytes: b"\x1b[6n".to_vec(),
                    }],
                },
                v1::FromHolder::ReplayDone {
                    offset: 5,
                    queued_queries: vec![v1::QueuedQuery {
                        offset: 4,
                        age_ms: 20,
                        bytes: b"\x1b[6n".to_vec(),
                    }],
                },
            ),
            (FromHolder::Status(st), v1::FromHolder::Status(st1)),
            (
                FromHolder::ChildExited {
                    exit_code: None,
                    signal: Some(9),
                },
                v1::FromHolder::ChildExited {
                    exit_code: None,
                    signal: Some(9),
                },
            ),
            (
                FromHolder::CheckpointWanted { offset: 8 },
                v1::FromHolder::CheckpointWanted { offset: 8 },
            ),
            (FromHolder::FgChanged, v1::FromHolder::FgChanged),
            (
                FromHolder::Ping { nonce: 1 },
                v1::FromHolder::Ping { nonce: 1 },
            ),
            (
                FromHolder::Pong { nonce: 2 },
                v1::FromHolder::Pong { nonce: 2 },
            ),
        ];
        for (s, s1) in [
            (Stream::Pty, v1::Stream::Pty),
            (Stream::Stdout, v1::Stream::Stdout),
            (Stream::Stderr, v1::Stream::Stderr),
        ] {
            v.push((
                FromHolder::Output {
                    offset: 11,
                    stream: s,
                    bytes: b"hi\x1b[0m".to_vec(),
                    replay: true,
                },
                v1::FromHolder::Output {
                    offset: 11,
                    stream: s1,
                    bytes: b"hi\x1b[0m".to_vec(),
                    replay: true,
                },
            ));
        }
        for (k, k1) in [
            (
                MarkerKind::Resize {
                    cols: 80,
                    rows: 24,
                    px_w: 1,
                    px_h: 2,
                },
                v1::MarkerKind::Resize {
                    cols: 80,
                    rows: 24,
                    px_w: 1,
                    px_h: 2,
                },
            ),
            (
                MarkerKind::InputWritten { input_id: 5 },
                v1::MarkerKind::InputWritten { input_id: 5 },
            ),
            (MarkerKind::ServerDetached, v1::MarkerKind::ServerDetached),
            (
                MarkerKind::ServerAttached { epoch: 3 },
                v1::MarkerKind::ServerAttached { epoch: 3 },
            ),
        ] {
            v.push((
                FromHolder::Marker {
                    offset: 12,
                    kind: k,
                },
                v1::FromHolder::Marker {
                    offset: 12,
                    kind: k1,
                },
            ));
        }
        for (s, s1) in [
            (InputStatus::Written, v1::InputStatus::Written),
            (InputStatus::Duplicate, v1::InputStatus::Duplicate),
            (InputStatus::ChildExited, v1::InputStatus::ChildExited),
            (InputStatus::Failed, v1::InputStatus::Failed),
        ] {
            v.push((
                FromHolder::InputAck {
                    input_id: 99,
                    offset_at_write: 100,
                    status: s,
                },
                v1::FromHolder::InputAck {
                    input_id: 99,
                    offset_at_write: 100,
                    status: s1,
                },
            ));
        }
        v
    }

    #[test]
    fn current_server_decodes_every_holder_v1_frame() {
        for (cur, old) in from_holder_pairs() {
            let b = bytes(&old);
            assert_eq!(b, bytes(&cur), "{cur:?} encodes differently from holder/1");
            let decoded: FromHolder = crate::frame::decode(&b).unwrap();
            assert_eq!(decoded, cur);
        }
    }

    #[test]
    fn holder_v1_decodes_every_frame_the_server_sends_a_pty_holder() {
        let pairs: Vec<(ToHolder, v1::ToHolder)> = vec![
            (
                ToHolder::Hello {
                    proto_min: PROTO_MIN,
                    proto_max: PROTO,
                    server_pid: 5,
                    server_boot_id: "b".into(),
                },
                v1::ToHolder::Hello {
                    proto_min: PROTO_MIN,
                    proto_max: PROTO,
                    server_pid: 5,
                    server_boot_id: "b".into(),
                },
            ),
            (
                ToHolder::Acquire {
                    epoch: 2,
                    server_pid: 5,
                    hmac: vec![9; 32],
                },
                v1::ToHolder::Acquire {
                    epoch: 2,
                    server_pid: 5,
                    hmac: vec![9; 32],
                },
            ),
            (
                ToHolder::Attach {
                    epoch: 2,
                    from_offset: 40,
                },
                v1::ToHolder::Attach {
                    epoch: 2,
                    from_offset: 40,
                },
            ),
            (
                ToHolder::Input {
                    epoch: 2,
                    input_id: 1 << 62,
                    bytes: b"ls\r".to_vec(),
                },
                v1::ToHolder::Input {
                    epoch: 2,
                    input_id: 1 << 62,
                    bytes: b"ls\r".to_vec(),
                },
            ),
            (
                ToHolder::Resize {
                    epoch: 2,
                    cols: 100,
                    rows: 30,
                    px_w: 0,
                    px_h: 0,
                },
                v1::ToHolder::Resize {
                    epoch: 2,
                    cols: 100,
                    rows: 30,
                    px_w: 0,
                    px_h: 0,
                },
            ),
            (ToHolder::StatusQuery, v1::ToHolder::StatusQuery),
            (
                ToHolder::AckExit { epoch: 2 },
                v1::ToHolder::AckExit { epoch: 2 },
            ),
            (
                ToHolder::Checkpoint {
                    epoch: 2,
                    offset: 3,
                },
                v1::ToHolder::Checkpoint {
                    epoch: 2,
                    offset: 3,
                },
            ),
            (ToHolder::Ping { nonce: 4 }, v1::ToHolder::Ping { nonce: 4 }),
            (ToHolder::Pong { nonce: 4 }, v1::ToHolder::Pong { nonce: 4 }),
        ];
        let sigs = [
            (Sig::Int, v1::Sig::Int),
            (Sig::Term, v1::Sig::Term),
            (Sig::Hup, v1::Sig::Hup),
            (Sig::Kill, v1::Sig::Kill),
            (Sig::Winch, v1::Sig::Winch),
            (Sig::Cont, v1::Sig::Cont),
            (Sig::Stop, v1::Sig::Stop),
        ];
        let mut all = pairs;
        for (s, s1) in sigs {
            for (t, t1) in [
                (SigTarget::FgPgrp, v1::SigTarget::FgPgrp),
                (SigTarget::Child, v1::SigTarget::Child),
            ] {
                all.push((
                    ToHolder::Signal {
                        epoch: 1,
                        sig: s,
                        target: t,
                    },
                    v1::ToHolder::Signal {
                        epoch: 1,
                        sig: s1,
                        target: t1,
                    },
                ));
            }
        }
        for (cur, old) in all {
            let b = bytes(&cur);
            assert_eq!(b, bytes(&old), "{cur:?} encodes differently for holder/1");
            let decoded: v1::ToHolder = postcard::from_bytes(&b).unwrap();
            assert_eq!(decoded, old);
        }
    }

    /// The holder/2 additions are appended variants: they never shift a holder/1 index, and a
    /// holder/1 peer refuses them instead of misreading them (which is why a pipe-mode holder
    /// only accepts servers with `proto_max >= PROTO_PIPE`).
    #[test]
    fn holder_v2_additions_are_appended_variants() {
        let out = FromHolder::Output {
            offset: 0,
            stream: Stream::Stdin,
            bytes: b"{}\n".to_vec(),
            replay: false,
        };
        assert!(postcard::from_bytes::<v1::FromHolder>(&bytes(&out)).is_err());
        assert_eq!(bytes(&Stream::Stdin), vec![3]);
        assert_eq!(bytes(&InputStatus::Unconfirmed), vec![4]);
        assert_eq!(
            bytes(&MarkerKind::Stream {
                stream: Stream::Stdout
            }),
            vec![4, 1]
        );
        // The spawn spec's appended `mode` round-trips and defaults to PTY.
        let spec = SpawnSpec {
            pane_id: "p".into(),
            socket: "/s".into(),
            argv: vec!["sh".into()],
            cwd: "/".into(),
            env: vec![],
            key: vec![1],
            cols: 80,
            rows: 24,
            ring_bytes: 1024,
            mode: Mode::Pipe,
        };
        let back: SpawnSpec = postcard::from_bytes(&bytes(&spec)).unwrap();
        assert_eq!(back, spec);
        assert_eq!(Mode::default(), Mode::Pty);
    }
}
