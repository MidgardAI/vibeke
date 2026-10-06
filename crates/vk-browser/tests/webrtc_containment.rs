//! Real-Chromium WebRTC containment (06 B5 / B3.4, 09). Skipped unless `VIBEKE_BROWSER_TESTS=1`.
//! Uses the Playwright builds already on disk (`$VIBEKE_CHROMIUM` overrides) and fresh temp
//! profiles; nothing is downloaded and no real browser profile is touched.
//!
//! A UDP sentinel listens on a local address the browser is not allowed to reach directly. A
//! page creates an `RTCPeerConnection` with the sentinel as its STUN server and gathers ICE
//! candidates; STUN binding requests are plain UDP. With the WebRTC policy in force the
//! sentinel must receive nothing. A control run with the old (broken) two-switch spelling
//! proves the sentinel would see the packets.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use vk_browser::cdp::{Browser, LaunchOptions, TempProfile};

fn enabled() -> bool {
    std::env::var("VIBEKE_BROWSER_TESTS").as_deref() == Ok("1")
}

/// A non-loopback address of this machine if it has one (no packet is sent: connecting a UDP
/// socket only picks a route), else 127.0.0.1.
fn sentinel_ip() -> std::net::IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or_else(|| "127.0.0.1".parse().unwrap())
}

/// Packets the sentinel received while a page gathered ICE candidates against it.
fn stun_packets(bin: &std::path::Path, extra: Vec<String>) -> usize {
    let sentinel = UdpSocket::bind(SocketAddr::new(sentinel_ip(), 0)).unwrap();
    sentinel
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let addr = sentinel.local_addr().unwrap();
    let prof = TempProfile::new(&std::env::temp_dir(), "webrtc").unwrap();
    let mut o = LaunchOptions::new(bin, &prof.path);
    o.headless_new = !bin.to_string_lossy().contains("headless");
    o.extra_args = extra;
    let browser = Browser::launch(&o).unwrap();
    let page = browser.new_page("about:blank").unwrap();
    let js = format!(
        "(async () => {{ const pc = new RTCPeerConnection({{iceServers: [{{urls: 'stun:{}:{}'}}]}}); \
         pc.createDataChannel('x'); await pc.setLocalDescription(await pc.createOffer()); \
         window.__pc = pc; return 'ok'; }})()",
        addr.ip(),
        addr.port()
    );
    assert_eq!(page.eval(&js).unwrap().as_str(), Some("ok"));
    let mut n = 0;
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if sentinel.recv_from(&mut buf).is_ok() {
            n += 1;
        }
    }
    let _ = browser.close();
    n
}

fn binaries() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Some(s) = vk_browser::cdp::discover_chromium(true) {
        v.push(s);
    }
    if let Some(f) = vk_browser::cdp::discover_chromium(false)
        && !v.contains(&f)
    {
        v.push(f);
    }
    v
}

fn policy() -> Vec<String> {
    vk_browser::headless::WEBRTC_UDP_POLICY
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The agents' headless browser (headless shell and full Chromium): the isolation flags keep
/// STUN off the network. Controls: without the policy STUN reaches the sentinel; on the
/// headless shell the old spelling (bare force switch + separate policy switch) leaked too.
#[test]
fn headless_isolation_sends_no_stun() {
    if !enabled() {
        eprintln!("skipped (VIBEKE_BROWSER_TESTS!=1)");
        return;
    }
    let isolation = vk_browser::headless::isolation_args();
    let without: Vec<String> = isolation
        .iter()
        .filter(|a| !a.contains("webrtc"))
        .cloned()
        .collect();
    for bin in binaries() {
        let leaked = stun_packets(&bin, without.clone());
        assert!(
            leaked > 0,
            "{}: control run sent no STUN; the sentinel cannot prove anything",
            bin.display()
        );
        if vk_browser::headless::is_headless_shell(&bin) {
            let mut old = without.clone();
            old.push("--force-webrtc-ip-handling-policy".into());
            old.push("--webrtc-ip-handling-policy=disable_non_proxied_udp".into());
            assert!(
                stun_packets(&bin, old) > 0,
                "the old spelling was expected to leak on the headless shell"
            );
        }
        let n = stun_packets(&bin, isolation.clone());
        assert_eq!(n, 0, "{}: STUN reached the sentinel", bin.display());
    }
}

/// A remote-profile pane browser and the external window (`socks5://` route): non-proxied
/// UDP is off too, so WebRTC cannot leave this machine around the route.
#[test]
fn socks_routed_browser_sends_no_stun() {
    if !enabled() {
        eprintln!("skipped (VIBEKE_BROWSER_TESTS!=1)");
        return;
    }
    // Nothing listens on port 1: the SOCKS route is dead, as for a link that is down.
    let route = vec![
        "--proxy-server=socks5://127.0.0.1:1".to_string(),
        "--proxy-bypass-list=<-loopback>".to_string(),
    ];
    for bin in binaries() {
        let leaked = stun_packets(&bin, route.clone());
        assert!(
            leaked > 0,
            "{}: control run sent no STUN; the sentinel cannot prove anything",
            bin.display()
        );
        let mut with = route.clone();
        with.extend(policy());
        let n = stun_packets(&bin, with);
        assert_eq!(n, 0, "{}: STUN reached the sentinel", bin.display());
    }
}
