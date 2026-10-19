//! The cryptography of Web Push: the desktop's VAPID identity (RFC 8292) and
//! the `aes128gcm` payload encryption (RFC 8291), both on `ring`.

use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use ring::{aead, agreement, hkdf};

use super::tls::write_private;

const VAPID_FILE: &str = "vapid.pk8";
const JWT_LIFETIME_SECS: u64 = 43_200;
/// Record size announced in the header; one record carries the whole payload.
const RECORD_SIZE: u32 = 4096;
const P256_POINT: usize = 65;
const SALT_LEN: usize = 16;
const HEADER_LEN: usize = SALT_LEN + 4 + 1 + P256_POINT;
/// Padding delimiter that ends the last (only) record.
const LAST_RECORD: u8 = 0x02;
const TAG_LEN: usize = 16;
/// What fits in one record next to the delimiter and the AEAD tag.
pub const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - TAG_LEN - 1;

/// The key the desktop signs its push requests with. It persists, so the
/// phones' subscriptions (bound to its public half) stay valid.
pub struct Vapid {
    pkcs8: Vec<u8>,
    pub public_b64: String,
}

fn key_pair(pkcs8: &[u8]) -> Result<EcdsaKeyPair, String> {
    EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        pkcs8,
        &SystemRandom::new(),
    )
    .map_err(|error| format!("invalid push key: {error}"))
}

impl Vapid {
    /// Read `<dir>/remote/vapid.pk8`, or create it (mode 0600).
    pub fn load_or_create(dir: &Path) -> Result<Self, String> {
        let folder = dir.join("remote");
        let path = folder.join(VAPID_FILE);
        let pkcs8 = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let generated = EcdsaKeyPair::generate_pkcs8(
                    &ECDSA_P256_SHA256_FIXED_SIGNING,
                    &SystemRandom::new(),
                )
                .map_err(|_| "could not generate the push key".to_owned())?;
                crate::private_fs::private_dirs(true)
                    .create(&folder)
                    .map_err(|error| format!("create {}: {error}", folder.display()))?;
                write_private(&path, generated.as_ref())?;
                generated.as_ref().to_vec()
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        let public_b64 = URL_SAFE_NO_PAD.encode(key_pair(&pkcs8)?.public_key().as_ref());
        Ok(Self { pkcs8, public_b64 })
    }

    /// An ES256 token for a push service at `aud`, valid 12 hours from `now`.
    pub fn jwt(&self, aud: &str, sub: &str, now: u64) -> Result<String, String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": aud, "exp": now + JWT_LIFETIME_SECS, "sub": sub,
        });
        let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let signature = key_pair(&self.pkcs8)?
            .sign(&SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| "could not sign the push token".to_owned())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

/// An HKDF output length for `ring`.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn derive(salt: &[u8], secret: &[u8], info: &[u8], out: &mut [u8]) -> Result<(), String> {
    hkdf::Salt::new(hkdf::HKDF_SHA256, salt)
        .extract(secret)
        .expand(&[info], Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .map_err(|_| "key derivation failed".to_owned())
}

/// The content-encryption key and nonce of one message (RFC 8291 §3.3-3.4).
fn content_keys(
    ecdh: &[u8],
    auth: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12]), String> {
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public);
    let mut ikm = [0_u8; 32];
    derive(auth, ecdh, &info, &mut ikm)?;
    let mut cek = [0_u8; 16];
    derive(salt, &ikm, b"Content-Encoding: aes128gcm\0", &mut cek)?;
    let mut nonce = [0_u8; 12];
    derive(salt, &ikm, b"Content-Encoding: nonce\0", &mut nonce)?;
    Ok((cek, nonce))
}

/// The request body for a subscription's `p256dh` (65 bytes) and `auth`
/// (16 bytes): header (salt, record size, sender key) and one sealed record.
pub fn encrypt(sub_p256dh: &[u8], sub_auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    if sub_p256dh.len() != P256_POINT || sub_auth.len() != 16 {
        return Err("bad subscription keys".to_owned());
    }
    if plaintext.len() > MAX_PLAINTEXT {
        return Err("push payload too large".to_owned());
    }
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| "could not generate a push key".to_owned())?;
    let as_public = private
        .compute_public_key()
        .map_err(|_| "could not derive the push key".to_owned())?;
    let as_public = as_public.as_ref();
    let mut salt = [0_u8; SALT_LEN];
    rng.fill(&mut salt)
        .map_err(|_| "could not generate secure random bytes".to_owned())?;

    let (cek, nonce) = agreement::agree_ephemeral(
        private,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, sub_p256dh),
        |ecdh| content_keys(ecdh, sub_auth, sub_p256dh, as_public, &salt),
    )
    .map_err(|_| "bad subscription key".to_owned())??;

    let record = seal(&cek, nonce, plaintext)?;

    let mut body = Vec::with_capacity(HEADER_LEN + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(u8::try_from(P256_POINT).map_err(|error| error.to_string())?);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// The one record: plaintext, the last-record delimiter, then the AEAD tag.
fn seal(cek: &[u8; 16], nonce: [u8; 12], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut record = Vec::with_capacity(plaintext.len() + 1 + TAG_LEN);
    record.extend_from_slice(plaintext);
    record.push(LAST_RECORD);
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, cek)
            .map_err(|_| "could not set up the cipher".to_owned())?,
    );
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| "could not encrypt the push payload".to_owned())?;
    Ok(record)
}

/// Receiver side of [`encrypt`], for tests in this module's siblings.
#[cfg(test)]
pub fn decrypt_for_test(
    private: agreement::EphemeralPrivateKey,
    ua_public: &[u8],
    auth: &[u8],
    body: &[u8],
) -> Result<Vec<u8>, String> {
    let salt = &body[..SALT_LEN];
    let as_public = &body[SALT_LEN + 5..HEADER_LEN];
    let (cek, nonce) = agreement::agree_ephemeral(
        private,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public),
        |ecdh| content_keys(ecdh, auth, ua_public, as_public, salt),
    )
    .map_err(|_| "bad sender key".to_owned())??;
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
    let mut record = body[HEADER_LEN..].to_vec();
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::empty(),
            &mut record,
        )
        .map_err(|_| "decrypt failed".to_owned())?;
    let end = plain
        .iter()
        .rposition(|&byte| byte != 0)
        .ok_or("empty record")?;
    if plain[end] != LAST_RECORD {
        return Err("bad delimiter".to_owned());
    }
    Ok(plain[..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).unwrap()
    }

    const ECDH: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AS_PUBLIC: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";

    /// RFC 8291 Appendix A: derived key and nonce.
    #[test]
    fn content_keys_match_rfc_8291_appendix_a() {
        let (cek, nonce) = content_keys(
            &b64(ECDH),
            &b64(AUTH),
            &b64(UA_PUBLIC),
            &b64(AS_PUBLIC),
            &b64(SALT),
        )
        .unwrap();
        assert_eq!(cek.to_vec(), b64("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce.to_vec(), b64("4h_95klXJ5E_qnoN"));
    }

    /// RFC 8291 Appendix A: the sealed record.
    #[test]
    fn seal_matches_rfc_8291_appendix_a() {
        let cek: [u8; 16] = b64("oIhVW04MRdy2XN9CiKLxTg").try_into().unwrap();
        let nonce: [u8; 12] = b64("4h_95klXJ5E_qnoN").try_into().unwrap();
        let record = seal(&cek, nonce, b"When I grow up, I want to be a watermelon").unwrap();
        assert_eq!(
            record,
            b64("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ")
        );
    }
}
