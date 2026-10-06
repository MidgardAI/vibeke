//! Web Push (spec 16 §7.8, §8): RFC 8291 `aes128gcm` payload encryption, RFC 8292 VAPID with the
//! **device's** key, and an SSRF-guarded sender. Pure Rust (no OpenSSL) so static builds work.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use vk_e2e::b64;

const RECORD_SIZE: u32 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

/// RFC 8291 §3.4 encryption into a single `aes128gcm` record (RFC 8188).
/// `as_secret` and `salt` are injectable for the RFC test vector.
pub fn encrypt(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
    as_secret: &SecretKey,
    salt: &[u8; 16],
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        plaintext.len() + 1 + 16 <= RECORD_SIZE as usize,
        "push payload too large"
    );
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| anyhow::anyhow!("bad p256dh"))?;
    let as_public = as_secret.public_key().to_encoded_point(false);
    let as_public = as_public.as_bytes();
    let ecdh = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());

    // PRK_key = HMAC(auth_secret, ecdh_secret); IKM = HMAC(PRK_key, key_info || 0x01)
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let hk = Hkdf::<Sha256>::new(Some(auth_secret), ecdh.raw_secret_bytes());
    let mut ikm = [0u8; 32];
    hk.expand(&key_info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("hkdf"))?;

    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| anyhow::anyhow!("hkdf"))?;
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow::anyhow!("hkdf"))?;

    let mut padded = plaintext.to_vec();
    padded.push(2); // last-record delimiter
    let ct = Aes128Gcm::new_from_slice(&cek)
        .expect("16-byte key")
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &padded,
                aad: b"",
            },
        )
        .map_err(|_| anyhow::anyhow!("aes-gcm"))?;

    let mut out = Vec::with_capacity(86 + ct.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// `Authorization: vapid t=<jwt>, k=<public>` (RFC 8292) signed with the device's VAPID key.
pub fn vapid_authorization(
    vapid_private: &[u8],
    endpoint: &reqwest::Url,
    subject: &str,
    now_s: u64,
) -> anyhow::Result<String> {
    let key =
        SigningKey::from_slice(vapid_private).map_err(|_| anyhow::anyhow!("bad VAPID key"))?;
    let public = key.verifying_key().to_encoded_point(false);
    let aud = endpoint.origin().ascii_serialization();
    let header = b64::encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = b64::encode(serde_json::to_vec(
        &serde_json::json!({"aud": aud, "exp": now_s + 12 * 3600, "sub": subject}),
    )?);
    let signing_input = format!("{header}.{claims}");
    let sig: Signature = key.sign(signing_input.as_bytes());
    Ok(format!(
        "vapid t={signing_input}.{}, k={}",
        b64::encode(sig.to_bytes()),
        b64::encode(public.as_bytes())
    ))
}

/// Public key (uncompressed SEC1, base64url) for a VAPID private key.
pub fn vapid_public(vapid_private: &[u8]) -> anyhow::Result<String> {
    let key =
        SigningKey::from_slice(vapid_private).map_err(|_| anyhow::anyhow!("bad VAPID key"))?;
    Ok(b64::encode(
        key.verifying_key().to_encoded_point(false).as_bytes(),
    ))
}

/// Push service hosts we will contact (spec 16 §8.2). Suffix match on a label boundary.
pub const DEFAULT_ALLOWED: &[&str] = &[
    "push.apple.com",
    "fcm.googleapis.com",
    "push.services.mozilla.com",
    "notify.windows.com",
];

pub fn endpoint_allowed(endpoint: &str, allowed: &[String]) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    allowed
        .iter()
        .any(|a| host == *a || host.ends_with(&format!(".{a}")))
        .then_some(url)
}

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
                || (o[0] == 198 && (o[1] & 0xfe) == 18)) // benchmarking
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80) // link local
        }
    }
}

/// DNS resolver that only returns public addresses, so a push endpoint cannot reach the host's network.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 443))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("push endpoint resolves to no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Ok,
    /// 404/410: delete the subscription.
    Gone,
    /// 429: retry after.
    RetryAfter(Duration),
    Failed(String),
}

#[derive(Clone)]
pub struct Sender {
    http: reqwest::Client,
    pub allowed: Arc<Vec<String>>,
    pub subject: String,
}

pub struct Message<'a> {
    pub payload: &'a [u8],
    pub ttl: u32,
    pub urgency: &'static str,
    pub topic: Option<&'a str>,
}

impl Sender {
    pub fn new(allowed: Vec<String>, subject: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .dns_resolver(Arc::new(PublicOnly))
            .https_only(true)
            .build()?;
        Ok(Sender {
            http,
            allowed: Arc::new(allowed),
            subject,
        })
    }

    pub async fn send(
        &self,
        sub: &Subscription,
        vapid_private: &[u8],
        msg: &Message<'_>,
    ) -> SendOutcome {
        match self.try_send(sub, vapid_private, msg).await {
            Ok(o) => o,
            Err(e) => SendOutcome::Failed(e.to_string()),
        }
    }

    async fn try_send(
        &self,
        sub: &Subscription,
        vapid_private: &[u8],
        msg: &Message<'_>,
    ) -> anyhow::Result<SendOutcome> {
        let url = endpoint_allowed(&sub.endpoint, &self.allowed)
            .ok_or_else(|| anyhow::anyhow!("endpoint not allowed"))?;
        let ua = b64::decode(&sub.keys.p256dh)?;
        let auth = b64::decode(&sub.keys.auth)?;
        let as_secret = SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let salt: [u8; 16] = vk_e2e::keys::random_bytes();
        let body = encrypt(&ua, &auth, msg.payload, &as_secret, &salt)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let mut req = self
            .http
            .post(url.clone())
            .header("TTL", msg.ttl.to_string())
            .header("Urgency", msg.urgency)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header(
                "Authorization",
                vapid_authorization(vapid_private, &url, &self.subject, now)?,
            )
            .body(body);
        if let Some(t) = msg.topic {
            req = req.header("Topic", t);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        Ok(match status {
            200..=299 => SendOutcome::Ok,
            404 | 410 => SendOutcome::Gone,
            429 => SendOutcome::RetryAfter(Duration::from_secs(
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60),
            )),
            s => SendOutcome::Failed(format!("push service returned {s}")),
        })
    }
}

/// Verify a VAPID JWT (used by tests and the mock push service).
#[doc(hidden)]
pub fn verify_vapid(authorization: &str) -> bool {
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;
    let Some(rest) = authorization.strip_prefix("vapid t=") else {
        return false;
    };
    let Some((jwt, k)) = rest.split_once(", k=") else {
        return false;
    };
    let Some((input, sig)) = jwt.rsplit_once('.') else {
        return false;
    };
    let (Ok(k), Ok(sig)) = (b64::decode(k), b64::decode(sig)) else {
        return false;
    };
    let (Ok(vk), Ok(sig)) = (
        VerifyingKey::from_sec1_bytes(&k),
        Signature::from_slice(&sig),
    ) else {
        return false;
    };
    vk.verify(input.as_bytes(), &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<u8> {
        b64::decode(&s.replace([' ', '\n'], "")).unwrap()
    }

    /// RFC 8291 Appendix A.
    #[test]
    fn rfc8291_vector() {
        let plaintext = d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let as_private =
            SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let ua_public = d(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");
        let out = encrypt(&ua_public, &auth, &plaintext, &as_private, &salt).unwrap();
        let header = d(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        let ct =
            d("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ");
        assert_eq!(&out[..86], &header[..]);
        assert_eq!(&out[86..], &ct[..]);
    }

    #[test]
    fn vapid_signs_and_verifies() {
        let key = SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let url = reqwest::Url::parse("https://web.push.apple.com/abc").unwrap();
        let h = vapid_authorization(&key.to_bytes(), &url, "mailto:x@example.com", 1).unwrap();
        assert!(verify_vapid(&h));
        assert!(h.contains(&vapid_public(&key.to_bytes()).unwrap()));
    }

    #[test]
    fn ssrf_guard() {
        let allowed: Vec<String> = DEFAULT_ALLOWED.iter().map(|s| s.to_string()).collect();
        assert!(endpoint_allowed("https://web.push.apple.com/x", &allowed).is_some());
        assert!(endpoint_allowed("https://fcm.googleapis.com/fcm/send/x", &allowed).is_some());
        assert!(endpoint_allowed("http://fcm.googleapis.com/x", &allowed).is_none());
        assert!(endpoint_allowed("https://evilpush.apple.com.attacker.net/x", &allowed).is_none());
        assert!(endpoint_allowed("https://notpush.apple.com/x", &allowed).is_none());
        assert!(endpoint_allowed("https://u:p@web.push.apple.com/x", &allowed).is_none());
        assert!(!is_public("127.0.0.1".parse().unwrap()));
        assert!(!is_public("10.1.2.3".parse().unwrap()));
        assert!(!is_public("100.100.1.1".parse().unwrap()));
        assert!(!is_public("169.254.169.254".parse().unwrap()));
        assert!(!is_public("::ffff:192.168.1.1".parse().unwrap()));
        assert!(!is_public("fd00::1".parse().unwrap()));
        assert!(is_public("17.57.146.52".parse().unwrap()));
    }
}
