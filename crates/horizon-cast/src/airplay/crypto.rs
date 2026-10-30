//! The AirPlay 1 cryptography: the fixed RAOP RSA identity, AES-128-CBC and the
//! little bit of base64 the RTSP/SDP layer needs.
//!
//! All primitives go through Windows CNG (`bcrypt.dll`) rather than a bundled
//! implementation. The offline registry has no AES or RSA crate, and rolling
//! either by hand would be a liability in code that handles a stream.

use std::ffi::c_void;
use std::ptr;

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Security::Cryptography::*;

use crate::error::{CastError, CastResult};

// The AirPort Express key, PKCS#1 DER, base64 without the PEM armour. Public
// knowledge, embedded by every AirPlay 1 receiver because senders insist on it.
const RAOP_KEY_DER: &str = "\
MIIEpQIBAAKCAQEA59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Qg90u3sG/1CUt\
wC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6DZHJ2YCCLlDRKSKv6kDqnw4U\
wPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLvqr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf\
/+sG+NCK3eQJVxqcaJ/vEHKIVd2M+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/\
UAaHqn9JdsBWLUEpVviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEewIDAQABAoIBAQDl8Axy9XfW\
BLmkzkEiqoSwF0PsmVrPzH9KsnwLGH+QZlvjWd8SWYGN7u1507HvhF5N3drJoVU3O14nDY4TFQAa\
LlJ9VM35AApXaLyY1ERrN7u9ALKd2LUwYhM7Km539O4yUFYikE2nIPscEsA5ltpxOgUGCY7b7ez5\
NtD6nL1ZKauw7aNXmVAvmJTcuPxWmoktF3gDJKK2wxZuNGcJE0uFQEG4Z3BrWP7yoNuSK3dii2jm\
lpPHr0O/KnPQtzI3eguhe0TwUem/eYSdyzMyVx/YpwkzwtYL3sR5k0o9rKQLtvLzfAqdBxBurciz\
aaA/L0HIgAmOit1GJA2saMxTVPNhAoGBAPfgv1oeZxgxmotiCcMXFEQEWflzhWYTsXrhUIuz5jFu\
a39GLS99ZEErhLdrwj8rDDViRVJ5skOp9zFvlYAHs0xh92ji1E7V/ysnKBfsMrPkk5KSKPrnjndM\
oPdevWnVkgJ5jxFuNgxkOLMuG9i53B4yMvDTCRiIPMQ++N2iLDaRAoGBAO9v//mU8eVkQaoANf0Z\
oMjW8CN4xwWA2cSEIHkd9AfFkftuv8oyLDCG3ZAf0vrhrrtkrfa7ef+AUb69DNggq4mHQAYBp7L+\
k5DKzJrKuO0r+R0YbY9pZD1+/g9dVt91d6LQNepUE/yY2PP5CNoFmjedpLHMOPFdVgqDzDFxU8hL\
AoGBANDrr7xAJbqBjHVwIzQ4To9pb4BNeqDndk5Qe7fT3+/H1njGaC0/rXE0Qb7q5ySgnsCb3DvA\
cJyRM9SJ7OKlGt0FMSdJD5KG0XPIpAVNwgpXXH5MDJg09KHeh0kXo+QA6viFBi21y340NonnEfdf\
54PX4ZGS/Xac1UK+pLkBB+zRAoGAf0AY3H3qKS2lMEI4bzEFoHeK3G895pDaK3TFBVmD7fV0Zhov\
17fegFPMwOII8MisYm9ZfT2Z0s5Ro3s5rkt+nvLAdfC/PYPKzTLalpGSwomSNYJcB9HNMlmhkGzc\
1JnLYT4iyUyx6pcZBmCd8bD0iwY/FzcgNDaUmbX9+XDvRA0CgYEAkE7pIPlE71qvfJQgoA9em0gI\
LAuE4Pu13aKiJnfft7hIjbK+5kyb3TysZvoyDnb3HOKvInK7vXbKuU4ISgxB2bB3HcYzQMGsz1qJ\
2gG0N5hvJpzwwhbhXqFKA4zaaSrw622wDniAK5MlIE0tIAKKP4yxNGjoD2QYjhBGuhvkWKY=";

fn nt(status: NTSTATUS) -> CastResult<()> {
    if status.0 >= 0 {
        Ok(())
    } else {
        Err(CastError::Hresult(status.0))
    }
}

/// The RAOP identity: one RSA key, used both to unwrap session AES keys and to
/// answer `Apple-Challenge`.
pub struct RsaIdentity {
    algorithm: BCRYPT_ALG_HANDLE,
    key: BCRYPT_KEY_HANDLE,
}

impl RsaIdentity {
    pub fn raop() -> CastResult<Self> {
        let der = base64_decode(RAOP_KEY_DER).ok_or(CastError::Malformed("raop key"))?;
        let blob = rsa_private_blob(&der)?;
        let mut algorithm = BCRYPT_ALG_HANDLE::default();
        unsafe {
            nt(BCryptOpenAlgorithmProvider(
                &mut algorithm,
                BCRYPT_RSA_ALGORITHM,
                None,
                BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
            ))?;
        }
        let mut key = BCRYPT_KEY_HANDLE::default();
        let status = unsafe {
            BCryptImportKeyPair(
                algorithm,
                None,
                BCRYPT_RSAFULLPRIVATE_BLOB,
                &mut key,
                &blob,
                0,
            )
        };
        if let Err(e) = nt(status) {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(algorithm, 0);
            }
            return Err(e);
        }
        Ok(Self { algorithm, key })
    }

    /// Unwraps `a=rsaaeskey` into the 16-byte AES key. RAOP uses OAEP with
    /// SHA-1, which is what every sender emits, including current iTunes.
    pub fn unwrap_aes_key(&self, wrapped: &[u8]) -> CastResult<Vec<u8>> {
        let padding = BCRYPT_OAEP_PADDING_INFO {
            pszAlgId: w!("SHA1"),
            pbLabel: ptr::null_mut(),
            cbLabel: 0,
        };
        let mut out = vec![0u8; 256];
        let mut written = 0u32;
        unsafe {
            nt(BCryptDecrypt(
                self.key,
                Some(wrapped),
                Some(&padding as *const _ as *const c_void),
                None,
                Some(&mut out),
                &mut written,
                BCRYPT_PAD_OAEP,
            ))?;
        }
        out.truncate(written as usize);
        Ok(out)
    }

    /// Encrypts with the public half of the identity. `BCryptEncrypt` applies the
    /// public exponent even on a handle imported with the private key, which is
    /// what lets one identity both play a receiver and simulate a sender.
    pub fn wrap_aes_key(&self, key: &[u8]) -> CastResult<Vec<u8>> {
        let padding = BCRYPT_OAEP_PADDING_INFO {
            pszAlgId: w!("SHA1"),
            pbLabel: ptr::null_mut(),
            cbLabel: 0,
        };
        let mut out = vec![0u8; 256];
        let mut written = 0u32;
        unsafe {
            nt(BCryptEncrypt(
                self.key,
                Some(key),
                Some(&padding as *const _ as *const c_void),
                None,
                Some(&mut out),
                &mut written,
                BCRYPT_PAD_OAEP,
            ))?;
        }
        out.truncate(written as usize);
        Ok(out)
    }

    /// The `Apple-Response` value: PKCS#1 v1.5 private-key operation over the
    /// challenge, address and MAC, with no digest.
    pub fn answer_challenge(&self, challenge: &[u8], address: [u8; 4], mac: [u8; 6]) -> CastResult<String> {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(challenge);
        buf.extend_from_slice(&address);
        buf.extend_from_slice(&mac);
        if buf.len() < 32 {
            buf.resize(32, 0);
        }
        // `pszAlgId = NULL` tells CNG the bytes are already the message, not a
        // digest, so no DigestInfo is prepended. `BCryptEncrypt` would apply the
        // public exponent even on a private key handle, which is not what RAOP
        // wants here.
        let padding = BCRYPT_PKCS1_PADDING_INFO {
            pszAlgId: PCWSTR::null(),
        };
        let mut out = vec![0u8; 256];
        let mut written = 0u32;
        unsafe {
            nt(BCryptSignHash(
                self.key,
                Some(&padding as *const _ as *const c_void),
                &buf,
                Some(&mut out),
                &mut written,
                BCRYPT_PAD_PKCS1,
            ))?;
        }
        out.truncate(written as usize);
        Ok(base64_encode(&out).trim_end_matches('=').to_string())
    }
}

impl Drop for RsaIdentity {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyKey(self.key);
            let _ = BCryptCloseAlgorithmProvider(self.algorithm, 0);
        }
    }
}

/// AES-128-CBC with the stream's fixed IV, exactly as RAOP applies it.
pub struct AesStream {
    algorithm: BCRYPT_ALG_HANDLE,
    key: BCRYPT_KEY_HANDLE,
    iv: [u8; 16],
}

impl AesStream {
    pub fn new(key: &[u8], iv: &[u8]) -> CastResult<Self> {
        if key.len() != 16 || iv.len() != 16 {
            return Err(CastError::Malformed("aes key or iv is not 16 bytes"));
        }
        let mut algorithm = BCRYPT_ALG_HANDLE::default();
        unsafe {
            nt(BCryptOpenAlgorithmProvider(
                &mut algorithm,
                BCRYPT_AES_ALGORITHM,
                None,
                BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
            ))?;
        }
        let mut mode_bytes: Vec<u8> = "ChainingModeCBC"
            .encode_utf16()
            .flat_map(|unit| unit.to_ne_bytes())
            .collect();
        mode_bytes.extend_from_slice(&0u16.to_ne_bytes());
        if let Err(e) = unsafe {
            nt(BCryptSetProperty(
                algorithm.into(),
                BCRYPT_CHAINING_MODE,
                &mode_bytes,
                0,
            ))
        } {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(algorithm, 0);
            }
            return Err(e);
        }
        let mut key_handle = BCRYPT_KEY_HANDLE::default();
        if let Err(e) = unsafe {
            nt(BCryptGenerateSymmetricKey(
                algorithm,
                &mut key_handle,
                None,
                key,
                0,
            ))
        } {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(algorithm, 0);
            }
            return Err(e);
        }
        let mut iv_bytes = [0u8; 16];
        iv_bytes.copy_from_slice(iv);
        Ok(Self {
            algorithm,
            key: key_handle,
            iv: iv_bytes,
        })
    }

    /// Decrypts `data`, which must be a multiple of 16 bytes. CNG mutates the IV,
    /// so each call starts from the sender's fixed one.
    pub fn decrypt(&self, data: &[u8]) -> CastResult<Vec<u8>> {
        if data.is_empty() || data.len() % 16 != 0 {
            return Err(CastError::Malformed("aes payload is not a block multiple"));
        }
        let mut iv = self.iv;
        let mut out = vec![0u8; data.len()];
        let mut written = 0u32;
        unsafe {
            nt(BCryptDecrypt(
                self.key,
                Some(data),
                None,
                Some(&mut iv),
                Some(&mut out),
                &mut written,
                BCRYPT_FLAGS(0),
            ))?;
        }
        if written as usize != data.len() {
            return Err(CastError::Malformed("aes decrypted length changed"));
        }
        Ok(out)
    }

    /// Encrypts a block-multiple payload with the fixed stream IV. The sender
    /// side of the same wire format, used by `airplay-probe --selftest`.
    pub fn encrypt(&self, data: &[u8]) -> CastResult<Vec<u8>> {
        if data.is_empty() || data.len() % 16 != 0 {
            return Err(CastError::Malformed("aes payload is not a block multiple"));
        }
        let mut iv = self.iv;
        let mut out = vec![0u8; data.len()];
        let mut written = 0u32;
        unsafe {
            nt(BCryptEncrypt(
                self.key,
                Some(data),
                None,
                Some(&mut iv),
                Some(&mut out),
                &mut written,
                BCRYPT_FLAGS(0),
            ))?;
        }
        if written as usize != data.len() {
            return Err(CastError::Malformed("aes encrypted length changed"));
        }
        Ok(out)
    }
}

impl Drop for AesStream {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyKey(self.key);
            let _ = BCryptCloseAlgorithmProvider(self.algorithm, 0);
        }
    }
}

// CNG key handles have no thread affinity; the audio thread decrypts while the
// RTSP thread owns the identity, and the handle is only ever used by one thread
// at a time.
unsafe impl Send for AesStream {}

/// AES-128-CTR as AirPlay uses it: one long keystream that starts at the IV and
/// survives across messages.
///
/// Pair-verify encrypts two 64-byte signatures with the same cipher (the second
/// must continue where the first stopped), and the mirroring stream treats
/// every frame as a continuation of one stream. A per-call CTR API would restart
/// the counter each time and break both, so this keeps the counter and the
/// partially-consumed keystream block between calls.
///
/// CNG does the AES block; the counter is incremented here, big-endian, the way
/// every CTR specification defines it.
/// How many counter blocks one `BCryptEncrypt` call covers. The mirror stream
/// pushes 200 KB frames through here on the socket thread, and every extra call
/// is fixed overhead paid while frames sit unread in the TCP buffer.
const KEYSTREAM_BLOCKS: usize = 2048;
const KEYSTREAM_BYTES: usize = KEYSTREAM_BLOCKS * 16;

pub struct AesCtr {
    algorithm: BCRYPT_ALG_HANDLE,
    key: BCRYPT_KEY_HANDLE,
    /// The next counter that has not been written into `keystream`.
    counter: [u8; 16],
    /// Counter values waiting to be encrypted; CNG refuses in-place output.
    staging: Box<[u8; KEYSTREAM_BYTES]>,
    keystream: Box<[u8; KEYSTREAM_BYTES]>,
    /// Next unused byte in `keystream`; `KEYSTREAM_BYTES` means exhausted.
    offset: usize,
}

impl AesCtr {
    pub fn new(key: &[u8], iv: &[u8]) -> CastResult<Self> {
        if key.len() != 16 || iv.len() != 16 {
            return Err(CastError::Malformed("aes key or iv is not 16 bytes"));
        }
        let mut algorithm = BCRYPT_ALG_HANDLE::default();
        unsafe {
            nt(BCryptOpenAlgorithmProvider(
                &mut algorithm,
                BCRYPT_AES_ALGORITHM,
                None,
                BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
            ))?;
        }
        // ECB is the primitive; CTR is built on top of it by XORing counters.
        let mut mode_bytes: Vec<u8> = "ChainingModeECB"
            .encode_utf16()
            .flat_map(|unit| unit.to_ne_bytes())
            .collect();
        mode_bytes.extend_from_slice(&0u16.to_ne_bytes());
        if let Err(e) = unsafe {
            nt(BCryptSetProperty(
                algorithm.into(),
                BCRYPT_CHAINING_MODE,
                &mode_bytes,
                0,
            ))
        } {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(algorithm, 0);
            }
            return Err(e);
        }
        let mut key_handle = BCRYPT_KEY_HANDLE::default();
        if let Err(e) = unsafe {
            nt(BCryptGenerateSymmetricKey(
                algorithm,
                &mut key_handle,
                None,
                key,
                0,
            ))
        } {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(algorithm, 0);
            }
            return Err(e);
        }
        let mut counter = [0u8; 16];
        counter.copy_from_slice(iv);
        Ok(Self {
            algorithm,
            key: key_handle,
            counter,
            staging: Box::new([0u8; KEYSTREAM_BYTES]),
            keystream: Box::new([0u8; KEYSTREAM_BYTES]),
            offset: KEYSTREAM_BYTES,
        })
    }

    fn refill(&mut self) -> CastResult<()> {
        // Lay the next `KEYSTREAM_BLOCKS` counters out back to back, then let CNG
        // run them all as one ECB buffer. The counter is big-endian, so a u128
        // add is the same increment the block loop used to do by hand.
        let value = u128::from_be_bytes(self.counter);
        for (slot, index) in self
            .staging
            .chunks_exact_mut(16)
            .zip(0..KEYSTREAM_BLOCKS as u128)
        {
            slot.copy_from_slice(&value.wrapping_add(index).to_be_bytes());
        }
        let mut written = 0u32;
        unsafe {
            nt(BCryptEncrypt(
                self.key,
                Some(&self.staging[..]),
                None,
                None,
                Some(&mut self.keystream[..]),
                &mut written,
                BCRYPT_FLAGS(0),
            ))?;
        }
        if written as usize != KEYSTREAM_BYTES {
            return Err(CastError::Malformed("aes-ctr keystream length changed"));
        }
        self.counter = value.wrapping_add(KEYSTREAM_BLOCKS as u128).to_be_bytes();
        self.offset = 0;
        Ok(())
    }

    /// XORs the keystream into `data` in place. Encryption and decryption are
    /// the same operation in CTR mode.
    pub fn apply_keystream(&mut self, data: &mut [u8]) -> CastResult<()> {
        let mut at = 0usize;
        while at < data.len() {
            if self.offset == KEYSTREAM_BYTES {
                self.refill()?;
            }
            let take = (KEYSTREAM_BYTES - self.offset).min(data.len() - at);
            let end = self.offset + take;
            for (byte, key) in data[at..at + take]
                .iter_mut()
                .zip(self.keystream[self.offset..end].iter())
            {
                *byte ^= key;
            }
            self.offset = end;
            at += take;
        }
        Ok(())
    }

    pub fn process(&mut self, data: &[u8]) -> CastResult<Vec<u8>> {
        let mut out = data.to_vec();
        self.apply_keystream(&mut out)?;
        Ok(out)
    }
}

impl Drop for AesCtr {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyKey(self.key);
            let _ = BCryptCloseAlgorithmProvider(self.algorithm, 0);
        }
    }
}

unsafe impl Send for AesCtr {}

/// Builds `BCRYPT_RSAFULLPRIVATE_BLOB` from a PKCS#1 `RSAPrivateKey` DER.
/// CNG rejects the shorter `RSA2` form with `STATUS_INVALID_PARAMETER`, so the
/// primes, CRT exponents and coefficient all have to come along.
fn rsa_private_blob(der: &[u8]) -> CastResult<Vec<u8>> {
    let mut at = 0usize;
    let sequence = der_element(der, &mut at, 0x30)?;
    let mut inner = 0usize;
    let _version = der_element(sequence, &mut inner, 0x02)?;
    let modulus = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let exponent = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let private = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let prime1 = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let prime2 = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let exponent1 = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let exponent2 = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);
    let coefficient = strip_leading_zeros(der_element(sequence, &mut inner, 0x02)?);

    if modulus.len() < 64
        || exponent.is_empty()
        || private.is_empty()
        || prime1.is_empty()
        || prime2.is_empty()
        || exponent1.is_empty()
        || exponent2.is_empty()
        || coefficient.is_empty()
    {
        return Err(CastError::Malformed("raop key is not rsa"));
    }

    let total = 24
        + exponent.len()
        + modulus.len()
        + prime1.len()
        + prime2.len()
        + exponent1.len()
        + exponent2.len()
        + coefficient.len()
        + private.len();
    let mut blob = Vec::with_capacity(total);
    blob.extend_from_slice(&0x3341_5352u32.to_le_bytes()); // BCRYPT_RSAFULLPRIVATE_MAGIC "RSA3"
    blob.extend_from_slice(&((modulus.len() * 8) as u32).to_le_bytes());
    blob.extend_from_slice(&(exponent.len() as u32).to_le_bytes());
    blob.extend_from_slice(&(modulus.len() as u32).to_le_bytes());
    blob.extend_from_slice(&(prime1.len() as u32).to_le_bytes());
    blob.extend_from_slice(&(prime2.len() as u32).to_le_bytes());
    blob.extend_from_slice(exponent);
    blob.extend_from_slice(modulus);
    blob.extend_from_slice(prime1);
    blob.extend_from_slice(prime2);
    blob.extend_from_slice(exponent1);
    blob.extend_from_slice(exponent2);
    blob.extend_from_slice(coefficient);
    blob.extend_from_slice(private);
    Ok(blob)
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

fn der_element<'a>(input: &'a [u8], at: &mut usize, tag: u8) -> CastResult<&'a [u8]> {
    let actual = *input.get(*at).ok_or(CastError::Malformed("der is truncated"))?;
    if actual != tag {
        return Err(CastError::Malformed("der tag mismatch"));
    }
    *at += 1;
    let first = *input.get(*at).ok_or(CastError::Malformed("der is truncated"))?;
    *at += 1;
    let len = if first & 0x80 == 0 {
        first as usize
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 3 {
            return Err(CastError::Malformed("der length"));
        }
        let mut len = 0usize;
        for _ in 0..count {
            let byte = *input.get(*at).ok_or(CastError::Malformed("der is truncated"))?;
            *at += 1;
            len = (len << 8) | byte as usize;
        }
        len
    };
    let end = at.checked_add(len).ok_or(CastError::Malformed("der length"))?;
    let slice = input
        .get(*at..end)
        .ok_or(CastError::Malformed("der past end"))?;
    *at = end;
    Ok(slice)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0] as u32;
        let b = *chunk.get(1).unwrap_or(&0) as u32;
        let c = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(B64[(triple >> 18) as usize & 63] as char);
        out.push(B64[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\r' | b'\n' | b'\t' => continue,
            _ => return None,
        } as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_ignores_padding() {
        for case in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0u8, 255, 128, 3]] {
            let encoded = base64_encode(case);
            assert_eq!(base64_decode(&encoded).unwrap(), case);
        }
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn the_raop_key_imports_and_unwraps_the_session_key() {
        let identity = RsaIdentity::raop().expect("import raop key");
        // Produced by Node: RSA-OAEP(SHA-1) of the fixed key 000102…0f.
        let wrapped = base64_decode(
            "uiWSo+WEKHGnnyklbJkT+MFvoY3aHDtk071mtQ0iH25ZV3UAXa/0Cc6J38IGvxUxgcnMwA/e60pgFRiec+\
             lnEKLADOK8XN8qYNh1Gu7sY9xGbpmUkZcbGfKrDnRn3KB3uVrvWto7zp8ahh+X5RXt8oKKqUAZ3iN4vF+67\
             c7RirQucaoeyVxGlAqvpb9TNZ37v6VVJ7LaMJ9rq9xlydBgQxebqvz3MZc8CqcuVM1XDVPa1JI7jV4MnAVS\
             IgyKCmBzMBl4izA2pWyIozc8mE2Cb5W2HvHMFIi7gjGMJvD6DLu+ItqwmqONUeFpO8rhMZSaZ+IabWVYmU\
             OgCrm5YXUjog==",
        )
        .unwrap();
        let key = identity.unwrap_aes_key(&wrapped).expect("unwrap");
        assert_eq!(key, (0u8..16).collect::<Vec<_>>());
    }

    #[test]
    fn a_challenge_response_matches_the_openssl_private_operation() {
        let identity = RsaIdentity::raop().expect("import raop key");
        let challenge: Vec<u8> = (1..=32u8).collect();
        let response = identity
            .answer_challenge(&challenge, [192, 168, 0, 52], [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .expect("answer");
        // Node `privateEncrypt` with RSA_PKCS1_PADDING over challenge||ip||mac,
        // base64 with the '=' padding shairport-sync also strips.
        assert_eq!(
            response,
            "XtsaBExgKvtgBbeSoauJF1S4a9CCsQwkXD61kom5Q69xqI0qlFt5YuX28fM0xbnnlwQZM9zCTHhfzyr2+cqrXy\
             TggpkovTghf+knUQ3Jvi/V8V0f/VL2GHPCyh3V2/zW9sROFJ5+9+m0+gJKhrN8zo7bGjCv6lrkm4U79psZ/ytSUp\
             5gJN2646YfpdohY5QpYtbmMLyoMLuqtuh2Rcpdq19sWBFf38ec6EPmuV3wkPaPg11mX2IHrikRFkndKoW2ZVt5XoH\
             9Ghi+tD57ittmn+UmC0/a4tUf2YDwCV+lgFlICKAaTgwAA/6A0h7nd8/95X0p34wdAr8zgmHjx8xNOA"
        );
    }

    #[test]
    fn aes_cbc_decrypts_one_pcm_block() {
        let key: Vec<u8> = (0u8..16).collect();
        // The sender uses the same IV for every packet in the stream.
        let iv: Vec<u8> = (16u8..32).collect();
        let cipher = base64_decode("Vd9aejp8MXuyQbv/I70cER7JGr4S7/gnAjVMAjzubHmSn4U+O6AxpOp0g8Ec1DTieE8n/Bj/fi5c/GbU7TXaFg==")
            .unwrap();
        let aes = AesStream::new(&key, &iv).expect("aes");
        let packet = aes.decrypt(&cipher).expect("decrypt");
        for i in 0..16i16 {
            let left = i16::from_le_bytes([packet[(i as usize) * 4], packet[(i as usize) * 4 + 1]]);
            let right =
                i16::from_le_bytes([packet[(i as usize) * 4 + 2], packet[(i as usize) * 4 + 3]]);
            assert_eq!(left, i % 1000);
            assert_eq!(right, -(i % 1000));
        }
    }

    #[test]
    fn a_payload_off_the_block_grid_is_refused() {
        let aes = AesStream::new(&(0u8..16).collect::<Vec<_>>(), &(16u8..32).collect::<Vec<_>>())
            .expect("aes");
        let bad = vec![0u8; 15];
        assert!(aes.decrypt(&bad).is_err());
        assert!(aes.encrypt(&bad).is_err());
    }

    #[test]
    fn wrapping_and_unwrapping_the_session_key_round_trips() {
        let identity = RsaIdentity::raop().expect("import raop key");
        let key: Vec<u8> = (0u8..16).map(|byte| byte * 3).collect();
        let wrapped = identity.wrap_aes_key(&key).expect("wrap");
        assert_eq!(wrapped.len(), 256);
        assert_eq!(identity.unwrap_aes_key(&wrapped).unwrap(), key);
    }

    #[test]
    fn aes_encrypt_is_the_inverse_of_decrypt() {
        let aes = AesStream::new(&(0u8..16).collect::<Vec<_>>(), &(16u8..32).collect::<Vec<_>>())
            .expect("aes");
        let mut plain = vec![0u8; 1408];
        for (at, byte) in plain.iter_mut().enumerate() {
            *byte = (at * 7) as u8;
        }
        let cipher = aes.encrypt(&plain).expect("encrypt");
        assert_ne!(cipher, plain);
        assert_eq!(aes.decrypt(&cipher).unwrap(), plain);
    }

    const CTR_KEY: [u8; 16] = [
        0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f,
        0x3c,
    ];
    const CTR_IV: [u8; 16] = [
        0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe,
        0xff,
    ];
    const CTR_PLAIN: &str = concat!(
        "6bc1bee22e409f96e93d7e117393172a",
        "ae2d8a571e03ac9c9eb76fac45af8e51",
        "30c81c46a35ce411e5fbc1191a0a52ef",
        "f69f2445df4f9b17ad2b417be66c3710"
    );

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn aes_ctr_matches_the_nist_vector() {
        let mut cipher = AesCtr::new(&CTR_KEY, &CTR_IV).expect("ctr");
        let out = cipher.process(&hex(CTR_PLAIN)).expect("process");
        assert_eq!(out, hex("874d6191b620e3261bef6864990db6ce\
                             9806f66b7970fdff8617187bb9fffdff\
                             5ae4df3edbd5d35e5b4f09020db03eab\
                             1e031dda2fbe03d1792170a0f3009cee"));
    }

    #[test]
    fn aes_ctr_carries_a_partial_block_across_calls() {
        // 37 bytes is deliberately not a block multiple: the second call has to
        // resume inside the keystream block rather than restart the counter.
        let plain = hex("00112233445566778899aabbccddeeff\
                         0102030405060708090a0b0c0d0e0f10\
                         1112131415161718191a1b1c1d1e1f20\
                         21222324");
        let expected = hex("ec9dfd40dc351ac77a4bbcce26434f1b\
                            37297f386275566b11aa7cdbf15e7cbe\
                            7b3ed06c6d9f2057a7aed3070aa47364\
                            c9be1abb");
        let mut cipher = AesCtr::new(&CTR_KEY, &CTR_IV).expect("ctr");
        let mut out = Vec::new();
        for chunk in plain.chunks(5) {
            out.extend(cipher.process(chunk).expect("chunk"));
        }
        assert_eq!(out, expected);

        let mut whole = AesCtr::new(&CTR_KEY, &CTR_IV).expect("ctr");
        assert_eq!(whole.process(&plain).unwrap(), expected);
    }

    /// Throughput gate for the mirror path. A 1080p60 sender pushes roughly
    /// 12 MB/s of H.264 through here on the socket thread, and any per-block
    /// cost that is paid one call at a time shows up as growing end-to-end
    /// latency rather than as a wrong byte.
    #[test]
    #[ignore = "throughput benchmark; run with --ignored"]
    fn aes_ctr_sustains_a_mirror_stream() {
        let mut cipher = AesCtr::new(&CTR_KEY, &CTR_IV).expect("ctr");
        let mut frame = vec![0u8; 200_000];
        let started = std::time::Instant::now();
        let mut frames = 0usize;
        while started.elapsed() < std::time::Duration::from_secs(2) {
            cipher.apply_keystream(&mut frame).expect("keystream");
            frames += 1;
        }
        let elapsed = started.elapsed().as_secs_f64();
        let megabytes = frames as f64 * frame.len() as f64 / 1_000_000.0;
        println!(
            "aes-ctr: {:.1} MB/s, {:.3} ms per 200 KB frame",
            megabytes / elapsed,
            elapsed * 1_000.0 / frames as f64
        );
    }

    #[test]
    fn pairing_ctr_continues_between_the_two_signatures() {
        // Key derivation matches pair-verify: SHA-512 of the label plus the
        // X25519 shared secret, truncated to 16 bytes. Both the key stream and
        // the two 64-byte ciphertexts were produced by Node's OpenSSL.
        let shared: Vec<u8> = (0..32u8).map(|index| index * 3 + 1).collect();
        let message: Vec<u8> = (0..64u32).map(|index| (255 - index * 2) as u8).collect();
        let key = sha512_label("Pair-Verify-AES-Key", &shared);
        let iv = sha512_label("Pair-Verify-AES-IV", &shared);
        assert_eq!(key, hex("7a535416d2bef69b2d8a80441c8b0422"));
        assert_eq!(iv, hex("2d2fb8bfbf4c9ea8a5d53e8934cdc45c"));

        let mut cipher = AesCtr::new(&key, &iv).expect("ctr");
        let first = cipher.process(&message).expect("first signature");
        assert_eq!(
            first,
            hex("122bfe0c0596e9fba86a3b17fcee0d1f\
                 184eb7bc35ead3d14d9eb4d6441e6e81\
                 354be2d03ede2f1f4f5fce6e2dac7a4b\
                 ccf4c08d531792c18c09ffc93cb020ba")
        );
        let second = cipher.process(&message.iter().rev().copied().collect::<Vec<_>>()).expect("second signature");
        assert_eq!(
            second,
            hex("694e2acfd4f2d94561b8a0b0e564c186\
                 f3436f51fabc9a7f61b03a519bbb3f7a\
                 44927aa75ea38e0b441c1cbcdccbd20c\
                 5dccd77e9dad4a552095ed963b68c477")
        );
    }

    fn sha512_label(label: &str, shared: &[u8]) -> Vec<u8> {
        use sha2::{Digest, Sha512};
        let mut hasher = Sha512::new();
        hasher.update(label.as_bytes());
        hasher.update(shared);
        hasher.finalize()[..16].to_vec()
    }
}
