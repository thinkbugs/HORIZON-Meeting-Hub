//! AirPlay pairing: `/pair-setup` and `/pair-verify`.
//!
//! A sender will not start a mirroring session until it has verified the
//! receiver's identity. AirPlay 1 does that with a short Ed25519/X25519
//! handshake rather than the SRP exchange AirPlay 2 uses for HomeKit:
//!
//! | step | sender | receiver |
//! |---|---|---|
//! | `/pair-setup` | POST (empty) | 200, 32-byte Ed25519 public key |
//! | `/pair-verify` M1 | `01 00 00 00` + X25519 pub + Ed25519 pub (68 B) | ephemeral X25519 pub + encrypted Ed25519 signature over `ours‖theirs` (96 B) |
//! | `/pair-verify` M2 | `00 00 00 00` + encrypted signature over `theirs‖ours` (68 B) | 200, empty body |
//!
//! Both signatures travel through one AES-128-CTR keystream. The second must
//! continue where the first stopped, which is why [`crypto::AesCtr`] is
//! streaming rather than per-message. The CTR key and IV are the first 16 bytes
//! of `SHA-512("Pair-Verify-AES-Key"‖shared)` and
//! `SHA-512("Pair-Verify-AES-IV"‖shared)`.
//!
//! The identity is a seed plus its Ed25519 public key. It is generated once per
//! receiver and reused for the mDNS `pk` record and every pairing, because the
//! sender checks that the key it verified is the key the receiver advertises.
//!
//! ## What this is not
//!
//! There is no PIN, no SRP and no persistent record of paired senders. That is
//! the legacy AirPlay 1 path: a sender whose `/info` says `pinRequired` is false
//! and which does not find `/pair-setup-pin` falls back to it. Senders that
//! insist on HomeKit-style pairing are refused, deliberately, rather than being
//! half-supported.

use sha2::{Digest, Sha512};
use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

use crate::airplay::crypto::AesCtr;
use crate::airplay::nacl;
use crate::error::{CastError, CastResult};

/// The X25519 base point, little-endian.
const BASE_POINT: [u8; 32] = [
    9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0,
];

const M1_LENGTH: usize = 4 + 32 + 32;
const M2_LENGTH: usize = 4 + 64;

/// The receiver's long-term Ed25519 identity, advertised as `pk`.
#[derive(Clone)]
pub struct Identity {
    seed: [u8; 32],
    public: [u8; 32],
}

impl Identity {
    /// A fresh identity from the system CSPRNG. Every receiver start gets one;
    /// nothing is persisted, which means a sender pairing to a restarted
    /// receiver simply pairs again.
    pub fn random() -> CastResult<Self> {
        let mut seed = [0u8; 32];
        let status = unsafe {
            BCryptGenRandom(None, &mut seed, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status.0 < 0 {
            return Err(CastError::Hresult(status.0));
        }
        Ok(Self::from_seed(seed))
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        let public = nacl::ed25519_public_key(&seed);
        Self { seed, public }
    }

    pub fn public_key(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn public_key_hex(&self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in self.public {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// The answer to one `/pair-verify` POST.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyReply {
    /// M1: 96 bytes, the ephemeral key followed by the encrypted signature.
    Challenge(Vec<u8>),
    /// M2 accepted: the sender's signature checked out.
    Verified,
}

struct VerifyState {
    shared: [u8; 32],
    ecdh_ours: [u8; 32],
    ecdh_theirs: [u8; 32],
    ed_theirs: [u8; 32],
    cipher: AesCtr,
}

/// One connection's pairing state. A session owns one for its lifetime.
pub struct Pairing {
    identity: Identity,
    verify: Option<VerifyState>,
    verified: bool,
}

impl Pairing {
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            verify: None,
            verified: false,
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn is_verified(&self) -> bool {
        self.verified
    }

    /// The X25519 shared secret once M1 has run. The mirroring keystream mixes
    /// it with the FairPlay key, so SETUP cannot start before this exists.
    pub fn shared_secret(&self) -> Option<&[u8; 32]> {
        self.verify.as_ref().map(|state| &state.shared)
    }

    /// `/pair-setup`: the sender's first request has no body and gets the raw
    /// public key back. The same 32 bytes are what mDNS advertises as `pk`.
    pub fn pair_setup(&self) -> Vec<u8> {
        self.identity.public.to_vec()
    }

    pub fn pair_verify(&mut self, body: &[u8]) -> CastResult<VerifyReply> {
        let flag = *body
            .first()
            .ok_or(CastError::Malformed("pair-verify is empty"))?;
        if flag != 0 {
            if body.len() < M1_LENGTH {
                return Err(CastError::Malformed("pair-verify M1 is short"));
            }
            self.challenge(&body[4..M1_LENGTH])
        } else {
            if body.len() < M2_LENGTH {
                return Err(CastError::Malformed("pair-verify M2 is short"));
            }
            self.check(&body[4..M2_LENGTH])
        }
    }

    fn challenge(&mut self, payload: &[u8]) -> CastResult<VerifyReply> {
        let mut ecdh_theirs = [0u8; 32];
        ecdh_theirs.copy_from_slice(&payload[..32]);
        let mut ed_theirs = [0u8; 32];
        ed_theirs.copy_from_slice(&payload[32..64]);

        let mut secret = [0u8; 32];
        let status =
            unsafe { BCryptGenRandom(None, &mut secret, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        if status.0 < 0 {
            return Err(CastError::Hresult(status.0));
        }
        let ecdh_ours = nacl::x25519(&secret, &BASE_POINT);
        let shared = nacl::x25519(&secret, &ecdh_theirs);
        let (key, iv) = verify_keys(&shared);

        let mut data_to_sign = [0u8; 64];
        data_to_sign[..32].copy_from_slice(&ecdh_ours);
        data_to_sign[32..].copy_from_slice(&ecdh_theirs);
        let signature = nacl::ed25519_sign(&self.identity.seed, &data_to_sign);
        let mut cipher = AesCtr::new(&key, &iv)?;
        let encrypted = cipher.process(&signature)?;

        let mut reply = Vec::with_capacity(96);
        reply.extend_from_slice(&ecdh_ours);
        reply.extend_from_slice(&encrypted);

        self.verify = Some(VerifyState {
            shared,
            ecdh_ours,
            ecdh_theirs,
            ed_theirs,
            cipher,
        });
        self.verified = false;
        Ok(VerifyReply::Challenge(reply))
    }

    fn check(&mut self, payload: &[u8]) -> CastResult<VerifyReply> {
        let state = self
            .verify
            .as_mut()
            .ok_or(CastError::Malformed("pair-verify M2 without M1"))?;
        let signature = state.cipher.process(payload)?;

        let mut message = [0u8; 64];
        message[..32].copy_from_slice(&state.ecdh_theirs);
        message[32..].copy_from_slice(&state.ecdh_ours);
        let signature: [u8; 64] = signature
            .try_into()
            .map_err(|_| CastError::Malformed("pair-verify signature length"))?;
        if !nacl::ed25519_verify(&state.ed_theirs, &message, &signature) {
            return Err(CastError::InvalidState("pair-verify signature"));
        }
        self.verified = true;
        Ok(VerifyReply::Verified)
    }
}

fn verify_keys(shared: &[u8]) -> ([u8; 16], [u8; 16]) {
    let mut key = [0u8; 16];
    key.copy_from_slice(&label_hash("Pair-Verify-AES-Key", shared));
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&label_hash("Pair-Verify-AES-IV", shared));
    (key, iv)
}

fn label_hash(label: &str, shared: &[u8]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label.as_bytes());
    hasher.update(shared);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted sender: everything an iOS controller does in the handshake,
    /// with its own keys and the receiver's published public key.
    struct Sender {
        ed_seed: [u8; 32],
        ed_public: [u8; 32],
        x_secret: [u8; 32],
        x_public: [u8; 32],
        shared: [u8; 32],
        ecdh_ours: [u8; 32],
        cipher: AesCtr,
    }

    impl Sender {
        fn new(receiver: &Identity) -> Self {
            let mut ed_seed = [0u8; 32];
            for (index, byte) in ed_seed.iter_mut().enumerate() {
                *byte = index as u8 + 1;
            }
            let mut x_secret = [0u8; 32];
            for (index, byte) in x_secret.iter_mut().enumerate() {
                *byte = (index as u8).wrapping_mul(5).wrapping_add(7);
            }
            let ed_public = nacl::ed25519_public_key(&ed_seed);
            let x_public = nacl::x25519(&x_secret, &BASE_POINT);
            let _ = receiver;
            Self {
                ed_seed,
                ed_public,
                x_secret,
                x_public,
                shared: [0u8; 32],
                ecdh_ours: [0u8; 32],
                cipher: AesCtr::new(&[0u8; 16], &[0u8; 16]).unwrap(),
            }
        }

        fn m1(&self) -> Vec<u8> {
            let mut body = vec![1, 0, 0, 0];
            body.extend_from_slice(&self.x_public);
            body.extend_from_slice(&self.ed_public);
            body
        }

        /// Completes the handshake against the receiver's 96-byte M1 reply and
        /// produces M2. Returns the M2 body and the receiver's signature as the
        /// sender would recover it.
        fn finish(&mut self, reply: &[u8], receiver: &Identity) -> (Vec<u8>, [u8; 64]) {
            let mut ecdh_receiver = [0u8; 32];
            ecdh_receiver.copy_from_slice(&reply[..32]);
            self.shared = nacl::x25519(&self.x_secret, &ecdh_receiver);
            let (key, iv) = verify_keys(&self.shared);
            self.cipher = AesCtr::new(&key, &iv).unwrap();
            let signature = self.cipher.process(&reply[32..96]).unwrap();

            let mut message = [0u8; 64];
            message[..32].copy_from_slice(&ecdh_receiver);
            message[32..].copy_from_slice(&self.x_public);
            let signature_array: [u8; 64] = signature.as_slice().try_into().unwrap();
            assert!(
                nacl::ed25519_verify(receiver.public_key(), &message, &signature_array),
                "the receiver must sign the ephemeral keys with its advertised identity"
            );

            self.ecdh_ours = ecdh_receiver;
            let mut answer = [0u8; 64];
            answer[..32].copy_from_slice(&self.x_public);
            answer[32..].copy_from_slice(&ecdh_receiver);
            let own_signature = nacl::ed25519_sign(&self.ed_seed, &answer);
            let mut body = vec![0, 0, 0, 0];
            body.extend_from_slice(&self.cipher.process(&own_signature).unwrap());
            (body, signature_array)
        }
    }

    #[test]
    fn pair_setup_returns_the_advertised_key() {
        let identity = Identity::from_seed([7u8; 32]);
        let pairing = Pairing::new(identity.clone());
        assert_eq!(pairing.pair_setup(), identity.public.to_vec());
        assert_eq!(identity.public_key_hex().len(), 64);
        assert!(identity.public_key_hex().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_scripted_sender_verifies_in_two_messages() {
        let receiver = Identity::from_seed([3u8; 32]);
        let mut pairing = Pairing::new(receiver.clone());
        let mut sender = Sender::new(&receiver);

        let reply = match pairing.pair_verify(&sender.m1()).expect("M1") {
            VerifyReply::Challenge(bytes) => bytes,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(reply.len(), 96);
        assert!(!pairing.is_verified());

        let (m2, _) = sender.finish(&reply, &receiver);
        assert_eq!(pairing.pair_verify(&m2).expect("M2"), VerifyReply::Verified);
        assert!(pairing.is_verified());
    }

    #[test]
    fn a_forged_m2_is_refused() {
        let receiver = Identity::from_seed([4u8; 32]);
        let mut pairing = Pairing::new(receiver.clone());
        let mut sender = Sender::new(&receiver);

        let reply = match pairing.pair_verify(&sender.m1()).expect("M1") {
            VerifyReply::Challenge(bytes) => bytes,
            other => panic!("unexpected {other:?}"),
        };
        let (mut m2, _) = sender.finish(&reply, &receiver);
        m2[10] ^= 0x80;
        assert!(pairing.pair_verify(&m2).is_err());
        assert!(!pairing.is_verified());
    }

    #[test]
    fn m2_before_m1_is_refused() {
        let mut pairing = Pairing::new(Identity::from_seed([5u8; 32]));
        assert!(pairing.pair_verify(&[0u8; 68]).is_err());
        assert!(!pairing.is_verified());
    }

    #[test]
    fn a_short_message_is_refused() {
        let mut pairing = Pairing::new(Identity::from_seed([6u8; 32]));
        assert!(pairing.pair_verify(&[]).is_err());
        assert!(pairing.pair_verify(&[1, 0, 0, 0, 9]).is_err());
        assert!(pairing.pair_verify(&[0, 0, 0, 0, 9]).is_err());
    }

    #[test]
    fn identities_are_stable_for_a_seed() {
        let a = Identity::from_seed([9u8; 32]);
        let b = Identity::from_seed([9u8; 32]);
        assert_eq!(a.public_key(), b.public_key());
        let random = Identity::random().expect("random identity");
        assert_ne!(random.public_key(), a.public_key());
    }
}
