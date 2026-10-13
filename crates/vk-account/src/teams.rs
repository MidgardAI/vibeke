//! Teams and the sharing defaults on the account server (spec 20 §2.5, §6.4).
//!
//! A team is a Vibeke group on the account server. Everyone in a team may open the GitHub links a
//! teammate shares with that team. Teams are named by id (a lowercase UUID); names are for
//! display only and never authorize anything.
//!
//! - `GET /v1/teams` → `{teams: [{id, name, role, members}]}`: the caller's teams.
//! - `GET /v1/sharing` → `{people: [{id, login}], teams: [{id, name}]}`: who a GitHub link is
//!   for when the caller names nobody. `teams` are the caller's teams ticked "share with by
//!   default" on the account page.
//!
//! An account server without these routes answers 404, which counts as no teams.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attest::GithubUser;
use crate::{Account, Client, Error, Result};

/// One of the caller's teams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    /// `owner` or `member`.
    #[serde(default)]
    pub role: String,
    /// How many people are in the team.
    #[serde(default)]
    pub members: u64,
}

/// A team as a link names it: the id counts, the name is for display.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRef {
    pub id: String,
    pub name: String,
}

/// `GET /v1/sharing`: who a GitHub link is for by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sharing {
    pub people: Vec<GithubUser>,
    pub teams: Vec<TeamRef>,
}

/// A team id as the account server makes them: a lowercase, hyphenated UUID.
pub fn valid_team_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// The `teams` list of an answer: entries with a valid id and a string name. Anything else is
/// skipped, so one bad entry cannot name another team.
fn parse_list<T: serde::de::DeserializeOwned>(v: Option<&Value>, id_of: fn(&T) -> &str) -> Vec<T> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| serde_json::from_value::<T>(t.clone()).ok())
        .filter(|t| valid_team_id(id_of(t)))
        .collect()
}

/// The answer of `GET /v1/teams`.
pub fn parse_teams(v: &Value) -> Vec<Team> {
    parse_list(v.get("teams"), |t: &Team| t.id.as_str())
}

/// The answer of `GET /v1/sharing`. An older server sends `people` only.
pub fn parse_sharing(v: &Value) -> Result<Sharing> {
    let people = serde_json::from_value(v.get("people").cloned().unwrap_or(Value::Array(vec![])))
        .map_err(|e| Error::Http(format!("bad sharing list: {e}")))?;
    Ok(Sharing {
        people,
        teams: parse_list(v.get("teams"), |t: &TeamRef| t.id.as_str()),
    })
}

impl Client {
    /// `GET /v1/teams`: the teams this account belongs to.
    pub async fn teams(&self, access_token: &str) -> Result<Vec<Team>> {
        match self
            .send(reqwest::Method::GET, "/v1/teams", Some(access_token), None)
            .await?
        {
            (200, v) => Ok(parse_teams(&v)),
            (401, _) => Err(Error::Unauthorized),
            // An account server without teams yet: none.
            (404, _) => Ok(Vec::new()),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    /// `GET /v1/sharing`: the people and teams a GitHub link is for by default, as the account
    /// page (`<server>/account`) keeps them (spec 20 §2.2).
    pub async fn sharing(&self, access_token: &str) -> Result<Sharing> {
        match self
            .send(
                reqwest::Method::GET,
                "/v1/sharing",
                Some(access_token),
                None,
            )
            .await?
        {
            (200, v) => parse_sharing(&v),
            (401, _) => Err(Error::Unauthorized),
            // An account server without the list yet: nobody.
            (404, _) => Ok(Sharing::default()),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    /// The people of [`Client::sharing`].
    pub async fn sharing_people(&self, access_token: &str) -> Result<Vec<GithubUser>> {
        self.sharing(access_token).await.map(|s| s.people)
    }
    /// `GET /v1/teams/{id}`: the GitHub ids of the team's members now. A team this account is
    /// not in (or that is gone) has nobody: `Ok(vec![])`.
    pub async fn team_member_ids(&self, access_token: &str, team: &str) -> Result<Vec<u64>> {
        if !valid_team_id(team) {
            return Ok(Vec::new());
        }
        match self
            .send(
                reqwest::Method::GET,
                &format!("/v1/teams/{team}"),
                Some(access_token),
                None,
            )
            .await?
        {
            (200, v) => Ok(v["members"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_u64())
                .collect()),
            (401, _) => Err(Error::Unauthorized),
            (404, _) => Ok(Vec::new()),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }
}

impl Account {
    /// [`Client::teams`] with this account's token (refreshed once when refused).
    pub async fn teams(&self) -> Result<Vec<Team>> {
        self.with_access(|t| async move { self.client.teams(&t).await })
            .await
    }

    /// [`Client::sharing`] with this account's token (refreshed once when refused).
    pub async fn sharing(&self) -> Result<Sharing> {
        self.with_access(|t| async move { self.client.sharing(&t).await })
            .await
    }

    /// The people of [`Account::sharing`].
    pub async fn sharing_people(&self) -> Result<Vec<GithubUser>> {
        self.sharing().await.map(|s| s.people)
    }
    /// [`Client::team_member_ids`] with this account's token.
    pub async fn team_member_ids(&self, team: &str) -> Result<Vec<u64>> {
        self.with_access(|t| async move { self.client.team_member_ids(&t, team).await })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T1: &str = "0b5c3f7e-1d2a-4c8b-9e6f-3a1b2c3d4e5f";

    #[test]
    fn team_ids() {
        assert!(valid_team_id(T1));
        for bad in [
            "",
            "acme",
            "0B5C3F7E-1D2A-4C8B-9E6F-3A1B2C3D4E5F",
            "0b5c3f7e1d2a4c8b9e6f3a1b2c3d4e5f",
            "0b5c3f7e-1d2a-4c8b-9e6f-3a1b2c3d4e5",
            "0b5c3f7e-1d2a-4c8b-9e6f-3a1b2c3d4e5g",
            "../../../../../../../../../../../..",
        ] {
            assert!(!valid_team_id(bad), "{bad}");
        }
    }

    #[test]
    fn answers_parse_and_skip_bad_entries() {
        let v = json!({"teams": [
            {"id": T1, "name": "Acme", "role": "owner", "members": 3},
            {"id": "acme", "name": "Bad id"},
            {"id": T1},
            "x",
        ]});
        assert_eq!(
            parse_teams(&v),
            [Team {
                id: T1.into(),
                name: "Acme".into(),
                role: "owner".into(),
                members: 3
            }]
        );
        assert!(parse_teams(&json!({})).is_empty());
        // An older server: people only.
        let s = parse_sharing(&json!({"people": [{"id": 2, "login": "alice"}]})).unwrap();
        assert_eq!(s.people.len(), 1);
        assert!(s.teams.is_empty());
        let s =
            parse_sharing(&json!({"people": [], "teams": [{"id": T1, "name": "Acme"}]})).unwrap();
        assert_eq!(
            s.teams,
            [TeamRef {
                id: T1.into(),
                name: "Acme".into()
            }]
        );
        assert!(parse_sharing(&json!({"people": "x"})).is_err());
    }

    #[tokio::test]
    async fn teams_and_sharing_from_the_account_server() {
        use crate::CredentialStore;
        let f = crate::fake::FakeServer::start().await;
        let c = Client::new(&f.url).unwrap();
        assert_eq!(c.teams("nope").await, Err(Error::Unauthorized));
        let store = std::sync::Arc::new(crate::MemoryStore::default());
        let acct = Account::new(c.clone(), store.clone());
        f.with(|s| {
            s.access = Some("a1".into());
            s.refresh = Some("r1".into());
            s.teams = vec![crate::fake::FakeTeam::new(T1, "Acme", &[4242])];
            s.sharing = vec![(2, "alice".into())];
            s.sharing_teams = vec![T1.into()];
        });
        store
            .save(&crate::Credential {
                server: c.server().into(),
                login: "octo".into(),
                refresh_token: "r1".into(),
                access_token: Some("a1".into()),
                access_exp: Some(u64::MAX / 2),
                saved_at: 0,
            })
            .unwrap();
        let teams = acct.teams().await.unwrap();
        assert_eq!(teams.len(), 1);
        assert_eq!((teams[0].name.as_str(), teams[0].members), ("Acme", 1));
        let s = acct.sharing().await.unwrap();
        assert_eq!(s.people[0].login, "alice");
        assert_eq!(s.teams[0].name, "Acme");
        assert_eq!(acct.sharing_people().await.unwrap(), s.people);
    }
}
