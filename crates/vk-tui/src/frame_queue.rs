//! Bounded browser output. Never replay input after a disconnect or an overflow.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use vk_proto::frame;
use vk_proto::render::ClientFrame;

const MAX_BYTES: usize = 1024 * 1024;
const MAX_FRAMES: usize = 1024;
const MAX_FRAME: usize = 256 * 1024;
const MAX_BATCH: usize = 512 * 1024;

#[derive(Default)]
struct State {
    frames: VecDeque<(Option<String>, Vec<u8>)>,
    bytes: usize,
    failed: bool,
    hidden: bool,
}
#[derive(Clone, Default)]
pub struct Sender(Arc<Mutex<State>>);

impl Sender {
    pub fn set_hidden(&self, hidden: bool) {
        self.0.lock().unwrap().hidden = hidden;
    }

    pub fn send(&self, mut f: ClientFrame) -> Result<(), &'static str> {
        let mut s = self.0.lock().unwrap();
        // Focus/model events can arrive after visibility changes. Never let them
        // restore screen subscriptions before the browser becomes visible again.
        if s.hidden {
            match &mut f {
                ClientFrame::ViewHint { panes, active } => {
                    panes.clear();
                    *active = false;
                }
                ClientFrame::MediaView { panes, .. } => panes.clear(),
                _ => {}
            }
        }
        let key = match &f {
            ClientFrame::ViewHint { .. } => Some("view".into()),
            ClientFrame::MediaView { .. } => Some("media".into()),
            ClientFrame::ScrollView { pane, .. } => Some(format!("scroll:{pane}")),
            _ => None,
        };
        if s.failed {
            return Err("Input queue is paused");
        }
        let bytes = match frame::encode(&f) {
            Ok(bytes) => bytes,
            Err(_) => {
                s.failed = true;
                s.frames.clear();
                s.bytes = 0;
                return Err("Could not encode terminal input");
            }
        };
        // Replace view updates only after the last ordered input/command/ACK barrier.
        if let Some(key) = &key {
            let replace = s
                .frames
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, (k, _))| k.is_some())
                .find(|(_, (k, _))| k.as_ref() == Some(key))
                .map(|(i, _)| i);
            if let Some(i) = replace {
                s.bytes -= s.frames.remove(i).unwrap().1.len();
            }
        }
        if bytes.len() > MAX_FRAME
            || s.bytes + bytes.len() > MAX_BYTES
            || s.frames.len() >= MAX_FRAMES
        {
            s.failed = true;
            s.frames.clear();
            s.bytes = 0;
            return Err("Input queue is full");
        }
        s.bytes += bytes.len();
        s.frames.push_back((key, bytes));
        Ok(())
    }

    pub fn drain(&self) -> Result<Vec<u8>, &'static str> {
        let mut s = self.0.lock().unwrap();
        if s.failed {
            return Err(
                "Input paused because the connection could not keep up. Unsent input was discarded. Check the terminal before reconnecting.",
            );
        }
        let mut bytes = Vec::new();
        for _ in 0..64 {
            if s.frames
                .front()
                .is_none_or(|(_, b)| bytes.len() + b.len() > MAX_BATCH)
            {
                break;
            }
            let (_, b) = s.frames.pop_front().unwrap();
            s.bytes -= b.len();
            bytes.extend(b);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view(active: bool) -> ClientFrame {
        ClientFrame::ViewHint {
            panes: vec![],
            active,
        }
    }
    #[test]
    fn hidden_focus_events_cannot_resubscribe() {
        let q = Sender::default();
        q.set_hidden(true);
        q.send(view(true)).unwrap();
        let data = q.drain().unwrap();
        assert!(
            matches!(frame::read_frame::<_, ClientFrame>(&mut data.as_slice()).unwrap(), ClientFrame::ViewHint { active: false, panes } if panes.is_empty())
        );
        q.set_hidden(false);
        q.send(view(true)).unwrap();
        let data = q.drain().unwrap();
        assert!(matches!(
            frame::read_frame::<_, ClientFrame>(&mut data.as_slice()).unwrap(),
            ClientFrame::ViewHint { active: true, .. }
        ));
    }
    #[test]
    fn coalesces_views_without_crossing_ordered_input() {
        let q = Sender::default();
        q.send(view(false)).unwrap();
        q.send(view(true)).unwrap();
        q.send(ClientFrame::Ping { nonce: 1 }).unwrap();
        q.send(view(false)).unwrap();
        let data = q.drain().unwrap();
        let mut data = data.as_slice();
        assert!(matches!(
            frame::read_frame::<_, ClientFrame>(&mut data).unwrap(),
            ClientFrame::ViewHint { active: true, .. }
        ));
        assert!(matches!(
            frame::read_frame::<_, ClientFrame>(&mut data).unwrap(),
            ClientFrame::Ping { .. }
        ));
        assert!(matches!(
            frame::read_frame::<_, ClientFrame>(&mut data).unwrap(),
            ClientFrame::ViewHint { active: false, .. }
        ));
        assert!(data.is_empty());
    }
    #[test]
    fn overflow_is_explicit_and_never_replays() {
        let q = Sender::default();
        for nonce in 0..MAX_FRAMES {
            q.send(ClientFrame::Ping {
                nonce: nonce as u64,
            })
            .unwrap();
        }
        assert!(q.send(ClientFrame::Ping { nonce: 9999 }).is_err());
        assert!(q.drain().is_err());
        assert_eq!(q.0.lock().unwrap().bytes, 0);
        assert!(q.send(view(true)).is_err());
    }
    #[test]
    fn bounds_bytes_and_batch_sizes() {
        let q = Sender::default();
        for n in 0..4 {
            q.send(ClientFrame::RawInput {
                input_id: n,
                pane: "p".into(),
                bytes: vec![b'x'; 240 * 1024],
            })
            .unwrap();
        }
        assert!(q.0.lock().unwrap().bytes <= MAX_BYTES);
        let first = q.drain().unwrap();
        assert!(first.len() <= MAX_BATCH);
        assert!(!q.drain().unwrap().is_empty());
        assert!(q.drain().unwrap().is_empty());
        assert!(
            q.send(ClientFrame::RawInput {
                input_id: 9,
                pane: "p".into(),
                bytes: vec![0; MAX_FRAME + 1]
            })
            .is_err()
        );
    }
}
