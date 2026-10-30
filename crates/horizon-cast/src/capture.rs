//! Screen acquisition: DXGI desktop duplication, with a GDI BitBlt fallback.
//!
//! The contract this module keeps is the one [`crate::wire`] is built on: a
//! buffer plus a list of rectangles that *may* have changed. That list is a hint
//! and never a claim of correctness — the tile encoder re-diffs every rectangle
//! it is given — so a wrong or missing dirty rect from the operating system costs
//! bandwidth or CPU time but can never leave a stale pixel on the screen.
//!
//! Both backends are here because neither is sufficient alone:
//! - **DXGI duplication** is the only way to see what is actually on the screen:
//!   DWM's composited output at up to the display's refresh rate, with the changed
//!   rectangles handed to us for free. It is refused on a secure desktop, by some
//!   remote sessions and by some drivers, and it dies whenever the display
//!   topology changes. It can also fail at the very first step: measured on this
//!   host from a disconnected remote session, `EnumAdapters` returned three
//!   adapters and `EnumOutputs(0)` answered `DXGI_ERROR_NOT_FOUND` (0x887A0002) on
//!   every one of them, because the desktop there was rendered by a virtual
//!   display driver DXGI does not bind an output to. From the signed-in console
//!   session the same call graph yields `\\.\DISPLAY1 1920x1080` and duplication
//!   works, which is why the fallback has to be a fallback and not a dead branch.
//! - **GDI BitBlt** never fails, reports no rectangles, and gives no cursor shape —
//!   so it hands the encoder the whole frame, lets the tile diff decide what
//!   changed, and shows whatever the desktop driver put in the copy. It is the
//!   fallback, not the fast path.
//!
//! The monitor list follows the same rule: DXGI's adapter/output tree when it has
//! outputs, Win32's `EnumDisplayMonitors` when it has not. `Capture::open` therefore
//! works on the machine above, where a duplication-only design could not even name
//! a display.
//!
//! Both write into a buffer they own and hand back a borrow, because allocating
//! 33 MB per frame at 30 fps costs more than the encoding does.
//!
//! ## What the first live run measured (`capture-probe`, 1920x1080 duplication)
//!
//! - A cursor move costs **no pixel copy at all**: the frame reuses the buffer and
//!   reports one or two tiles, so `copy_us` is 0 for 97% of the frames in a
//!   pointer-driven run and p99 is 2.4 ms on the frames that really re-read the
//!   desktop (an 8 MB `CopyResource` + map + memcpy). The 3 ms capture budget is
//!   the whole-frame case, and it holds.
//! - This driver sends the pointer as a **colour** shape (`Type=2`, 4 KB), so the
//!   premultiplied blend path — not the monochrome one — is what a share here is
//!   made of. Both are implemented because the shape type is the driver's choice,
//!   not ours.
//! - `GetFrameDirtyRects(0, NULL, &needed)` — the documented size query — is
//!   **refused** by this driver, which is why both rect queries are a single call
//!   into a 1024-entry buffer that lives on the capture and treats
//!   `DXGI_ERROR_MORE_DATA` as "re-diff the frame".
//!
//! See [`crate::wire`] for the encode side of the same run.

use std::time::{Duration, Instant};

use crate::error::{CastError, CastResult};
use crate::wire::Rect;

/// A captured desktop frame. `bgra` is BGRA with `stride` bytes per row: the
/// texture's own pitch for DXGI, `width * 4` for GDI.
#[derive(Debug)]
pub struct Frame<'a> {
    pub bgra: &'a [u8],
    pub stride: usize,
    pub width: u16,
    pub height: u16,
    /// Rectangles that may have changed since the previous frame.
    pub dirty: Vec<Rect>,
    /// Microseconds spent moving the pixels, with any wait for the desktop to
    /// present something excluded. This, not the wall time between two calls, is
    /// what the CPU budget is judged against — and it is zero on a frame that
    /// reused the buffer because only the cursor moved.
    pub copy_us: u64,
    /// QPC microseconds at the moment the pixels were in hand — the first mark of
    /// the glass-to-glass clock.
    pub captured_us: u64,
    pub frame_no: u64,
    pub backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Dxgi,
    Gdi,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Dxgi => "dxgi-duplication",
            Backend::Gdi => "gdi-bitblt",
        }
    }
}

/// One display, in a single deterministic order: the primary first, then by
/// desktop position.
///
/// Two enumerators can produce this list — DXGI's adapter/output tree, which is the
/// one that can be *duplicated*, and Win32's monitor list, which exists even where
/// DXGI sees no outputs (a virtual display driver, or a session whose desktop is not
/// bound to a DXGI output). Sorting both the same way is what lets the UI show one
/// numbered list while the capture layer still prefers the duplicable one;
/// `adapter`/`output` are `u32::MAX` on an entry that only the GDI enumerator saw.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub index: u32,
    pub adapter: u32,
    pub output: u32,
    pub primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub attached: bool,
    pub name: String,
}

impl Monitor {
    /// The display's own rate, which is the ceiling on how often a new picture
    /// can exist. Zero means DXGI would not say, and 60 is the assumption.
    pub fn fps_ceiling(&self) -> u32 {
        if self.refresh_hz > 5.0 {
            self.refresh_hz.round().max(1.0) as u32
        } else {
            60
        }
    }
}

/// A desktop source. There is deliberately no trait: a second platform would need
/// its own capture API, and an abstraction with one implementation is how
/// half-finished backends get shipped.
pub enum Capture {
    #[cfg(windows)]
    Dxgi(DxgiCapture),
    #[cfg(windows)]
    Gdi(GdiCapture),
}

impl Capture {
    /// Open the best backend for a monitor: duplication if the desktop allows it,
    /// BitBlt if it does not.
    pub fn open(monitor_index: u32) -> CastResult<Self> {
        #[cfg(windows)]
        {
            match DxgiCapture::open(monitor_index) {
                Ok(d) => Ok(Capture::Dxgi(d)),
                // Duplication is refused by a secure desktop, by some remote
                // sessions and by some drivers. That is a fallback, not a
                // failure: the room still gets a picture, only slower, and the
                // backend name in the status line says which one it is.
                Err(_) => GdiCapture::open(monitor_index).map(Capture::Gdi),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = monitor_index;
            Err(CastError::Unsupported("capture needs Windows"))
        }
    }

    pub fn backend(&self) -> Backend {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(_) => Backend::Dxgi,
            #[cfg(windows)]
            Capture::Gdi(_) => Backend::Gdi,
        }
    }

    pub fn width(&self) -> u16 {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(d) => d.width,
            #[cfg(windows)]
            Capture::Gdi(g) => g.width,
        }
    }

    pub fn height(&self) -> u16 {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(d) => d.height,
            #[cfg(windows)]
            Capture::Gdi(g) => g.height,
        }
    }

    pub fn stride(&self) -> usize {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(d) => d.stride,
            #[cfg(windows)]
            Capture::Gdi(g) => g.stride,
        }
    }

    /// Which monitor this source reads, with the size, the desktop offset and the
    /// refresh ceiling the sender needs before it can pick a frame rate.
    pub fn monitor(&self) -> &Monitor {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(d) => &d.monitor,
            #[cfg(windows)]
            Capture::Gdi(g) => &g.monitor,
        }
    }

    /// The cursor shape the driver last sent: `(type, buffer size)`, both zero on
    /// a source that has never reported one. The probe prints this because the
    /// documentation describes what a driver *may* do and only a run says what
    /// this one does.
    pub fn pointer_shape(&self) -> (u32, u32) {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(d) => (d.shape_type, d.shape_bytes),
            #[cfg(windows)]
            Capture::Gdi(_) => (0, 0),
        }
    }

    /// Wait until the desktop presents something new, up to `timeout`.
    ///
    /// `Duration::ZERO` polls and `Duration::MAX` waits forever. `Ok(None)` means
    /// nothing was presented inside the window — the normal answer for an idle
    /// desktop, and the reason the sender costs no CPU when nothing moves.
    pub fn capture(&mut self, timeout: Duration) -> CastResult<Option<Frame<'_>>> {
        match self {
            #[cfg(windows)]
            Capture::Dxgi(d) => d.capture(timeout),
            #[cfg(windows)]
            Capture::Gdi(g) => g.capture(timeout),
        }
    }

    pub fn monitors() -> CastResult<Vec<Monitor>> {
        #[cfg(windows)]
        {
            monitors()
        }
        #[cfg(not(windows))]
        {
            Err(CastError::Unsupported("capture needs Windows"))
        }
    }
}

/// The pointer shape DXGI attached to a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerShape {
    /// Premultiplied ARGB, `pitch` bytes per row. This is what a composited
    /// desktop hands out on Windows 10 and 11.
    Color,
    /// A 1bpp AND bitmap followed by a 1bpp XOR bitmap.
    Monochrome,
}

/// Paint a cursor into a captured frame at `x, y`, already hotspot-adjusted.
///
/// The pointer is part of what the room sees; a share without one looks frozen
/// even while every pixel of content moves. Clipping is explicit because a cursor
/// half off the edge of the screen is a normal state, not a surprise.
///
/// Returns the rectangle actually touched, or `None` when nothing was drawn, so
/// the caller never adds a dirty rect for a no-op.
#[allow(clippy::too_many_arguments)]
pub fn blend_pointer(
    dst: &mut [u8],
    dst_stride: usize,
    frame_w: usize,
    frame_h: usize,
    x: i32,
    y: i32,
    kind: PointerShape,
    shape: &[u8],
    w: usize,
    h: usize,
    pitch: usize,
) -> Option<Rect> {
    if w == 0 || h == 0 || pitch == 0 {
        return None;
    }
    let r = clipped_rect(x, y, w, h, frame_w, frame_h)?;
    let bit = |row: usize, col: usize| -> u8 {
        let byte = row * pitch + col / 8;
        shape.get(byte).copied().unwrap_or(0) >> (7 - col % 8) & 1
    };
    for sy in (r.y as usize)..(r.y1() as usize) {
        for sx in (r.x as usize)..(r.x1() as usize) {
            let d = sy * dst_stride + sx * 4;
            let Some(px) = dst.get_mut(d..d + 4) else { continue };
            // Coordinates inside the shape, in i32 because the shape can start at
            // a negative offset when the cursor is partly off screen.
            let cx = (sx as i32 - x) as usize;
            let cy = (sy as i32 - y) as usize;
            match kind {
                PointerShape::Color => {
                    let s = cy * pitch + cx * 4;
                    let Some(argb) = shape.get(s..s + 4) else { continue };
                    let a = argb[3] as u16;
                    if a == 0 {
                        continue;
                    }
                    let inv = 255 - a;
                    for (dst_c, src_c) in px[..3].iter_mut().zip(argb[..3].iter()) {
                        *dst_c = (*src_c as u16 + (*dst_c as u16 * inv) / 255).min(255) as u8;
                    }
                    px[3] = 255;
                }
                PointerShape::Monochrome => {
                    // `dst = (dst & and) ^ xor`, where one mask bit stands for a
                    // whole pixel. All four cases are real: a classic arrow paints
                    // its outline with (0, 0) and its body with (0, 1), and (1, 1)
                    // is the invert-the-desktop case the text-cursor blink uses.
                    let and = bit(cy, cx);
                    let xor = bit(cy + h, cx);
                    match (and, xor) {
                        (1, 0) => continue,
                        (1, 1) => px[..3].iter_mut().for_each(|c| *c = !*c),
                        (0, 0) => px[..3].fill(0),
                        _ => px[..3].fill(255),
                    }
                    px[3] = 255;
                }
            }
        }
    }
    Some(r)
}

#[cfg(windows)]
pub(crate) fn shape_type(kind: u32) -> Option<PointerShape> {
    match kind {
        2 => Some(PointerShape::Color),
        1 => Some(PointerShape::Monochrome),
        // 4 is MASKED_COLOR — an AND mask over a 32-bit XOR bitmap, whose row
        // layout the documentation describes only for the mask. It is what older
        // drivers hand out; drawing it wrong would put a black blob over the
        // desktop, so the probe reports it and nothing more.
        _ => None,
    }
}

#[cfg(windows)]
use windows::core::{BOOL, Interface};
#[cfg(windows)]
use windows::Win32::Foundation::{HMODULE, RECT};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory, IDXGIFactory1, IDXGIOutput,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ENUM_MODES, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_MORE_DATA, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, EnumDisplayMonitors, SRCCOPY,
    GetDC, GetDIBits, GetMonitorInfoW, HMONITOR, ReleaseDC, SelectObject, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, DEVMODEW, DIB_RGB_COLORS, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
    HBITMAP, HDC, HGDIOBJ, MONITORINFO, MONITORINFOEXW,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

#[cfg(windows)]
/// How many rectangles one frame query may return. A full-screen drag on a
/// multi-monitor desktop can exceed it, and the caller's answer to that is to
/// re-diff the frame, so this is a memory bound, not a correctness one.
const MAX_RECTS: usize = 1024;

#[cfg(windows)]
fn factory() -> CastResult<IDXGIFactory> {
    // `CreateDXGIFactory1` is the only entry point that puts the factory on the
    // desktop's window station correctly for duplication; the vtable it returns is
    // a factory1, which is QI'd down to the plain interface that enumerates
    // adapters with outputs attached to them.
    let f1: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    Ok(f1.cast()?)
}

/// The adapter/output tree, which is the only enumeration that can name a
/// duplication target.
#[cfg(windows)]
fn dxgi_monitors() -> CastResult<Vec<Monitor>> {
    let factory = factory()?;
    let mut out = Vec::new();
    for adapter_index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters(adapter_index) }) else {
            break;
        };
        for output_index in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                break;
            };
            out.push(Monitor {
                index: 0,
                adapter: adapter_index,
                output: output_index,
                primary: desc.DesktopCoordinates.left == 0 && desc.DesktopCoordinates.top == 0,
                x: desc.DesktopCoordinates.left,
                y: desc.DesktopCoordinates.top,
                width: (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32,
                height: (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32,
                refresh_hz: native_rate(&output),
                attached: desc.AttachedToDesktop.as_bool(),
                name: utf16(&desc.DeviceName),
            });
        }
    }
    Ok(usable(&mut out))
}

/// The desktop's monitor list from Win32. This is what answers on a machine where
/// DXGI reports no outputs at all — measured on the build host, where three
/// adapters enumerate fine and every one of them returns `DXGI_ERROR_NOT_FOUND` for
/// output 0, because the desktop is being rendered by a virtual display driver.
/// A capture layer that only spoke DXGI would have no picture there.
#[cfg(windows)]
fn gdi_monitors() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    let data = windows::Win32::Foundation::LPARAM(&mut out as *mut Vec<Monitor> as isize);
    unsafe {
        // `None` for the clip rect means "every monitor on the desktop", and the
        // callback runs on this thread, so the pointer inside `LPARAM` cannot
        // outlive the vector it points at.
        // The return value only says whether the enumeration ran to completion; the
        // vector the callback filled is the answer either way.
        let _ = EnumDisplayMonitors(None, None, Some(push_monitor), data);
    }
    usable(&mut out)
}

#[cfg(windows)]
unsafe extern "system" fn push_monitor(
    monitor: HMONITOR,
    _dc: HDC,
    _bounds: *mut RECT,
    data: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    let list = &mut *(data.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if !GetMonitorInfoW(
        monitor,
        &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
    )
    .as_bool()
    {
        return BOOL(1);
    }
    let r = info.monitorInfo.rcMonitor;
    list.push(Monitor {
        index: 0,
        adapter: u32::MAX,
        output: u32::MAX,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
        refresh_hz: current_rate(&info.szDevice),
        // A handle `EnumDisplayMonitors` returned is, by definition, on the desktop.
        attached: true,
        name: utf16(&info.szDevice),
    });
    // Anything else would stop the enumeration early and hide a monitor.
    BOOL(1)
}

/// The mode the display is in right now.
///
/// `DEVMODEW` is filled in by the driver only when the structure size it is handed
/// matches, so a binding whose layout ever drifts makes this return 0 — a missing
/// refresh rate — instead of reading a wrong one out of the neighbouring fields.
#[cfg(windows)]
fn current_rate(device: &[u16; 32]) -> f64 {
    let mut dm = DEVMODEW::default();
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR(device.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
    };
    if !ok.as_bool() || dm.dmDisplayFrequency == 0 {
        return 0.0;
    }
    dm.dmDisplayFrequency as f64
}

/// Drop displays that cannot be captured, put the primary first, and number what
/// is left. One order for both enumerators is what makes "monitor 1" mean the same
/// thing in the UI and in the capture thread.
#[cfg(windows)]
fn usable(list: &mut Vec<Monitor>) -> Vec<Monitor> {
    list.retain(|m| m.attached && m.width > 0 && m.height > 0);
    list.sort_by(|a, b| {
        (!a.primary)
            .cmp(&(!b.primary))
            .then(a.y.cmp(&b.y))
            .then(a.x.cmp(&b.x))
    });
    for (i, m) in list.iter_mut().enumerate() {
        m.index = i as u32;
    }
    std::mem::take(list)
}

fn utf16(raw: &[u16]) -> String {
    String::from_utf16_lossy(
        raw.iter()
            .take_while(|c| **c != 0)
            .copied()
            .collect::<Vec<u16>>()
            .as_slice(),
    )
}

/// Every display this machine can cast, preferring the enumeration that duplication
/// can act on.
#[cfg(windows)]
fn monitors() -> CastResult<Vec<Monitor>> {
    let dxgi = dxgi_monitors()?;
    if !dxgi.is_empty() {
        return Ok(dxgi);
    }
    let gdi = gdi_monitors();
    if gdi.is_empty() {
        return Err(CastError::InvalidState("no display is attached to this desktop"));
    }
    Ok(gdi)
}

/// The first mode DXGI lists for an output is its current one. Asking for the
/// list, rather than trusting `DesktopBounds`, is what makes a 120 Hz panel look
/// like 120 Hz and not like a 60 Hz guess.
#[cfg(windows)]
fn native_rate(output: &IDXGIOutput) -> f64 {
    let mut count = 0u32;
    if unsafe {
        output.GetDisplayModeList(
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_ENUM_MODES(0),
            &mut count,
            None,
        )
    }
    .is_err()
        || count == 0
    {
        return 0.0;
    }
    let mut list = vec![DXGI_MODE_DESC::default(); count as usize];
    if unsafe {
        output.GetDisplayModeList(
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_ENUM_MODES(0),
            &mut count,
            Some(list.as_mut_ptr()),
        )
    }
    .is_err()
    {
        return 0.0;
    }
    let r = list[0].RefreshRate;
    r.Numerator as f64 / r.Denominator.max(1) as f64
}

/// The part of a cursor placed at `x, y` that is actually on the desktop.
///
/// `blend_pointer` clips with this and the pointer layer uses the same rectangle
/// to remember the pixels underneath, so "what the cursor covers" is computed one
/// way — a divergence between the two would leave the erase writing back pixels
/// that were never overwritten, or worse.
fn clipped_rect(x: i32, y: i32, w: usize, h: usize, frame_w: usize, frame_h: usize) -> Option<Rect> {
    let (fw, fh) = (
        frame_w.min(u16::MAX as usize) as i32,
        frame_h.min(u16::MAX as usize) as i32,
    );
    let (x0, y0) = (x.clamp(0, fw), y.clamp(0, fh));
    let (x1, y1) = (
        (x + w as i32).clamp(0, fw),
        (y + h as i32).clamp(0, fh),
    );
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(Rect::new(
        x0 as u16,
        y0 as u16,
        (x1 - x0) as u16,
        (y1 - y0) as u16,
    ))
}

/// The desktop texture duplication hands out does **not** contain the mouse
/// pointer: the shape and the position arrive as metadata, and a share that
/// ignored them shows a frozen cursor over moving content.
///
/// Compositing the cursor into the captured pixels costs a few hundred pixels per
/// frame. Undoing it — needed the moment it moves, or the old cursor smears across
/// the picture — costs the same, because the desktop pixels underneath the last
/// blend are kept in `patch` instead of re-copying eight megabytes for a mouse
/// move.
#[cfg(windows)]
struct PointerLayer {
    shape: Vec<u8>,
    info: Option<DXGI_OUTDUPL_POINTER_SHAPE_INFO>,
    kind: Option<PointerShape>,
    patch: Vec<u8>,
    drawn: Option<Rect>,
}

#[cfg(windows)]
impl PointerLayer {
    /// A new shape from the driver. The shape only changes when the cursor sprite
    /// changes (arrow to I-beam, say), so caching it is what makes a plain move a
    /// two-tile frame instead of a full refresh.
    fn set_shape(&mut self, shape: Vec<u8>, info: DXGI_OUTDUPL_POINTER_SHAPE_INFO, kind: PointerShape) {
        self.shape = shape;
        self.info = Some(info);
        self.kind = Some(kind);
    }

    /// A shape this module cannot draw (a masked-colour cursor from an old driver).
    /// Forgetting the cache is the honest answer: an un-cached cursor draws nothing
    /// rather than drawing something wrong.
    fn clear_shape(&mut self) {
        self.shape.clear();
        self.info = None;
        self.kind = None;
    }

    fn blend(
        &mut self,
        buffer: &mut [u8],
        stride: usize,
        frame_w: usize,
        frame_h: usize,
        px: i32,
        py: i32,
    ) -> Option<Rect> {
        let info = self.info?;
        let kind = self.kind?;
        let (w, h) = (info.Width as usize, info.Height as usize);
        let (x, y) = (px - info.HotSpot.x, py - info.HotSpot.y);
        let r = clipped_rect(x, y, w, h, frame_w, frame_h)?;
        self.save_patch(buffer, stride, r);
        blend_pointer(buffer, stride, frame_w, frame_h, x, y, kind, &self.shape, w, h, info.Pitch as usize)
    }

    fn save_patch(&mut self, buffer: &[u8], stride: usize, r: Rect) {
        let row = r.w as usize * 4;
        self.patch.clear();
        self.patch.resize(row * r.h as usize, 0);
        for i in 0..r.h as usize {
            let s = (r.y as usize + i) * stride + r.x as usize * 4;
            let end = s + row;
            let Some(src) = buffer.get(s..end) else { continue };
            self.patch[i * row..(i + 1) * row].copy_from_slice(src);
        }
        self.drawn = Some(r);
    }

    /// Put the desktop back the way it was and report the rectangle that changed.
    fn erase(&mut self, buffer: &mut [u8], stride: usize) -> Option<Rect> {
        let r = self.drawn.take()?;
        let row = r.w as usize * 4;
        if self.patch.len() < row * r.h as usize {
            return None;
        }
        for i in 0..r.h as usize {
            let d = (r.y as usize + i) * stride + r.x as usize * 4;
            let end = d + row;
            let Some(dst) = buffer.get_mut(d..end) else { continue };
            dst.copy_from_slice(&self.patch[i * row..(i + 1) * row]);
        }
        Some(r)
    }

    /// The buffer was replaced by a fresh desktop copy, which has no cursor in it
    /// at all — so there is nothing to restore, and the patch is stale.
    fn reset(&mut self) {
        self.drawn = None;
    }
}

#[cfg(windows)]
pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output1: IDXGIOutput1,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    buffer: Vec<u8>,
    width: u16,
    height: u16,
    stride: usize,
    frame_no: u64,
    monitor: Monitor,
    pointer: PointerLayer,
    /// Whether `buffer` holds a desktop copy yet, which decides whether a
    /// metadata-only frame can reuse it.
    copied: bool,
    /// Cost of the last pixel move, reset at the top of each frame read so a
    /// reused buffer reports zero rather than the previous frame's copy.
    copy_us: u64,
    /// Held across frames because the rectangle queries fill them: a per-frame
    /// allocation inside the capture path would be a malloc on the critical path.
    rect_buf: Vec<RECT>,
    move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    /// Raw pointer-shape fields from the last frame that had one, kept so the
    /// probe can say what this machine actually does with the cursor instead of
    /// what the documentation implies.
    pub shape_type: u32,
    pub shape_bytes: u32,
    pub desc: DXGI_OUTDUPL_DESC,
}

#[cfg(windows)]
impl DxgiCapture {
    pub fn open(monitor_index: u32) -> CastResult<Self> {
        let list = dxgi_monitors()?;
        let monitor = list
            .into_iter()
            .find(|m| m.index == monitor_index)
            .ok_or(CastError::InvalidState("no such monitor"))?;
        if !monitor.attached {
            return Err(CastError::InvalidState("monitor is not attached to the desktop"));
        }
        let factory = factory()?;
        let adapter: IDXGIAdapter = unsafe { factory.EnumAdapters(monitor.adapter)? };
        let output: IDXGIOutput = unsafe { adapter.EnumOutputs(monitor.output)? };
        let output1: IDXGIOutput1 = output.cast()?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or(CastError::InvalidState("no D3D11 device"))?;
        let context = context.ok_or(CastError::InvalidState("no D3D11 context"))?;
        let dxgi_device: IDXGIDevice = device.cast()?;
        let dup = unsafe { output1.DuplicateOutput(&dxgi_device)? };
        let desc = unsafe { dup.GetDesc() };
        Ok(Self {
            device,
            context,
            output1,
            dup,
            staging: None,
            buffer: Vec::new(),
            width: desc.ModeDesc.Width as u16,
            height: desc.ModeDesc.Height as u16,
            stride: desc.ModeDesc.Width as usize * 4,
            frame_no: 0,
            monitor,
            pointer: PointerLayer {
                shape: Vec::new(),
                info: None,
                kind: None,
                patch: Vec::new(),
                drawn: None,
            },
            copied: false,
            copy_us: 0,
            rect_buf: vec![RECT::default(); MAX_RECTS],
            move_buf: vec![DXGI_OUTDUPL_MOVE_RECT::default(); MAX_RECTS],
            shape_type: 0,
            shape_bytes: 0,
            desc,
        })
    }

    /// A resolution or topology change kills the duplication object. Re-opening it
    /// here means the sender's loop does not have to learn that the monitor
    /// changed underneath it, which is a Tuesday, not an exception.
    fn reopen(&mut self) -> CastResult<()> {
        let dxgi_device: IDXGIDevice = self.device.cast()?;
        self.dup = unsafe { self.output1.DuplicateOutput(&dxgi_device)? };
        self.desc = unsafe { self.dup.GetDesc() };
        self.width = self.desc.ModeDesc.Width as u16;
        self.height = self.desc.ModeDesc.Height as u16;
        self.stride = self.width as usize * 4;
        self.staging = None;
        Ok(())
    }

    /// A CPU-readable twin of the desktop texture. Created lazily because the
    /// texture's size is not known until the first frame is acquired.
    fn ensure_staging(&mut self, src: &ID3D11Texture2D) -> CastResult<()> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut sd) };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: sd.Width,
            Height: sd.Height,
            MipLevels: 1,
            ArraySize: sd.ArraySize,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        self.staging = Some(tex.ok_or(CastError::InvalidState("no staging texture"))?);
        self.width = desc.Width as u16;
        self.height = desc.Height as u16;
        self.stride = desc.Width as usize * 4;
        self.buffer
            .resize(desc.Height as usize * self.stride, 0);
        Ok(())
    }

    pub fn capture(&mut self, timeout: Duration) -> CastResult<Option<Frame<'_>>> {
        let poll = timeout.is_zero();
        let mut wait_ms = match timeout {
            t if t.is_zero() => 0,
            t if t >= Duration::MAX => u32::MAX,
            t => t.as_millis().min(u32::MAX as u128 - 1) as u32,
        };
        let deadline = Instant::now() + timeout.min(Duration::from_secs(3600 * 24));
        loop {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let acquired = unsafe { self.dup.AcquireNextFrame(wait_ms, &mut info, &mut resource) };
            if let Err(e) = acquired {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if e.code() == DXGI_ERROR_ACCESS_LOST {
                    self.reopen()?;
                    continue;
                }
                return Err(CastError::from(e.code()));
            }
            let resource = resource.ok_or(CastError::InvalidState("no desktop resource"))?;
            let read = self.read_frame(&resource, &info);
            // Everything that touches these pixels happens before the frame goes
            // back, or the driver may recycle the texture under the copy.
            let _ = unsafe { self.dup.ReleaseFrame() };
            let dirty = read?;
            if dirty.is_empty() {
                // Nothing was presented that moves a pixel. Hand the frame back to
                // the driver and wait again rather than emitting a no-op frame.
                if poll || Instant::now() >= deadline {
                    return Ok(None);
                }
                // The desktop was idle for part of the window: keep waiting, but
                // never longer than the caller asked for.
                let left = deadline.saturating_duration_since(Instant::now());
                wait_ms = left.as_millis().clamp(1, u32::MAX as u128 - 1) as u32;
                continue;
            }
            self.frame_no += 1;
            return Ok(Some(Frame {
                bgra: &self.buffer,
                stride: self.stride,
                width: self.width,
                height: self.height,
                dirty,
                copy_us: self.copy_us,
                captured_us: crate::metrics::now_us(),
                frame_no: self.frame_no,
                backend: Backend::Dxgi,
            }));
        }
    }

    /// Copy the desktop texture to CPU memory and gather what changed.
    ///
    /// The desktop texture duplication hands out does **not** contain the mouse
    /// pointer — the pointer is delivered separately as a shape plus a position,
    /// which is exactly what a share needs, since the alternative is a frozen
    /// cursor over moving content. Compositing it here costs a few hundred pixels;
    /// undoing it costs the same, because the patch underneath the cursor is kept
    /// instead of re-copying eight megabytes for a mouse move.
    fn read_frame(
        &mut self,
        resource: &IDXGIResource,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> CastResult<Vec<Rect>> {
        let texture: ID3D11Texture2D = resource.cast()?;
        if self.staging.is_none() {
            self.ensure_staging(&texture)?;
        }
        self.copy_us = 0;
        let mut dirty: Vec<Rect> = Vec::new();

        // `AccumulatedFrames == 0` means the driver had nothing new to show: the
        // texture still holds the previous desktop, so only the metadata (a pointer
        // move, in practice) is worth reading. Reusing the buffer is what keeps an
        // idle desktop at zero cost and a mouse move at two tiles.
        let fresh_desktop = !self.copied || info.AccumulatedFrames > 0 || info.RectsCoalesced.as_bool();
        if fresh_desktop {
            self.copy_desktop(&texture)?;
            // A raw desktop arrived, so the cursor we baked into the last copy is
            // gone from the buffer already; the patch must not be restored over it.
            self.pointer.reset();
            if info.RectsCoalesced.as_bool() {
                // The driver merged updates and can no longer say where they were.
                dirty.push(Rect::new(0, 0, self.width, self.height));
            } else {
                dirty.extend(self.dirty_rects()?);
                dirty.extend(self.move_rects()?);
            }
        } else if let Some(r) = self.pointer.erase(&mut self.buffer, self.stride) {
            dirty.push(r);
        }

        self.shape_bytes = info.PointerShapeBufferSize;
        if info.PointerShapeBufferSize > 0 {
            let mut shape = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut needed = 0u32;
            let got = unsafe {
                self.dup.GetFramePointerShape(
                    info.PointerShapeBufferSize,
                    shape.as_mut_ptr() as *mut core::ffi::c_void,
                    &mut needed,
                    &mut si,
                )
            };
            self.shape_type = si.Type;
            if got.is_ok() && needed > 0 {
                if let Some(kind) = shape_type(si.Type) {
                    self.pointer.set_shape(shape, si, kind);
                } else {
                    self.pointer.clear_shape();
                }
            }
        }

        if info.PointerPosition.Visible.as_bool() {
            let (px, py) = (
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            );
            if let Some(r) = self
                .pointer
                .blend(&mut self.buffer, self.stride, self.width as usize, self.height as usize, px, py)
            {
                dirty.push(r);
            }
        } else if let Some(r) = self.pointer.erase(&mut self.buffer, self.stride) {
            dirty.push(r);
        }

        self.copied = true;
        Ok(dirty)
    }

    /// One GPU→CPU copy of the whole desktop into the owned buffer.
    ///
    /// The copy is of the *full* desktop even when eight pixels changed, because
    /// the alternative is asking the driver for a sub-region texture per rect. The
    /// measured cost on the build host is what says whether that trade is right;
    /// `copy_us` records it instead of guessing.
    fn copy_desktop(&mut self, texture: &ID3D11Texture2D) -> CastResult<()> {
        let t0 = crate::metrics::now_us();
        let staging = self
            .staging
            .as_ref()
            .ok_or(CastError::InvalidState("no staging texture"))?;
        let dst: ID3D11Resource = staging.cast()?;
        let src: ID3D11Resource = texture.cast()?;
        unsafe { self.context.CopyResource(&src, &dst) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        let pitch = mapped.RowPitch as usize;
        let bytes = pitch * self.height as usize;
        if self.buffer.len() < bytes {
            self.buffer.resize(bytes, 0);
        }
        unsafe {
            let src_rows = std::slice::from_raw_parts(mapped.pData as *const u8, bytes);
            self.buffer[..bytes].copy_from_slice(&src_rows[..bytes]);
        }
        unsafe { self.context.Unmap(&dst, 0) };
        // The texture's pitch is the row stride the encoder has to live with, so
        // it is reported rather than normalised away by a second copy.
        self.stride = pitch;
        self.copy_us = crate::metrics::now_us() - t0;
        Ok(())
    }

    /// The driver's own rectangles, from the frame that is still ours to read.
    ///
    /// One call into a buffer that already exists. The documented size-query form —
    /// zero length and a null pointer — is refused by at least one driver on this
    /// project's test host, and a second round trip after `ReleaseFrame` would be
    /// worse than guessing: the rectangles have to be read while the frame is held.
    /// `DXGI_ERROR_MORE_DATA` says the change was larger than the buffer can
    /// describe, so the honest answer is the whole frame; the tile diff is what
    /// makes that a bandwidth question rather than a CPU one.
    fn dirty_rects(&mut self) -> CastResult<Vec<Rect>> {
        let mut needed = 0u32;
        let byte_len = (self.rect_buf.len() * std::mem::size_of::<RECT>()) as u32;
        let got = unsafe {
            self.dup
                .GetFrameDirtyRects(byte_len, self.rect_buf.as_mut_ptr(), &mut needed)
        };
        if let Err(e) = got {
            if e.code() == DXGI_ERROR_MORE_DATA {
                return Ok(vec![Rect::new(0, 0, self.width, self.height)]);
            }
            return Err(CastError::Hresult(e.code().0));
        }
        Ok(self.rect_buf[..(needed as usize / std::mem::size_of::<RECT>())]
            .iter()
            .filter_map(rect_to_wire)
            .collect())
    }

    /// A window drag is reported as a *move*: the pixels came from somewhere else
    /// in the same frame. The destination is what has to be re-read, because the
    /// texture already holds the answer.
    fn move_rects(&mut self) -> CastResult<Vec<Rect>> {
        let mut needed = 0u32;
        let byte_len = (self.move_buf.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32;
        let got = unsafe {
            self.dup
                .GetFrameMoveRects(byte_len, self.move_buf.as_mut_ptr(), &mut needed)
        };
        if let Err(e) = got {
            if e.code() == DXGI_ERROR_MORE_DATA {
                return Ok(vec![Rect::new(0, 0, self.width, self.height)]);
            }
            return Err(CastError::Hresult(e.code().0));
        }
        let count = needed as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        Ok(self.move_buf[..count]
            .iter()
            .map(|m| m.DestinationRect)
            .filter_map(|r| rect_to_wire(&r))
            .collect())
    }
}

fn rect_to_wire(r: &RECT) -> Option<Rect> {
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(Rect::new(
        r.left.max(0) as u16,
        r.top.max(0) as u16,
        w.min(u16::MAX as i32) as u16,
        h.min(u16::MAX as i32) as u16,
    ))
}

/// The desktop copied with BitBlt. `CreateCompatibleBitmap` of the screen DC
/// captures DWM's output, so the picture is what the room sees, but there is no
/// metadata at all: every frame is reported as fully dirty and the tile diff does
/// the pruning.
#[cfg(windows)]
pub struct GdiCapture {
    monitor: Monitor,
    screen: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    buffer: Vec<u8>,
    width: u16,
    height: u16,
    stride: usize,
    frame_no: u64,
    /// Wall time of the last BitBlt + GetDIBits round trip, so the fallback's
    /// cost is a number and not an argument.
    pub last_copy_us: u64,
    /// When the previous copy started, used to space this one out.
    prev_start_us: u64,
}

#[cfg(windows)]
impl GdiCapture {
    pub fn open(monitor_index: u32) -> CastResult<Self> {
        let list = gdi_monitors();
        let monitor = list
            .into_iter()
            .find(|m| m.index == monitor_index)
            .ok_or(CastError::InvalidState("no such monitor"))?;
        let (w, h) = (monitor.width as i32, monitor.height as i32);
        if w <= 0 || h <= 0 {
            return Err(CastError::InvalidState("monitor has no desktop bounds"));
        }
        let screen = unsafe { GetDC(None) };
        if screen.is_invalid() {
            return Err(CastError::InvalidState("no screen DC"));
        }
        let mem_dc = unsafe { CreateCompatibleDC(Some(screen)) };
        if mem_dc.is_invalid() {
            unsafe { ReleaseDC(None, screen) };
            return Err(CastError::InvalidState("CreateCompatibleDC failed"));
        }
        let bitmap = unsafe { CreateCompatibleBitmap(screen, w, h) };
        if bitmap.is_invalid() {
            unsafe {
                let _ = DeleteDC(mem_dc);
                ReleaseDC(None, screen);
            }
            return Err(CastError::InvalidState("CreateCompatibleBitmap failed"));
        }
        let old = unsafe { SelectObject(mem_dc, HGDIOBJ(bitmap.0)) };
        Ok(Self {
            monitor,
            screen,
            mem_dc,
            bitmap,
            old,
            buffer: vec![0u8; w as usize * h as usize * 4],
            width: w as u16,
            height: h as u16,
            stride: w as usize * 4,
            frame_no: 0,
            last_copy_us: 0,
            prev_start_us: 0,
        })
    }

    /// BitBlt has no "the desktop changed" event to block on, so `timeout` is
    /// honoured as the spacing between copies instead: the fallback can never
    /// report an idle desktop, but it will not spin a core doing nothing either.
    pub fn capture(&mut self, timeout: Duration) -> CastResult<Option<Frame<'_>>> {
        let t0 = crate::metrics::now_us();
        if !timeout.is_zero() && self.prev_start_us != 0 {
            let due = self.prev_start_us + timeout.as_micros() as u64;
            if due > t0 {
                std::thread::sleep(Duration::from_micros(due - t0));
            }
        }
        let t0 = crate::metrics::now_us();
        let ok = unsafe {
            BitBlt(
                self.mem_dc,
                0,
                0,
                self.width as i32,
                self.height as i32,
                Some(self.screen),
                self.monitor.x,
                self.monitor.y,
                SRCCOPY,
            )
        };
        if let Err(e) = ok {
            // The driver's own code is the only useful diagnostic here: whether the
            // desktop refused the copy for security, for memory or because it is a
            // virtual surface is a different answer in each case.
            return Err(CastError::Hresult(e.code().0));
        }
        // A negative biHeight asks for top-down rows, which is the order the wire
        // codec and the presenter both work in.
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width as i32,
                biHeight: -(self.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = unsafe {
            GetDIBits(
                self.mem_dc,
                self.bitmap,
                0,
                self.height as u32,
                Some(self.buffer.as_mut_ptr() as *mut core::ffi::c_void),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if rows == 0 {
            return Err(CastError::from(windows::core::Error::from_win32()));
        }
        self.last_copy_us = crate::metrics::now_us() - t0;
        self.prev_start_us = t0;
        self.frame_no += 1;
        Ok(Some(Frame {
            bgra: &self.buffer,
            stride: self.stride,
            width: self.width,
            height: self.height,
            // BitBlt gives no metadata, so the honest answer is to check
            // everything; the encoder's tile diff is what makes that cheap.
            dirty: vec![Rect::new(0, 0, self.width, self.height)],
            copy_us: self.last_copy_us,
            captured_us: crate::metrics::now_us(),
            frame_no: self.frame_no,
            backend: Backend::Gdi,
        }))
    }
}

#[cfg(windows)]
impl Drop for GdiCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.mem_dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.mem_dc);
            ReleaseDC(None, self.screen);
        }
    }
}

#[cfg(windows)]
impl Drop for DxgiCapture {
    fn drop(&mut self) {
        // Releasing the duplication before the device is what the API asks for;
        // COM would clean up anyway, but leaving a duplication alive keeps DWM
        // compositing into a texture nobody reads.
        let _ = unsafe { self.dup.ReleaseFrame() };
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn rect_converts_and_drops_degenerate_bounds() {
        assert_eq!(
            rect_to_wire(&RECT {
                left: 4,
                top: 8,
                right: 68,
                bottom: 72,
            }),
            Some(Rect::new(4, 8, 64, 64))
        );
        assert_eq!(
            rect_to_wire(&RECT {
                left: 10,
                top: 10,
                right: 10,
                bottom: 20,
            }),
            None
        );
        // A rect the driver reports at negative coordinates — which happens for a
        // window dragged off the left edge — starts at zero and keeps its width.
        assert_eq!(
            rect_to_wire(&RECT {
                left: -8,
                top: 0,
                right: 56,
                bottom: 8,
            }),
            Some(Rect::new(0, 0, 64, 8))
        );
    }

    #[test]
    fn a_colour_pointer_blends_over_the_desktop_by_its_own_alpha() {
        // The destination is a blue desktop pixel: BGRA in memory order.
        let mut fb = vec![0u8; 4 * 4 * 4];
        for p in fb.chunks_exact_mut(4) {
            p.copy_from_slice(&[255, 0, 0, 255]);
        }
        // One premultiplied red cursor pixel at half alpha, then a fully
        // transparent one that must not touch the desktop.
        let shape = [0u8, 0, 128, 0x80, 0, 0, 0, 0];
        let r = blend_pointer(&mut fb, 16, 4, 4, 0, 0, PointerShape::Color, &shape, 1, 1, 4)
            .unwrap();
        assert_eq!(r, Rect::new(0, 0, 1, 1));
        assert_eq!(fb[3], 255);
        assert_eq!(fb[2], 128, "the premultiplied source channel is what lands");
        assert_eq!(fb[0], 127, "the blue destination fades by (1 - alpha)");
        let clear = [0u8, 0, 0, 0];
        blend_pointer(&mut fb, 16, 4, 4, 2, 2, PointerShape::Color, &clear, 1, 1, 4);
        assert_eq!(&fb[2 * 16 + 2 * 4..2 * 16 + 2 * 4 + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn a_monochrome_pointer_follows_the_and_xor_rule() {
        let mut fb = vec![0u8; 4 * 4 * 4];
        for p in fb.chunks_exact_mut(4) {
            p.copy_from_slice(&[10, 20, 30, 255]);
        }
        // Four pixels, one row of AND and one row of XOR, covering every case the
        // formula `dst = (dst & and) ^ xor` can produce.
        let shape = [0b1100_0000, 0b0101_0000];
        blend_pointer(
            &mut fb, 16, 4, 1, 0, 0, PointerShape::Monochrome, &shape, 4, 1, 1,
        )
        .unwrap();
        assert_eq!(&fb[0..4], &[10, 20, 30, 255], "AND=1 XOR=0 leaves the pixel");
        assert_eq!(&fb[4..8], &[245, 235, 225, 255], "AND=1 XOR=1 inverts it");
        assert_eq!(&fb[8..12], &[0, 0, 0, 255], "AND=0 XOR=0 is the black outline");
        assert_eq!(&fb[12..16], &[255, 255, 255, 255], "AND=0 XOR=1 is the white body");
    }

    #[test]
    fn a_pointer_off_the_edge_is_clipped_and_reported() {
        let mut fb = vec![0u8; 4 * 4 * 4];
        let shape = [255u8; 32];
        // A 3x3 cursor placed one pixel past the right edge touches nothing.
        assert_eq!(
            blend_pointer(&mut fb, 16, 4, 4, 4, 0, PointerShape::Color, &shape, 3, 3, 12),
            None
        );
        let r = blend_pointer(&mut fb, 16, 4, 4, 2, 3, PointerShape::Color, &shape, 3, 3, 12)
            .unwrap();
        assert_eq!(r, Rect::new(2, 3, 2, 1), "clipped to what is on screen");
        assert_eq!(&fb[3 * 16 + 2 * 4..3 * 16 + 2 * 4 + 4], &[255, 255, 255, 255]);
    }

    #[test]
    fn only_the_pointer_shapes_we_can_draw_are_claimed() {
        assert_eq!(shape_type(2), Some(PointerShape::Color));
        assert_eq!(shape_type(1), Some(PointerShape::Monochrome));
        assert_eq!(shape_type(4), None);
        assert_eq!(shape_type(0), None);
    }

    #[test]
    fn monitor_enumeration_answers_on_this_machine() {
        // Not a behaviour test — a guard against the capture layer being written
        // against an API that this build does not have.
        let list = Capture::monitors().expect("DXGI enumeration must work");
        assert!(!list.is_empty(), "no monitors enumerated");
        let primary = &list[0];
        assert!(primary.width >= 800, "unexpected desktop width {primary:?}");
        assert!(primary.fps_ceiling() >= 60);
    }
}
