//! Device presets shared by the headless browser (`--device`) and the browser pane: viewport,
//! device pixel ratio, touch/mobile emulation and user agent.

/// One emulated device. `width`/`height` are CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Device {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub dpr: f64,
    pub mobile: bool,
    pub user_agent: &'static str,
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
    },
    Device {
        name: "pixel-8",
        width: 412,
        height: 915,
        dpr: 2.625,
        mobile: true,
        user_agent: UA_PIXEL,
    },
    Device {
        name: "ipad",
        width: 820,
        height: 1180,
        dpr: 2.0,
        mobile: true,
        user_agent: UA_IPAD,
    },
    Device {
        name: "desktop-1280",
        width: 1280,
        height: 800,
        dpr: 1.0,
        mobile: false,
        user_agent: UA_DESKTOP,
    },
    Device {
        name: "desktop-1440",
        width: 1440,
        height: 900,
        dpr: 1.0,
        mobile: false,
        user_agent: UA_DESKTOP,
    },
    Device {
        name: "desktop-1920",
        width: 1920,
        height: 1080,
        dpr: 1.0,
        mobile: false,
        user_agent: UA_DESKTOP,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_forgiving() {
        assert_eq!(preset("iphone-15").unwrap().width, 393);
        assert_eq!(preset("iPhone 15").unwrap().name, "iphone-15");
        assert_eq!(preset("pixel_8").unwrap().dpr, 2.625);
        assert!(preset("desktop-1920").is_some_and(|d| !d.mobile));
        assert!(preset("nokia").is_none() && preset("").is_none());
    }

    #[test]
    fn presets_are_sane() {
        for d in PRESETS {
            assert!(
                d.width >= 100 && d.height >= 100 && d.dpr >= 1.0,
                "{}",
                d.name
            );
            assert!(!d.user_agent.is_empty());
        }
    }
}
