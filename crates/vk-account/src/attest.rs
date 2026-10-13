//! GitHub identity attestations (spec 20 §2.3): the account server signs who a browser user is
//! on GitHub, bound to one host (`aud`) and one invitation plus device key (`nonce`). A gateway
//! checks the attestation a device presents with `pair.claim` against the keys the account server
//! publishes at `GET /v1/keys`.
//!
//! Token: `"v1." ‖ b64url(json) ‖ "." ‖ b64url(sig)`, where `sig` is Ed25519 over
//! `"vibeke-cloud/1 attest\0" ‖ json` (the exact decoded payload bytes) and the JSON is
//! `{v: 1, kid, gh_id, login, aud, nonce, iat, exp}`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vk_e2e::b64;

use crate::{Account, Client, Error, Result};

/// Signed-message prefix for an attestation.
pub const DOMAIN: &[u8] = b"vibeke-cloud/1 attest\0";
/// Hash prefix for the nonce that binds an attestation to an invitation and a device key.
pub const NONCE_DOMAIN: &[u8] = b"vibeke-attest/1\0";
/// Allowed clock difference between the account server and this host (seconds).
pub const SKEW_S: u64 = 60;
/// Longest lifetime (`exp - iat`) accepted. The account server issues 300 s.
pub const MAX_LIFETIME_S: u64 = 600;
/// Longest token accepted, so a device cannot make the gateway decode megabytes.
pub const MAX_TOKEN: usize = 4096;

/// One published attestation key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestKey {
    pub kid: String,
    pub key: [u8; 32],
}

/// The signed claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestClaims {
    pub v: u32,
    pub kid: String,
    pub gh_id: u64,
    pub login: String,
    pub aud: String,
    pub nonce: String,
    pub iat: u64,
    pub exp: u64,
}

/// A GitHub account as the account server resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubUser {
    pub id: u64,
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestError {
    /// Not a `v1.<json>.<sig>` token, too long, or the JSON has the wrong shape.
    Malformed,
    /// The claims name a version other than 1.
    Version,
    /// No published key has this `kid` (the caller may refetch the keys once).
    UnknownKid(String),
    BadSignature,
    /// `exp` is past.
    Expired,
    /// `iat` is in the future.
    NotYetValid,
}

impl std::fmt::Display for AttestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttestError::Malformed => f.write_str("the attestation is malformed"),
            AttestError::Version => f.write_str("unsupported attestation version"),
            AttestError::UnknownKid(k) => write!(f, "unknown attestation key {k}"),
            AttestError::BadSignature => f.write_str("the attestation signature is invalid"),
            AttestError::Expired => f.write_str("the attestation expired"),
            AttestError::NotYetValid => f.write_str("the attestation is not valid yet"),
        }
    }
}

impl std::error::Error for AttestError {}

/// `kid` of a public key: the first 8 bytes of its SHA-256, hex.
pub fn kid_of(pubkey: &[u8; 32]) -> String {
    Sha256::digest(pubkey)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `b64url(sha256("vibeke-attest/1\0" ‖ pid ‖ "\0" ‖ device_static))`.
pub fn nonce(pid: &str, device_static: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(NONCE_DOMAIN);
    h.update(pid.as_bytes());
    h.update(b"\0");
    h.update(device_static);
    b64::encode(h.finalize())
}

/// The claims of a token, without checking anything (to find its `kid`).
pub fn peek(token: &str) -> Result<(AttestClaims, Vec<u8>, [u8; 64]), AttestError> {
    if token.len() > MAX_TOKEN {
        return Err(AttestError::Malformed);
    }
    let mut parts = token.split('.');
    let (Some("v1"), Some(payload), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AttestError::Malformed);
    };
    let json = b64::decode(payload).map_err(|_| AttestError::Malformed)?;
    let sig = b64::decode_array::<64>(sig).map_err(|_| AttestError::Malformed)?;
    let claims: AttestClaims = serde_json::from_slice(&json).map_err(|_| AttestError::Malformed)?;
    Ok((claims, json, sig))
}

/// Check a token's signature with the key it names and its times against `now_s`. The caller
/// checks `aud`, `nonce` and `gh_id`.
pub fn verify(token: &str, keys: &[AttestKey], now_s: u64) -> Result<AttestClaims, AttestError> {
    let (claims, json, sig) = peek(token)?;
    let key = keys
        .iter()
        .find(|k| k.kid == claims.kid)
        .ok_or_else(|| AttestError::UnknownKid(claims.kid.clone()))?;
    let vk =
        ed25519_dalek::VerifyingKey::from_bytes(&key.key).map_err(|_| AttestError::BadSignature)?;
    let msg = [DOMAIN, json.as_slice()].concat();
    vk.verify_strict(&msg, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| AttestError::BadSignature)?;
    if claims.v != 1 {
        return Err(AttestError::Version);
    }
    if claims.exp <= claims.iat || claims.exp - claims.iat > MAX_LIFETIME_S {
        return Err(AttestError::Malformed);
    }
    if claims.iat > now_s.saturating_add(SKEW_S) {
        return Err(AttestError::NotYetValid);
    }
    if claims.exp.saturating_add(SKEW_S) < now_s {
        return Err(AttestError::Expired);
    }
    Ok(claims)
}

/// Sign `claims` with the Ed25519 key from `seed` (tests and the fake account server only).
#[cfg(any(test, feature = "fake-server"))]
pub fn sign(seed: &[u8; 32], claims: &AttestClaims) -> String {
    use ed25519_dalek::Signer;
    let json = serde_json::to_vec(claims).expect("claims serialize");
    let sk = ed25519_dalek::SigningKey::from_bytes(seed);
    let sig = sk.sign(&[DOMAIN, json.as_slice()].concat()).to_bytes();
    format!("v1.{}.{}", b64::encode(&json), b64::encode(sig))
}

/// The public key for a signing seed (tests and the fake account server only).
#[cfg(any(test, feature = "fake-server"))]
pub fn public_of(seed: &[u8; 32]) -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(seed)
        .verifying_key()
        .to_bytes()
}

/// A GitHub login as GitHub allows it: 1 to 39 ASCII letters, digits and single hyphens, not at
/// either end. Anything else can't name a user, and never reaches a URL path.
pub fn valid_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
}

/// The `{attest: [{kid, alg, pub}]}` answer of `/v1/keys`: Ed25519 keys whose `kid` matches
/// their public key. Other entries are skipped.
pub fn parse_keys(v: &Value) -> Vec<AttestKey> {
    v.get("attest")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|k| k.get("alg").and_then(Value::as_str) == Some("ed25519"))
        .filter_map(|k| {
            let kid = k.get("kid")?.as_str()?;
            let key = b64::decode_array::<32>(k.get("pub")?.as_str()?).ok()?;
            (kid_of(&key) == kid).then(|| AttestKey {
                kid: kid.to_string(),
                key,
            })
        })
        .collect()
}

impl Client {
    /// The account server's published attestation keys (`GET /v1/keys`, no auth).
    pub async fn attest_keys(&self) -> Result<Vec<AttestKey>> {
        match self
            .send(reqwest::Method::GET, "/v1/keys", None, None)
            .await?
        {
            (200, v) => Ok(parse_keys(&v)),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    /// Resolve a GitHub login to its id (`GET /v1/github/users/{login}`). A login GitHub doesn't
    /// know, or one that can't be a login, is [`Error::UnknownUser`].
    pub async fn github_user(&self, access_token: &str, login: &str) -> Result<GithubUser> {
        if !valid_login(login) {
            return Err(Error::UnknownUser);
        }
        match self
            .send(
                reqwest::Method::GET,
                &format!("/v1/github/users/{login}"),
                Some(access_token),
                None,
            )
            .await?
        {
            (200, v) => Self::parse(v),
            (401, _) => Err(Error::Unauthorized),
            (404, _) => Err(Error::UnknownUser),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }
}

impl Account {
    /// [`Client::github_user`] with this account's token (refreshed once when refused).
    pub async fn github_user(&self, login: &str) -> Result<GithubUser> {
        let access = self.access_token(false).await?;
        match self.client.github_user(&access, login).await {
            Err(Error::Unauthorized) => {
                let access = self.access_token(true).await?;
                self.client
                    .github_user(&access, login)
                    .await
                    .map_err(|e| match e {
                        Error::Unauthorized => Error::LoginRequired,
                        e => e,
                    })
            }
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7; 32];
    /// Vectors computed independently (Node's `crypto`): seed `[7; 32]`, device key `[9; 32]`,
    /// pid `pid123`. vibeke-cloud checks the same values.
    const PUB_HEX: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const KID: &str = "fe812c12f3ab4ce6";
    const NONCE: &str = "VAV5hsn-wiB-evnhoW6SD9RDC8UjUJneorZ3gWm7dds";
    const TOKEN: &str = "v1.eyJ2IjoxLCJraWQiOiJmZTgxMmMxMmYzYWI0Y2U2IiwiZ2hfaWQiOjU4MzIzMSwibG9naW4iOiJvY3RvY2F0IiwiYXVkIjoiaG9zdC1hYmMiLCJub25jZSI6IlZBVjVoc24td2lCLWV2bmhvVzZTRDlSREM4VWpVSm5lb3JaM2dXbTdkZHMiLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6MTcwMDAwMDMwMH0.Xpagkp32pMsEimMBzyy7ZKc9_mJzWSJ5bF_N5Nut_7eETVwtA5S1dMWjVIFoOK_42qVdAjH41LXYrPjwQPF4BQ";

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn claims() -> AttestClaims {
        AttestClaims {
            v: 1,
            kid: KID.into(),
            gh_id: 583231,
            login: "octocat".into(),
            aud: "host-abc".into(),
            nonce: NONCE.into(),
            iat: 1_700_000_000,
            exp: 1_700_000_300,
        }
    }

    fn keys() -> Vec<AttestKey> {
        vec![AttestKey {
            kid: KID.into(),
            key: public_of(&SEED),
        }]
    }

    #[test]
    fn fixed_vectors() {
        assert_eq!(hex(&public_of(&SEED)), PUB_HEX);
        assert_eq!(kid_of(&public_of(&SEED)), KID);
        assert_eq!(nonce("pid123", &[9; 32]), NONCE);
        assert_eq!(sign(&SEED, &claims()), TOKEN);
        assert_eq!(verify(TOKEN, &keys(), 1_700_000_100).unwrap(), claims());
    }

    #[test]
    fn nonce_binds_pid_and_device() {
        let n = nonce("p1", &[1; 32]);
        assert_ne!(n, nonce("p2", &[1; 32]));
        assert_ne!(n, nonce("p1", &[2; 32]));
        // The separator keeps `pid` from running into the key bytes.
        assert_ne!(nonce("p1\0", &[1; 32]), n);
    }

    #[test]
    fn verification_failures() {
        let now = 1_700_000_100;
        // Signed by another key under the same kid.
        let forged = sign(&[8; 32], &claims());
        assert_eq!(
            verify(&forged, &keys(), now),
            Err(AttestError::BadSignature)
        );
        // Unknown kid.
        let other = AttestClaims {
            kid: "0011223344556677".into(),
            ..claims()
        };
        assert_eq!(
            verify(&sign(&SEED, &other), &keys(), now),
            Err(AttestError::UnknownKid("0011223344556677".into()))
        );
        // Payload changed after signing.
        let (_, _, sig) = peek(TOKEN).unwrap();
        let tampered = AttestClaims {
            gh_id: 1,
            ..claims()
        };
        let t = format!(
            "v1.{}.{}",
            b64::encode(serde_json::to_vec(&tampered).unwrap()),
            b64::encode(sig)
        );
        assert_eq!(verify(&t, &keys(), now), Err(AttestError::BadSignature));
        // Expired (past the skew) and issued in the future (past the skew).
        assert_eq!(
            verify(TOKEN, &keys(), 1_700_000_300 + SKEW_S + 1),
            Err(AttestError::Expired)
        );
        assert!(verify(TOKEN, &keys(), 1_700_000_300 + SKEW_S).is_ok());
        assert_eq!(
            verify(TOKEN, &keys(), 1_700_000_000 - SKEW_S - 1),
            Err(AttestError::NotYetValid)
        );
        // Version and lifetime.
        let v2 = AttestClaims { v: 2, ..claims() };
        assert_eq!(
            verify(&sign(&SEED, &v2), &keys(), now),
            Err(AttestError::Version)
        );
        let long = AttestClaims {
            exp: 1_700_000_000 + MAX_LIFETIME_S + 1,
            ..claims()
        };
        assert_eq!(
            verify(&sign(&SEED, &long), &keys(), now),
            Err(AttestError::Malformed)
        );
        // Shapes.
        for bad in [
            "",
            "v1.x",
            "v2.a.b",
            TOKEN.replacen("v1.", "v0.", 1).as_str(),
            format!("{TOKEN}.x").as_str(),
            "v1.e30.AAAA",
            "v1.".repeat(2000).as_str(),
        ] {
            assert!(verify(bad, &keys(), now).is_err(), "{bad}");
        }
    }

    #[test]
    fn keys_must_match_their_kid() {
        let good = b64::encode(public_of(&SEED));
        let v = serde_json::json!({"attest": [
            {"kid": KID, "alg": "ed25519", "pub": good},
            {"kid": "0000000000000000", "alg": "ed25519", "pub": good},
            {"kid": KID, "alg": "rsa", "pub": good},
            {"kid": KID, "alg": "ed25519", "pub": "short"},
        ]});
        assert_eq!(parse_keys(&v), keys());
        assert!(parse_keys(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn logins() {
        for ok in ["octocat", "a", "a-b", "A1", "x".repeat(39).as_str()] {
            assert!(valid_login(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "a--b",
            "a/b",
            "../x",
            "a b",
            "x".repeat(40).as_str(),
            "é",
        ] {
            assert!(!valid_login(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn keys_and_users_from_the_account_server() {
        use crate::CredentialStore;
        let f = crate::fake::FakeServer::start().await;
        let c = Client::new(&f.url).unwrap();
        let ks = c.attest_keys().await.unwrap();
        assert_eq!(ks.len(), 1);
        assert_eq!(ks[0].key, public_of(&crate::fake::ATTEST_SEED));
        // Lookups need the account's token.
        assert_eq!(
            c.github_user("nope", "octocat").await,
            Err(Error::Unauthorized)
        );
        let store = std::sync::Arc::new(crate::MemoryStore::default());
        let acct = Account::new(c.clone(), store.clone());
        f.with(|s| {
            s.access = Some("a1".into());
            s.refresh = Some("r1".into());
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
        let u = acct.github_user("Octocat").await.unwrap();
        assert_eq!(
            u,
            GithubUser {
                id: 583231,
                login: "octocat".into()
            }
        );
        assert_eq!(acct.github_user("ghost").await, Err(Error::UnknownUser));
        assert_eq!(acct.github_user("../x").await, Err(Error::UnknownUser));
    }
}
