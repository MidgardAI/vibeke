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

/// The longest one membership lookup may take.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The members of `team` now, through the host account, within [`LOOKUP_TIMEOUT`].
async fn members_of(acct: &vk_account::Account, team: &str) -> Result<Vec<u64>, ApiError> {
    tokio::time::timeout(LOOKUP_TIMEOUT, acct.team_member_ids(team))
        .await
        .map_err(|_| ApiError::unavailable("checking team membership timed out"))?
        .map_err(|e| ApiError::unavailable(format!("checking team membership: {e}")))
}

fn host_account(gw: &Gateway) -> Result<vk_account::Account, ApiError> {
    let server = crate::account::account_server(&gw.cfg);
    crate::account::account(&server, &gw.state.dir)
        .map_err(|e| ApiError::unavailable(format!("account server: {e:#}")))
}

/// Is the device's GitHub account in one of its teams? `lookup` answers per team; `Err` when
/// no team said yes and at least one could not be asked.
async fn member<F, Fut>(device: &Device, mut lookup: F) -> Result<bool, ApiError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u64>, ApiError>>,
{
    let (Some(check), Some(gh)) = (&device.team_check, &device.github) else {
        return Ok(true);
    };
    let mut failed = None;
    for team in &check.teams {
        match lookup(team.clone()).await {
            Ok(ids) if ids.contains(&gh.id) => return Ok(true),
            Ok(_) => {}
            Err(e) => failed = Some(e),
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(false),
    }
}

/// Act on a membership answer for a stale device: refresh the time when it is still a member,
/// revoke it when not (its sessions close either way), refuse it when the account server could
/// not answer.
async fn settle(
    gw: &Arc<Gateway>,
    device: &Device,
    answer: Result<bool, ApiError>,
) -> Result<(), ApiError> {
    match answer? {
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
            // Close its sessions even if saving the revocation fails.
            gw.disconnect(&device.id);
            let _ = gw.revoke(&device.id).await;
            Err(ApiError::new(
                "revoked",
                "you are no longer in the team this link is for",
            ))
        }
    }
}

/// Confirm a team-admitted device when its last confirmation is stale (reconnects and claims).
/// Other devices pass untouched.
pub async fn confirm(gw: &Arc<Gateway>, device: &Device) -> Result<(), ApiError> {
    if !stale(device, now_s()) {
        return Ok(());
    }
    let acct = host_account(gw)?;
    let answer = member(device, |t| {
        let acct = acct.clone();
        async move { members_of(&acct, &t).await }
    })
    .await;
    settle(gw, device, answer).await
}

/// Check every team-admitted device every [`SWEEP`]. Each team is asked once per sweep, with a
/// short timeout, so an outage cannot stretch the sweep. A device whose account left the team is
/// revoked; when membership cannot be confirmed its sessions are closed (fail closed) and the
/// device kept.
pub async fn run(gw: Arc<Gateway>) {
    loop {
        tokio::time::sleep(SWEEP).await;
        sweep(&gw).await;
    }
}

pub(crate) async fn sweep(gw: &Arc<Gateway>) {
    let now = now_s();
    let due: Vec<Device> = gw.devices().into_iter().filter(|d| stale(d, now)).collect();
    if due.is_empty() {
        return;
    }
    let acct = match host_account(gw) {
        Ok(a) => a,
        Err(_) => {
            for d in &due {
                gw.disconnect(&d.id);
            }
            return;
        }
    };
    let mut teams: Vec<String> = due
        .iter()
        .filter_map(|d| d.team_check.as_ref())
        .flat_map(|c| c.teams.iter().cloned())
        .collect();
    teams.sort();
    teams.dedup();
    let answers: std::collections::HashMap<String, Result<Vec<u64>, ApiError>> =
        futures::future::join_all(teams.into_iter().map(|t| {
            let acct = acct.clone();
            async move {
                let r = members_of(&acct, &t).await;
                (t, r)
            }
        }))
        .await
        .into_iter()
        .collect();
    for d in due {
        let answer = member(&d, |t| {
            let r = answers
                .get(&t)
                .cloned()
                .unwrap_or_else(|| Err(ApiError::unavailable("not asked")));
            async move { r }
        })
        .await;
        if let Err(e) = settle(gw, &d, answer).await
            && e.kind != "revoked"
        {
            gw.disconnect(&d.id);
        }
    }
}
