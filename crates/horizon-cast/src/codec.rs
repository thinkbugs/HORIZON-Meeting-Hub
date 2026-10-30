//! Media Foundation H.264 encode/decode.
//!
//! Why Media Foundation and not a bundled encoder: this machine's GPU has no
//! video encode engine at all, and cargo can only reach crates already in the
//! local registry — which excludes openh264, x264 and every other software
//! encoder. Media Foundation is the one H.264 codec guaranteed to exist on every
//! Windows install, so the pipeline is built on it and on nothing else.
//!
//! The latency contract of this module:
//! - `MF_LOW_LATENCY` is set on the transform's own attribute store *before* any
//!   type is negotiated. For the Microsoft H.264 encoder that is what disables
//!   B-frames and lookahead; set late, the encoder has already been built for
//!   quality and will buffer.
//! - one input sample must yield one output sample. [`H264Encoder::queue_depth`]
//!   is the audit for that, because a two-frame encoder silently costs 66 ms at
//!   30 fps and looks exactly like a slow network.
//!
//! The encoder keeps that promise. The in-box *decoder* does not, and `mf-probe
//! --decapi` measured how badly, against one fixed bitstream with the frame rate,
//! the worker-thread count and `CODECAPI_AVLowLatencyMode` each varied one at a
//! time:
//!
//! | decoder setting | first output | steady-state lag | per-frame cost |
//! |---|---|---|---|
//! | as shipped, 20 logical CPUs | AU #26 | 900 ms | 1.3 ms |
//! | `AVDecNumWorkerThreads = 8` | AU #23 | 800 ms | 1.6 ms |
//! | `AVDecNumWorkerThreads = 4` | AU #17 | 600 ms | 1.2 ms |
//! | `AVDecNumWorkerThreads = 2` | AU #14 | 400 ms | 1.4 ms |
//! | `AVDecNumWorkerThreads = 1` | AU #11 | 333 ms | 3.5 ms |
//!
//! The lag is constant, measured against the timestamps we submitted, so it is a
//! real pipeline rather than dropped frames — and end-of-stream releases none of
//! it. `CODECAPI_AVLowLatencyMode` is refused outright on this decoder
//! (`E_INVALIDARG` both before and after the stream starts), and the SPS says
//! `num_ref_frames = 1`, so nothing about the bitstream explains it: the pipeline
//! is the decoder's frame-parallel buffer, and even a single thread keeps ten
//! frames in flight.
//!
//! That is the reason [`crate::wire`]'s own channel does not carry H.264 into this
//! decoder. 333 ms is eight times the whole end-to-end budget, and no amount of
//! network tuning recovers it. H.264 stays as the interop format — the DLNA,
//! AirPlay and Cast peers on the other end decode it in their own hardware, in a
//! frame or two — while the low-latency path uses a codec with a one-frame
//! pipeline by construction.

use crate::error::{CastError, CastResult};
use crate::metrics::now_us;
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_BOOL};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_I4};

pub use windows::Win32::Media::MediaFoundation::{
    MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER,
};

/// What happened when a codec-API property was pushed into a transform.
///
/// Kept as a value rather than a bool so `mf-probe` can tell the three cases apart:
/// a transform with no `ICodecAPI` at all, one that refuses the property, and one
/// that accepted it but still holds frames. They need different fixes and only the
/// last one is interesting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiOutcome {
    NoCodecApi,
    Unsupported,
    Set,
    Failed(HRESULT),
}

impl ApiOutcome {
    pub fn label(self) -> &'static str {
        match self {
            ApiOutcome::NoCodecApi => "no ICodecAPI",
            ApiOutcome::Unsupported => "property unsupported",
            ApiOutcome::Set => "set",
            ApiOutcome::Failed(_) => "SetValue failed",
        }
    }
}

impl std::fmt::Display for ApiOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiOutcome::Failed(c) => write!(f, "{} {:#010x}", self.label(), c.0),
            _ => write!(f, "{}", self.label()),
        }
    }
}

/// When a codec-API property is applied while opening a transform.
///
/// The order is not cosmetic: the in-box H.264 decoder answers `E_INVALIDARG` for
/// properties that arrive before it has a negotiated stream, while the ones it takes
/// before types are the ones that change how it allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropStage {
    BeforeTypes,
    AfterStreamStart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropValue {
    /// `VT_BOOL`, whose payload is `-1` for true rather than `1`.
    Bool(bool),
    /// `VT_I4`.
    Int(i32),
}

/// A codec-API property and the stage it belongs to.
#[derive(Clone, Copy, Debug)]
pub struct CodecProp {
    pub name: &'static str,
    pub guid: GUID,
    pub value: PropValue,
    pub stage: PropStage,
}

impl CodecProp {
    pub const fn new(
        name: &'static str,
        guid: GUID,
        value: PropValue,
        stage: PropStage,
    ) -> Self {
        Self { name, guid, value, stage }
    }

    /// Push the property. `IsSupported` returns `S_OK` even for "present but
    /// read-only", so the real answer comes from `SetValue`, and nothing here is
    /// fatal: an MFT that refuses a property still decodes, just not the way we asked.
    unsafe fn apply(&self, mft: &IMFTransform) -> ApiOutcome {
        let api = match mft.cast::<ICodecAPI>() {
            Ok(a) => a,
            Err(_) => return ApiOutcome::NoCodecApi,
        };
        if api.IsSupported(&self.guid).is_err() {
            return ApiOutcome::Unsupported;
        }
        let mut value = VARIANT::default();
        let inner = &mut *value.Anonymous.Anonymous;
        match self.value {
            PropValue::Bool(b) => {
                inner.vt = VT_BOOL;
                inner.Anonymous.boolVal = VARIANT_BOOL(if b { -1 } else { 0 });
            }
            PropValue::Int(n) => {
                inner.vt = VT_I4;
                inner.Anonymous.lVal = n;
            }
        }
        match api.SetValue(&self.guid, &value) {
            Ok(()) => ApiOutcome::Set,
            Err(e) => ApiOutcome::Failed(e.code()),
        }
    }
}

/// The switch that is supposed to stop a codec from buffering frames.
///
/// `MF_LOW_LATENCY` on the attribute store is a hint the MFT may read at
/// `SetOutputType` time; `CODECAPI_AVLowLatencyMode` is the codec manager's own
/// property, which is why the probe tries it at both stages.
const LOW_LATENCY: CodecProp = CodecProp {
    name: "AVLowLatencyMode",
    guid: CODECAPI_AVLowLatencyMode,
    value: PropValue::Bool(true),
    stage: PropStage::BeforeTypes,
};

unsafe fn set_low_latency_api(mft: &IMFTransform) -> ApiOutcome {
    LOW_LATENCY.apply(mft)
}

/// H.264 profile and level values for `MF_MT_MPEG2_PROFILE` / `MF_MT_MPEG2_LEVEL`.
///
/// These are the *bitstream* ids, not the `MF_PROFILE_H264_*` enum in mfidl.h:
/// `mf-probe --ladder` ran every candidate through the in-box encoder and only
/// `66/77/100` with `40`/`41` were accepted, each producing an SPS that says the
/// same number back. The mfidl form (`66 | MF_PROFILE_IDT_COMPLEX` = 322) and the
/// times-ten level convention (410) are both rejected with `E_INVALIDARG` here,
/// and mfidl.h is not even installed on this machine — so the ladder result is the
/// documentation.
pub mod h264 {
    pub const PROFILE_BASELINE: u32 = 66;
    pub const PROFILE_MAIN: u32 = 77;
    pub const PROFILE_HIGH: u32 = 100;

    pub const LEVEL_3_1: u32 = 31;
    pub const LEVEL_4_0: u32 = 40;
    pub const LEVEL_4_1: u32 = 41;
    pub const LEVEL_4_2: u32 = 42;
    pub const LEVEL_5_0: u32 = 50;
}

/// `MF_E_TRANSFORM_NEED_MORE_INPUT` — the normal "nothing ready yet" answer when
/// draining a transform, not a failure.
const NEED_MORE_INPUT: HRESULT = HRESULT(0xC00D_6D72_u32 as i32);
/// `MF_E_TRANSFORM_STREAM_CHANGE` — a decoder re-declaring its output format
/// after it has read the sequence header.
const STREAM_CHANGE: HRESULT = HRESULT(0xC00D_6D61_u32 as i32);
/// `MFVideoInterlace_Progressive`.
const INTERLACE_PROGRESSIVE: u32 = 2;

/// Initialise COM and Media Foundation for this process. Idempotent.
pub fn startup() -> CastResult<()> {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    let mut result: CastResult<()> = Ok(());
    ONCE.call_once(|| {
        // MFTs are in-proc COM objects; MFStartup does not initialise COM for us.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            result = Err(CastError::from(hr));
            return;
        }
        result = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(CastError::from)
            .map(|_| ());
    });
    result
}

/// One entry from `MFTEnumEx`, enough to name and identify a codec.
#[derive(Debug, Clone)]
pub struct MftInfo {
    pub friendly_name: String,
    pub clsid: GUID,
    pub hardware: bool,
    pub merit: u32,
}

impl MftInfo {
    pub fn label(&self) -> String {
        let kind = if self.hardware { "hw" } else { "sw" };
        if self.friendly_name.is_empty() {
            format!("[{kind}]")
        } else {
            format!("[{kind}] {}", self.friendly_name)
        }
    }
}

unsafe fn attributes<T: Interface>(obj: &T) -> CastResult<IMFAttributes> {
    Ok(obj.cast::<IMFAttributes>()?)
}

unsafe fn friendly_name(attrs: &IMFAttributes) -> String {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    if attrs
        .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len))
        .is_ok()
    {
        let n = (len as usize).min(buf.len());
        String::from_utf16_lossy(&buf[..n])
    } else {
        String::new()
    }
}

/// Enumerate transforms in a category. `hardware` selects between the GPU codec
/// and the CPU one, which is the first thing to know about any machine.
pub fn enumerate_mfts(
    category: &GUID,
    input: Option<(GUID, GUID)>,
    output: Option<(GUID, GUID)>,
    hardware: bool,
) -> CastResult<Vec<MftInfo>> {
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE
    } else {
        MFT_ENUM_FLAG_SYNCMFT
    };
    let in_info = input.map(|(major, sub)| MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: sub,
    });
    let out_info = output.map(|(major, sub)| MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: sub,
    });

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            *category,
            flags,
            in_info.as_ref().map(|i| i as *const _),
            out_info.as_ref().map(|i| i as *const _),
            &mut activates,
            &mut count,
        )
        .map_err(CastError::from)?;
    }
    drain_activates(activates, count, hardware)
}

fn drain_activates(
    activates: *mut Option<IMFActivate>,
    count: u32,
    hardware: bool,
) -> CastResult<Vec<MftInfo>> {
    let mut out = Vec::new();
    if count == 0 || activates.is_null() {
        return Ok(out);
    }
    unsafe {
        for i in 0..count as usize {
            let slot = &mut *activates.add(i);
            let Some(activate) = slot.take() else { continue };
            let attrs = attributes(&activate).ok();
            let (name, clsid, merit) = match attrs {
                Some(a) => (
                    friendly_name(&a),
                    a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute)
                        .unwrap_or(GUID::zeroed()),
                    a.GetUINT32(&MFT_CODEC_MERIT_Attribute).unwrap_or(0),
                ),
                None => (String::new(), GUID::zeroed(), 0),
            };
            out.push(MftInfo {
                friendly_name: name,
                clsid,
                hardware,
                merit,
            });
        }
        CoTaskMemFree(Some(activates as *const _));
    }
    Ok(out)
}

pub fn encoder_candidates(hardware: bool) -> CastResult<Vec<MftInfo>> {
    enumerate_mfts(
        &MFT_CATEGORY_VIDEO_ENCODER,
        None,
        Some((MFMediaType_Video, MFVideoFormat_H264)),
        hardware,
    )
}

pub fn decoder_candidates(hardware: bool) -> CastResult<Vec<MftInfo>> {
    enumerate_mfts(
        &MFT_CATEGORY_VIDEO_DECODER,
        Some((MFMediaType_Video, MFVideoFormat_H264)),
        None,
        hardware,
    )
}

/// The four-character codes a transform advertises on one stream, e.g.
/// `["NV12", "YUY2"]`.
///
/// This is how the probe answers "does the encoder take RGB32?" from the machine
/// instead of from documentation: whether a colour conversion has to live in the
/// capture stage is decided here, and guessing wrong costs a full rewrite of the
/// frame path.
pub fn stream_types(
    category: &GUID,
    hardware: bool,
    input: bool,
    want_other: Option<&GUID>,
) -> Vec<(String, Vec<String>)> {
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE
    } else {
        MFT_ENUM_FLAG_SYNCMFT
    };
    let mut out = Vec::new();
    unsafe {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(
            *category,
            flags,
            None,
            None,
            &mut activates,
            &mut count,
        )
        .is_err()
        {
            return out;
        }
        if !activates.is_null() {
            for i in 0..count as usize {
                let slot = &mut *activates.add(i);
                let Some(activate) = slot.take() else { continue };
                let name = attributes(&activate)
                    .ok()
                    .map(|a| friendly_name(&a))
                    .unwrap_or_default();
                let transform: IMFTransform = match activate.ActivateObject() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let mut subtypes = Vec::new();
                // An MFT will not list one stream's types until the *other* stream is
                // negotiated, so prime it first — and prime it with the codec we
                // actually care about, so that this box's WMV and HEVC encoders do not
                // answer the question on behalf of the H.264 one.
                let primed = prime_other_stream(&transform, input, want_other);
                // When the caller named a codec, only transforms that really carry it
                // on the far stream belong in the answer.
                if want_other.is_some() && !primed {
                    continue;
                }
                for t in 0..64u32 {
                    let mt = if input {
                        transform.GetInputAvailableType(0, t)
                    } else {
                        transform.GetOutputAvailableType(0, t)
                    };
                    let Ok(mt) = mt else { break };
                    let Ok(guid) = mt.GetGUID(&MF_MT_SUBTYPE) else { break };
                    subtypes.push(fourcc(&guid));
                }
                if !name.is_empty() || !subtypes.is_empty() {
                    out.push((name, subtypes));
                }
            }
            CoTaskMemFree(Some(activates as *const _));
        }
    }
    out
}

/// The profile and level the in-box H.264 encoder advertises on its own output
/// type, as `(profile, level, friendly name)`.
///
/// mfidl.h is not installed here and the windows crate does not export
/// `MF_PROFILE_H264_*`, so the first guess at the enum was wrong and every
/// profile-carrying negotiation failed. The transform's own answer is the only
/// version of the truth that matters, so the probe reads it rather than
/// remembering a constant.
pub fn advertised_h264_profile() -> CastResult<(u32, u32, String)> {
    startup()?;
    unsafe {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let out_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT,
            None,
            Some(&out_info),
            &mut activates,
            &mut count,
        )
        .map_err(CastError::from)?;
        if activates.is_null() || count == 0 {
            return Err(CastError::Unsupported("no H.264 encoder"));
        }
        let result = (|| -> CastResult<(u32, u32, String)> {
            let activate = (*activates.add(0))
                .take()
                .ok_or(CastError::InvalidState("empty activate"))?;
            let name = attributes(&activate)
                .ok()
                .map(|a| friendly_name(&a))
                .unwrap_or_default();
            let transform: IMFTransform = activate.ActivateObject().map_err(CastError::from)?;
            for t in 0..32u32 {
                let Ok(mt) = transform.GetOutputAvailableType(0, t) else {
                    break;
                };
                if mt.GetGUID(&MF_MT_SUBTYPE).map(|g| g != MFVideoFormat_H264).unwrap_or(true) {
                    continue;
                }
                let a = attributes(&mt)?;
                let profile = a.GetUINT32(&MF_MT_MPEG2_PROFILE).unwrap_or(0);
                let level = a.GetUINT32(&MF_MT_MPEG2_LEVEL).unwrap_or(0);
                return Ok((profile, level, name));
            }
            Err(CastError::Unsupported("encoder advertises no H264 output type"))
        })();
        CoTaskMemFree(Some(activates as *const _));
        result
    }
}

/// Try candidate `(profile, level)` encodings on fresh encoders and report the
/// ones the transform accepts *as configured*.
///
/// The windows crate does not export `MF_PROFILE_H264_*`, mfidl.h is not installed
/// on this machine, and the encoder advertises no profile of its own, so asking the
/// transform with each candidate is the only way to learn which number actually
/// puts Constrained Baseline in the SPS.
pub fn profile_ladder(
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    profiles: &[u32],
    levels: &[u32],
) -> Vec<(u32, u32)> {
    let mut accepted = Vec::new();
    for &profile in profiles {
        for &level in levels {
            let mut cfg = EncoderConfig::new(width, height, fps, bitrate, InputFormat::Nv12);
            cfg.profile = profile;
            cfg.level = level;
            if let Ok(enc) = H264Encoder::open(cfg) {
                if enc.negotiation() == "configured profile" {
                    accepted.push((profile, level));
                }
            }
        }
    }
    accepted
}

/// Negotiate one stream so the other stream's `Get*AvailableType` starts working.
///
/// `want` is the subtype the caller cares about; when it is set, only a type
/// carrying that subtype counts as a successful prime.
unsafe fn prime_other_stream(
    transform: &IMFTransform,
    on_output: bool,
    want: Option<&GUID>,
) -> bool {
    for t in 0..64u32 {
        let mt = if on_output {
            transform.GetOutputAvailableType(0, t)
        } else {
            transform.GetInputAvailableType(0, t)
        };
        let Ok(mt) = mt else { break };
        if let Some(want) = want {
            let matches = mt
                .GetGUID(&MF_MT_SUBTYPE)
                .map(|g| g == *want)
                .unwrap_or(false);
            if !matches {
                continue;
            }
        }
        let set = if on_output {
            transform.SetOutputType(0, &mt, 0)
        } else {
            transform.SetInputType(0, &mt, 0)
        };
        if set.is_ok() {
            return true;
        }
    }
    false
}

/// A readable name for a subtype GUID.
///
/// The FOURCC byte order inside `data1` is easy to get wrong and the mistakes are
/// silent, so the known formats are matched against Media Foundation's own
/// constants and anything else falls back to hex.
fn fourcc(g: &GUID) -> String {
    const KNOWN: [(&GUID, &str); 13] = [
        (&MFVideoFormat_NV12, "NV12"),
        (&MFVideoFormat_YUY2, "YUY2"),
        (&MFVideoFormat_UYVY, "UYVY"),
        (&MFVideoFormat_YV12, "YV12"),
        (&MFVideoFormat_I420, "I420"),
        (&MFVideoFormat_IYUV, "IYUV"),
        (&MFVideoFormat_AYUV, "AYUV"),
        (&MFVideoFormat_RGB32, "RGB32"),
        (&MFVideoFormat_RGB24, "RGB24"),
        (&MFVideoFormat_ARGB32, "ARGB32"),
        (&MFVideoFormat_H264, "H264"),
        (&MFVideoFormat_HEVC, "HEVC"),
        (&MFVideoFormat_MJPG, "MJPG"),
    ];
    for (known, name) in KNOWN.iter() {
        if g == *known {
            return (*name).to_string();
        }
    }
    format!("{g:?}")
}

/// A live transform plus what it was called.
pub struct MftSession {
    pub transform: IMFTransform,
    pub info: MftInfo,
}

/// Activate transforms until one accepts the caller's negotiation.
///
/// Trying rather than trusting is deliberate: with a broken GPU driver,
/// `MFT_ENUM_FLAG_HARDWARE` happily returns an entry that cannot be created, and
/// the only correct response is to fall through to the software codec.
unsafe fn open_mft<F>(
    category: &GUID,
    input: (GUID, GUID),
    output: (GUID, GUID),
    hardware: bool,
    mut setup: F,
) -> CastResult<MftSession>
where
    F: FnMut(&IMFTransform) -> CastResult<()>,
{
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE
    } else {
        MFT_ENUM_FLAG_SYNCMFT
    };
    let in_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: input.0,
        guidSubtype: input.1,
    };
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: output.0,
        guidSubtype: output.1,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        *category,
        flags,
        Some(&in_info),
        Some(&out_info),
        &mut activates,
        &mut count,
    )
    .map_err(CastError::from)?;

    let mut last = CastError::Unsupported("no transform matched the requested types");
    if !activates.is_null() {
        for i in 0..count as usize {
            let slot = &mut *activates.add(i);
            let Some(activate) = slot.take() else { continue };
            let info = read_mft_info(&activate, hardware);
            let transform: IMFTransform = match activate.ActivateObject() {
                Ok(t) => t,
                Err(e) => {
                    last = CastError::from(e);
                    continue;
                }
            };
            match setup(&transform) {
                Ok(()) => {
                    CoTaskMemFree(Some(activates as *const _));
                    return Ok(MftSession { transform, info });
                }
                Err(e) => last = e,
            }
        }
        CoTaskMemFree(Some(activates as *const _));
    }
    Err(last)
}

unsafe fn read_mft_info(activate: &IMFActivate, hardware: bool) -> MftInfo {
    let attrs = attributes(activate).ok();
    match attrs {
        Some(a) => MftInfo {
            friendly_name: friendly_name(&a),
            clsid: a
                .GetGUID(&MFT_TRANSFORM_CLSID_Attribute)
                .unwrap_or(GUID::zeroed()),
            hardware,
            merit: a.GetUINT32(&MFT_CODEC_MERIT_Attribute).unwrap_or(0),
        },
        None => MftInfo {
            friendly_name: String::new(),
            clsid: GUID::zeroed(),
            hardware,
            merit: 0,
        },
    }
}

/// Wrap a byte slice as an `IMFSample`.
unsafe fn sample_from_bytes(bytes: &[u8], pts_us: i64, dur_us: i64) -> CastResult<IMFSample> {
    let buffer = MFCreateMemoryBuffer(bytes.len() as u32).map_err(CastError::from)?;
    let mut dst: *mut u8 = std::ptr::null_mut();
    let mut max = 0u32;
    buffer
        .Lock(&mut dst, Some(&mut max), None)
        .map_err(CastError::from)?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    buffer.Unlock().map_err(CastError::from)?;
    buffer
        .SetCurrentLength(bytes.len() as u32)
        .map_err(CastError::from)?;
    let sample = MFCreateSample().map_err(CastError::from)?;
    sample.AddBuffer(&buffer).map_err(CastError::from)?;
    sample
        .SetSampleTime(pts_us * 10)
        .map_err(CastError::from)?; // 100 ns units
    // Rate control needs to know how long the frame occupies; without a duration
    // the H.264 encoder rejects ProcessInput with MF_E_NO_SAMPLE_DURATION.
    sample
        .SetSampleDuration(dur_us * 10)
        .map_err(CastError::from)?;
    Ok(sample)
}

fn sample_bytes(sample: &IMFSample) -> CastResult<(Vec<u8>, i64)> {
    unsafe {
        let buffer = sample.GetBufferByIndex(0).map_err(CastError::from)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max = 0u32;
        let mut len = 0u32;
        buffer
            .Lock(&mut ptr, Some(&mut max), Some(&mut len))
            .map_err(CastError::from)?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();
        let time = sample.GetSampleTime().unwrap_or(0);
        Ok((data, time))
    }
}

/// True when the access unit contains an IDR, SPS or PPS NAL.
pub fn is_keyframe_annexb(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            let n = if data[i + 2] == 1 {
                Some(3)
            } else if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                Some(4)
            } else {
                None
            };
            if let Some(n) = n {
                if i + n < data.len() {
                    let nal = data[i + n] & 0x1F;
                    if nal == 5 || nal == 7 || nal == 8 {
                        return true;
                    }
                }
                i += n;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// True when the AU starts with `00 00 00 01` / `00 00 01`, i.e. the encoder
/// really handed back Annex B and the packetiser may rely on it.
pub fn has_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

/// The first NAL types present in an Annex-B access unit, for diagnostics.
pub fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            let n = if data[i + 2] == 1 {
                Some(3)
            } else if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                Some(4)
            } else {
                None
            };
            if let Some(n) = n {
                if i + n < data.len() {
                    out.push(data[i + n] & 0x1F);
                }
                i += n;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The `profile_idc` and `level_idc` the encoder actually wrote into its SPS.
///
/// Setting a profile attribute is a claim; the bitstream is the evidence. This is
/// how `mf-probe` proves receivers will get Baseline (66) rather than whatever the
/// encoder defaulted to, which matters because a Main-profile stream with B-frames
/// is one a phone-as-receiver app cannot open.
pub fn sps_profile_level(annexb: &[u8]) -> Option<(u8, u8)> {
    let mut i = 0usize;
    while i + 4 < annexb.len() {
        if annexb[i] != 0 || annexb[i + 1] != 0 {
            i += 1;
            continue;
        }
        let n = if annexb[i + 2] == 1 {
            3
        } else if annexb[i + 2] == 0 && annexb[i + 3] == 1 {
            4
        } else {
            i += 1;
            continue;
        };
        let nal = i + n;
        if annexb[nal] & 0x1F == 7 {
            return annexb.get(nal + 3).map(|_| (annexb[nal + 1], annexb[nal + 3]));
        }
        i = nal;
    }
    None
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Reading the reference-frame count straight out of the SPS.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn u(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let i = self.bit;
            if i >= self.data.len() * 8 {
                return None;
            }
            let b = (self.data[i / 8] >> (7 - i % 8)) & 1;
            v = (v << 1) | b as u32;
            self.bit += 1;
        }
        Some(v)
    }

    /// Unsigned Exponential-Golomb: `leading zeros + value`, i.e. `2^len - 1 + bits`.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0usize;
        loop {
            if self.u(1)? == 1 {
                break;
            }
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }

    /// Signed Exponential-Golomb, needed only to step over POC type 1 fields.
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// `num_ref_frames` from the first SPS in an Annex-B access unit, if it is there.
///
/// This is the number the receiver's startup delay is made of: a decoder will not
/// emit the first picture until it has filled its reference list, so an encoder that
/// signs 27 references silently costs 27 frame times (~900 ms at 30 fps) before the
/// first pixel lands. Guessing it from a spec table is how that bug survives review,
/// so `mf-probe` reads it from the bitstream the encoder actually produced.
///
/// Only profiles without the chroma-format extension are decoded here; the
/// extension carries a variable-length scaling list and the in-box encoder emits
/// Baseline anyway, so `None` for High profile is an honest "not parsed", not a bug.
pub fn sps_num_ref_frames(annexb: &[u8]) -> Option<u32> {
    let nal = nal_payload(annexb, 7)?;
    // `nal_payload` drops the one-byte NAL header, so the SPS body starts here with
    // profile_idc, constraint flags and level_idc.
    let profile = *nal.first()?;
    if matches!(
        profile,
        100 | 110 | 120 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        return None;
    }
    let mut r = BitReader {
        data: nal.get(3..)?,
        bit: 0,
    };
    let _seq_parameter_set_id = r.ue()?;
    let _log2_max_frame_num_minus4 = r.ue()?;
    match r.ue()? {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            let cycle = r.ue()?;
            for _ in 0..cycle {
                r.se()?;
            }
        }
        // Type 2 is the low-delay coding the in-box encoder picks for Baseline, and
        // it declares nothing between the type and the reference count. Treating it
        // as unparseable is how the first probe run reported "not parsed" on a
        // perfectly ordinary SPS.
        2 => {}
        _ => return None,
    }
    r.ue()
}

/// Locate a NAL of the given type and return its payload — header byte gone and
/// the emulation prevention `0x03` removed.
///
/// Leaving `00 00 03` in place would shift the bit reader by eight bits at the
/// first escape, and a parser that is wrong only for some streams is worse than one
/// that says `None`.
fn nal_payload(annexb: &[u8], kind: u8) -> Option<Vec<u8>> {
    let mut i = 0usize;
    while i + 4 < annexb.len() {
        if annexb[i] != 0 || annexb[i + 1] != 0 {
            i += 1;
            continue;
        }
        let n = if annexb[i + 2] == 1 {
            3
        } else if annexb[i + 2] == 0 && annexb[i + 3] == 1 {
            4
        } else {
            i += 1;
            continue;
        };
        let start = i + n;
        let end = find_start_code(annexb, start);
        if annexb[start] & 0x1F == kind {
            let body = annexb.get(start + 1..end)?;
            let mut out = Vec::with_capacity(body.len());
            let mut zeros = 0u32;
            let mut b = body.iter();
            while let Some(&byte) = b.next() {
                if byte == 3 && zeros >= 2 {
                    zeros = 0;
                    continue;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                out.push(byte);
            }
            return Some(out);
        }
        i = start;
    }
    None
}

fn find_start_code(annexb: &[u8], from: usize) -> usize {
    let mut i = from + 1;
    while i + 3 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && (annexb[i + 2] == 1 || (annexb[i + 2] == 0 && i + 4 < annexb.len() && annexb[i + 3] == 1))
        {
            return i;
        }
        i += 1;
    }
    annexb.len()
}

/// What the encoder reads.
///
/// `mf-probe` settled this on real hardware: the in-box H.264 Encoder MFT lists no
/// RGB input type at all, so `Rgb32` only opens when a third-party encoder is
/// registered. `Nv12` is the guaranteed path, which is why [`bgra_to_nv12`] is a
/// first-class stage of the pipeline and not an optimisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Rgb32,
    Nv12,
}

impl InputFormat {
    pub fn subtype(&self) -> GUID {
        match self {
            InputFormat::Rgb32 => MFVideoFormat_RGB32,
            InputFormat::Nv12 => MFVideoFormat_NV12,
        }
    }

    /// Row length for an unpadded frame of the given width.
    pub fn stride(&self, width: u32) -> usize {
        match self {
            InputFormat::Rgb32 => width as usize * 4,
            InputFormat::Nv12 => width as usize,
        }
    }

    pub fn frame_bytes(&self, width: u32, height: u32) -> usize {
        match self {
            InputFormat::Rgb32 => width as usize * height as usize * 4,
            InputFormat::Nv12 => (width as usize * height as usize * 3) / 2,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub input: InputFormat,
    /// Try the GPU codec first. It will not exist on this machine, but the flag
    /// is what makes the same code fast on a machine that has one.
    pub prefer_hardware: bool,
    pub profile: u32,
    pub level: u32,
}

impl EncoderConfig {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32, input: InputFormat) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_bps,
            input,
            prefer_hardware: false,
            profile: h264::PROFILE_BASELINE,
            level: h264::LEVEL_4_1,
        }
    }
}

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    pub bytes: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: i64,
    pub encode_us: u64,
}

/// A synchronous H.264 encoder driven straight through `IMFTransform`.
pub struct H264Encoder {
    session: MftSession,
    cfg: EncoderConfig,
    app_provides_output: bool,
    out_cb_size: u32,
    variant: usize,
    low_latency: ApiOutcome,
    next_pts_us: i64,
    frames_in: u64,
    frames_out: u64,
}

impl H264Encoder {
    pub fn open(cfg: EncoderConfig) -> CastResult<Self> {
        startup()?;
        let input = (MFMediaType_Video, cfg.input.subtype());
        let output = (MFMediaType_Video, MFVideoFormat_H264);

        let mut attempt: Option<MftSession> = None;
        let mut last_err = CastError::Unsupported("no H.264 encoder found");
        let mut variant_used = 0usize;
        let mut low_latency = ApiOutcome::NoCodecApi;
        for hardware in [cfg.prefer_hardware, false] {
            if hardware && !cfg.prefer_hardware {
                continue;
            }
            let result = unsafe {
                open_mft(
                    &MFT_CATEGORY_VIDEO_ENCODER,
                    input,
                    output,
                    hardware,
                    |mft| Self::negotiate(mft, &cfg, &mut variant_used, &mut low_latency),
                )
            };
            match result {
                Ok(s) => {
                    attempt = Some(s);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let session = attempt.ok_or(last_err)?;

        let info = unsafe { session.transform.GetOutputStreamInfo(0) }
            .map_err(CastError::from)?;
        let provides = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
        Ok(Self {
            session,
            cfg,
            app_provides_output: info.dwFlags & provides == 0,
            out_cb_size: info.cbSize,
            variant: variant_used,
            low_latency,
            next_pts_us: 0,
            frames_in: 0,
            frames_out: 0,
        })
    }

    /// Which negotiation actually landed, for the probe's audit line.
    pub fn negotiation(&self) -> &'static str {
        match self.variant {
            0 => "configured profile",
            1 => "Baseline fallback",
            _ => "encoder default (no profile/level set)",
        }
    }

    /// Whether `CODECAPI_AVLowLatencyMode` was accepted, for the probe's audit line.
    pub fn low_latency_api(&self) -> ApiOutcome {
        self.low_latency
    }

    fn frame_dur_us(&self) -> i64 {
        1_000_000 / self.cfg.fps.max(1) as i64
    }

    /// Attribute setup + type negotiation, run while the transform is fresh.
    ///
    /// The variant ladder is not defensive noise: `MF_MT_MPEG2_PROFILE` values are
    /// validated by the encoder, an unsupported one fails the whole
    /// `SetOutputType`, and the failing set differs between the in-box Microsoft
    /// encoder and a vendor one. Recording which variant landed is also how
    /// `mf-probe` proves the encoder really got Constrained Baseline rather than
    /// silently opening with a B-frame-allowing default.
    unsafe fn negotiate(
        mft: &IMFTransform,
        cfg: &EncoderConfig,
        variant_used: &mut usize,
        low_latency: &mut ApiOutcome,
    ) -> CastResult<()> {
        // Before anything is negotiated: the encoder chooses whether to buffer at
        // SetOutputType time.
        let store = attributes(mft)?;
        store
            .SetUINT32(&MF_LOW_LATENCY, 1)
            .map_err(CastError::from)?;
        *low_latency = set_low_latency_api(mft);

        let mut last = CastError::InvalidState("encoder rejected every profile");
        for (i, &(with_profile, profile, level)) in Self::variants(cfg).iter().enumerate() {
            match Self::negotiate_once(mft, cfg, with_profile, profile, level) {
                Ok(()) => {
                    *variant_used = i;
                    return Ok(());
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// `(sets_profile, profile, level)` attempts, strictest first.
    fn variants(cfg: &EncoderConfig) -> [(bool, u32, u32); 3] {
        [
            (true, cfg.profile, cfg.level),
            (true, h264::PROFILE_BASELINE, h264::LEVEL_4_0),
            (false, 0, 0),
        ]
    }

    unsafe fn negotiate_once(
        mft: &IMFTransform,
        cfg: &EncoderConfig,
        with_profile: bool,
        profile: u32,
        level: u32,
    ) -> CastResult<()> {
        let out_type = MFCreateMediaType().map_err(CastError::from)?;
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(CastError::from)?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(CastError::from)?;
        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)
            .map_err(CastError::from)?;
        if with_profile {
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
                .map_err(CastError::from)?;
            out_type
                .SetUINT32(&MF_MT_MPEG2_LEVEL, level)
                .map_err(CastError::from)?;
        }
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))
            .map_err(CastError::from)?;
        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps, 1))
            .map_err(CastError::from)?;
        out_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)
            .map_err(CastError::from)?;
        // The low-latency hint belongs on the media type too: some MFTs only
        // read it from there during negotiation.
        out_type
            .SetUINT32(&MF_LOW_LATENCY, 1)
            .map_err(CastError::from)?;
        mft.SetOutputType(0, &out_type, 0)
            .map_err(CastError::from)?;

        let in_type = MFCreateMediaType().map_err(CastError::from)?;
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(CastError::from)?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &cfg.input.subtype())
            .map_err(CastError::from)?;
        in_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))
            .map_err(CastError::from)?;
        in_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps, 1))
            .map_err(CastError::from)?;
        in_type
            .SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.input.stride(cfg.width) as u32)
            .map_err(CastError::from)?;
        in_type
            .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
            .map_err(CastError::from)?;
        in_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)
            .map_err(CastError::from)?;
        mft.SetInputType(0, &in_type, 0).map_err(CastError::from)?;

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(CastError::from)?;
        Ok(())
    }

    pub fn description(&self) -> String {
        self.session.info.label()
    }

    /// Encoder latency audit: `frames_out / frames_in` must reach 1.0 once warm.
    pub fn output_ratio(&self) -> f64 {
        if self.frames_in == 0 {
            1.0
        } else {
            self.frames_out as f64 / self.frames_in as f64
        }
    }

    /// Frames the encoder is holding. A sustained non-zero value means the
    /// low-latency hint did not take, and no amount of network tuning will fix
    /// the latency that follows.
    pub fn queue_depth(&self) -> i64 {
        self.frames_in as i64 - self.frames_out as i64
    }

    pub fn encode(&mut self, src: &[u8]) -> CastResult<Option<AccessUnit>> {
        let expected = self.cfg.input.frame_bytes(self.cfg.width, self.cfg.height);
        if src.len() < expected {
            return Err(CastError::InvalidState("frame shorter than the format requires"));
        }
        let t0 = now_us();
        let pts = self.next_pts_us;
        unsafe {
            let status = self.session.transform.GetInputStatus(0).map_err(CastError::from)?;
            if status & MFT_INPUT_STATUS_ACCEPT_DATA.0 as u32 == 0 {
                return Err(CastError::InvalidState("encoder is not accepting input"));
            }
            let sample = sample_from_bytes(&src[..expected], pts, self.frame_dur_us())?;
            self.session
                .transform
                .ProcessInput(0, &sample, 0)
                .map_err(CastError::from)?;
        }
        self.next_pts_us += self.frame_dur_us();
        self.frames_in += 1;
        self.drain(t0, pts)
    }

    /// Pull every access unit currently available. A sync low-latency encoder
    /// returns exactly one; draining in a loop is what makes a buffering encoder
    /// observable instead of silently lagging.
    fn drain(&mut self, t0: u64, pts: i64) -> CastResult<Option<AccessUnit>> {
        let mut newest: Option<AccessUnit> = None;
        loop {
            let au = unsafe {
                let mut buf = MFT_OUTPUT_DATA_BUFFER::default();
                if self.app_provides_output {
                    let b = MFCreateMemoryBuffer(self.out_cb_size.max(1))
                        .map_err(CastError::from)?;
                    let s = MFCreateSample().map_err(CastError::from)?;
                    s.AddBuffer(&b).map_err(CastError::from)?;
                    buf.pSample = std::mem::ManuallyDrop::new(Some(s));
                }
                let mut status = 0u32;
                let hr = self
                    .session
                    .transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
                let sample = std::mem::ManuallyDrop::into_inner(buf.pSample);
                let _events = std::mem::ManuallyDrop::into_inner(buf.pEvents);
                match hr {
                    Ok(()) => {}
                    Err(e) if e.code() == NEED_MORE_INPUT => return Ok(newest),
                    Err(e) => return Err(CastError::from(e)),
                }
                let Some(sample) = sample else { return Ok(newest) };
                let (bytes, _) = sample_bytes(&sample)?;
                AccessUnit {
                    keyframe: is_keyframe_annexb(&bytes),
                    encode_us: now_us() - t0,
                    bytes,
                    pts_us: pts,
                }
            };
            self.frames_out += 1;
            newest = Some(au);
        }
    }

    /// Force an IDR by re-negotiating the current output type. This is the
    /// documented escape from `ICodecAPI`/VARIANT plumbing and is cheap enough
    /// for the two times we need it: a new receiver joins, or the channel
    /// reports sustained loss.
    pub fn force_keyframe(&mut self) -> CastResult<()> {
        unsafe {
            let t = self
                .session
                .transform
                .GetOutputCurrentType(0)
                .map_err(CastError::from)?;
            self.session
                .transform
                .SetOutputType(0, &t, 0)
                .map_err(CastError::from)
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .session
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .session
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// One decoded frame: planar NV12 in system memory.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub decode_us: u64,
    /// Presentation time the decoder stamped on the sample, in microseconds, or
    /// `i64::MIN` when it stamped nothing.
    ///
    /// This is the only way to tell a decoder that *delays* a frame from one that
    /// *drops* it: a pipeline shows up as `input_pts - pts_us` staying constant, a
    /// drop shows up as that difference going to zero after the first frames.
    pub pts_us: i64,
}

pub struct H264Decoder {
    session: MftSession,
    width: usize,
    height: usize,
    app_provides_output: bool,
    out_cb_size: u32,
    api_report: Vec<String>,
    frames_in: u64,
    frames_out: u64,
    /// How many times the transform said "nothing ready" per input frame.
    ///
    /// This is what separates a decoder with a real pipeline from a decoder we are
    /// draining wrongly: `need_more / frames_in` is the number of `ProcessOutput`
    /// calls that came back empty, and it is measured rather than assumed.
    need_more: u64,
    /// The HRESULT of the last failed `ProcessOutput`, for the end-of-stream audit.
    last_error: i32,
}

impl H264Decoder {
    pub fn open(width: u32, height: u32) -> CastResult<Self> {
        Self::open_with(width, height, 0, &[LOW_LATENCY])
    }

    /// Open with an explicit frame rate and codec-API property set.
    ///
    /// This exists because the receiver's startup delay turned out not to be the
    /// reference list (`num_ref_frames` read back as 1), so the only way to find what
    /// actually holds frames is to change one thing at a time and measure. `fps` of
    /// zero means "declare nothing", which is what the naive path used to do.
    pub fn open_with(width: u32, height: u32, fps: u32, props: &[CodecProp]) -> CastResult<Self> {
        startup()?;
        let input = (MFMediaType_Video, MFVideoFormat_H264);
        let output = (MFMediaType_Video, MFVideoFormat_NV12);
        let mut session = None;
        let mut last_err = CastError::Unsupported("no H.264 decoder found");
        let mut report = Vec::new();
        for hardware in [true, false] {
            let r = unsafe {
                open_mft(
                    &MFT_CATEGORY_VIDEO_DECODER,
                    input,
                    output,
                    hardware,
                    |mft| Self::negotiate(mft, width, height, fps, props, &mut report),
                )
            };
            match r {
                Ok(s) => {
                    session = Some(s);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let session = session.ok_or(last_err)?;
        let info = unsafe { session.transform.GetOutputStreamInfo(0) }
            .map_err(CastError::from)?;
        let provides = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
        Ok(Self {
            session,
            width: width as usize,
            height: height as usize,
            app_provides_output: info.dwFlags & provides == 0,
            out_cb_size: info.cbSize,
            api_report: report,
            frames_in: 0,
            frames_out: 0,
            need_more: 0,
            last_error: 0,
        })
    }

    /// What each codec-API property answered, for the probe's audit line.
    pub fn api_report(&self) -> String {
        if self.api_report.is_empty() {
            "no codec-API properties".to_string()
        } else {
            self.api_report.join(", ")
        }
    }

    /// `NEED_MORE_INPUT` answers per submitted frame, and the last other HRESULT.
    pub fn stall_stats(&self) -> (u64, i32) {
        (self.need_more, self.last_error)
    }

    unsafe fn negotiate(
        mft: &IMFTransform,
        width: u32,
        height: u32,
        fps: u32,
        props: &[CodecProp],
        report: &mut Vec<String>,
    ) -> CastResult<()> {
        // Best effort: the in-box decoder does not answer IMFAttributes on every
        // Windows build, and a decoder that ignores the hint is still a decoder.
        if let Ok(store) = attributes(mft) {
            let _ = store.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let apply = |stage: PropStage, report: &mut Vec<String>| {
            for p in props.iter().filter(|p| p.stage == stage) {
                let outcome = p.apply(mft);
                report.push(format!("{}={}", p.name, outcome));
            }
        };
        apply(PropStage::BeforeTypes, report);

        let in_type = MFCreateMediaType().map_err(CastError::from)?;
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(CastError::from)?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(CastError::from)?;
        // No profile/level here on purpose: the bitstream declares them in its own
        // SPS, and pre-declaring a wrong value only gives the decoder a reason to
        // refuse the type.
        in_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack2(width, height))
            .map_err(CastError::from)?;
        in_type
            .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
            .map_err(CastError::from)?;
        if fps > 0 {
            // Declaring the frame rate is what a real stream does through the SPS
            // timing fields, and the sweep exists because the decoder behaves
            // differently when it has to guess.
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, pack2(fps, 1))
                .map_err(CastError::from)?;
        }
        mft.SetInputType(0, &in_type, 0).map_err(CastError::from)?;

        let out_type = MFCreateMediaType().map_err(CastError::from)?;
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(CastError::from)?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(CastError::from)?;
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack2(width, height))
            .map_err(CastError::from)?;
        out_type
            .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
            .map_err(CastError::from)?;
        out_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)
            .map_err(CastError::from)?;
        out_type
            .SetUINT32(&MF_LOW_LATENCY, 1)
            .map_err(CastError::from)?;
        mft.SetOutputType(0, &out_type, 0)
            .map_err(CastError::from)?;

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(CastError::from)?;
        apply(PropStage::AfterStreamStart, report);
        Ok(())
    }

    pub fn description(&self) -> String {
        self.session.info.label()
    }

    pub fn queue_depth(&self) -> i64 {
        self.frames_in as i64 - self.frames_out as i64
    }

    pub fn output_ratio(&self) -> f64 {
        if self.frames_in == 0 {
            1.0
        } else {
            self.frames_out as f64 / self.frames_in as f64
        }
    }

    /// Re-read the decoder's own output type and resize to what it says.
    unsafe fn adopt_stream_change(&mut self) -> CastResult<()> {
        let t = self
            .session
            .transform
            .GetOutputCurrentType(0)
            .map_err(CastError::from)?;
        let packed = attributes(&t)?
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(CastError::from)?;
        let (w, h) = unpack2(packed);
        if w != 0 && h != 0 {
            self.width = w as usize;
            self.height = h as usize;
        }
        let info = self
            .session
            .transform
            .GetOutputStreamInfo(0)
            .map_err(CastError::from)?;
        self.out_cb_size = info.cbSize;
        Ok(())
    }

    /// Submit one access unit and take back every frame the decoder released.
    ///
    /// A `Vec` rather than an `Option`: while it is filling its reference buffer a
    /// decoder can hand back two frames on one input, and keeping only the newest
    /// would drop a displayed frame on the floor without anyone noticing.
    pub fn decode(&mut self, au: &[u8]) -> CastResult<Vec<DecodedFrame>> {
        let t0 = now_us();
        unsafe {
            let sample = sample_from_bytes(au, self.frames_in as i64 * 33_333, 33_333)?;
            self.session
                .transform
                .ProcessInput(0, &sample, 0)
                .map_err(CastError::from)?;
            self.frames_in += 1;
        }
        self.drain(t0, false)
    }

    /// End the stream and collect everything still buffered.
    ///
    /// The number of frames that come back here is the decoder's true pipeline
    /// depth, which is the difference between a 40 ms and a second-latency
    /// receiver, so `mf-probe` measures it instead of assuming it.
    pub fn flush(&mut self) -> CastResult<Vec<DecodedFrame>> {
        let t0 = now_us();
        unsafe {
            // The stream is over as far as the sender is concerned; after this the
            // decoder is only obliged to give back what it is holding.
            self.session.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        }
        .map_err(CastError::from)?;
        self.drain(t0, true)
    }

    /// Pull frames until the transform says it has none. With `to_end`, any error
    /// means "nothing left", because a flushed decoder reports end of stream in
    /// whichever way it finds convenient.
    fn drain(&mut self, t0: u64, to_end: bool) -> CastResult<Vec<DecodedFrame>> {
        let mut out = Vec::new();
        let mut changes = 0u32;
        loop {
            let frame = unsafe {
                let mut buf = MFT_OUTPUT_DATA_BUFFER::default();
                if self.app_provides_output {
                    let b = MFCreateMemoryBuffer(self.out_cb_size.max(1))
                        .map_err(CastError::from)?;
                    let s = MFCreateSample().map_err(CastError::from)?;
                    s.AddBuffer(&b).map_err(CastError::from)?;
                    buf.pSample = std::mem::ManuallyDrop::new(Some(s));
                }
                let mut status = 0u32;
                let hr = self
                    .session
                    .transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
                let sample = std::mem::ManuallyDrop::into_inner(buf.pSample);
                let _events = std::mem::ManuallyDrop::into_inner(buf.pEvents);
                match hr {
                    Ok(()) => sample,
                    Err(e) if e.code() == NEED_MORE_INPUT => {
                        self.need_more += 1;
                        return Ok(out);
                    }
                    Err(e) => {
                        // Recorded rather than swallowed: whether the transform keeps
                        // answering `NEED_MORE_INPUT` after end-of-stream is the
                        // difference between a decoder with a pipeline and a drain
                        // loop that gave up early.
                        self.last_error = e.code().0;
                        if to_end {
                            return Ok(out);
                        }
                        if e.code() != STREAM_CHANGE {
                            return Err(CastError::from(e));
                        }
                        // The decoder has parsed the SPS and is re-declaring its output
                        // format. Adopting it instead of failing is also what makes a
                        // mid-stream resolution change survivable on a real call.
                        if changes > 2 {
                            return Err(CastError::InvalidState("decoder repeated stream change"));
                        }
                        changes += 1;
                        self.adopt_stream_change()?;
                        continue;
                    }
                }
            };
            let Some(sample) = frame else { return Ok(out) };
            let pts_us = unsafe { sample.GetSampleTime() }.unwrap_or(i64::MIN) / 10;
            let (bytes, _) = sample_bytes(&sample)?;
            self.frames_out += 1;
            out.push(split_nv12(bytes, self.width, self.height, now_us() - t0, pts_us));
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .session
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .session
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

fn split_nv12(
    bytes: Vec<u8>,
    width: usize,
    height: usize,
    decode_us: u64,
    pts_us: i64,
) -> DecodedFrame {
    let y_len = width * height;
    let y_len = y_len.min(bytes.len());
    DecodedFrame {
        y: bytes[..y_len].to_vec(),
        uv: bytes[y_len..].to_vec(),
        width,
        height,
        decode_us,
        pts_us,
    }
}

fn pack2(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn unpack2(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

// ---------------------------------------------------------------------------
// Colour conversion
// ---------------------------------------------------------------------------

/// BT.601 limited-range BGRA -> NV12 into a packed `Y | UV` buffer.
///
/// Written by hand rather than delegated to `IMFVideoFormatConverter` because
/// that is a whole MFT graph: it costs more than the encode budget at 1080p and
/// buffers on some drivers. Q6 fixed point keeps it to shifts and adds.
///
/// `threads` splits the frame into two-row bands, which is the natural unit for
/// 4:2:0 — a band is only valid if it starts on an even row.
pub fn bgra_to_nv12(src: &[u8], width: usize, height: usize, dst: &mut [u8], threads: usize) {
    assert!(width % 2 == 0 && height % 2 == 0, "4:2:0 needs even dimensions");
    let need = width * height * 3 / 2;
    assert!(dst.len() >= need, "NV12 destination too small");
    assert!(
        src.len() >= width * height * 4,
        "BGRA source too small"
    );

    let bands = band_starts(height, threads);
    let (yplane, uvplane) = dst.split_at_mut(width * height);
    std::thread::scope(|s| {
        let (mut ycur, mut uvcur) = (yplane, uvplane);
        for w in bands.windows(2) {
            let (y0, y1) = (w[0], w[1]);
            let rows = y1 - y0;
            let (yslice, ytail) = ycur.split_at_mut(rows * width);
            let (uvslice, uvtail) = uvcur.split_at_mut(rows / 2 * width);
            ycur = ytail;
            uvcur = uvtail;
            let src = &src[y0 * width * 4..y1 * width * 4];
            s.spawn(move || convert_bands(src, width, yslice, uvslice));
        }
    });
}

/// The scalar reference for the whole frame, single threaded.
///
/// Exposed because a hand-written vector kernel is only trustworthy next to
/// something obviously correct: the test suite diffs this against the SSE2 path,
/// and it is the fallback on any target without SSE2.
pub fn bgra_to_nv12_scalar(src: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let need = width * height * 3 / 2;
    assert!(dst.len() >= need, "NV12 destination too small");
    assert!(src.len() >= width * height * 4, "BGRA source too small");
    let (yplane, uvplane) = dst.split_at_mut(width * height);
    convert_bands_cols(src, width, yplane, uvplane, 0, width);
}

/// Even-aligned band boundaries covering `[0, height)`.
fn band_starts(height: usize, threads: usize) -> Vec<usize> {
    let workers = threads.clamp(1, 16).min(height.div_ceil(2).max(1));
    let rows_per = (height.div_ceil(2) * workers).div_ceil(workers * workers).max(1);
    let step = (rows_per * 2).max(2);
    let mut out = Vec::with_capacity(workers + 1);
    let mut y = 0usize;
    while y < height {
        out.push(y);
        y += step;
    }
    out.push(height);
    // Collapse empty tail bands.
    while out.len() > 2 && out[out.len() - 2] >= height {
        out.remove(out.len() - 2);
    }
    out
}

/// Convert rows `[y0, y1)` of a frame. `src` starts at row `y0`, and `yplane` /
/// `uvplane` are exactly the bytes those rows own, so no worker ever touches
/// another worker's memory.
fn convert_bands(src: &[u8], width: usize, yplane: &mut [u8], uvplane: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse2") {
            unsafe { sse2::convert(src, width, yplane, uvplane) };
            return;
        }
    }
    convert_bands_cols(src, width, yplane, uvplane, 0, width);
}

/// The scalar reference, over columns `[x0, x1)`.
///
/// Kept live even though the SSE2 kernel is the production path: `x1` is normally
/// `width`, and the vector path calls it for the columns past its last full
/// vector block. That makes the two paths comparable in tests, which is the only
/// reason a hand-written kernel can be trusted.
fn convert_bands_cols(
    src: &[u8],
    width: usize,
    yplane: &mut [u8],
    uvplane: &mut [u8],
    x0: usize,
    x1: usize,
) {
    let rows = yplane.len() / width;
    let half_w = width / 2;
    let cx0 = (x0 / 2).max(0);
    let cx1 = x1.div_ceil(2).min(half_w);

    for cy in (0..rows).step_by(2) {
        let row0 = (cy * width) * 4;
        let row1 = ((cy + 1) * width) * 4;
        let uv_row = (cy / 2) * width;
        for cx in cx0..cx1 {
            let x = cx * 2;
            // One dot product per source pixel: four for luma, and the same four
            // feed the chroma average, so a 2x2 block costs exactly four calls.
            let a = row0 + x * 4;
            let (p00, q00, r00) = luma_cb_cr(&src[a..a + 4]);
            let (p01, q01, r01) = luma_cb_cr(&src[a + 4..a + 8]);
            yplane[cy * width + x] = p00;
            yplane[cy * width + x + 1] = p01;

            // Band boundaries are even and `height` is even, so a row pair is
            // always complete inside one band.
            let c = row1 + x * 4;
            let (p10, q10, r10) = luma_cb_cr(&src[c..c + 4]);
            let (p11, q11, r11) = luma_cb_cr(&src[c + 4..c + 8]);
            yplane[(cy + 1) * width + x] = p10;
            yplane[(cy + 1) * width + x + 1] = p11;

            uvplane[uv_row + cx * 2] = ((q00 + q01 + q10 + q11 + 2) / 4).clamp(16, 240) as u8;
            uvplane[uv_row + cx * 2 + 1] = ((r00 + r01 + r10 + r11 + 2) / 4).clamp(16, 240) as u8;
        }
    }
}

/// Luma and chroma of one BGRA pixel, Q0 integers (chroma is returned unrounded
/// so four of them can be averaged without losing precision).
#[inline(always)]
fn luma_cb_cr(p: &[u8]) -> (u8, i32, i32) {
    let b = p[0] as i32;
    let g = p[1] as i32;
    let r = p[2] as i32;
    let y = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(16, 235);
    let cb = ((-38 * r - 75 * g + 112 * b + 128) >> 8) + 128;
    let cr = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (y as u8, cb, cr)
}

/// SSE2 BGRA -> NV12.
///
/// The scalar path measured 10.5 ms for a 1080p frame on this machine, which is
/// more than the entire encode budget, so the same arithmetic runs on four and
/// eight pixels at a time.
///
/// The layout does most of the work: a BGRA pixel seen as 16-bit words splits
/// into `[B, R]` after masking the odd bytes and `[G, A]` after shifting right by
/// eight, so `PMADDWD` against `(25, 66)` and `(129, 0)` is the whole luma dot
/// product for four pixels in two instructions. Chroma takes the 2x2 average with
/// `PAVGB`, which is exactly the box filter 4:2:0 wants.
#[cfg(target_arch = "x86_64")]
pub mod sse2 {
    use super::convert_bands_cols;
    use core::arch::x86_64::*;

    /// Two signed 16-bit coefficients in one 32-bit lane, the operand shape
    /// `PMADDWD` reads a word pair from.
    const fn w2(a: i16, b: i16) -> i32 {
        ((a as u16 as u32) | ((b as u16 as u32) << 16)) as i32
    }

    const BR_Y: i32 = w2(25, 66);
    const GA_Y: i32 = w2(129, 0);
    const BR_CB: i32 = w2(112, -38);
    const GA_CB: i32 = w2(-75, 0);
    const BR_CR: i32 = w2(-18, 112);
    const GA_CR: i32 = w2(-94, 0);
    /// Bytes at even 16-bit positions: keeps B and R, drops G and A.
    const EVEN: i32 = 0x00FF_00FF;

    /// One 16-byte load (4 BGRA pixels) reduced to four clamped luma dwords.
    #[inline(always)]
    unsafe fn luma4(v: __m128i) -> __m128i {
        let br = _mm_and_si128(v, _mm_set1_epi32(EVEN));
        let ga = _mm_srli_epi16(v, 8);
        let dot = _mm_add_epi32(
            _mm_madd_epi16(br, _mm_set1_epi32(BR_Y)),
            _mm_madd_epi16(ga, _mm_set1_epi32(GA_Y)),
        );
        let y = _mm_add_epi32(_mm_srai_epi32(_mm_add_epi32(dot, _mm_set1_epi32(128)), 8), _mm_set1_epi32(16));
        _mm_max_epi32(_mm_min_epi32(y, _mm_set1_epi32(235)), _mm_set1_epi32(16))
    }

    /// 16 source pixels of one row as 16 luma bytes.
    #[inline(always)]
    unsafe fn luma16(p: *const __m128i) -> __m128i {
        let a = _mm_packs_epi32(luma4(_mm_loadu_si128(p)), luma4(_mm_loadu_si128(p.add(1))));
        let b = _mm_packs_epi32(luma4(_mm_loadu_si128(p.add(2))), luma4(_mm_loadu_si128(p.add(3))));
        _mm_packus_epi16(a, b)
    }

    /// Lane k becomes `avg(px k, px k+1)`; lanes 0 and 2 are then the
    /// non-overlapping horizontal pairs, which the shuffle duplicates so the low
    /// 64 bits can be taken with one `UNPDPD`.
    #[inline(always)]
    unsafe fn pair_avg(v: __m128i) -> __m128i {
        let a = _mm_avg_epu8(v, _mm_srli_si128(v, 4));
        _mm_shuffle_epi32(a, 0b1000_1000)
    }

    #[inline(always)]
    unsafe fn low_halves(a: __m128i, b: __m128i) -> __m128i {
        _mm_castpd_si128(_mm_unpacklo_pd(_mm_castsi128_pd(a), _mm_castsi128_pd(b)))
    }

    /// Four chroma samples (already horizontally averaged) as Cb/Cr dwords.
    #[inline(always)]
    unsafe fn chroma4(c: __m128i) -> (__m128i, __m128i) {
        let br = _mm_and_si128(c, _mm_set1_epi32(EVEN));
        let ga = _mm_srli_epi16(c, 8);
        let round = _mm_set1_epi32(128);
        let bias = round;
        let cb = _mm_add_epi32(
            _mm_srai_epi32(
                _mm_add_epi32(
                    _mm_madd_epi16(br, _mm_set1_epi32(BR_CB)),
                    _mm_madd_epi16(ga, _mm_set1_epi32(GA_CB)),
                ),
                8,
            ),
            bias,
        );
        let cr = _mm_add_epi32(
            _mm_srai_epi32(
                _mm_add_epi32(
                    _mm_madd_epi16(br, _mm_set1_epi32(BR_CR)),
                    _mm_madd_epi16(ga, _mm_set1_epi32(GA_CR)),
                ),
                8,
            ),
            bias,
        );
        let lo = _mm_set1_epi32(16);
        let hi = _mm_set1_epi32(240);
        (
            _mm_max_epi32(_mm_min_epi32(cb, hi), lo),
            _mm_max_epi32(_mm_min_epi32(cr, hi), lo),
        )
    }

    /// Convert one even-row band: 16 columns per vector step, then the scalar
    /// kernel finishes the remaining columns so odd screen widths are not slower
    /// than even ones.
    ///
    /// # Safety
    /// `src` must cover `width * yplane.len() / width * 4` bytes laid out as
    /// tightly packed BGRA rows, and the planes must be exactly the NV12 size for
    /// `width` by `yplane.len() / width` rows.
    pub unsafe fn convert(src: &[u8], width: usize, yplane: &mut [u8], uvplane: &mut [u8]) {
        let rows = yplane.len() / width;
        if rows % 2 != 0 || width == 0 {
            convert_bands_cols(src, width, yplane, uvplane, 0, width);
            return;
        }
        let blocks = width / 16;
        let sp = src.as_ptr();
        let yp = yplane.as_mut_ptr();
        let uvp = uvplane.as_mut_ptr();

        for cy in (0..rows).step_by(2) {
            // Row starts are byte offsets cast to vector pointers: `width / 4`
            // would truncate for any width that is not a multiple of four, which
            // is exactly the 1366-pixel laptop panel class.
            let r0 = sp.add(cy * width * 4) as *const __m128i;
            let r1 = sp.add((cy + 1) * width * 4) as *const __m128i;
            let uv_row = (cy / 2) * width;
            for bx in 0..blocks {
                let o = bx * 4;
                _mm_storeu_si128(
                    yp.add(cy * width + bx * 16) as *mut __m128i,
                    luma16(r0.add(o)),
                );
                _mm_storeu_si128(
                    yp.add((cy + 1) * width + bx * 16) as *mut __m128i,
                    luma16(r1.add(o)),
                );
                let c0 = low_halves(
                    pair_avg(_mm_loadu_si128(r0.add(o))),
                    pair_avg(_mm_loadu_si128(r0.add(o + 1))),
                );
                let c0 = _mm_avg_epu8(
                    c0,
                    low_halves(
                        pair_avg(_mm_loadu_si128(r1.add(o))),
                        pair_avg(_mm_loadu_si128(r1.add(o + 1))),
                    ),
                );
                let c1 = low_halves(
                    pair_avg(_mm_loadu_si128(r0.add(o + 2))),
                    pair_avg(_mm_loadu_si128(r0.add(o + 3))),
                );
                let c1 = _mm_avg_epu8(
                    c1,
                    low_halves(
                        pair_avg(_mm_loadu_si128(r1.add(o + 2))),
                        pair_avg(_mm_loadu_si128(r1.add(o + 3))),
                    ),
                );
                let (a, b) = chroma4(c0);
                let (c, d) = chroma4(c1);
                let cb = _mm_packus_epi16(_mm_packs_epi32(a, c), _mm_packs_epi32(a, c));
                let cr = _mm_packus_epi16(_mm_packs_epi32(b, d), _mm_packs_epi32(b, d));
                _mm_storeu_si128(
                    uvp.add(uv_row + bx * 16) as *mut __m128i,
                    _mm_unpacklo_epi8(cb, cr),
                );
            }
        }
        let done = blocks * 16;
        if done < width {
            convert_bands_cols(src, width, yplane, uvplane, done, width);
        }
    }
}

/// NV12 -> BGRA, the receiver's present path when no GPU texture is in play.
///
/// The coefficients are the BT.601 limited-range inverse scaled by 256, and
/// `1.164 * 256 = 298` is the part that is easy to forget: using 256 here makes
/// every reconstructed frame visibly dark.
pub fn nv12_to_bgra(y: &[u8], uv: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    for row in 0..height {
        for col in 0..width {
            let yy = y[row * width + col] as i32 - 16;
            let uv_idx = (row / 2) * width + (col / 2) * 2;
            let cb = uv.get(uv_idx).copied().unwrap_or(128) as i32 - 128;
            let cr = uv.get(uv_idx + 1).copied().unwrap_or(128) as i32 - 128;
            let o = (row * width + col) * 4;
            dst[o] = ((298 * yy + 516 * cb + 128) >> 8).clamp(0, 255) as u8;
            dst[o + 1] = ((298 * yy - 100 * cb - 213 * cr + 128) >> 8).clamp(0, 255) as u8;
            dst[o + 2] = ((298 * yy + 409 * cr + 128) >> 8).clamp(0, 255) as u8;
            dst[o + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_helpers_read_real_start_codes() {
        let idr = [0u8, 0, 0, 1, 5, 0xAA, 0xBB];
        assert!(has_start_code(&idr));
        assert!(is_keyframe_annexb(&idr));
        let non_idr = [0u8, 0, 0, 1, 1, 0xAA];
        assert!(!is_keyframe_annexb(&non_idr));
        assert_eq!(nal_types(&idr), vec![5]);
        let three_byte = [0u8, 0, 1, 7, 0];
        assert!(is_keyframe_annexb(&three_byte));
    }

    #[test]
    fn sps_reference_count_is_read_from_the_bitstream() {
        // Baseline SPS, hand-assembled: profile 66, constraint flags, level 41, then
        // seq_parameter_set_id / log2_max_frame_num_minus4 / pic_order_cnt_type /
        // log2_max_pic_order_cnt_lsb_minus4 all zero (four `1` bits) and
        // num_ref_frames = 3, whose Exp-Golomb code is `00100`.
        // 1 1 1 1 00100 padded -> 11110010 = F2.
        let sps = [0u8, 0, 0, 1, 0x67, 66, 0xF0, 41, 0xF2, 0x00, 0x00];
        assert_eq!(sps_profile_level(&sps), Some((66, 41)));
        assert_eq!(sps_num_ref_frames(&sps), Some(3));
        // The same tail as it goes on the wire: an encoder escapes a third zero with
        // 0x03, and a parser that ignored the escape would read eight bits off.
        let escaped = [0u8, 0, 0, 1, 0x67, 66, 0xF0, 41, 0xF2, 0x00, 0x00, 0x03, 0x00];
        assert_eq!(sps_num_ref_frames(&escaped), Some(3));
    }

    #[test]
    fn band_starts_are_even_and_cover_the_frame() {
        for height in [1usize, 2, 3, 100, 1080, 1081] {
            for threads in [1usize, 2, 3, 4, 8, 16] {
                let b = band_starts(height, threads);
                assert_eq!(b.first().copied(), Some(0));
                assert_eq!(b.last().copied(), Some(height));
                for w in b.windows(2) {
                    assert!(w[0] % 2 == 0, "band start must be a even row");
                    assert!(w[1] > w[0]);
                }
                assert_eq!(*b.last().unwrap(), height);
            }
        }
    }

    #[test]
    fn conversion_matches_a_reference_at_known_colours() {
        // Pure white BGRA -> Y 235, Cb/Cr 128; pure black -> Y 16.
        let w = 2usize;
        let h = 2usize;
        let mut src = vec![0u8; w * h * 4];
        for p in src.chunks_exact_mut(4) {
            p.copy_from_slice(&[255, 255, 255, 255]);
        }
        let mut dst = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&src, w, h, &mut dst, 1);
        assert_eq!(dst[..4], [235, 235, 235, 235]);
        // Integer BT.601 Cb of pure white lands on 127, not the nominal 128;
        // pinning the exact value here documents that the rounding is ours.
        assert_eq!(dst[4], 127);
        assert_eq!(dst[5], 128);

        for p in src.chunks_exact_mut(4) {
            p.copy_from_slice(&[0, 0, 0, 255]);
        }
        bgra_to_nv12(&src, w, h, &mut dst, 1);
        assert_eq!(dst[..4], [16, 16, 16, 16]);
    }

    #[test]
    fn round_trip_is_within_band() {
        let (w, h) = (4usize, 2usize);
        let mut src = vec![0u8; w * h * 4];
        for (i, p) in src.chunks_exact_mut(4).enumerate() {
            let v = (i as u8) * 20;
            p.copy_from_slice(&[v, 255 - v, v / 2, 255]);
        }
        let mut nv = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&src, w, h, &mut nv, 2);
        let mut back = vec![0u8; w * h * 4];
        nv12_to_bgra(&nv[..w * h], &nv[w * h..], w, h, &mut back);
        for i in 0..w * h {
            // 4:2:0 chroma subsampling is lossy by construction; the check is
            // that luminance survived, which is where detail lives.
            let (a, b) = (
                luma_cb_cr(&src[i * 4..]).0,
                luma_cb_cr(&back[i * 4..]).0,
            );
            assert!(a.abs_diff(b) <= 3, "pixel {i}: {a} vs {b}");
        }
    }

    #[test]
    fn pack2_matches_mf_packing() {
        assert_eq!(pack2(1920, 1080), (1920u64 << 32) | 1080);
    }

    /// The vector kernel is only believable next to the scalar reference, and the
    /// width is deliberately `16 * 2 + 2` so the tail path is covered too.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sse2_matches_scalar_within_rounding() {
        let (w, h) = (34usize, 8usize);
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                src[o] = (x * 7 + y * 3) as u8;
                src[o + 1] = 255 - ((x * 13) % 256) as u8;
                src[o + 2] = (x * x + y * 31) as u8;
                src[o + 3] = 255;
            }
        }
        let mut v = vec![0u8; w * h * 3 / 2];
        let mut s = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&src, w, h, &mut v, 1);
        bgra_to_nv12_scalar(&src, w, h, &mut s);

        let mut worst_luma = 0u8;
        for i in 0..w * h {
            worst_luma = worst_luma.max(v[i].abs_diff(s[i]));
        }
        assert!(worst_luma <= 1, "luma differs by {worst_luma}");

        let mut worst_chroma = 0u8;
        for i in w * h..v.len() {
            worst_chroma = worst_chroma.max(v[i].abs_diff(s[i]));
        }
        // Chroma is averaged in a different order: the scalar path averages four
        // full-precision dot products, the vector path dot-products four averaged
        // samples. That is worth at most a couple of codes.
        assert!(worst_chroma <= 3, "chroma differs by {worst_chroma}");
    }
}
