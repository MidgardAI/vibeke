//! Stage 0 follow-up: drive frames ourselves with `HeadlessExperimental.beginFrame` (headless
//! shell, `--enable-begin-frame-control`) instead of `Page.startScreencast`, to see whether the
//! ~25 fps screencast ceiling is screencast pacing or rendering cost.
//!
//! Chromium 153 refuses this on macOS (`BeginFrameControl is not supported on MacOS yet`), so
//! it only runs on Linux (the devbox: agent browsers and the plain-SSH topology, 06 B3.2/B5).
//!
//! ```text
//! cargo run -p vk-browser --example beginframe --release
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use base64::Engine as _;
use serde_json::json;
use vk_browser::cdp::{Browser, LaunchOptions, TempProfile, discover_chromium, tree_cpu_seconds};
use vk_browser::frame::{self, Rgba, TileDiffer};
use vk_browser::kitty::{TileEncoder, Transfer};
use vk_browser::local_http::LocalServer;

const IND: u32 = 40;
const PALETTE: [[u8; 3]; 8] = [
    [255, 0, 0],
    [0, 200, 0],
    [0, 0, 255],
    [255, 255, 0],
    [255, 0, 255],
    [0, 255, 255],
    [0, 0, 0],
    [255, 128, 0],
];

fn page_html() -> String {
    let pal: Vec<String> = PALETTE
        .iter()
        .map(|c| format!("'rgb({},{},{})'", c[0], c[1], c[2]))
        .collect();
    let mut rows = String::new();
    for i in 0..600 {
        rows.push_str(&format!(
            "<div style='height:48px;padding:8px 60px;border-bottom:1px solid #ddd;background:hsl({},60%,92%)'>Row {i}: the quick brown fox jumps over the lazy dog, {}</div>",
            (i * 37) % 360,
            "lorem ipsum dolor sit amet ".repeat(2)
        ));
    }
    format!(
        "<!doctype html><body style='margin:0;font:15px sans-serif'>\
         <div id=ind style='position:fixed;left:0;top:0;width:{IND}px;height:{IND}px;background:rgb(128,128,128);z-index:9'></div>{rows}\
         <input id=t style='position:fixed;right:10px;top:10px'>\
         <script>const P=[{}];let n=0;addEventListener('wheel',()=>{{n++;ind.style.background=P[n%8]}},{{passive:true}});\
         t.addEventListener('input',()=>{{n++;ind.style.background=P[n%8]}});</script></body>",
        pal.join(",")
    )
}

fn indicator(img: &Rgba, css_w: u32) -> Option<usize> {
    let scale = img.width as f64 / css_w as f64;
    let c = (IND as f64 * scale / 2.0) as u32;
    let r = (6.0 * scale) as u32;
    let m = img.mean_rgb(c - r, c - r, 2 * r, 2 * r);
    let (mut best, mut bd) = (0, f32::MAX);
    for (i, p) in PALETTE.iter().enumerate() {
        let d = (0..3).map(|k| (m[k] - p[k] as f32).powi(2)).sum::<f32>();
        if d < bd {
            bd = d;
            best = i;
        }
    }
    (bd < 3600.0).then_some(best)
}

fn pct(v: &[f64], p: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s.get(((s.len() as f64 - 1.0) * p).round() as usize)
        .copied()
        .unwrap_or(f64::NAN)
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    // SAFETY: plain FFI call on the current thread.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    let quality: u8 = std::env::args()
        .skip_while(|a| a != "--quality")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(80);
    let bin = discover_chromium(true).ok_or_else(|| anyhow!("no Chromium"))?;
    let prof = TempProfile::new(&std::env::temp_dir(), "beginframe")?;
    let mut o = LaunchOptions::new(&bin, &prof.path);
    o.headless_new = false;
    o.device_scale_factor = Some(2.0);
    o.extra_args = vec![
        "--enable-begin-frame-control".into(),
        "--deterministic-mode".into(),
        "--run-all-compositor-stages-before-draw".into(),
    ];
    let browser = Browser::launch(&o)?;
    let srv = LocalServer::start(HashMap::from([("/".to_owned(), page_html())]))?;
    let page =
        browser.new_page_with(json!({ "url": "about:blank", "enableBeginFrameControl": true }))?;
    page.enable()?;
    let (css_w, css_h) = page.set_viewport_for_cells(100, 40, 16, 32, 2.0)?;
    page.call("Page.navigate", json!({ "url": srv.url("/") }))?;
    let begin = |shot: bool| -> Result<Option<Vec<u8>>> {
        let mut p = json!({ "interval": 16.666 });
        if shot {
            p["screenshot"] =
                json!({ "format": "jpeg", "quality": quality, "optimizeForSpeed": true });
        }
        let r = page.call("HeadlessExperimental.beginFrame", p)?;
        Ok(r["screenshotData"]
            .as_str()
            .map(|d| base64::engine::general_purpose::STANDARD.decode(d))
            .transpose()?)
    };
    // Pump frames until the page loaded.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        begin(false)?;
        if page.eval("document.readyState")?.as_str() == Some("complete") {
            break;
        }
    }
    let mut differ = TileDiffer::cell_aligned(16, 32, 4, 2);
    let mut enc = TileEncoder::new(Transfer::Shm, 1, 16, 32);
    let mut process = |data: &[u8]| -> Result<Rgba> {
        let img = frame::decode(data)?;
        let ch = differ.diff(&img);
        let mut out = Vec::new();
        enc.encode(&img, &ch, &mut out)?;
        enc.cleanup();
        Ok(img)
    };

    // Throughput while scrolling: wheel at 60 Hz from another thread, beginFrame as fast as the
    // round trip allows (each call renders one frame and returns its screenshot when damaged).
    let stop = Arc::new(AtomicBool::new(false));
    let (wp, st) = (page.clone(), stop.clone());
    let (cx, cy) = (css_w as f64 / 2.0, css_h as f64 / 2.0);
    let wheel = std::thread::spawn(move || {
        while !st.load(Ordering::Relaxed) {
            let _ = wp.wheel(cx, cy, 0.0, 40.0);
            std::thread::sleep(Duration::from_micros(16_667));
        }
    });
    let pid = browser.pid();
    let c0 = tree_cpu_seconds(pid)?;
    let t0 = Instant::now();
    let (mut frames, mut shots) = (0, 0);
    let mut bf_ms = Vec::new();
    while t0.elapsed() < Duration::from_secs(5) {
        let b = Instant::now();
        let shot = begin(true)?;
        bf_ms.push(b.elapsed().as_secs_f64() * 1000.0);
        frames += 1;
        if let Some(d) = shot {
            process(&d)?;
            shots += 1;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let cpu = (tree_cpu_seconds(pid)? - c0) / secs * 100.0;
    stop.store(true, Ordering::Relaxed);
    wheel.join().ok();
    println!(
        "beginFrame jpeg q{quality} 1600x1280: {:.1} frames/s, {:.1} damaged frames/s; beginFrame round trip p50 {:.1} ms p95 {:.1} ms; Chromium CPU {cpu:.0}%",
        frames as f64 / secs,
        shots as f64 / secs,
        pct(&bf_ms, 0.5),
        pct(&bf_ms, 0.95)
    );

    // Latency: wheel / key, then beginFrame until the indicator shows the new count.
    for kind in ["wheel", "key"] {
        let mut lat = Vec::new();
        let mut n = page.eval("n")?.as_u64().unwrap_or(0) as usize;
        if kind == "key" {
            page.click(css_w as f64 - 60.0, 20.0)?;
            begin(false)?;
            n = page.eval("n")?.as_u64().unwrap_or(0) as usize;
        }
        for i in 0..30 {
            n += 1;
            let t = Instant::now();
            if kind == "wheel" {
                page.wheel(cx, cy, 0.0, if i % 10 < 5 { 100.0 } else { -100.0 })?;
            } else {
                page.send(
                    "Input.dispatchKeyEvent",
                    json!({"type":"keyDown","key":"a","code":"KeyA","text":"a","windowsVirtualKeyCode":65}),
                )?;
                page.send(
                    "Input.dispatchKeyEvent",
                    json!({"type":"keyUp","key":"a","code":"KeyA","windowsVirtualKeyCode":65}),
                )?;
            }
            let deadline = t + Duration::from_secs(1);
            while Instant::now() < deadline {
                if let Some(d) = begin(true)?
                    && indicator(&process(&d)?, css_w) == Some(n % 8)
                {
                    lat.push(t.elapsed().as_secs_f64() * 1000.0);
                    break;
                }
            }
            for _ in 0..6 {
                begin(false)?;
            }
        }
        println!(
            "beginFrame {kind}→indicator: p50 {:.1} ms p95 {:.1} ms ({} of 30 matched)",
            pct(&lat, 0.5),
            pct(&lat, 0.95),
            lat.len()
        );
    }
    // Idle: no beginFrame calls → no frames, CPU only from timers.
    let c0 = tree_cpu_seconds(pid)?;
    std::thread::sleep(Duration::from_secs(3));
    println!(
        "idle (no beginFrame): Chromium CPU {:.1}%",
        (tree_cpu_seconds(pid)? - c0) / 3.0 * 100.0
    );
    drop(browser);
    drop(prof);
    Ok(())
}
