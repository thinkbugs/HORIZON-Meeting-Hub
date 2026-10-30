//! X25519 and Ed25519, ported from TweetNaCl.
//!
//! AirPlay pairing needs exactly two primitives that Windows CNG does not
//! expose in a usable form: an X25519 Diffie-Hellman and a detached Ed25519
//! signature. Both are short enough to carry here instead of pulling in a
//! crypto crate, and both are checked against RFC 7748 / RFC 8032 vectors plus
//! cross-checks with Node's OpenSSL bindings in the tests below.
//!
//! The code is a direct translation of TweetNaCl's `crypto_scalarmult` and
//! `crypto_sign` families: 16 limbs of 16 bits in `i64`, schoolbook multiply,
//! and the standard Montgomery ladder / Edwards double-and-add. It is not
//! constant-time in the strictest sense (the original's `sel25519` mask is kept,
//! but Rust offers no guarantee about bounds-check elision); the inputs here are
//! a pairing handshake on a trusted LAN, not long-term secrets under attack.

use sha2::{Digest, Sha512};

type Gf = [i64; 16];

const GF0: Gf = [0; 16];
const GF1: Gf = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const D: Gf = [
    0x78a3, 0x1359, 0x4dca, 0x75eb, 0xd8ab, 0x4141, 0x0a4d, 0x0070, 0xe898, 0x7779, 0x4079, 0x8cc7,
    0xfe73, 0x2b6f, 0x6cee, 0x5203,
];
const D2: Gf = [
    0xf159, 0x26b2, 0x9b94, 0xebd6, 0xb156, 0x8283, 0x149a, 0x00e0, 0xd130, 0xeef3, 0x80f2, 0x198e,
    0xfce7, 0x56df, 0xd9dc, 0x2406,
];
const X: Gf = [
    0xd51a, 0x8f25, 0x2d60, 0xc956, 0xa7b2, 0x9525, 0xc760, 0x692c, 0xdc5c, 0xfdd6, 0xe231, 0xc0a4,
    0x53fe, 0xcd6e, 0x36d3, 0x2169,
];
const Y: Gf = [
    0x6658, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666,
    0x6666, 0x6666, 0x6666, 0x6666,
];
const I: Gf = [
    0xa0b0, 0x4a0e, 0x1b27, 0xc4ee, 0xe478, 0xad2f, 0x1806, 0x2f43, 0xd7a7, 0x3dfb, 0x0099, 0x2b4d,
    0xdf0b, 0x4fc1, 0x2480, 0x2b83,
];
const A24: Gf = [0xdb41, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

const L: [i64; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

fn car25519(o: &mut Gf) {
    for i in 0..16 {
        o[i] += 1 << 16;
        let c = o[i] >> 16;
        if i < 15 {
            o[i + 1] += c - 1;
        } else {
            o[0] += 38 * (c - 1);
        }
        o[i] -= c << 16;
    }
}

fn sel25519(p: &mut Gf, q: &mut Gf, b: i64) {
    let c = !(b - 1);
    for i in 0..16 {
        let t = c & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

fn pack25519(o: &mut [u8; 32], n: &Gf) {
    let mut t = *n;
    car25519(&mut t);
    car25519(&mut t);
    car25519(&mut t);
    for _ in 0..2 {
        let mut m = [0i64; 16];
        m[0] = t[0] - 0xffed;
        for i in 1..15 {
            m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
            m[i - 1] &= 0xffff;
        }
        m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
        let b = (m[15] >> 16) & 1;
        m[14] &= 0xffff;
        sel25519(&mut t, &mut m, 1 - b);
    }
    for i in 0..16 {
        o[2 * i] = (t[i] & 0xff) as u8;
        o[2 * i + 1] = (t[i] >> 8) as u8;
    }
}

fn neq25519(a: &Gf, b: &Gf) -> bool {
    let (mut c, mut d) = ([0u8; 32], [0u8; 32]);
    pack25519(&mut c, a);
    pack25519(&mut d, b);
    c != d
}

fn par25519(a: &Gf) -> u8 {
    let mut d = [0u8; 32];
    pack25519(&mut d, a);
    d[0] & 1
}

fn unpack25519(o: &mut Gf, n: &[u8; 32]) {
    for i in 0..16 {
        o[i] = n[2 * i] as i64 + ((n[2 * i + 1] as i64) << 8);
    }
    o[15] &= 0x7fff;
}

fn add_raw(o: &mut Gf, a: &Gf, b: &Gf) {
    for i in 0..16 {
        o[i] = a[i] + b[i];
    }
}

fn sub_raw(o: &mut Gf, a: &Gf, b: &Gf) {
    for i in 0..16 {
        o[i] = a[i] - b[i];
    }
}

fn mul_raw(o: &mut Gf, a: &Gf, b: &Gf) {
    let mut t = [0i64; 31];
    for i in 0..16 {
        for j in 0..16 {
            t[i + j] += a[i] * b[j];
        }
    }
    for i in 0..15 {
        t[i] += 38 * t[i + 16];
    }
    o.copy_from_slice(&t[..16]);
    car25519(o);
    car25519(o);
}

fn sq_raw(o: &mut Gf, a: &Gf) {
    mul_raw(o, a, a);
}

// Each wrapper materialises the result in a fresh buffer and only then stores
// it. Binding `let a = $a` instead would shadow the caller's own `a` when a
// call looks like `add_gf!(&mut a, a, c)`, and the write would land in the
// shadowed copy.
macro_rules! add_gf {
    ($o:expr, $a:expr, $b:expr) => {{
        let mut out = GF0;
        add_raw(&mut out, &$a, &$b);
        *$o = out;
    }};
}
macro_rules! sub_gf {
    ($o:expr, $a:expr, $b:expr) => {{
        let mut out = GF0;
        sub_raw(&mut out, &$a, &$b);
        *$o = out;
    }};
}
macro_rules! mul_gf {
    ($o:expr, $a:expr, $b:expr) => {{
        let mut out = GF0;
        mul_raw(&mut out, &$a, &$b);
        *$o = out;
    }};
}
macro_rules! sq_gf {
    ($o:expr, $a:expr) => {{
        let mut out = GF0;
        sq_raw(&mut out, &$a);
        *$o = out;
    }};
}

fn inv25519(o: &mut Gf, i: &Gf) {
    let mut c = *i;
    for a in (0..=253).rev() {
        sq_gf!(&mut c, c);
        if a != 2 && a != 4 {
            let c2 = c;
            mul_gf!(&mut c, c2, *i);
        }
    }
    *o = c;
}

fn pow2523(o: &mut Gf, i: &Gf) {
    let mut c = *i;
    for a in (0..=250).rev() {
        sq_gf!(&mut c, c);
        if a != 1 {
            let c2 = c;
            mul_gf!(&mut c, c2, *i);
        }
    }
    *o = c;
}

/// X25519 scalar multiplication: the Diffie-Hellman shared secret.
pub fn x25519(secret: &[u8; 32], point: &[u8; 32]) -> [u8; 32] {
    let mut z = *secret;
    z[31] = (z[31] & 127) | 64;
    z[0] &= 248;

    let mut x = GF0;
    unpack25519(&mut x, point);

    let (mut a, mut c, mut d, mut e, mut f) = (GF0, GF0, GF0, GF0, GF0);
    let mut b = x;
    a[0] = 1;
    d[0] = 1;

    for i in (0..=254).rev() {
        let r = ((z[i >> 3] >> (i & 7)) & 1) as i64;
        sel25519(&mut a, &mut b, r);
        sel25519(&mut c, &mut d, r);
        add_gf!(&mut e, a, c);
        sub_gf!(&mut a, a, c);
        add_gf!(&mut c, b, d);
        sub_gf!(&mut b, b, d);
        sq_gf!(&mut d, e);
        sq_gf!(&mut f, a);
        mul_gf!(&mut a, c, a);
        mul_gf!(&mut c, b, e);
        add_gf!(&mut e, a, c);
        sub_gf!(&mut a, a, c);
        sq_gf!(&mut b, a);
        sub_gf!(&mut c, d, f);
        mul_gf!(&mut a, c, A24);
        add_gf!(&mut a, a, d);
        mul_gf!(&mut c, c, a);
        mul_gf!(&mut a, d, f);
        mul_gf!(&mut d, b, x);
        sq_gf!(&mut b, e);
        sel25519(&mut a, &mut b, r);
        sel25519(&mut c, &mut d, r);
    }

    let a16: Gf = a;
    let c16: Gf = c;
    let mut inverse = c16;
    let base = inverse;
    inv25519(&mut inverse, &base);
    let mut out_limb = a16;
    mul_gf!(&mut out_limb, out_limb, inverse);
    let mut out = [0u8; 32];
    pack25519(&mut out, &out_limb);
    out
}

fn point_add(p: &mut [Gf; 4], q: &[Gf; 4]) {
    let (mut a, mut b, mut c, mut d, mut t, mut e, mut f, mut g, mut h) = (
        GF0, GF0, GF0, GF0, GF0, GF0, GF0, GF0, GF0,
    );
    sub_gf!(&mut a, p[1], p[0]);
    sub_gf!(&mut t, q[1], q[0]);
    let aa = a;
    mul_gf!(&mut a, aa, t);
    add_gf!(&mut b, p[0], p[1]);
    add_gf!(&mut t, q[0], q[1]);
    let bb = b;
    mul_gf!(&mut b, bb, t);
    mul_gf!(&mut c, p[3], q[3]);
    let cc = c;
    mul_gf!(&mut c, cc, D2);
    mul_gf!(&mut d, p[2], q[2]);
    let dd = d;
    add_gf!(&mut d, dd, dd);
    sub_gf!(&mut e, b, a);
    sub_gf!(&mut f, d, c);
    add_gf!(&mut g, d, c);
    add_gf!(&mut h, b, a);

    let (ee, hh, gg, ff) = (e, h, g, f);
    mul_gf!(&mut p[0], ee, ff);
    mul_gf!(&mut p[1], hh, gg);
    mul_gf!(&mut p[2], gg, ff);
    mul_gf!(&mut p[3], ee, hh);
}

fn cswap(p: &mut [Gf; 4], q: &mut [Gf; 4], b: i64) {
    for i in 0..4 {
        sel25519(&mut p[i], &mut q[i], b);
    }
}

fn pack_point(r: &mut [u8; 32], p: &[Gf; 4]) {
    let mut tx = GF0;
    let mut ty = GF0;
    let mut zi = GF0;
    inv25519(&mut zi, &p[2]);
    let p0 = p[0];
    mul_gf!(&mut tx, p0, zi);
    let p1 = p[1];
    mul_gf!(&mut ty, p1, zi);
    pack25519(r, &ty);
    r[31] ^= par25519(&tx) << 7;
}

fn scalarmult_point(p: &mut [Gf; 4], q: &mut [Gf; 4], s: &[u8; 32]) {
    p[0] = GF0;
    p[1] = GF1;
    p[2] = GF1;
    p[3] = GF0;
    for i in (0..=255).rev() {
        let b = ((s[i / 8] >> (i & 7)) & 1) as i64;
        cswap(p, q, b);
        point_add(q, p);
        let pp = *p;
        point_add(p, &pp);
        cswap(p, q, b);
    }
}

fn scalarbase(p: &mut [Gf; 4], s: &[u8; 32]) {
    let mut q = [GF0; 4];
    q[0] = X;
    q[1] = Y;
    q[2] = GF1;
    let (x, y) = (X, Y);
    mul_gf!(&mut q[3], x, y);
    scalarmult_point(p, &mut q, s);
}

fn mod_l(x: &mut [i64; 64]) -> [u8; 32] {
    for i in (32..64).rev() {
        let mut carry = 0i64;
        for j in (i - 32)..(i - 12) {
            x[j] += carry - 16 * x[i] * L[j - (i - 32)];
            carry = (x[j] + 128) >> 8;
            x[j] -= carry << 8;
        }
        let j = i - 12;
        x[j] += carry;
        x[i] = 0;
    }
    let mut carry = 0i64;
    for j in 0..32 {
        x[j] += carry - (x[31] >> 4) * L[j];
        carry = x[j] >> 8;
        x[j] &= 255;
    }
    for j in 0..32 {
        x[j] -= carry * L[j];
    }
    let mut r = [0u8; 32];
    for i in 0..32 {
        x[i + 1] += x[i] >> 8;
        r[i] = (x[i] & 255) as u8;
    }
    r
}

fn reduce(hash: &[u8; 64]) -> [u8; 32] {
    let mut x = [0i64; 64];
    for i in 0..64 {
        x[i] = hash[i] as i64;
    }
    mod_l(&mut x)
}

fn unpack_neg(r: &mut [Gf; 4], p: &[u8; 32]) -> bool {
    r[2] = GF1;
    unpack25519(&mut r[1], p);
    let mut num = GF0;
    let mut den = GF0;
    let mut den2 = GF0;
    let mut den4 = GF0;
    let mut den6 = GF0;
    let mut t = GF0;
    let mut chk = GF0;

    sq_gf!(&mut num, r[1]);
    let n = num;
    mul_gf!(&mut den, n, D);
    sub_gf!(&mut num, num, r[2]);
    add_gf!(&mut den, r[2], den);

    sq_gf!(&mut den2, den);
    sq_gf!(&mut den4, den2);
    mul_gf!(&mut den6, den4, den2);
    mul_gf!(&mut t, den6, num);
    let tt = t;
    mul_gf!(&mut t, tt, den);

    let base = t;
    pow2523(&mut t, &base);
    let tt = t;
    mul_gf!(&mut t, tt, num);
    let tt = t;
    mul_gf!(&mut t, tt, den);
    let tt = t;
    mul_gf!(&mut t, tt, den);
    mul_gf!(&mut r[0], t, den);

    sq_gf!(&mut chk, r[0]);
    let cc = chk;
    mul_gf!(&mut chk, cc, den);
    if neq25519(&chk, &num) {
        let r0 = r[0];
        mul_gf!(&mut r[0], r0, I);
    }

    sq_gf!(&mut chk, r[0]);
    let cc = chk;
    mul_gf!(&mut chk, cc, den);
    if neq25519(&chk, &num) {
        return true;
    }

    if par25519(&r[0]) == (p[31] >> 7) {
        sub_gf!(&mut r[0], GF0, r[0]);
    }

    let r0 = r[0];
    mul_gf!(&mut r[3], r0, r[1]);
    false
}

/// Derives the 32-byte Ed25519 public key from a 32-byte seed.
pub fn ed25519_public_key(seed: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha512::new();
    hasher.update(seed);
    let digest: [u8; 64] = hasher.finalize().into();
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&digest[..32]);
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    let mut p = [GF0; 4];
    scalarbase(&mut p, &scalar);
    let mut public = [0u8; 32];
    pack_point(&mut public, &p);
    public
}

/// Signs `message` with the seed, returning the detached 64-byte signature.
pub fn ed25519_sign(seed: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(seed);
    let mut expanded: [u8; 64] = hasher.finalize().into();
    // TweetNaCl clamps the hash output in place, and the low 32 bytes are used
    // again below as the private scalar of S = r + h*a. Clamping a copy only
    // would leave S wrong while R and the public key stay correct.
    expanded[0] &= 248;
    expanded[31] &= 127;
    expanded[31] |= 64;
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&expanded[..32]);

    let mut p = [GF0; 4];
    scalarbase(&mut p, &scalar);
    let mut public = [0u8; 32];
    pack_point(&mut public, &p);

    let mut r_input = Sha512::new();
    r_input.update(&expanded[32..64]);
    r_input.update(message);
    let r_hash: [u8; 64] = r_input.finalize().into();
    let r = reduce(&r_hash);

    let mut signature = [0u8; 64];
    let mut rp = [GF0; 4];
    scalarbase(&mut rp, &r);
    let mut r_point = [0u8; 32];
    pack_point(&mut r_point, &rp);
    signature[..32].copy_from_slice(&r_point);

    let mut h_input = Sha512::new();
    h_input.update(&r_point);
    h_input.update(&public);
    h_input.update(message);
    let h_hash: [u8; 64] = h_input.finalize().into();
    let h = reduce(&h_hash);

    let mut x = [0i64; 64];
    for i in 0..32 {
        x[i] = r[i] as i64;
    }
    for i in 0..32 {
        for j in 0..32 {
            x[i + j] += h[i] as i64 * expanded[j] as i64;
        }
    }
    let s = mod_l(&mut x);
    signature[32..].copy_from_slice(&s);
    signature
}

/// Verifies a detached Ed25519 signature.
pub fn ed25519_verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let mut q = [GF0; 4];
    if unpack_neg(&mut q, public) {
        return false;
    }

    let mut h_input = Sha512::new();
    h_input.update(&signature[..32]);
    h_input.update(public);
    h_input.update(message);
    let h_hash: [u8; 64] = h_input.finalize().into();
    let h = reduce(&h_hash);

    let mut p = [GF0; 4];
    scalarmult_point(&mut p, &mut q, &h);

    let mut s = [0u8; 32];
    s.copy_from_slice(&signature[32..64]);
    let mut q2 = [GF0; 4];
    scalarbase(&mut q2, &s);
    let qq = q2;
    point_add(&mut p, &qq);

    let mut t = [0u8; 32];
    pack_point(&mut t, &p);
    t == signature[..32]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn array32(text: &str) -> [u8; 32] {
        unhex(text).try_into().unwrap()
    }

    #[test]
    fn debug_field_math() {
        let mut two = GF0;
        two[0] = 2;
        let mut inverse = GF0;
        inv25519(&mut inverse, &two);
        let mut product = GF0;
        mul_raw(&mut product, &two, &inverse);
        let mut out = [0u8; 32];
        pack25519(&mut out, &product);
        eprintln!("2*inv(2) = {}", hex(&out));
        assert_eq!(out[0], 1);
        for byte in &out[1..] {
            assert_eq!(*byte, 0);
        }

        let mut three = GF0;
        three[0] = 3;
        let mut square = GF0;
        sq_raw(&mut square, &three);
        pack25519(&mut out, &square);
        eprintln!("3^2 = {}", hex(&out));
        assert_eq!(out[0], 9);
    }

    #[test]
    fn rfc7748_diffie_hellman() {
        let alice_secret = array32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let alice_public = array32("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        let bob_secret = array32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let bob_public = array32("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let shared = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";

        assert_eq!(hex(&x25519(&alice_secret, &bob_public)), shared);
        assert_eq!(hex(&x25519(&bob_secret, &alice_public)), shared);
    }

    #[test]
    fn rfc8032_empty_message() {
        let seed = array32("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let public = array32("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        assert_eq!(ed25519_public_key(&seed), public);

        let signature = ed25519_sign(&seed, b"");
        let expected = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
                        5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";
        assert_eq!(hex(&signature), expected);
        assert!(ed25519_verify(&public, b"", &signature));
        assert!(!ed25519_verify(&public, b"x", &signature));
    }

    #[test]
    fn rfc8032_one_byte_message() {
        let seed = array32("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let public = array32("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
        let signature = ed25519_sign(&seed, &[0x72]);
        let expected = "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
                        085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00";
        assert_eq!(hex(&signature), expected);
        assert!(ed25519_verify(&public, &[0x72], &signature));
    }

    /// Cross-checks generated with Node 24 (OpenSSL): a Diffie-Hellman agreement
    /// between two freshly generated key pairs, and a signature OpenSSL produced
    /// that this code must accept. Ed25519 is deterministic, so the signature
    /// this code produces must also equal OpenSSL's byte for byte.
    #[test]
    fn cross_check_with_openssl_vectors() {
        let alice_secret =
            array32("f0e46200a8a931fc7be4bf014e9c5b41b0b27eb125483e21abbeb55b2f518774");
        let bob_public =
            array32("8a54860b9fee1cf37f342c8ad0a7b80447a4ba53e7da4cc0442bfdf9568d8261");
        let shared = "4655980c3d54613641cc1923e86ba94545483781a47f5efb5eec0244e6c4a27a";
        assert_eq!(hex(&x25519(&alice_secret, &bob_public)), shared);

        let seed = array32("c7f49d9062cbc535241de6eb64574e79db90ecbad88fe63da52d5ffa78f40f69");
        let public = array32("23b07791d65fcd64a2fc1a7449aa59965587b371e085ff824eb8390d6e088e13");
        assert_eq!(ed25519_public_key(&seed), public);

        let message = b"pair-verify";
        let signature: [u8; 64] = unhex(
            "a0f7a41e77ab1569796df896e6f71428585fdcf41c20d16cd702f9d770e32240\
             7b501b92c88aca8838977b0c4ff0bfa2874260e0900525baec64bb9c03a94703",
        )
        .try_into()
        .unwrap();
        assert!(ed25519_verify(&public, message, &signature));
        assert_eq!(ed25519_sign(&seed, message), signature);
    }
}
