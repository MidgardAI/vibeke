//! Device presets and pinned viewports, shared by the agents' headless browser (`--device`,
//! 06 B5) and the browser pane (06 B3.2 "`--viewport 390x844` / `--device iphone-15` pins a
//! device size and letterboxes it"): viewport, device pixel ratio, mobile/touch emulation and
//! user agent. [`preset`] looks a name up, [`PRESETS`] lists them. A pinned viewport that
//! doesn't match the pane's aspect is shown centred on a neutral fill ([`Letterbox`]).

use crate::frame::{Rgba, scale_to_fit};
use serde_json::{Value, json};

/// One emulated device. `width`/`height` are CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Device {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub dpr: f64,
    pub mobile: bool,
    pub user_agent: &'static str,
    /// Touch events (`Emulation.setTouchEmulationEnabled`).
    pub touch: bool,
}

const UA_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
const UA_IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
const UA_PIXEL: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36";
const UA_DESKTOP: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

pub const PRESETS: &[Device] = &[
    Device {
        name: "iphone-15",
        width: 393,
        height: 852,
        dpr: 3.0,
        mobile: true,
        user_agent: UA_IPHONE,
        touch: true,
    },
    Device {
        name: "pixel-8",
        width: 412,
        height: 915,
        dpr: 2.625,
        mobile: true,
        user_agent: UA_PIXEL,
        touch: true,
    },
    Device {
        name: "ipad",
        width: 820,
        height: 1180,
        dpr: 2.0,
        mobile: true,
        user_agent: UA_IPAD,
        touch: true,
    },
    Device {
        name: "desktop-1280",
        width: 1280,
        height: 800,
        dpr: 1.0,
        mobile: false,
        user_agent: UA_DESKTOP,
        touch: false,
    },
    Device {
        name: "desktop-1440",
        width: 1440,
        height: 900,
        dpr: 1.0,
        mobile: false,
        user_agent: UA_DESKTOP,
        touch: false,
    },
    Device {
        name: "desktop-1920",
        width: 1920,
        height: 1080,
        dpr: 1.0,
        mobile: false,
        user_agent: UA_DESKTOP,
        touch: false,
    },
];

/// Look a preset up by name (case-insensitive; `_`, spaces and a missing dash are tolerated:
/// `iPhone 15`, `iphone15`, `pixel_8`).
pub fn preset(name: &str) -> Option<Device> {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    let want = norm(name);
    if want.is_empty() {
        return None;
    }
    PRESETS.iter().copied().find(|d| norm(d.name) == want)
}

/// Preset names for error messages.
pub fn names() -> Vec<&'static str> {
    PRESETS.iter().map(|d| d.name).collect()
}

/// Smallest and largest CSS edge a pinned viewport may have.
pub const MIN_EDGE: u32 = 64;
pub const MAX_EDGE: u32 = 8192;

/// `390x844` (also `390×844`, `390X844`) → CSS size, bounded by [`MIN_EDGE`]..=[`MAX_EDGE`].
pub fn parse_viewport(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let (w, h) = s
        .split_once('x')
        .or_else(|| s.split_once('X'))
        .or_else(|| s.split_once('×'))?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    let ok = |v: u32| (MIN_EDGE..=MAX_EDGE).contains(&v);
    (ok(w) && ok(h)).then_some((w, h))
}

/// What a pinned browser viewport emulates: a preset, or a bare size (`--viewport`) that keeps
/// the host's DPR and a desktop user agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    pub width: u32,
    pub height: u32,
    /// `None` = the host's DPR (a bare `--viewport`).
    pub dpr: Option<f64>,
    pub mobile: bool,
    pub touch: bool,
    pub user_agent: Option<String>,
    /// Preset name, if any (screenshots record it).
    pub device: Option<String>,
}

impl Pin {
    /// From a device name or a `WxH` viewport (the device wins when both are given). Unknown
    /// names and malformed sizes give `None` (a newer owner's preset on an older media host).
    pub fn from_spec(device: Option<&str>, viewport: Option<&str>) -> Option<Pin> {
        if let Some(d) = device.filter(|d| !d.is_empty()).and_then(preset) {
            return Some(Pin::from(d));
        }
        let (w, h) = viewport
            .filter(|v| !v.is_empty())
            .and_then(parse_viewport)?;
        Some(Pin {
            width: w,
            height: h,
            dpr: None,
            mobile: false,
            touch: false,
            user_agent: None,
            device: None,
        })
    }

    /// `Emulation.setDeviceMetricsOverride` params (`host_dpr` when the pin has none).
    pub fn metrics(&self, host_dpr: f64) -> Value {
        json!({
            "width": self.width, "height": self.height,
            "deviceScaleFactor": self.dpr.unwrap_or(host_dpr),
            "mobile": self.mobile,
            "screenWidth": self.width, "screenHeight": self.height,
        })
    }
}

impl From<Device> for Pin {
    fn from(d: Device) -> Pin {
        Pin {
            width: d.width,
            height: d.height,
            dpr: Some(d.dpr),
            mobile: d.mobile,
            touch: d.touch,
            // A desktop pin keeps the browser's own user agent; phones/tablets get theirs.
            user_agent: d.mobile.then(|| d.user_agent.to_string()),
            device: Some(d.name.to_string()),
        }
    }
}

/// Neutral fill around a letterboxed viewport (a dark grey that reads as "not the page" on
/// light and dark pages alike).
pub const FILL: [u8; 4] = [38, 38, 40, 255];

/// Where a pinned CSS viewport goes inside a pane's content area (device pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    /// Content area (device px).
    pub area: (u32, u32),
    /// The page rectangle inside it: x, y, w, h (device px), centred.
    pub rect: (u32, u32, u32, u32),
    /// Device px per CSS px of the page.
    pub scale: f64,
    /// The pinned CSS viewport.
    pub css: (u32, u32),
}

impl Letterbox {
    /// Fit `css` into `area` keeping its aspect, never above `max_scale` device px per CSS px
    /// (the host's DPR: a phone in a big pane is shown 1:1, not blown up), centred.
    pub fn fit(area: (u32, u32), css: (u32, u32), max_scale: f64) -> Letterbox {
        let (aw, ah) = (area.0.max(1), area.1.max(1));
        let (cw, ch) = (css.0.max(1), css.1.max(1));
        let mut s = (aw as f64 / cw as f64).min(ah as f64 / ch as f64);
        if max_scale > 0.0 {
            s = s.min(max_scale);
        }
        let w = ((cw as f64 * s).round() as u32).clamp(1, aw);
        let h = ((ch as f64 * s).round() as u32).clamp(1, ah);
        Letterbox {
            area: (aw, ah),
            rect: ((aw - w) / 2, (ah - h) / 2, w, h),
            scale: s,
            css: (cw, ch),
        }
    }

    /// The page rectangle exactly fills the content area (nothing to letterbox).
    pub fn is_full(&self) -> bool {
        self.rect.2 == self.area.0 && self.rect.3 == self.area.1
    }

    /// A content-area device pixel → page CSS px; `None` outside the page rectangle.
    pub fn to_page(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (rx, ry, rw, rh) = self.rect;
        let (lx, ly) = (x - rx as f64, y - ry as f64);
        if lx < 0.0 || ly < 0.0 || lx >= rw as f64 || ly >= rh as f64 {
            return None;
        }
        Some((lx / self.scale, ly / self.scale))
    }

    /// Like [`Letterbox::to_page`], clamped to the page (drag/release that left the page).
    pub fn to_page_clamped(&self, x: f64, y: f64) -> (f64, f64) {
        let (rx, ry, rw, rh) = self.rect;
        let lx = (x - rx as f64).clamp(0.0, (rw.max(1) - 1) as f64);
        let ly = (y - ry as f64).clamp(0.0, (rh.max(1) - 1) as f64);
        (lx / self.scale, ly / self.scale)
    }

    /// The frame placed in the content area: scaled to the page rectangle (whatever size the
    /// browser produced), centred, the rest [`FILL`].
    pub fn compose(&self, frame: &Rgba) -> Rgba {
        let (aw, ah) = self.area;
        let (rx, ry, rw, rh) = self.rect;
        let img = scale_to_fit(frame, rw, rh);
        let mut out = Rgba::new(aw, ah);
        for px in out.data.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&FILL);
        }
        // Centre what scale_to_fit produced inside the rectangle (aspect rounding).
        let ox = rx + (rw.saturating_sub(img.width)) / 2;
        let oy = ry + (rh.saturating_sub(img.height)) / 2;
        let w = img.width.min(aw.saturating_sub(ox)) as usize;
        for y in 0..img.height.min(ah.saturating_sub(oy)) {
            let src = (y * img.width) as usize * 4;
            let dst = ((oy + y) * aw + ox) as usize * 4;
            out.data[dst..dst + w * 4].copy_from_slice(&img.data[src..src + w * 4]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_and_names() {
        let p = preset("iPhone-15").unwrap();
        assert_eq!((p.width, p.height, p.dpr, p.mobile), (393, 852, 3.0, true));
        assert!(p.user_agent.contains("iPhone") && p.touch);
        assert_eq!(preset("pixel_8").unwrap().name, "pixel-8");
        assert_eq!(preset("desktop-1440").unwrap().width, 1440);
        assert!(!preset("desktop-1920").unwrap().mobile);
        assert!(preset("nokia-3310").is_none() && preset("").is_none());
        assert_eq!(preset("iPhone 15").unwrap().name, "iphone-15");
        assert_eq!(Pin::from(preset("desktop-1280").unwrap()).user_agent, None);
        assert_eq!(names().len(), PRESETS.len());
        for d in PRESETS {
            assert_eq!(preset(d.name), Some(*d));
        }
    }

    #[test]
    fn viewports_parse_and_bound() {
        assert_eq!(parse_viewport("390x844"), Some((390, 844)));
        assert_eq!(parse_viewport(" 1280 × 800 "), Some((1280, 800)));
        assert_eq!(parse_viewport("1x1"), None);
        assert_eq!(parse_viewport("390"), None);
        assert_eq!(parse_viewport("99999x10"), None);
        let pin = Pin::from_spec(None, Some("390x844")).unwrap();
        assert_eq!(
            (pin.width, pin.dpr, pin.user_agent.as_deref()),
            (390, None, None)
        );
        assert_eq!(pin.metrics(2.0)["deviceScaleFactor"], 2.0);
        let pin = Pin::from_spec(Some("ipad"), Some("390x844")).unwrap();
        assert_eq!((pin.width, pin.device.as_deref()), (820, Some("ipad")));
        assert_eq!(pin.metrics(1.0)["mobile"], true);
        assert!(Pin::from_spec(Some("unknown"), None).is_none());
    }

    #[test]
    fn letterbox_centres_and_maps_input() {
        // A phone (393×852) in a wide 1600×800 area at DPR 2: height-bound, 1600 not used.
        let lb = Letterbox::fit((1600, 800), (393, 852), 2.0);
        let s = 800.0 / 852.0;
        assert!((lb.scale - s).abs() < 1e-9);
        let w = (393.0 * s).round() as u32;
        assert_eq!(lb.rect, ((1600 - w) / 2, 0, w, 800));
        assert!(!lb.is_full());
        // The page centre maps to the CSS centre; the margin maps to nothing.
        let (cx, cy) = lb
            .to_page(lb.rect.0 as f64 + w as f64 / 2.0, 400.0)
            .unwrap();
        assert!(
            (cx - 196.5).abs() < 1.0 && (cy - 426.0).abs() < 1.0,
            "{cx},{cy}"
        );
        assert!(lb.to_page(5.0, 400.0).is_none());
        assert_eq!(lb.to_page_clamped(5.0, 400.0).0, 0.0);
        // Never above the host DPR: a desktop page in a huge area is shown 1:1 device px.
        let lb = Letterbox::fit((4000, 3000), (1280, 800), 2.0);
        assert_eq!(lb.scale, 2.0);
        assert_eq!(lb.rect, ((4000 - 2560) / 2, (3000 - 1600) / 2, 2560, 1600));
        // Exact fit: nothing to letterbox.
        assert!(Letterbox::fit((780, 1688), (390, 844), 2.0).is_full());
    }

    #[test]
    fn compose_fills_the_margins_and_scales_the_frame() {
        let lb = Letterbox::fit((100, 40), (20, 20), 4.0);
        assert_eq!(lb.rect, (30, 0, 40, 40));
        let mut f = Rgba::new(10, 10);
        f.fill_rect(0, 0, 10, 10, [200, 10, 10, 255]);
        let out = lb.compose(&f);
        assert_eq!((out.width, out.height), (100, 40));
        assert_eq!(out.pixel(5, 20), FILL);
        assert_eq!(out.pixel(95, 20), FILL);
        let mid = out.pixel(50, 20);
        assert!(mid[0] > 150 && mid[1] < 60, "{mid:?}");
        // A frame bigger than the rectangle is scaled down into it.
        let mut big = Rgba::new(400, 400);
        big.fill_rect(0, 0, 400, 400, [10, 200, 10, 255]);
        let out = lb.compose(&big);
        assert_eq!(out.pixel(29, 0), FILL);
        assert!(out.pixel(31, 1)[1] > 150);
    }
}
