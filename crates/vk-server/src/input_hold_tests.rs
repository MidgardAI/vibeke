use super::*;
use tokio::sync::mpsc::UnboundedReceiver;

fn input(bytes: &[u8]) -> PaneCmd {
    PaneCmd::Input {
        id: 0,
        bytes: bytes.to_vec(),
        ack: None,
    }
}

/// The bytes of every input command waiting in `rx`, in order.
fn drain(rx: &mut UnboundedReceiver<PaneCmd>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let PaneCmd::Input { bytes, .. } = cmd {
            out.push(bytes);
        }
    }
    out
}

#[tokio::test]
async fn input_waits_for_the_hold_and_keeps_its_order() {
    let holds = Holds::default();
    let (rt, mut rx) = PaneRt::new("p", 80, 24);
    assert!(!holds.send("p", &rt, input(b"a")));
    assert_eq!(drain(&mut rx), vec![b"a".to_vec()]);
    let guard = holds.hold("p");
    assert!(holds.is_held("p"));
    assert!(holds.send("p", &rt, input(b"b")));
    assert!(holds.send("p", &rt, input(b"c")));
    // Another pane is not held.
    let (rt2, mut rx2) = PaneRt::new("q", 80, 24);
    assert!(!holds.send("q", &rt2, input(b"x")));
    assert_eq!(drain(&mut rx2), vec![b"x".to_vec()]);
    assert!(drain(&mut rx).is_empty(), "held input waits");
    drop(guard);
    assert!(!holds.is_held("p"));
    assert_eq!(drain(&mut rx), vec![b"b".to_vec(), b"c".to_vec()]);
    assert!(!holds.send("p", &rt, input(b"d")));
    assert_eq!(drain(&mut rx), vec![b"d".to_vec()]);
}

#[tokio::test]
async fn a_nested_guard_ends_the_shared_hold_and_an_old_guard_leaves_a_new_hold_alone() {
    let holds = Holds::default();
    let (rt, mut rx) = PaneRt::new("p", 80, 24);
    let outer = holds.hold("p");
    let inner = holds.hold("p");
    assert!(holds.send("p", &rt, input(b"typed")));
    // The inner guard (the Enter was written) ends the hold.
    drop(inner);
    assert_eq!(drain(&mut rx), vec![b"typed".to_vec()]);
    assert!(!holds.is_held("p"));
    // A new hold is not ended by the outer guard of the earlier one.
    let next = holds.hold("p");
    drop(outer);
    assert!(holds.is_held("p"));
    drop(next);
    assert!(!holds.is_held("p"));
}

#[tokio::test]
async fn a_hold_ends_by_itself_after_the_deadline() {
    let holds = Holds::default();
    let (rt, mut rx) = PaneRt::new("p", 80, 24);
    let guard = holds.hold("p");
    assert!(holds.send("p", &rt, input(b"k")));
    // The timer task flushes the buffer at the deadline, while the guard is still alive.
    let deadline = Instant::now() + MAX_HOLD + Duration::from_secs(3);
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        got = drain(&mut rx);
    }
    assert_eq!(got, vec![b"k".to_vec()]);
    assert!(!holds.is_held("p"));
    // Input after the deadline goes straight through.
    assert!(!holds.send("p", &rt, input(b"l")));
    drop(guard);
}
