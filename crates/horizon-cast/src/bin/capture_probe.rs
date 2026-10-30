//! Screen acquisition probe: the P1-A gate.
//!
//! The tile codec is only fast if the pixels arrive fast, and "fast" is a number
//! this machine has to give rather than an assumption. Answers, in order:
//! - can DXGI enumerate the adapters and outputs here, and if a call fails, which
//!   one and with what HRESULT;
//! - which backend `Capture::open` ends up using;
//! - cost per acquired frame, with the wait excluded, and what the dirty-rect
//!   distribution looks like on real content;
//! - what this driver does with the pointer: shape type and size, and whether a
//!   mouse move alone produces a frame;
//! - capture → encode → decode against [`Budget`], on the live desktop.

use horizon_cast::capture::{Backend, Capture};
use horizon_cast::metrics::{now_us, Budget, Samples};
use horizon_cast::wire::{FrameKind, WireDecoder, WireEncoder};
use std::time::Duration;

fn hresult(code: i32) -> String {
    format!("0x{:08X}", code as u32)
}

/// Walk the adapter/output tree by hand, so a failure says which call broke.
/// `Capture::monitors` can only answer "here is the list"; the HRESULT is the
/// difference between "this machine has no display" and "the API moved".
fn enumerate() {
    println!("== enumeration ==");
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory, IDXGIFactory1};
    let f1 = match unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
        Ok(f) => f,
        Err(e) => {
            println!("  CreateDXGIFactory1 -> {}", hresult(e.code().0));
            return;
        }
    };
    let factory = match f1.cast::<IDXGIFactory>() {
        Ok(f) => f,
        Err(e) => {
            println!("  cast to IDXGIFactory -> {}", hresult(e.code().0));
            return;
        }
    };
    for adapter_index in 0..8u32 {
        let adapter = match unsafe { factory.EnumAdapters(adapter_index) } {
            Ok(a) => a,
            Err(e) => {
                println!(
                    "  EnumAdapters({adapter_index}) -> {}",
                    hresult(e.code().0)
                );
                break;
            }
        };
        match unsafe { adapter.GetDesc() } {
            Ok(d) => println!(
                "  adapter {adapter_index}: {} (vid {:04X} pid {:04X})",
                utf16(&d.Description),
                d.VendorId,
                d.DeviceId
            ),
            Err(e) => println!("  adapter {adapter_index}: GetDesc -> {}", hresult(e.code().0)),
        }
        for output_index in 0..8u32 {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(e) => {
                    println!(
                        "    EnumOutputs({output_index}) -> {}",
                        hresult(e.code().0)
                    );
                    break;
                }
            };
            match unsafe { output.GetDesc() } {
                Ok(d) => println!(
                    "    output {output_index}: [{},{} {}x{}] attached={} name={}",
                    d.DesktopCoordinates.left,
                    d.DesktopCoordinates.top,
                    d.DesktopCoordinates.right - d.DesktopCoordinates.left,
                    d.DesktopCoordinates.bottom - d.DesktopCoordinates.top,
                    d.AttachedToDesktop.as_bool(),
                    utf16(&d.DeviceName)
                ),
                Err(e) => {
                    println!(
                        "    output {output_index}: GetDesc -> {}",
                        hresult(e.code().0)
                    );
                    break;
                }
            }
        }
    }
    match Capture::monitors() {
        Ok(list) => {
            println!("  Capture::monitors() -> {} entries", list.len());
            for m in list {
                println!(
                    "    #{:<2} {} {}x{} @{}hz desktop=({},{}) attached={} primary={} duplicable={}",
                    m.index,
                    m.name,
                    m.width,
                    m.height,
                    m.fps_ceiling(),
                    m.x,
                    m.y,
                    m.attached,
                    m.primary,
                    m.adapter != u32::MAX
                );
            }
        }
        Err(e) => println!("  Capture::monitors() -> {e}"),
    }
}

fn utf16(raw: &[u16]) -> String {
    String::from_utf16_lossy(raw.iter().take_while(|c| **c != 0).copied().collect::<Vec<u16>>().as_slice())
}

/// Do the six GDI calls the fallback depends on one at a time, so a failure names
/// the call rather than the module. Both the small probe and the full desktop size
/// are exercised: a copy that works at 320x240 and fails at the monitor's own
/// dimensions is a surface-size problem, not a handle problem.
fn gdi_selftest(width: i32, height: i32) {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, HGDIOBJ, ReleaseDC, SelectObject, BI_RGB, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
    };
    println!("\n== gdi self-test ==");
    unsafe {
        let screen = GetDC(None);
        println!(
            "  GetDC              ={:p} valid={}",
            screen.0,
            !screen.is_invalid()
        );
        let mem = CreateCompatibleDC(Some(screen));
        println!(
            "  CreateCompatibleDC ={:p} valid={}",
            mem.0,
            !mem.is_invalid()
        );
        for (w, h) in [(320i32, 240i32), (width, height)] {
            let bmp = CreateCompatibleBitmap(screen, w, h);
            println!("  -- copy {w}x{h} --");
            println!("  CompatibleBitmap   ={:p}", bmp.0);
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            println!("  SelectObject       ->old={:p}", old.0);
            if let Err(e) = BitBlt(mem, 0, 0, w, h, Some(screen), 0, 0, SRCCOPY) {
                println!("  BitBlt             = {}", hresult(e.code().0));
                let _ = SelectObject(mem, old);
                let _ = DeleteObject(HGDIOBJ(bmp.0));
                continue;
            }
            println!("  BitBlt             = ok");
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (w as usize * h as usize * 4).min(4096)];
            let rows = GetDIBits(
                mem,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut info,
                DIB_RGB_COLORS,
            );
            let centre = (w as usize / 2 + h as usize / 2 * w as usize) * 4;
            println!(
                "  GetDIBits          = {rows} rows, centre BGRA={:?}",
                &buf[centre..centre + 4]
            );
            let _ = SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
}

fn main() {
    enumerate();

    println!("\n== open ==");
    let mut cap = match Capture::open(0) {
        Ok(c) => c,
        Err(e) => {
            println!("  Capture::open(0) -> {e}");
            return;
        }
    };
    println!(
        "  backend={} size={}x{} stride={} ceiling={}hz",
        cap.backend().name(),
        cap.width(),
        cap.height(),
        cap.stride(),
        cap.monitor().fps_ceiling()
    );
    if cap.backend() == Backend::Gdi {
        println!("  duplication was refused — everything below measures the fallback");
        gdi_selftest(cap.width() as i32, cap.height() as i32);
    }

    let frames = acquisition(&mut cap);
    let tick = tick_for(&cap);
    pipeline(&mut cap, frames, tick);

    println!("\n== budget for this mode ==");
    let b = Budget::LOW_LATENCY_1080P30;
    println!(
        "  capture+copy {}us   encode {}us   dejitter {}us   decode {}us   total {}us",
        b.capture_us + b.convert_us,
        b.encode_us,
        b.dejitter_us,
        b.decode_us,
        b.total_us()
    );
}

/// How often to ask this backend for a frame.
///
/// Duplication blocks until the desktop presents something, so a long timeout costs
/// nothing. BitBlt has no event to wait for, so the sender's own frame clock — 30 fps
/// here — is what spaces the copies.
fn tick_for(cap: &Capture) -> Duration {
    if cap.backend() == Backend::Dxgi {
        Duration::from_millis(400)
    } else {
        Duration::from_millis(33)
    }
}

/// Drive the loop long enough to see real content: a mouse move is the frame type
/// that decides whether a share feels alive or frozen.
///
/// The desktop here does not move on its own while a measurement is running, and a
/// run with nothing changing answers "idle cost" — which matters — but not the one
/// question this section is really about: what a cursor move costs. So the probe
/// moves the pointer itself, in a small circle, and puts it back where it was.
///
/// Two numbers come out and they mean different things: `copy` is the CPU spent
/// getting pixels, which is what the budget is written against; `interval` is the
/// gap between frames, which on a duplication source is mostly the desktop's own
/// presentation schedule.
fn acquisition(cap: &mut Capture) -> usize {
    println!("\n== acquisition (self-driven pointer moves) ==");
    let b = Budget::LOW_LATENCY_1080P30;
    let tick = tick_for(cap);
    let mut copy = Samples::new(5000);
    let mut interval = Samples::new(5000);
    let mut rects = 0usize;
    let mut area = 0u64;
    let mut tiny = 0usize;
    let mut idle = 0usize;
    let mut frames = 0usize;
    let mut quiet = 0usize;
    let mut shape: (u32, u32) = (0, 0);
    let mut shape_types = [0u32; 8];
    let start = now_us();
    let home = cursor_pos();
    let (w, h) = (cap.width() as i32, cap.height() as i32);
    let mut last_end = 0u64;
    while frames < 400 && now_us() - start < 25_000_000 {
        // Every few frames, if the desktop has gone quiet, move the pointer. The
        // cadence is slower than the frame clock on purpose: two frames per poke
        // is what a hand-moving a mouse looks like, and anything faster measures
        // the probe rather than a person.
        if quiet >= 4 {
            let t = frames as f64 * 0.35;
            set_cursor(
                (w / 2 + (60.0 * t.cos()) as i32).clamp(1, w - 20),
                (h / 2 + (60.0 * t.sin()) as i32).clamp(1, h - 20),
            );
            quiet = 0;
        }
        match cap.capture(tick) {
            Ok(None) => {
                idle += 1;
                quiet += 1;
                continue;
            }
            Ok(Some(f)) => {
                let at = now_us();
                copy.push(f.copy_us);
                if last_end != 0 {
                    interval.push(at - last_end);
                }
                last_end = at;
                quiet += 1;
                frames += 1;
                rects += f.dirty.len();
                let a: u64 = f.dirty.iter().map(|r| r.w as u64 * r.h as u64).sum();
                area += a;
                if a < 64 * 64 * 4 {
                    tiny += 1;
                }
                let s = cap.pointer_shape();
                if s.0 != 0 && s.1 != 0 {
                    shape = s;
                    if (s.0 as usize) < shape_types.len() {
                        shape_types[s.0 as usize - 1] += 1;
                    }
                }
            }
            Err(e) => {
                println!("  capture error: {e}");
                break;
            }
        }
    }
    if let Some((x, y)) = home {
        set_cursor(x, y);
    }
    println!("  frames={frames} idle-returns={idle}");
    println!("  copy    {}", copy.summary());
    println!("  interval {interval}", interval = interval.summary());
    if frames > 0 {
        let small = (b.capture_us + b.convert_us) as u32;
        println!(
            "  copy p95 {}us vs capture budget {small}us: {}",
            copy.percentile_us(95.0),
            if copy.percentile_us(95.0) <= small {
                "INSIDE"
            } else {
                "OVER"
            }
        );
        println!(
            "  rects/frame={:.1} area/frame={:.0}px² tiny-frames={tiny} ({:.0}%)",
            rects as f64 / frames as f64,
            area as f64 / frames as f64,
            100.0 * tiny as f64 / frames as f64
        );
    }
    if shape.1 != 0 {
        println!(
            "  pointer shape: type={} bytes={} (drawn={})",
            shape.0,
            shape.1,
            matches!(shape.0, 1 | 2)
        );
        println!("  shape type histogram (mono/color/masked): {:?}", &shape_types[..3]);
    } else {
        println!("  pointer shape: never reported in this run");
    }
    frames
}

/// Where the pointer is, in desktop coordinates.
#[cfg(windows)]
fn cursor_pos() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok() }.map(|_| (p.x, p.y))
}

#[cfg(windows)]
fn set_cursor(x: i32, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
    let _ = unsafe { SetCursorPos(x, y) };
}

#[cfg(not(windows))]
fn cursor_pos() -> Option<(i32, i32)> {
    None
}

#[cfg(not(windows))]
fn set_cursor(_x: i32, _y: i32) {}

/// The sender's whole CPU path for one frame, on live pixels.
fn pipeline(cap: &mut Capture, wanted: usize, tick: Duration) {
    println!("\n== capture -> encode -> decode ==");
    let b = Budget::LOW_LATENCY_1080P30;
    let mut enc = WireEncoder::new(cap.width(), cap.height(), 16_000_000, 30);
    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(8);
    enc.set_threads(threads);
    let mut dec = WireDecoder::new(cap.width(), cap.height());
    let mut encode = Samples::new(5000);
    let mut decode = Samples::new(5000);
    let mut e2e = Samples::new(5000);
    let mut bytes = 0usize;
    let mut tiles = 0usize;
    let mut keys = 0usize;
    let mut applied = 0usize;
    let mut frames = 0usize;
    let mut copy = Samples::new(5000);
    let start = now_us();
    while frames < wanted.max(30) && now_us() - start < 25_000_000 {
        let Some(f) = cap.capture(tick).ok().flatten() else {
            continue;
        };
        copy.push(f.copy_us);
        // The encoder reads the capture buffer where it lies: no second copy, and
        // the capture stride is passed through rather than normalised away.
        let Ok(packet) = enc.encode(f.bgra, f.stride, &f.dirty, f.captured_us) else {
            continue;
        };
        encode.push(packet.encode_us);
        bytes += packet.bytes.len();
        tiles += packet.tiles;
        if packet.kind == FrameKind::Key {
            keys += 1;
        }
        let t2 = now_us();
        match dec.decode(&packet.bytes) {
            Ok(_) => {
                applied += 1;
                decode.push(now_us() - t2);
                // Loopback: the peer is this process, so the span from the capture
                // mark to the moment the framebuffer holds the picture is real
                // glass-to-glass minus only the wire and the present.
                e2e.push(now_us() - f.captured_us);
            }
            Err(e) => println!("  decode refused frame {}: {e}", packet.frame_id),
        }
        enc.commit(packet.frame_id);
        frames += 1;
    }
    println!("  threads={threads} frames={frames} applied={applied} key={keys}");
    println!("  copy      {}", copy.summary());
    println!("  encode    {}", encode.summary());
    println!("  decode    {}", decode.summary());
    println!("  capture->applied {}", e2e.summary());
    if frames > 0 {
        println!(
            "  bytes/frame={:.0} tiles/frame={:.1} quality={}",
            bytes as f64 / frames as f64,
            tiles as f64 / frames as f64,
            enc.quality()
        );
        println!(
            "  copy+encode p95 {}us vs budget {}us: {}",
            copy.percentile_us(95.0) + encode.percentile_us(95.0),
            b.capture_us + b.convert_us + b.encode_us,
            if (copy.percentile_us(95.0) + encode.percentile_us(95.0)) as u64
                <= b.capture_us + b.convert_us + b.encode_us
            {
                "INSIDE"
            } else {
                "OVER"
            }
        );
    }

    // A resync frame is the worst thing the encoder is ever asked to do, and the
    // only place where a full-desktop encode is not an artefact of the test: it
    // happens when a peer joins and whenever one reports a lost base. Measured
    // separately because "one frame took 40ms" is invisible in a delta-heavy
    // distribution and decides how long a join takes.
    if let Some(f) = cap.capture(tick).ok().flatten() {
        if let Ok(k) = enc.encode_with(f.bgra, f.stride, &f.dirty, f.captured_us, true) {
            let t1 = now_us();
            let decoded = dec.decode(&k.bytes).is_ok();
            println!(
                "  forced key: {} tiles, {} bytes, encode {}us, decode {}us, ok={decoded}",
                k.tiles,
                k.bytes.len(),
                k.encode_us,
                now_us() - t1
            );
            enc.commit(k.frame_id);
        }

        // Threading check. A resync is the only frame that encodes the whole grid,
        // so it is the only one where the lane count shows up at all — and a
        // static partition of spatially clustered content is exactly the case where
        // lane cost is uneven. One desktop copy, five lane counts, so the numbers
        // are comparable and say whether more threads buy anything.
        let (w, h, stride) = (f.width, f.height, f.stride);
        let still = f.bgra.to_vec();
        print!("  key encode by lanes (best of 3):");
        for lanes in [1usize, 2, 4, 8, 12, 16] {
            // A fresh encoder per sample: the first frame of an encoder is promoted
            // to a key on its own, and `encode_with` advances the frame id, so
            // reusing one would measure something other than a cold resync.
            let us = (0..3)
                .filter_map(|_| {
                    let mut solo = WireEncoder::new(w, h, 16_000_000, 30);
                    solo.set_threads(lanes);
                    solo.encode_with(&still, stride, &[], 0, true).ok()
                })
                .map(|k| k.encode_us)
                .min()
                .unwrap_or(0);
            print!(" {lanes}={us}us");
        }
        println!();

        // The receiver's own version of the same question. It matters on a laptop
        // presenter with a software decoder: the sender's frame is only as fast as
        // the slowest listener, and a resync is the one frame that makes every
        // listener decode the whole desktop at once.
        let bytes = enc
            .encode_with(&still, stride, &[], 0, true)
            .map(|k| k.bytes)
            .unwrap_or_default();
        print!("  key decode by lanes (best of 3):");
        for lanes in [1usize, 2, 4, 8, 12, 16] {
            let us = (0..3)
                .map(|_| {
                    let mut solo = WireDecoder::new(w, h);
                    solo.set_threads(lanes);
                    let t = now_us();
                    // Decode into a fresh receiver each time: a reused one would
                    // have the picture already in its framebuffer and would prove
                    // nothing about the cost of painting it.
                    let ok = solo.decode(&bytes).is_ok();
                    (ok, now_us() - t)
                })
                .filter(|(ok, _)| *ok)
                .map(|(_, us)| us)
                .min()
                .unwrap_or(0);
            print!(" {lanes}={us}us");
        }
        println!();
    }
}
