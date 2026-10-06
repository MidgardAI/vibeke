//! Preview lifecycle (06 B2): `suggested|declared → up ⇄ down → gone`. Pure functions over
//! an explicit `now`, so timing is testable without sleeping.

use vk_proto::model::{Preview, PreviewSource, PreviewStatus};

/// A discovered preview absent this long is gone (declared ones stay `down`).
pub const GONE_AFTER_MS: i64 = 60_000;

/// Apply one presence observation. Returns the event to emit, if the status changed.
pub fn observe(p: &mut Preview, present: bool, now: i64) -> Option<&'static str> {
    if p.status == PreviewStatus::Gone {
        return None;
    }
    if present {
        p.last_seen_ms = now;
        return match p.status {
            PreviewStatus::Declared | PreviewStatus::Down => {
                p.status = PreviewStatus::Up;
                Some("preview.up")
            }
            _ => None,
        };
    }
    let absent_for = now - p.last_seen_ms;
    match p.status {
        PreviewStatus::Declared | PreviewStatus::Up => {
            p.status = PreviewStatus::Down;
            Some("preview.down")
        }
        PreviewStatus::Suggested if absent_for >= GONE_AFTER_MS => {
            p.status = PreviewStatus::Gone;
            Some("preview.gone")
        }
        PreviewStatus::Down
            if p.source != PreviewSource::Declared && absent_for >= GONE_AFTER_MS =>
        {
            p.status = PreviewStatus::Gone;
            Some("preview.gone")
        }
        _ => None,
    }
}

/// A suggestion confirmed by the user or an agent (opened, promoted, declared on its port).
pub fn promote(p: &mut Preview) -> bool {
    if p.status == PreviewStatus::Suggested {
        p.status = PreviewStatus::Up;
        return true;
    }
    false
}

/// Pane closed or task removed.
pub fn retire(p: &mut Preview) -> Option<&'static str> {
    if p.status == PreviewStatus::Gone {
        return None;
    }
    p.status = PreviewStatus::Gone;
    Some("preview.gone")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(status: PreviewStatus, source: PreviewSource, t: i64) -> Preview {
        Preview {
            id: "x".into(),
            handle: "v1".into(),
            machine: "local".into(),
            pane: None,
            task: None,
            port: 5173,
            path: "/".into(),
            label: None,
            url: "http://localhost:5173/".into(),
            scheme: "http".into(),
            status,
            source,
            pid: None,
            first_seen_ms: t,
            last_seen_ms: t,
        }
    }

    #[test]
    fn declared_goes_up_down_and_never_gone() {
        let mut p = preview(PreviewStatus::Declared, PreviewSource::Declared, 0);
        assert_eq!(observe(&mut p, false, 100), Some("preview.down"));
        assert_eq!(observe(&mut p, false, 10 * GONE_AFTER_MS), None);
        assert_eq!(p.status, PreviewStatus::Down);
        assert_eq!(
            observe(&mut p, true, 10 * GONE_AFTER_MS + 1),
            Some("preview.up")
        );
        assert_eq!(observe(&mut p, true, 10 * GONE_AFTER_MS + 2), None);
        let mut q = preview(PreviewStatus::Declared, PreviewSource::Declared, 0);
        assert_eq!(observe(&mut q, true, 5), Some("preview.up"));
        assert_eq!(q.last_seen_ms, 5);
    }

    #[test]
    fn suggestion_expires_after_sixty_seconds_absent() {
        let t0 = 1_000_000;
        let mut p = preview(PreviewStatus::Suggested, PreviewSource::Listener, t0);
        assert_eq!(observe(&mut p, true, t0 + 2_000), None);
        assert_eq!(p.status, PreviewStatus::Suggested);
        // Absent: still a suggestion until 60 s after it was last seen.
        assert_eq!(observe(&mut p, false, t0 + 4_000), None);
        assert_eq!(observe(&mut p, false, t0 + 2_000 + GONE_AFTER_MS - 1), None);
        // Seen again in between resets the clock.
        assert_eq!(observe(&mut p, true, t0 + 30_000), None);
        assert_eq!(
            observe(&mut p, false, t0 + 30_000 + GONE_AFTER_MS - 1),
            None
        );
        assert_eq!(
            observe(&mut p, false, t0 + 30_000 + GONE_AFTER_MS),
            Some("preview.gone")
        );
        assert_eq!(observe(&mut p, true, t0 + 200_000), None, "gone is final");
    }

    #[test]
    fn promoted_discovery_goes_down_then_gone() {
        let mut p = preview(PreviewStatus::Suggested, PreviewSource::Banner, 0);
        assert!(promote(&mut p));
        assert!(!promote(&mut p));
        assert_eq!(p.status, PreviewStatus::Up);
        assert_eq!(observe(&mut p, false, 2_000), Some("preview.down"));
        assert_eq!(observe(&mut p, true, 4_000), Some("preview.up"));
        assert_eq!(observe(&mut p, false, 6_000), Some("preview.down"));
        assert_eq!(
            observe(&mut p, false, 4_000 + GONE_AFTER_MS),
            Some("preview.gone")
        );
        assert_eq!(retire(&mut p), None);
        let mut d = preview(PreviewStatus::Up, PreviewSource::Declared, 0);
        assert_eq!(retire(&mut d), Some("preview.gone"));
    }
}
