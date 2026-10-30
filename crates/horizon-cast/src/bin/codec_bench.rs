//! The tile codec's hot path, measured on synthetic frames.
//!
//! `capture-probe` measures the real desktop, which is the right thing to do until
//! a number comes back flat — and the first run did: a forced key frame took
//! ~25 ms with one thread and ~22 ms with eight. Either the lanes are not doing
//! what the code says they do, or something else is the cost. A live desktop cannot
//! answer that, because its content changes between samples; this bin generates the
//! same pixels every time and separates *how the work is shared* from *what the work
//! costs*.
//!
//! The first run answered it, and also exposed the other half: the receiver had no
//! lanes to begin with, and a photographic key cost it 54 ms — nine times the
//! 6 ms the budget gives the whole decode stage. So both sides are swept here.
//!
//! Three frames, because the codec picks its path per tile and the mix is what
//! decides both the byte count and the lane balance:
//! - `ui` — flat colour and hard edges: every tile takes the deflate path.
//! - `photo` — a smooth gradient with dither: every tile takes the JPEG path.
//! - `mixed` — half of each, spatially clustered. This is the case a static
//!   contiguous partition of the tile list handles worst, because one lane gets all
//!   the expensive tiles.

use horizon_cast::wire::{Rect, WireDecoder, WireEncoder};
use std::time::{Duration, Instant};

const W: u16 = 1920;
const H: u16 = 1080;

/// Deterministic noise: a plain LCG, so the frames are identical on every run and
/// two samples of the same lane count are comparable.
struct Rand(u64);

impl Rand {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
}

fn ui_frame() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut px = vec![0u8; w * h * 4];
    let mut r = Rand(1);
    // Flat office palette: a white page, a coloured ribbon, black text-like runs.
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        let y = (i / w) as u32;
        let band = if y < 90 {
            [216, 178, 124] // BGRA ribbon
        } else if y > h as u32 - 40 {
            [200, 200, 200]
        } else {
            [255, 255, 255]
        };
        p[..3].copy_from_slice(&band);
        p[3] = 255;
    }
    // Text: 8x16 black cells on a 12px pitch. High entropy per row, zero colour.
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        let y = (i / w) as u32;
        let x = (i % w) as u32;
        if y > 120 && y < 900 && x > 60 && x < 1500 && (y % 24) < 16 && (x % 12) < 8 {
            p[..3].fill(30);
        }
    }
    // A few flat-colour callouts, because a slide has shapes in it.
    for _ in 0..24 {
        let (x, y, bw, bh) = (
            80 + r.next() as usize % 1200,
            120 + r.next() as usize % 700,
            60 + r.next() as usize % 300,
            40 + r.next() as usize % 200,
        );
        let c = [r.byte(), r.byte(), r.byte()];
        for row in y..(y + bh).min(h) {
            for col in x..(x + bw).min(w) {
                let d = (row * w + col) * 4;
                px[d..d + 3].copy_from_slice(&c);
            }
        }
    }
    px
}

fn photo_frame() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut px = vec![0u8; w * h * 4];
    let mut r = Rand(7);
    // A two-axis gradient with per-pixel dither: no 8x8 block is ever uniform, so
    // every tile is photographic and the JPEG branch is taken everywhere.
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        let y = (i / w) as f32;
        let x = (i % w) as f32;
        p[0] = (x / w as f32 * 255.0) as u8;
        p[1] = (y / h as f32 * 255.0) as u8;
        p[2] = ((x + y) / (w + h) as f32 * 255.0) as u8;
        if i % 3 == 0 {
            p[1] = p[1].wrapping_add(r.byte() >> 5);
        }
        p[3] = 255;
    }
    px
}

fn mixed_frame() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut px = photo_frame();
    let flat = ui_frame();
    // Left half photographic, right half UI: the tiles are sorted by row, so the
    // expensive ones come first and stay together.
    for y in 0..h {
        let s = (y * w + w / 2) * 4;
        let d = (y * w + w / 2) * 4;
        px[d..d + w / 2 * 4].copy_from_slice(&flat[s..s + w / 2 * 4]);
    }
    px
}

/// One cold full-frame key encode per lane count, best of three, on a fresh
/// encoder every time so each sample really is a key frame.
fn key_encode(src: &[u8], lanes: usize, samples: usize) -> (u64, usize, usize, usize) {
    let mut best = u64::MAX;
    let mut bytes = 0;
    let mut lossy = 0;
    let mut tiles = 0;
    for _ in 0..samples {
        let mut enc = WireEncoder::new(W, H, 16_000_000, 30);
        enc.set_threads(lanes);
        let Ok(k) = enc.encode_with(src, W as usize * 4, &[], 0, true) else {
            continue;
        };
        if k.encode_us < best {
            best = k.encode_us;
            bytes = k.bytes.len();
            lossy = k.lossy;
            tiles = k.tiles;
        }
    }
    (best, bytes, lossy, tiles)
}

fn main() {
    let frames = [
        ("ui", ui_frame()),
        ("photo", photo_frame()),
        ("mixed", mixed_frame()),
    ];
    println!("key-frame encode, {W}x{H}, best of 3");
    println!("  lanes      1      2      4      8     16     20");
    for (name, px) in &frames {
        let mut row = format!("  {name:<5}");
        for lanes in [1usize, 2, 4, 8, 16, 20] {
            let (us, _, _, _) = key_encode(px, lanes, 3);
            row.push_str(&format!(" {:>5}", us / 1000));
        }
        println!("{row}  ms");
    }
    println!("\nbytes and the split, at eight lanes");
    for (name, px) in &frames {
        let (us, bytes, lossy, tiles) = key_encode(px, 8, 3);
        println!(
            "  {name:<5} {bytes:>7} bytes/frame  {lossy}/{tiles} tiles lossy  {us}us  {:.1} Mbit/s at 30fps",
            bytes as f64 * 8.0 * 30.0 / 1e6
        );
    }

    // Decode side: the receiver has to keep up with its own sender, and a key frame
    // is where it is most likely to miss a deadline. Measured across lane counts for
    // the same reason the encoder is — the first version of this bin found the
    // sender's flat scaling and the receiver's absence of any.
    println!("\nkey-frame decode, best of 3");
    println!("  lanes      1      2      4      8     16");
    for (name, px) in &frames {
        let mut enc = WireEncoder::new(W, H, 16_000_000, 30);
        enc.set_threads(8);
        let Ok(k) = enc.encode_with(px, W as usize * 4, &[], 0, true) else {
            continue;
        };
        enc.commit(k.frame_id);
        let mut row = format!("  {name:<5}");
        for lanes in [1usize, 2, 4, 8, 16] {
            let mut best = u64::MAX;
            for _ in 0..3 {
                let mut fresh = WireDecoder::new(W, H);
                fresh.set_threads(lanes);
                let t = Instant::now();
                if fresh.decode(&k.bytes).is_ok() {
                    best = best.min(t.elapsed().as_micros() as u64);
                }
            }
            row.push_str(&format!(" {:>5}", best / 1000));
        }
        println!("{row}  ms");
    }

    // And the steady state a receiver really lives in: a black bar painted into a
    // known region, so the frame is a genuine delta of a handful of tiles and the
    // lane floor decides whether the receiver spawns anything at all.
    println!("\ndelta of ten tiles, at eight sender lanes");
    for (name, px) in &frames {
        let mut enc = WireEncoder::new(W, H, 16_000_000, 30);
        enc.set_threads(8);
        let Ok(k) = enc.encode_with(px, W as usize * 4, &[], 0, true) else {
            continue;
        };
        enc.commit(k.frame_id);
        let mut dec = WireDecoder::new(W, H);
        let mut small = px.clone();
        let dirty = [Rect::new(384, 384, 640, 64)];
        for y in 384..448 {
            for x in 384..1024 {
                let d = (y * W as usize + x) * 4;
                small[d..d + 4].fill(0);
            }
        }
        let t0 = horizon_cast::metrics::now_us();
        let Ok(d1) = enc.encode(&small, W as usize * 4, &dirty, t0) else {
            continue;
        };
        let t = Instant::now();
        let _ = dec.decode(&d1.bytes);
        println!(
            "  {name:<5} {} tiles, {} bytes, encode {}us, decode {}us, kind={:?}",
            d1.tiles,
            d1.bytes.len(),
            d1.encode_us,
            t.elapsed().as_micros() as u64,
            d1.kind
        );
    }

    // The path's own floor: an 8 MB memcpy is what a whole-frame copy costs, and
    // any comparison between two encoders that ignores it is a comparison between
    // two numbers that are not the same thing.
    let mut buf = vec![0u8; W as usize * H as usize * 4];
    let src = &frames[0].1;
    let t = Instant::now();
    for _ in 0..5 {
        // Without black_box the loop is a store to a buffer nobody reads and the
        // optimiser deletes it; the first run of this bin printed 0us.
        std::hint::black_box(&mut buf);
        buf.copy_from_slice(std::hint::black_box(src));
    }
    std::hint::black_box(&buf);
    println!(
        "\n  8 MB copy: {}us each",
        t.elapsed().as_micros() as u64 / 5
    );
    let _ = Duration::from_millis(1);
}
