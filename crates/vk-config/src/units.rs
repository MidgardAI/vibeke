//! Small typed parsers for size strings, durations and port ranges.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

fn split_num(s: &str) -> Result<(f64, &str), String> {
    let s = s.trim();
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (n, u) = s.split_at(idx);
    if n.is_empty() {
        return Err(format!("expected a number in `{s}`"));
    }
    let v: f64 = n.parse().map_err(|_| format!("invalid number `{n}`"))?;
    Ok((v, u.trim()))
}

/// A byte count parsed from strings such as `"200MiB"`, `"8G"`, `"50MB"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const fn mib(n: u64) -> Self {
        ByteSize(n << 20)
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let (v, unit) = split_num(s)?;
        let mult: u64 = match unit.to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "k" | "kib" => 1 << 10,
            "m" | "mib" => 1 << 20,
            "g" | "gib" => 1 << 30,
            "t" | "tib" => 1 << 40,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "tb" => 1_000_000_000_000,
            other => return Err(format!("unknown size unit `{other}` in `{s}`")),
        };
        Ok(ByteSize((v * mult as f64).round() as u64))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        for (shift, name) in [(40, "TiB"), (30, "GiB"), (20, "MiB"), (10, "KiB")] {
            if b > 0 && b.is_multiple_of(1u64 << shift) {
                return write!(f, "{}{}", b >> shift, name);
            }
        }
        write!(f, "{b}B")
    }
}

/// A duration parsed from strings such as `"14d"`, `"30m"`, `"500ms"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dur(pub Duration);

impl Dur {
    pub const fn secs(n: u64) -> Self {
        Dur(Duration::from_secs(n))
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let (v, unit) = split_num(s)?;
        let ms: f64 = match unit {
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            "w" => 604_800_000.0,
            "" => return Err(format!("missing duration unit (ms/s/m/h/d/w) in `{s}`")),
            other => return Err(format!("unknown duration unit `{other}` in `{s}`")),
        };
        Ok(Dur(Duration::from_millis((v * ms).round() as u64)))
    }
}

impl fmt::Display for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis() as u64;
        if ms == 0 {
            return write!(f, "0s");
        }
        for (div, name) in [
            (604_800_000, "w"),
            (86_400_000, "d"),
            (3_600_000, "h"),
            (60_000, "m"),
            (1_000, "s"),
        ] {
            if ms.is_multiple_of(div) {
                return write!(f, "{}{}", ms / div, name);
            }
        }
        write!(f, "{ms}ms")
    }
}

/// An inclusive port range such as `"20000-29999"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (a, b) = s
            .split_once('-')
            .ok_or_else(|| format!("expected `START-END`, got `{s}`"))?;
        let start: u16 = a
            .trim()
            .parse()
            .map_err(|_| format!("invalid port `{}`", a.trim()))?;
        let end: u16 = b
            .trim()
            .parse()
            .map_err(|_| format!("invalid port `{}`", b.trim()))?;
        if start == 0 {
            return Err("port 0 is not allowed".into());
        }
        if start > end {
            return Err(format!("range start {start} is greater than end {end}"));
        }
        Ok(PortRange { start, end })
    }

    /// Number of ports in the range (always at least one).
    pub fn len(&self) -> u32 {
        self.end as u32 - self.start as u32 + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

macro_rules! str_serde {
    ($t:ty, $exp:literal) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> de::Visitor<'de> for V {
                    type Value = $t;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str($exp)
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$t, E> {
                        <$t>::parse(v).map_err(E::custom)
                    }
                    fn visit_i64<E: de::Error>(self, v: i64) -> Result<$t, E> {
                        <$t>::parse(&v.to_string()).map_err(E::custom)
                    }
                }
                d.deserialize_any(V)
            }
        }
    };
}

str_serde!(ByteSize, "a size string such as \"200MiB\"");
str_serde!(Dur, "a duration string such as \"14d\"");
str_serde!(PortRange, "a port range such as \"20000-29999\"");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(ByteSize::parse("200MiB").unwrap().0, 200 << 20);
        assert_eq!(ByteSize::parse("8G").unwrap().0, 8 << 30);
        assert_eq!(ByteSize::parse("50MB").unwrap().0, 50_000_000);
        assert_eq!(ByteSize::parse("1.5KiB").unwrap().0, 1536);
        assert!(ByteSize::parse("12 parsecs").is_err());
        assert!(ByteSize::parse("MiB").is_err());
        assert_eq!(ByteSize::mib(200).to_string(), "200MiB");
        assert_eq!(ByteSize(1000).to_string(), "1000B");
    }

    #[test]
    fn durations() {
        assert_eq!(Dur::parse("14d").unwrap(), Dur::secs(14 * 86400));
        assert_eq!(Dur::parse("30m").unwrap(), Dur::secs(1800));
        assert_eq!(Dur::parse("500ms").unwrap().0.as_millis(), 500);
        assert!(Dur::parse("30").is_err());
        assert!(Dur::parse("3x").is_err());
        assert_eq!(Dur::secs(86400 * 14).to_string(), "2w");
        assert_eq!(Dur::secs(1800).to_string(), "30m");
        assert_eq!(Dur::parse("1500ms").unwrap().to_string(), "1500ms");
    }

    #[test]
    fn ports() {
        let r = PortRange::parse("20000-29999").unwrap();
        assert_eq!((r.start, r.end, r.len()), (20000, 29999, 10000));
        assert!(PortRange::parse("30000-20000").is_err());
        assert!(PortRange::parse("0-10").is_err());
        assert!(PortRange::parse("20000").is_err());
        assert!(PortRange::parse("1-70000").is_err());
        assert_eq!(r.to_string(), "20000-29999");
    }
}
