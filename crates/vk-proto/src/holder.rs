//! Holder protocol `holder/1` (07 §4). Kept small and stable: the server must be able
//! to talk to holders started by the previous major version.

use serde::{Deserialize, Serialize};

pub const PROTO: u32 = 1;
pub const PROTO_MIN: u32 = 1;

/// Size of the dedupe window for input ids (01 §1.2).
pub const INPUT_DEDUPE_WINDOW: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Pty,
    Pipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Pty,
    Stdout,
    Stderr,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputStatus {
    Written,
    Duplicate,
    ChildExited,
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
}
