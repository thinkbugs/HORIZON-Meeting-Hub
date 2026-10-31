//! Throwaway probe: what does IMFMediaEngine report for a source's native size?
//! Used to check rotation/anamorphic handling; not part of the product.

use std::time::{Duration, Instant};

use windows::core::{implement, BSTR, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory, IMFMediaEngineNotify,
    IMFMediaEngineNotify_Impl, MFCreateAttributes, CLSID_MFMediaEngineClassFactory,
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_PLAYBACK_HWND,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassW,
    TranslateMessage, MSG, PM_REMOVE, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
};

#[implement(IMFMediaEngineNotify)]
struct Notify;
impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, _event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
        Ok(())
    }
}

fn main() -> windows::core::Result<()> {
    let url = std::env::args().nth(1).expect("usage: engine_probe <url>");
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            eprintln!("CoInitializeEx: {hr:?}");
        }
        horizon_cast::codec::startup().expect("MF startup");

        let class = HSTRING::from("EngineProbe");
        let wndclass = WNDCLASSW {
            lpfnWndProc: Some(DefWindowProcW),
            hInstance: GetModuleHandleW(None)?,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wndclass);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            &class,
            &HSTRING::from("probe"),
            WS_POPUP,
            0,
            0,
            320,
            180,
            None,
            None,
            Some(GetModuleHandleW(None)?.into()),
            None,
        )?;

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.expect("attrs");
        attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, hwnd.0 as u64)?;
        let notify: IMFMediaEngineNotify = windows::core::ComObject::new(Notify).into_interface();
        attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
        let factory: IMFMediaEngineClassFactory = CoCreateInstance(
            &CLSID_MFMediaEngineClassFactory,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )?;
        let engine: IMFMediaEngine = factory.CreateInstance(0, &attrs)?;
        if let Err(e) = engine.SetSource(&BSTR::from(url.as_str())) {
            println!("SetSource failed: {e}");
            return Ok(());
        }
        let _ = engine.Pause();

        let start = Instant::now();
        let mut last = (0u32, 0u32);
        let mut msg = MSG::default();
        while start.elapsed() < Duration::from_secs(15) {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let (mut w, mut h) = (0u32, 0u32);
            let hr = engine.GetNativeVideoSize(Some(&mut w), Some(&mut h));
            let ready = engine.GetReadyState();
            let error = engine.GetError().ok().map(|e| e.GetErrorCode());
            if (w, h) != last || ready != 0 {
                println!(
                    "{:>5}ms ready={ready} native={}x{} hr={hr:?} error={error:?} duration={:.2}",
                    start.elapsed().as_millis(),
                    w,
                    h,
                    engine.GetDuration()
                );
                last = (w, h);
            }
            if ready >= 1 && w > 0 && h > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            if start.elapsed() > Duration::from_secs(2) {
                break;
            }
        }
        // Give the header a moment even if the first poll was early.
        for _ in 0..20 {
            let (mut w, mut h) = (0u32, 0u32);
            let _ = engine.GetNativeVideoSize(Some(&mut w), Some(&mut h));
            println!(
                "final ready={} native={}x{} duration={:.2} error={:?}",
                engine.GetReadyState(),
                w,
                h,
                engine.GetDuration(),
                engine.GetError().ok().map(|e| e.GetErrorCode())
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}
