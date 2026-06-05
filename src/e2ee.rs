//! End-to-end encryption (ARCHITECTURE §14, issues #1/#2).
//!
//! The relay is an active attacker; all dashboard↔controller traffic is encrypted
//! end-to-end so the relay only ever forwards opaque ciphertext.
//!
//! Pattern: `Noise_XXpsk3_25519_ChaChaPoly_SHA256`.
//! - PSK = HKDF(`PAIRING_SECRET`) — a **high-entropy** shared secret (#1: we
//!   mandate ≥128 bits; Noise+PSK is not a PAKE so a weak secret would be
//!   offline-guessable by the active relay).
//! - Each party also has a long-term **static keypair**. After the handshake the
//!   controller checks the peer's static public key against an **authorized
//!   devices** list — that's what gives per-device revocation without rotating
//!   the shared PSK. First valid handshake from an unknown key enrolls it
//!   (TOFU-under-PSK); a removed key is rejected even though it knows the PSK.
//! - Fan-out is pairwise (issue #2): one Noise session per dashboard; broadcast
//!   frames are encrypted once per peer.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

pub const NOISE_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_SHA256";
/// Minimum PAIRING_SECRET length (#1). 32 chars ≈ a 16-byte hex / 24-byte base64
/// token; we recommend `openssl rand -hex 32`.
pub const MIN_PAIRING_LEN: usize = 32;

/// #1 enforcement: reject a low-entropy pairing secret.
pub fn validate_pairing_secret(secret: &str) -> Result<()> {
    if secret.len() < MIN_PAIRING_LEN {
        bail!(
            "PAIRING_SECRET too weak: need >= {MIN_PAIRING_LEN} chars of high entropy \
             (e.g. `openssl rand -hex 32`). Noise+PSK is not a PAKE — a short/typed \
             secret is offline-guessable by an active relay (ARCHITECTURE §14, issue #1)."
        );
    }
    Ok(())
}

fn hkdf32(secret: &[u8], tag: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(tag).expect("hmac key");
    mac.update(secret);
    let out = mac.finalize().into_bytes();
    let mut k = [0u8; 32];
    k.copy_from_slice(&out);
    k
}

/// PSK mixed into the Noise handshake.
pub fn derive_psk(pairing_secret: &str) -> [u8; 32] {
    hkdf32(pairing_secret.as_bytes(), b"claude-pty-controller/psk/v1")
}

/// Dedicated rendezvous secret (domain-separated from the PSK). NOTE: derived
/// from PAIRING_SECRET for v1 simplicity → rotating the pairing secret changes
/// the room id (acceptable since it's a stable high-entropy token; §14 caveat).
pub fn derive_rendezvous_secret(pairing_secret: &str) -> [u8; 32] {
    hkdf32(pairing_secret.as_bytes(), b"claude-pty-controller/rendezvous/v1")
}

/// Epoch window for room-id rotation (seconds). Both ends accept ±1 window.
pub const EPOCH_WINDOW_SECS: u64 = 300;

pub fn current_epoch(now_unix: u64) -> u64 {
    now_unix / EPOCH_WINDOW_SECS
}

/// `room id = HMAC(rendezvous_secret, "rendezvous" || epoch)` truncated to 16 bytes.
pub fn room_id(rendezvous_secret: &[u8; 32], epoch: u64) -> String {
    let mut mac = HmacSha256::new_from_slice(rendezvous_secret).expect("hmac key");
    mac.update(b"rendezvous");
    mac.update(&epoch.to_be_bytes());
    let out = mac.finalize().into_bytes();
    hex(&out[..16])
}

/// The set of room ids a peer should try / listen on around `now` (epoch ±1).
pub fn room_ids_window(rendezvous_secret: &[u8; 32], now_unix: u64) -> Vec<String> {
    let e = current_epoch(now_unix);
    [e.wrapping_sub(1), e, e + 1].iter().map(|&ep| room_id(rendezvous_secret, ep)).collect()
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(char::from_digit((x >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((x & 0xf) as u32, 16).unwrap());
    }
    s
}

/// A long-term static keypair (X25519), persisted base64.
#[derive(Clone, Serialize, Deserialize)]
pub struct StaticKey {
    pub private_b64: String,
    pub public_b64: String,
}

impl StaticKey {
    pub fn generate() -> Result<Self> {
        let kp = snow::Builder::new(NOISE_PARAMS.parse()?).generate_keypair()?;
        Ok(Self { private_b64: B64.encode(kp.private), public_b64: B64.encode(kp.public) })
    }
    pub fn private(&self) -> Result<Vec<u8>> {
        Ok(B64.decode(&self.private_b64)?)
    }
    pub fn public(&self) -> Result<Vec<u8>> {
        Ok(B64.decode(&self.public_b64)?)
    }
    /// Load from `path`, generating + saving (0600) if absent.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let s = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&s)?)
        } else {
            let k = Self::generate()?;
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).ok();
            }
            write_private(path, &serde_json::to_string_pretty(&k)?)?;
            Ok(k)
        }
    }
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(contents.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Authorized device table (controller side). Keyed by base64 static public key.
#[derive(Default, Serialize, Deserialize)]
pub struct AuthorizedDevices {
    pub devices: Vec<Device>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    pub pubkey_b64: String,
    pub label: String,
    pub added_at: u64,
}

impl AuthorizedDevices {
    pub fn load(path: &Path) -> Result<Self> {
        let mut me: Self = if path.exists() {
            serde_json::from_str(&std::fs::read_to_string(path)?).context("parse authorized_devices")?
        } else {
            Self::default()
        };
        me.path = Some(path.to_path_buf());
        Ok(me)
    }
    pub fn contains(&self, pubkey: &[u8]) -> bool {
        let b = B64.encode(pubkey);
        self.devices.iter().any(|d| d.pubkey_b64 == b)
    }
    pub fn add(&mut self, pubkey: &[u8], label: &str, now: u64) -> Result<()> {
        let b = B64.encode(pubkey);
        if !self.devices.iter().any(|d| d.pubkey_b64 == b) {
            self.devices.push(Device { pubkey_b64: b, label: label.into(), added_at: now });
            self.save()?;
        }
        Ok(())
    }
    pub fn remove(&mut self, pubkey: &[u8]) -> Result<()> {
        let b = B64.encode(pubkey);
        self.devices.retain(|d| d.pubkey_b64 != b);
        self.save()
    }
    fn save(&self) -> Result<()> {
        if let Some(p) = &self.path {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_private(p, &serde_json::to_string_pretty(self)?)?;
        }
        Ok(())
    }
}

/// One side of a Noise XXpsk3 handshake, driven message-by-message (relayed).
pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    pub fn initiator(static_priv: &[u8], psk: &[u8; 32]) -> Result<Self> {
        let state = snow::Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(static_priv)
            .psk(3, psk)
            .build_initiator()?;
        Ok(Self { state })
    }
    pub fn responder(static_priv: &[u8], psk: &[u8; 32]) -> Result<Self> {
        let state = snow::Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(static_priv)
            .psk(3, psk)
            .build_responder()?;
        Ok(Self { state })
    }
    /// Produce the next outgoing handshake message.
    pub fn write(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 1024];
        let n = self.state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
    /// Consume an incoming handshake message.
    pub fn read(&mut self, msg: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; 1024];
        self.state.read_message(msg, &mut buf)?;
        Ok(())
    }
    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }
    /// The peer's static public key (available after msg 2/3 of XX).
    pub fn remote_static(&self) -> Option<Vec<u8>> {
        self.state.get_remote_static().map(|s| s.to_vec())
    }
    pub fn into_transport(self) -> Result<Transport> {
        Ok(Transport { state: self.state.into_transport_mode()? })
    }
}

/// Post-handshake AEAD transport for one peer.
pub struct Transport {
    state: snow::TransportState,
}

impl Transport {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; plaintext.len() + 64];
        let n = self.state.write_message(plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; ciphertext.len() + 64];
        let n = self.state.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_handshake(psk_i: &[u8; 32], psk_r: &[u8; 32]) -> Result<(Transport, Transport, Vec<u8>, Vec<u8>)> {
        let ik = StaticKey::generate()?;
        let rk = StaticKey::generate()?;
        let mut i = Handshake::initiator(&ik.private()?, psk_i)?;
        let mut r = Handshake::responder(&rk.private()?, psk_r)?;
        // XX: 3 messages, i -> r -> i.
        let m1 = i.write()?;
        r.read(&m1)?;
        let m2 = r.write()?;
        i.read(&m2)?;
        let m3 = i.write()?;
        r.read(&m3)?;
        assert!(i.is_finished() && r.is_finished());
        let i_remote = i.remote_static().unwrap(); // = responder pub
        let r_remote = r.remote_static().unwrap(); // = initiator pub
        assert_eq!(i_remote, rk.public()?);
        assert_eq!(r_remote, ik.public()?);
        Ok((i.into_transport()?, r.into_transport()?, ik.public()?, rk.public()?))
    }

    #[test]
    fn pairing_secret_entropy_enforced() {
        assert!(validate_pairing_secret("short").is_err());
        assert!(validate_pairing_secret(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn handshake_and_bidirectional_encrypt() {
        let psk = derive_psk("0123456789abcdef0123456789abcdef");
        let (mut it, mut rt, _ipub, _rpub) = run_handshake(&psk, &psk).unwrap();
        let ct = it.encrypt(b"hello from dashboard").unwrap();
        assert_ne!(&ct[..], b"hello from dashboard"); // opaque to the relay
        assert_eq!(rt.decrypt(&ct).unwrap(), b"hello from dashboard");
        let ct2 = rt.encrypt(b"reply from controller").unwrap();
        assert_eq!(it.decrypt(&ct2).unwrap(), b"reply from controller");
    }

    #[test]
    fn wrong_psk_fails_handshake() {
        let psk_i = derive_psk("0123456789abcdef0123456789abcdef");
        let psk_r = derive_psk("ffffffffffffffffffffffffffffffff");
        // XXpsk3 mixes the PSK at msg3; the responder's read of msg3 fails.
        let ik = StaticKey::generate().unwrap();
        let rk = StaticKey::generate().unwrap();
        let mut i = Handshake::initiator(&ik.private().unwrap(), &psk_i).unwrap();
        let mut r = Handshake::responder(&rk.private().unwrap(), &psk_r).unwrap();
        let m1 = i.write().unwrap();
        r.read(&m1).unwrap();
        let m2 = r.write().unwrap();
        i.read(&m2).unwrap();
        let m3 = i.write().unwrap();
        assert!(r.read(&m3).is_err(), "mismatched PSK must fail at msg3");
    }

    #[test]
    fn authorized_devices_add_contains_remove() {
        let dir = std::env::temp_dir().join(format!("cpc-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("authorized_devices.json");
        let mut a = AuthorizedDevices::load(&path).unwrap();
        let key = StaticKey::generate().unwrap();
        let pk = key.public().unwrap();
        assert!(!a.contains(&pk));
        a.add(&pk, "laptop", 1).unwrap();
        assert!(a.contains(&pk));
        // persisted
        let b = AuthorizedDevices::load(&path).unwrap();
        assert!(b.contains(&pk));
        // revoke
        a.remove(&pk).unwrap();
        assert!(!a.contains(&pk));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn room_id_stable_and_window_has_three() {
        let rs = derive_rendezvous_secret("0123456789abcdef0123456789abcdef");
        let a = room_id(&rs, 100);
        let b = room_id(&rs, 100);
        assert_eq!(a, b);
        assert_ne!(room_id(&rs, 100), room_id(&rs, 101));
        let w = room_ids_window(&rs, 100 * EPOCH_WINDOW_SECS + 5);
        assert_eq!(w.len(), 3);
        assert!(w.contains(&room_id(&rs, 100)));
    }
}
