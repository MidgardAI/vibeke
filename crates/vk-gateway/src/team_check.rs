//! Devices that a team let in (spec 20 §2.5) keep their access only while their GitHub account
//! is still in one of the share's teams. Membership is confirmed through the host account
//! (`GET /v1/teams/{id}`): when such a device claims again or reconnects and the last
//! confirmation is older than [`RECHECK`], and every [`SWEEP`] for every such device. A device
//! whose account left the team is revoked. When the account server cannot answer, a stale device
//! is refused and its open sessions are closed (fail closed), but the device is kept, so it works
//! again once membership can be confirmed.

use crate::Gateway;
use crate::api::ApiError;
use crate::state::Device;
use std::sync::Arc;
use std::time::Duration;

/// How old a confirmation may be before a reconnect or claim checks again.
pub const RECHECK: u64 = 10 * 60;
/// How often connected devices are checked.
pub const SWEEP: Duration = Duration::from_secs(5 * 60);

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether `device` needs a fresh confirmation at `now`.
pub fn stale(device: &Device, now: u64) -> bool {
    device
        .team_check
        .as_ref()
        .is_some_and(|c| now.saturating_sub(c.checked_at) >= RECHECK)
}

/// Is the device's GitHub account in one of its teams now? `Err` when the account server could
/// not say.
async fn member(gw: &Gateway, device: &Device) -> Result<bool, ApiError> {
    let (Some(check), Some(gh)) = (&device.team_check, &device.github) else {
        return Ok(true);
    };
    let server = crate::account::account_server(&gw.cfg);
    let acct = crate::account::account(&server, &gw.state.dir)
        .map_err(|e| ApiError::unavailable(format!("account server: {e:#}")))?;
    for team in &check.teams {
        let ids = acct
            .team_member_ids(team)
            .await
            .map_err(|e| ApiError::unavailable(format!("checking team membership: {e}")))?;
        if ids.contains(&gh.id) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Confirm a team-admitted device when its last confirmation is stale: refresh the time when
/// it is still a member, revoke it when not, refuse (keep) it when the account server cannot
/// answer. Other devices pass untouched.
pub async fn confirm(gw: &Arc<Gateway>, device: &Device) -> Result<(), ApiError> {
    if !stale(device, now_s()) {
        return Ok(());
    }
    match member(gw, device).await? {
        true => {
            let now = now_s();
            let _ = gw.update_device(&device.id, |d| {
                if let Some(c) = d.team_check.as_mut() {
                    c.checked_at = now;
                }
            });
            Ok(())
        }
        false => {
            let _ = gw.revoke(&device.id).await;
            let mut e = ApiError::new("revoked", "you are no longer in the team this link is for");
            e.details = serde_json::json!({});
            Err(e)
        }
    }
}

/// Check every team-admitted device every [`SWEEP`]; a device whose account left the team is
/// revoked (which also closes its connections).
pub async fn run(gw: Arc<Gateway>) {
    loop {
        tokio::time::sleep(SWEEP).await;
        let now = now_s();
        let due: Vec<Device> = gw.devices().into_iter().filter(|d| stale(d, now)).collect();
        for d in due {
            // Left the team: `confirm` revoked it. The account server could not answer: close
            // its open sessions too (fail closed) but keep the device; reconnects are refused
            // until membership can be confirmed again.
            if let Err(e) = confirm(&gw, &d).await
                && e.kind != "revoked"
            {
                gw.disconnect(&d.id);
            }
        }
    }
}
