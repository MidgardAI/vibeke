//! Goal 03 Stage 0 frame-path bench (not a CI test).
//!
//! ```text
//! cargo run -p vk-browser --example bench --release -- [--full] [--quick] [--only NAME]
//! ```
//!
//! Launches a Playwright Chromium build found on disk (`$VIBEKE_CHROMIUM` overrides) with a fresh
//! temp profile, serves test pages from a loopback-only HTTP server, and measures per
//! configuration: fps (CSS animation, synthetic wheel scrolling), input→frame latency (wheel and
//! typed keys; the page paints an indicator whose colour encodes the event count), Chromium
//! process-tree CPU (idle, animating, scrolling), frame decode / tile diff / kitty encode cost and
//! kitty output bytes (full frames vs tile diffs, direct vs shm). Kitty output goes to memory
//! only, never to the terminal.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;
use vk_browser::cdp::{
    Browser, Event, LaunchOptions, Page, ScreencastFrame, ScreencastParams, TempProfile,
    discover_chromium, tree_cpu_seconds,
};
use vk_browser::frame::{self, Rgba, TileDiffer};
use vk_browser::kitty::{EncodeStats, PixelFormat, TileEncoder, Transfer};
use vk_browser::local_http::LocalServer;

// Pane geometry: 100×40 cells of 16×32 device px (Ghostty on a Retina screen is ~16×34), DPR 2.
const COLS: u32 = 100;
const ROWS: u32 = 40;
const CELL_W: u32 = 16;
const CELL_H: u32 = 32;
const DPR: f64 = 2.0;
const IND: u32 = 40; // indicator size, CSS px, at the top-left corner

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

fn palette_js() -> String {
    let v: Vec<String> = PALETTE
        .iter()
        .map(|c| format!("'rgb({},{},{})'", c[0], c[1], c[2]))
        .collect();
    format!("[{}]", v.join(","))
}

fn pages() -> HashMap<String, String> {
    let ind = format!(
        "<div id=ind style='position:fixed;left:0;top:0;width:{IND}px;height:{IND}px;background:rgb(128,128,128);z-index:9'></div>"
    );
    let pal = palette_js();
    let mut m = HashMap::new();
    m.insert(
        "/idle".into(),
        format!(
            "<!doctype html><body style='margin:0;font:16px sans-serif'>{ind}<div style='padding:60px'>{}</div></body>",
            "Static page. ".repeat(200)
        ),
    );
    m.insert(
        "/anim".into(),
        "<!doctype html><style>@keyframes s{from{transform:rotate(0)}to{transform:rotate(360deg)}}\
         @keyframes m{from{left:0}to{left:700px}}\
         .b{position:absolute;top:100px;left:100px;width:200px;height:200px;background:linear-gradient(#f80,#08f);animation:s 2s linear infinite}\
         .m{position:absolute;top:400px;width:60px;height:60px;background:#0a0;animation:m 1.5s linear infinite alternate}</style>\
         <body style='margin:0'><div class=b></div><div class=m></div><p style='margin:20px'>CSS animation</p></body>"
            .into(),
    );
    let mut rows = String::new();
    for i in 0..600 {
        rows.push_str(&format!(
            "<div style='height:48px;padding:8px 60px;border-bottom:1px solid #ddd;background:hsl({},60%,92%)'>Row {i}: the quick brown fox jumps over the lazy dog, {}</div>",
            (i * 37) % 360,
            "lorem ipsum dolor sit amet ".repeat(2)
        ));
    }
    m.insert(
        "/scroll".into(),
        format!(
            "<!doctype html><body style='margin:0;font:15px sans-serif'>{ind}{rows}\
             <script>const P={pal};let n=0;addEventListener('wheel',()=>{{n++;ind.style.background=P[n%8]}},{{passive:true}});</script></body>"
        ),
    );
    m.insert(
        "/input".into(),
        format!(
            "<!doctype html><body style='margin:0;font:16px sans-serif'>{ind}\
             <input id=t autofocus style='margin:60px;font-size:20px;width:500px'>\
             <script>const P={pal};let n=0;t.addEventListener('input',()=>{{n++;ind.style.background=P[n%8]}});</script></body>"
        ),
    );
    m
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    Screencast(ScreencastParams),
    /// `Page.captureScreenshot` polling (jpeg quality, or png when `png`).
    Screenshot {
        png: bool,
        quality: u8,
    },
}

#[derive(Clone, Debug)]
struct Config {
    name: String,
    source: Source,
    /// Ack immediately on receipt (true) or after decode + diff + encode (false).
    ack_early: bool,
}

struct Ctx {
    browser: Browser,
    events: Receiver<Event>,
    page: Page,
    css: (u32, u32),
    srv: LocalServer,
}

impl Ctx {
    fn drain(&self) {
        while self.events.try_recv().is_ok() {}
    }
}

/// Per-frame processing: decode, diff, encode with the production encoder (tile diff + shm).
struct Pipeline {
    differ: TileDiffer,
    shm: TileEncoder,
    decode_ms: Vec<f64>,
    diff_ms: Vec<f64>,
    encode_ms: Vec<f64>,
    tiles_changed: Vec<usize>,
    tiles_total: usize,
    shm_stats: Vec<EncodeStats>,
    jpeg_bytes: Vec<usize>,
    kept: Vec<Rgba>,
    keep: usize,
}

impl Pipeline {
    fn new(keep: usize) -> Pipeline {
        Pipeline {
            differ: TileDiffer::cell_aligned(CELL_W, CELL_H, 4, 2),
            shm: TileEncoder::new(Transfer::Shm, 1000, CELL_W, CELL_H),
            decode_ms: vec![],
            diff_ms: vec![],
            encode_ms: vec![],
            tiles_changed: vec![],
            tiles_total: 0,
            shm_stats: vec![],
            jpeg_bytes: vec![],
            kept: vec![],
            keep,
        }
    }

    fn process(&mut self, data: &[u8]) -> Result<Rgba> {
        let t0 = Instant::now();
        let img = frame::decode(data)?;
        let t1 = Instant::now();
        // Tiles are cell-aligned in *device* px; if the frame is smaller (CSS px), scale tiles.
        let changed = self.differ.diff(&img);
        let t2 = Instant::now();
        let mut out = Vec::new();
        let st = self.shm.encode(&img, &changed, &mut out)?;
        self.shm.cleanup();
        let t3 = Instant::now();
        self.decode_ms.push(ms(t1 - t0));
        self.diff_ms.push(ms(t2 - t1));
        self.encode_ms.push(ms(t3 - t2));
        let (gc, gr) = self.differ.grid();
        self.tiles_total = (gc * gr) as usize;
        self.tiles_changed.push(changed.len());
        self.shm_stats.push(st);
        self.jpeg_bytes.push(data.len());
        if self.kept.len() < self.keep {
            self.kept.push(img.clone());
        }
        Ok(img)
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((s.len() as f64 - 1.0) * p).round() as usize;
    s[i]
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Which palette entry the indicator shows (None if grey/ambiguous).
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
    (bd < 60.0 * 60.0).then_some(best)
}

fn launch(full: bool, extra: &[&str]) -> Result<Ctx> {
    let bin = discover_chromium(!full).ok_or_else(|| anyhow::anyhow!("no Chromium on disk"))?;
    let prof = TempProfile::new(&std::env::temp_dir(), "bench")?;
    let mut o = LaunchOptions::new(&bin, &prof.path);
    o.headless_new = !bin.to_string_lossy().contains("headless-shell");
    o.extra_args = extra.iter().map(|s| s.to_string()).collect();
    std::mem::forget(prof); // removed at the end of main via the temp dir sweep below
    let mut browser = Browser::launch(&o)?;
    let events = browser.take_events();
    let page = browser.new_page("about:blank")?;
    page.enable()?;
    page.call("Page.bringToFront", json!({}))?;
    page.call(
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled": true}),
    )?;
    let css = page.set_viewport_for_cells(COLS, ROWS, CELL_W, CELL_H, DPR)?;
    let srv = LocalServer::start(pages())?;
    Ok(Ctx {
        browser,
        events,
        page,
        css,
        srv,
    })
}

/// Pull frames for `dur`, processing each; returns (frames, frame arrival times).
fn pump_frames(
    ctx: &Ctx,
    cfg: &Config,
    pipe: &mut Pipeline,
    dur: Duration,
    mut on_frame: impl FnMut(&Rgba, Instant),
) -> Result<Vec<Instant>> {
    let mut times = Vec::new();
    let end = Instant::now() + dur;
    match cfg.source {
        Source::Screencast(_) => {
            while Instant::now() < end {
                let Ok(ev) = ctx.events.recv_timeout(Duration::from_millis(20)) else {
                    continue;
                };
                if ev.method != "Page.screencastFrame" {
                    continue;
                }
                let f = ScreencastFrame::from_event(&ev)?;
                if cfg.ack_early {
                    ctx.page.ack_frame(f.session_id)?;
                }
                let img = pipe.process(&f.data)?;
                if !cfg.ack_early {
                    ctx.page.ack_frame(f.session_id)?;
                }
                times.push(f.received);
                on_frame(&img, f.received);
            }
        }
        Source::Screenshot { png, quality } => {
            while Instant::now() < end {
                let data = ctx.page.capture_screenshot(png, quality)?;
                let at = Instant::now();
                let img = pipe.process(&data)?;
                times.push(at);
                on_frame(&img, at);
            }
        }
    }
    Ok(times)
}

fn start_source(ctx: &Ctx, cfg: &Config) -> Result<()> {
    if let Source::Screencast(p) = cfg.source {
        ctx.page.start_screencast(p)?;
    }
    Ok(())
}

fn stop_source(ctx: &Ctx, cfg: &Config) -> Result<()> {
    if let Source::Screencast(_) = cfg.source {
        ctx.page.stop_screencast()?;
    }
    std::thread::sleep(Duration::from_millis(100));
    ctx.drain();
    Ok(())
}

/// Distinct frames per second (frames whose tiles changed at all).
fn fps(times: &[Instant], changed: &[usize], dur: Duration) -> (f64, f64) {
    let all = times.len() as f64 / dur.as_secs_f64();
    let distinct = changed.iter().filter(|&&c| c > 0).count() as f64 / dur.as_secs_f64();
    (all, distinct)
}

#[derive(Default, Debug)]
struct Row {
    name: String,
    frame_px: (u32, u32),
    idle_fps: f64,
    idle_cpu: f64,
    idle_cpu_noscreencast: f64,
    anim_fps: f64,
    anim_cpu: f64,
    scroll_fps: f64,
    scroll_fps_distinct: f64,
    scroll_cpu: f64,
    vk_cpu_scroll: f64,
    wheel_p50: f64,
    wheel_p95: f64,
    wheel_miss: usize,
    scroll_move_p50: f64,
    scroll_move_p95: f64,
    key_p50: f64,
    key_p95: f64,
    key_miss: usize,
    decode_ms: f64,
    diff_ms: f64,
    shm_encode_ms: f64,
    tiles_changed_frac_scroll: f64,
    jpeg_kb_per_frame: f64,
    shm_pty_bytes_per_frame: f64,
    shm_side_mb_s: f64,
    variants: Vec<(String, f64, f64)>, // (name, KB/frame, encode ms/frame)
}

fn phase_secs(quick: bool) -> f64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--phase")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(if quick { 2.0 } else { 5.0 })
}

fn self_cpu() -> f64 {
    // SAFETY: getrusage with a valid out pointer.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn run_config(ctx: &Ctx, cfg: &Config, quick: bool) -> Result<Row> {
    let pid = ctx.browser.pid();
    let phase = Duration::from_secs_f64(phase_secs(quick));
    let samples = if quick { 12 } else { 30 };
    let mut row = Row {
        name: cfg.name.clone(),
        ..Default::default()
    };
    let css_w = ctx.css.0;

    // --- Idle page: CPU without and with the frame source running.
    ctx.page.navigate(&ctx.srv.url("/idle"))?;
    std::thread::sleep(Duration::from_millis(500));
    let c0 = tree_cpu_seconds(pid)?;
    std::thread::sleep(phase);
    row.idle_cpu_noscreencast = (tree_cpu_seconds(pid)? - c0) / phase.as_secs_f64() * 100.0;
    start_source(ctx, cfg)?;
    let mut pipe = Pipeline::new(0);
    // Let the initial frames settle.
    pump_frames(ctx, cfg, &mut pipe, Duration::from_millis(500), |_, _| {})?;
    let mut pipe = Pipeline::new(0);
    let c0 = tree_cpu_seconds(pid)?;
    let t = pump_frames(ctx, cfg, &mut pipe, phase, |_, _| {})?;
    row.idle_cpu = (tree_cpu_seconds(pid)? - c0) / phase.as_secs_f64() * 100.0;
    row.idle_fps = t.len() as f64 / phase.as_secs_f64();
    stop_source(ctx, cfg)?;

    // --- CSS animation.
    ctx.page.navigate(&ctx.srv.url("/anim"))?;
    start_source(ctx, cfg)?;
    let mut pipe = Pipeline::new(0);
    pump_frames(ctx, cfg, &mut pipe, Duration::from_millis(300), |_, _| {})?;
    let mut pipe = Pipeline::new(0);
    let c0 = tree_cpu_seconds(pid)?;
    let t = pump_frames(ctx, cfg, &mut pipe, phase, |_, _| {})?;
    row.anim_cpu = (tree_cpu_seconds(pid)? - c0) / phase.as_secs_f64() * 100.0;
    row.anim_fps = fps(&t, &pipe.tiles_changed, phase).1;
    stop_source(ctx, cfg)?;

    // --- Scrolling at 60 Hz wheel events (40 px each), measuring fps, CPU, bytes.
    ctx.page.navigate(&ctx.srv.url("/scroll"))?;
    start_source(ctx, cfg)?;
    let mut pipe = Pipeline::new(40);
    pump_frames(ctx, cfg, &mut pipe, Duration::from_millis(300), |_, _| {})?;
    let mut pipe = Pipeline::new(40);
    let stop = Arc::new(AtomicBool::new(false));
    let wheel_page = ctx.page.clone();
    let stop2 = stop.clone();
    let (cx, cy) = (css_w as f64 / 2.0, ctx.css.1 as f64 / 2.0);
    let wheel = std::thread::spawn(move || {
        let mut dir = 1.0;
        let mut n = 0;
        while !stop2.load(Ordering::Relaxed) {
            let _ = wheel_page.wheel(cx, cy, 0.0, 40.0 * dir);
            n += 1;
            if n % 300 == 0 {
                dir = -dir;
            }
            std::thread::sleep(Duration::from_micros(16_667));
        }
    });
    let c0 = tree_cpu_seconds(pid)?;
    let s0 = self_cpu();
    let t = pump_frames(ctx, cfg, &mut pipe, phase, |_, _| {})?;
    row.scroll_cpu = (tree_cpu_seconds(pid)? - c0) / phase.as_secs_f64() * 100.0;
    row.vk_cpu_scroll = (self_cpu() - s0) / phase.as_secs_f64() * 100.0;
    stop.store(true, Ordering::Relaxed);
    wheel.join().ok();
    let (all, distinct) = fps(&t, &pipe.tiles_changed, phase);
    row.scroll_fps = all;
    row.scroll_fps_distinct = distinct;
    row.decode_ms = mean(&pipe.decode_ms);
    row.diff_ms = mean(&pipe.diff_ms);
    row.shm_encode_ms = mean(&pipe.encode_ms);
    row.tiles_changed_frac_scroll = mean(
        &pipe
            .tiles_changed
            .iter()
            .map(|&c| c as f64 / pipe.tiles_total.max(1) as f64)
            .collect::<Vec<_>>(),
    );
    row.jpeg_kb_per_frame = mean(
        &pipe
            .jpeg_bytes
            .iter()
            .map(|&b| b as f64 / 1024.0)
            .collect::<Vec<_>>(),
    );
    row.shm_pty_bytes_per_frame = mean(
        &pipe
            .shm_stats
            .iter()
            .map(|s| s.pty_bytes as f64)
            .collect::<Vec<_>>(),
    );
    let side: usize = pipe.shm_stats.iter().map(|s| s.side_bytes).sum();
    row.shm_side_mb_s = side as f64 / phase.as_secs_f64() / 1e6;
    if let Some(f) = pipe.kept.first() {
        row.frame_px = (f.width, f.height);
    }
    row.variants = encode_variants(&pipe.kept)?;
    stop_source(ctx, cfg)?;

    // --- Wheel latency: one wheel event, wait for the indicator to show its count.
    ctx.page.navigate(&ctx.srv.url("/scroll"))?;
    start_source(ctx, cfg)?;
    let mut dir = 1.0;
    let lat = latency(ctx, cfg, samples, true, |n| {
        if n % 10 == 0 {
            dir = -dir;
        }
        ctx.page.wheel(cx, cy, 0.0, 100.0 * dir).map(|_| ())
    })?;
    row.wheel_p50 = pct(&lat.indicator, 0.5);
    row.wheel_p95 = pct(&lat.indicator, 0.95);
    row.wheel_miss = lat.miss;
    row.scroll_move_p50 = pct(&lat.content, 0.5);
    row.scroll_move_p95 = pct(&lat.content, 0.95);
    stop_source(ctx, cfg)?;

    // --- Key latency: type a character, wait for the indicator.
    ctx.page.navigate(&ctx.srv.url("/input"))?;
    ctx.page.click(100.0, 75.0)?;
    start_source(ctx, cfg)?;
    let lat = latency(ctx, cfg, samples, false, |n| {
        let c = (b'a' + (n % 26) as u8) as char;
        let s = c.to_string();
        ctx.page
            .send(
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","key":s,"text":s,"code":format!("Key{}", c.to_ascii_uppercase()),"windowsVirtualKeyCode": c.to_ascii_uppercase() as u32}),
            )
            .map(|_| ())?;
        ctx.page
            .send(
                "Input.dispatchKeyEvent",
                json!({"type":"keyUp","key":s,"code":format!("Key{}", c.to_ascii_uppercase()),"windowsVirtualKeyCode": c.to_ascii_uppercase() as u32}),
            )
            .map(|_| ())
    })?;
    row.key_p50 = pct(&lat.indicator, 0.5);
    row.key_p95 = pct(&lat.indicator, 0.95);
    row.key_miss = lat.miss;
    stop_source(ctx, cfg)?;
    Ok(row)
}

/// Sampled pixels outside the indicator, to notice content movement (scrolling).
fn content_sig(img: &Rgba, css_w: u32) -> Vec<u8> {
    let scale = img.width as f64 / css_w as f64;
    let skip = (IND as f64 * scale * 1.5) as u32;
    let mut v = Vec::new();
    let mut y = skip;
    while y < img.height {
        let mut x = 0;
        while x < img.width {
            v.extend_from_slice(&img.pixel(x, y)[..3]);
            x += 23;
        }
        y += 17;
    }
    v
}

struct Latency {
    /// Input → first frame showing the indicator colour for this event (main-thread paint).
    indicator: Vec<f64>,
    /// Input → first frame whose content moved (compositor scroll), when `content` is set.
    content: Vec<f64>,
    miss: usize,
}

/// Input → first processed frame showing the expected indicator colour (event count mod 8),
/// and optionally the first frame whose content changed. Includes CDP transport, render,
/// capture, JPEG encode in Chromium, the pipe, base64 + decode + tile diff + shm encode here.
fn latency(
    ctx: &Ctx,
    cfg: &Config,
    samples: usize,
    content: bool,
    mut fire: impl FnMut(usize) -> Result<()>,
) -> Result<Latency> {
    let css_w = ctx.css.0;
    let mut pipe = Pipeline::new(0);
    let mut last: Option<Vec<u8>> = None;
    pump_frames(ctx, cfg, &mut pipe, Duration::from_millis(300), |img, _| {
        last = Some(content_sig(img, css_w));
    })?;
    let mut out = Latency {
        indicator: vec![],
        content: vec![],
        miss: 0,
    };
    for n in 1..=samples {
        let expect = n % 8;
        let base = last.clone();
        let t0 = Instant::now();
        fire(n)?;
        let mut hit = None;
        let mut moved = None;
        let deadline = t0 + Duration::from_millis(1000);
        while (hit.is_none() || (content && moved.is_none())) && Instant::now() < deadline {
            pump_frames(ctx, cfg, &mut pipe, Duration::from_millis(1), |img, _| {
                let now = Instant::now();
                if hit.is_none() && indicator(img, css_w) == Some(expect) {
                    hit = Some(now);
                }
                if content && moved.is_none() {
                    let sig = content_sig(img, css_w);
                    if base.as_ref().is_some_and(|b| *b != sig) {
                        moved = Some(now);
                    }
                }
            })?;
        }
        match hit {
            Some(t) => out.indicator.push(ms(t - t0)),
            None => out.miss += 1,
        }
        if let Some(t) = moved {
            out.content.push(ms(t - t0));
        }
        // Space samples out (and let smooth scrolling settle) before the next event.
        let pause = Duration::from_millis(250 + (n as u64 * 17) % 50);
        pump_frames(ctx, cfg, &mut pipe, pause, |img, _| {
            last = Some(content_sig(img, css_w));
        })?;
    }
    if out.indicator.is_empty() {
        bail!("{}: no latency samples matched", cfg.name);
    }
    Ok(out)
}

/// Offline: bytes and encode cost of alternative kitty encodings over the kept scroll frames.
fn encode_variants(frames: &[Rgba]) -> Result<Vec<(String, f64, f64)>> {
    if frames.is_empty() {
        return Ok(vec![]);
    }
    let tmp = std::env::temp_dir();
    let variants: Vec<(&str, Transfer, bool)> = vec![
        (
            "full rgba+zlib direct",
            Transfer::Direct {
                format: PixelFormat::Rgba,
                zlib: true,
            },
            false,
        ),
        (
            "full rgb+zlib direct",
            Transfer::Direct {
                format: PixelFormat::Rgb,
                zlib: true,
            },
            false,
        ),
        (
            "full png direct",
            Transfer::Direct {
                format: PixelFormat::Png,
                zlib: false,
            },
            false,
        ),
        (
            "tiles rgba+zlib direct",
            Transfer::Direct {
                format: PixelFormat::Rgba,
                zlib: true,
            },
            true,
        ),
        (
            "tiles png direct",
            Transfer::Direct {
                format: PixelFormat::Png,
                zlib: false,
            },
            true,
        ),
        ("tiles shm (pty)", Transfer::Shm, true),
        (
            "tiles temp file (pty)",
            Transfer::TempFile { dir: tmp },
            true,
        ),
    ];
    let mut out = Vec::new();
    for (name, transfer, tiled) in variants {
        let mut enc = TileEncoder::new(transfer, 1, CELL_W, CELL_H);
        let mut differ = if tiled {
            TileDiffer::cell_aligned(CELL_W, CELL_H, 4, 2)
        } else {
            // One tile = the whole frame.
            TileDiffer::new(frames[0].width, frames[0].height)
        };
        let mut bytes = Vec::new();
        let mut cost = Vec::new();
        for f in frames {
            let mut buf = Vec::new();
            let t0 = Instant::now();
            let changed = if tiled {
                differ.diff(f)
            } else {
                differ.tiles(f.width, f.height)
            };
            enc.encode(f, &changed, &mut buf)?;
            cost.push(ms(t0.elapsed()));
            bytes.push(buf.len() as f64 / 1024.0);
            enc.cleanup();
        }
        out.push((name.to_owned(), mean(&bytes), mean(&cost)));
    }
    Ok(out)
}

fn configs(quick: bool) -> Vec<Config> {
    let sc = |png, quality, max: Option<(u32, u32)>, nth| {
        Source::Screencast(ScreencastParams {
            png,
            quality,
            max_width: max.map(|m| m.0),
            max_height: max.map(|m| m.1),
            every_nth_frame: nth,
        })
    };
    let dev = Some((COLS * CELL_W, ROWS * CELL_H));
    let mut v = vec![
        Config {
            name: "screencast jpeg q80 (default size)".into(),
            source: sc(false, 80, None, 1),
            ack_early: true,
        },
        Config {
            name: "screencast jpeg q80 max=device px".into(),
            source: sc(false, 80, dev, 1),
            ack_early: true,
        },
        Config {
            name: "screencast jpeg q60 max=device px".into(),
            source: sc(false, 60, dev, 1),
            ack_early: true,
        },
        Config {
            name: "screencast png max=device px".into(),
            source: sc(true, 0, dev, 1),
            ack_early: true,
        },
        Config {
            name: "screencast jpeg q80 max=device, ack late".into(),
            source: sc(false, 80, dev, 1),
            ack_early: false,
        },
        Config {
            name: "screencast jpeg q80 max=device, nth=2".into(),
            source: sc(false, 80, dev, 2),
            ack_early: true,
        },
        Config {
            name: "screencast jpeg q80 max=800x640".into(),
            source: sc(false, 80, Some((800, 640)), 1),
            ack_early: true,
        },
        Config {
            name: "captureScreenshot jpeg q80 polling".into(),
            source: Source::Screenshot {
                png: false,
                quality: 80,
            },
            ack_early: true,
        },
        Config {
            name: "captureScreenshot png polling".into(),
            source: Source::Screenshot {
                png: true,
                quality: 0,
            },
            ack_early: true,
        },
    ];
    if quick {
        v.truncate(2);
    }
    v
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Run like an interactive client (P-cores); threads created later inherit the class.
    #[cfg(target_os = "macos")]
    if !args.iter().any(|a| a == "--default-qos") {
        // SAFETY: plain FFI call on the current thread.
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
    }
    let full = args.iter().any(|a| a == "--full");
    let quick = args.iter().any(|a| a == "--quick");
    let only = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let extra: Vec<&str> = args
        .iter()
        .position(|a| a == "--")
        .map(|i| args[i + 1..].iter().map(String::as_str).collect())
        .unwrap_or_default();
    // Screencast frames come out at CSS px unless the browser itself runs at the pane's DPR.
    let dsf = format!("--force-device-scale-factor={DPR}");
    let mut extra = extra;
    if !args.iter().any(|a| a == "--no-dsf") {
        extra.insert(0, &dsf);
    }
    let ctx = launch(full, &extra)?;
    let v = ctx.browser.version()?;
    println!(
        "# browser: {} ({}), binary {}",
        v["product"].as_str().unwrap_or("?"),
        if full {
            "full, --headless=new"
        } else {
            "headless shell"
        },
        discover_chromium(!full).unwrap().display()
    );
    println!(
        "# pane {COLS}x{ROWS} cells of {CELL_W}x{CELL_H} px, dpr {DPR} → viewport {}x{} CSS px, {}x{} device px; extra args {extra:?}",
        ctx.css.0,
        ctx.css.1,
        COLS * CELL_W,
        ROWS * CELL_H
    );
    let mut rows = Vec::new();
    for cfg in configs(quick) {
        if let Some(o) = &only
            && !cfg.name.contains(o.as_str())
        {
            continue;
        }
        eprintln!("running: {}", cfg.name);
        match run_config(&ctx, &cfg, quick) {
            Ok(r) => {
                print_row(&r);
                rows.push(r);
            }
            Err(e) => println!("{}: FAILED {e:#}", cfg.name),
        }
    }
    drop(ctx);
    // Sweep our temp profiles.
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            if e.file_name()
                .to_string_lossy()
                .starts_with("vk-browser-bench-")
            {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Ok(())
}

fn print_row(r: &Row) {
    println!("\n## {}", r.name);
    println!(
        "frame {}x{} | idle: {:.1} fps, CPU {:.1}% (no source: {:.1}%) | anim: {:.1} fps, CPU {:.1}%",
        r.frame_px.0,
        r.frame_px.1,
        r.idle_fps,
        r.idle_cpu,
        r.idle_cpu_noscreencast,
        r.anim_fps,
        r.anim_cpu
    );
    println!(
        "scroll: {:.1} fps ({:.1} distinct), Chromium CPU {:.1}%, bench CPU {:.1}%, tiles changed {:.0}%",
        r.scroll_fps,
        r.scroll_fps_distinct,
        r.scroll_cpu,
        r.vk_cpu_scroll,
        r.tiles_changed_frac_scroll * 100.0
    );
    println!(
        "latency wheel→content moved p50 {:.1} ms p95 {:.1} ms",
        r.scroll_move_p50, r.scroll_move_p95
    );
    println!(
        "latency wheel→indicator p50 {:.1} ms p95 {:.1} ms (miss {}) | key→pixel p50 {:.1} ms p95 {:.1} ms (miss {})",
        r.wheel_p50, r.wheel_p95, r.wheel_miss, r.key_p50, r.key_p95, r.key_miss
    );
    println!(
        "per frame: source {:.0} KB, decode {:.2} ms, diff {:.2} ms, shm encode {:.2} ms; shm: {:.0} B/frame on the PTY, {:.0} MB/s through shm",
        r.jpeg_kb_per_frame,
        r.decode_ms,
        r.diff_ms,
        r.shm_encode_ms,
        r.shm_pty_bytes_per_frame,
        r.shm_side_mb_s
    );
    for (n, kb, ms) in &r.variants {
        println!(
            "  kitty {n:<24} {kb:>8.1} KB/frame  {:>7.1} MB/s @scroll fps  encode {ms:>6.2} ms/frame",
            kb * r.scroll_fps_distinct / 1024.0
        );
    }
}
