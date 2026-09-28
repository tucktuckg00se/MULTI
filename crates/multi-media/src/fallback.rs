//! Fallback picture while the input is gone (WP11): after
//! `fallback_after` without input data, a generated stream (black or a
//! still image, plus AAC silence) feeds the bridge instead of the input, so
//! every output keeps receiving a valid stream.
//!
//! ```text
//! videotestsrc pattern=black | filesrc ! pngdec|jpegdec ! imagefreeze
//!   ! videoconvert ! videoscale ! I420 WxH@fps ! openh264enc|x265enc ! parse ! appsink -> bridge
//! audiotestsrc wave=silence ! audioconvert ! audioresample ! avenc_aac ! aacparse ! appsink -> bridge
//! ```
//!
//! The encoders match the last input's codec, size, frame rate and (H.264)
//! profile, read from the output pipeline's parser caps. The bridge opens a
//! new session without the startup hold, so output timestamps carry on from
//! the last input frame. When the input returns, its buffers are dropped
//! until the first video keyframe; then the bridge is reset (a normal new
//! input session) and the fallback pipeline is torn down by the control
//! thread. No format known (no input yet) means no fallback. A missing
//! encoder is logged once and the fallback stays off; a failing fallback
//! pipeline is torn down and retried later. Nothing here touches the input.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use multi_core::config::FallbackMode;
use tracing::{error, info, warn};

use crate::bridge::{AUDIO, Bridge, VIDEO};
use crate::captions::FALLBACK_FPS;
use crate::output::Codec;
use crate::stats::{Counters, inc};
use crate::{Core, MediaConfig, wall_ns};

/// Wait before building the fallback again after it failed.
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// Fallback video bitrate (bit/s): plenty for black or a slate.
const BITRATE: u32 = 2_000_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the control thread should do this tick.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Nothing,
    /// Input silent long enough: start the fallback.
    Start,
    /// The switch back to the input happened: tear the fallback down.
    Stop,
}

/// Switch timing. `last_media_ns` is the wall time of the last input
/// buffer (0 = never), `allowed` covers the mode, a known-broken fallback
/// and the retry wait.
pub(crate) fn step(
    active: bool,
    running: bool,
    last_media_ns: u64,
    now_ns: u64,
    after_ns: u64,
    allowed: bool,
) -> Step {
    if active {
        return Step::Nothing;
    }
    if running {
        return Step::Stop;
    }
    if allowed && last_media_ns > 0 && now_ns.saturating_sub(last_media_ns) >= after_ns {
        return Step::Start;
    }
    Step::Nothing
}

/// True if an Annex B access unit holds an IDR (H.264) or IRAP (HEVC) picture.
pub(crate) fn is_keyframe(codec: Codec, data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let h = data[i + 3];
            let key = match codec {
                Codec::H264 => h & 0x1f == 5,
                Codec::Hevc => (16..=21).contains(&((h >> 1) & 0x3f)),
            };
            if key {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

fn codec_of(caps: Option<&gst::CapsRef>) -> Option<Codec> {
    match caps?.structure(0)?.name().as_str() {
        "video/x-h264" => Some(Codec::H264),
        "video/x-h265" => Some(Codec::Hevc),
        _ => None,
    }
}

/// Decides which source feeds the bridge. `active` mirrors `codec.is_some()`
/// for a lock-free check on the input's hot path.
pub(crate) struct Gate {
    active: AtomicBool,
    /// Codec of the running fallback (`Some` = fallback feeds the bridge).
    codec: Mutex<Option<Codec>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            codec: Mutex::new(None),
        }
    }

    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn set(&self, g: &mut Option<Codec>, c: Option<Codec>) {
        *g = c;
        self.active.store(c.is_some(), Ordering::Release);
    }

    /// An input sample. While the fallback runs, input is dropped until a
    /// video keyframe, which switches back to pass-through.
    pub fn push_input(&self, bridge: &Bridge, stream: usize, sample: &gst::Sample) {
        if !self.active() {
            bridge.push(stream, sample);
            return;
        }
        let mut g = lock(&self.codec);
        if g.is_some() {
            let key = stream == VIDEO
                && codec_of(sample.caps()).is_some_and(|c| {
                    sample
                        .buffer()
                        .and_then(|b| b.map_readable().ok())
                        .is_some_and(|m| is_keyframe(c, &m))
                });
            if !key {
                return;
            }
            self.set(&mut g, None);
            bridge.reset();
            info!("input back: switching to pass-through at its keyframe");
        }
        bridge.push(stream, sample);
    }

    /// A fallback sample; dropped unless the fallback is the active source.
    fn push_fallback(&self, bridge: &Bridge, stream: usize, sample: &gst::Sample) {
        let g = lock(&self.codec);
        if g.is_some()
            && let Some(sample) = running_time_stamps(sample)
        {
            bridge.push(stream, &sample);
        }
    }

    /// The input (re)appeared with `codec`: a fallback of another codec
    /// must stop feeding before the output pipeline is rebuilt for it.
    pub fn input_codec(&self, codec: Codec) {
        let mut g = lock(&self.codec);
        if g.is_some_and(|c| c != codec) {
            self.set(&mut g, None);
            info!(?codec, "input codec changed; fallback stopped");
        }
    }

    fn deactivate(&self) {
        let mut g = lock(&self.codec);
        self.set(&mut g, None);
    }
}

/// Re-stamps a fallback sample with its running time, so audio and video
/// share one timeline whatever offset an encoder adds (x265enc starts its
/// PTS at 1000 h to keep DTS positive).
fn running_time_stamps(sample: &gst::Sample) -> Option<gst::Sample> {
    let seg = sample
        .segment()?
        .downcast_ref::<gst::format::Time>()?
        .clone();
    let mut buf = sample.buffer_owned()?;
    let pts = seg.to_running_time(buf.pts()?)?;
    let dts = buf
        .dts()
        .and_then(|d| seg.to_running_time(d))
        .map(|d| d.min(pts));
    {
        let b = buf.make_mut();
        b.set_pts(pts);
        b.set_dts(dts.or(Some(pts)));
    }
    let caps = sample.caps_owned();
    let mut out = gst::Sample::builder().buffer(&buf);
    if let Some(caps) = &caps {
        out = out.caps(caps);
    }
    Some(out.build())
}

struct Ctl {
    pipeline: Option<gst::Pipeline>,
    /// Missing elements: logged once, never retried.
    broken: bool,
    retry_at: Option<Instant>,
    /// The image failed once: logged once, black is used instead.
    image_warned: bool,
}

pub(crate) struct Fallback {
    mode: FallbackMode,
    image: Option<PathBuf>,
    after: Duration,
    pub gate: Arc<Gate>,
    ctl: Mutex<Ctl>,
    counters: Arc<Counters>,
}

/// Stream format of the last input, as the output pipeline parsed it.
pub(crate) struct Format {
    pub codec: Codec,
    pub video: gst::Caps,
    pub audio: Option<gst::Caps>,
}

impl Fallback {
    pub fn new(cfg: &MediaConfig, counters: Arc<Counters>) -> Self {
        Self {
            mode: cfg.fallback,
            image: cfg.fallback_image.clone(),
            after: cfg.fallback_after,
            gate: Arc::new(Gate::new()),
            ctl: Mutex::new(Ctl {
                pipeline: None,
                broken: false,
                retry_at: None,
                image_warned: false,
            }),
            counters,
        }
    }

    pub fn active(&self) -> bool {
        self.gate.active()
    }

    /// Called by the control thread about every 100 ms.
    pub fn tick(&self, core: &Core) {
        if self.mode == FallbackMode::Off {
            return;
        }
        let mut ctl = lock(&self.ctl);
        if let Some(p) = &ctl.pipeline
            && let Some(bus) = p.bus()
        {
            let mut failed = false;
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        error!(src = ?msg.src().map(|s| s.path_string()), err = %e.error(), dbg = ?e.debug(), "fallback pipeline error; retrying later");
                        failed = true;
                    }
                    gst::MessageView::Warning(w) => {
                        warn!(err = %w.error(), "fallback pipeline warning")
                    }
                    _ => {}
                }
            }
            if failed {
                self.gate.deactivate();
                teardown(&mut ctl);
                ctl.retry_at = Some(Instant::now() + RETRY_AFTER);
                return;
            }
        }
        let allowed = !ctl.broken && ctl.retry_at.is_none_or(|t| Instant::now() >= t);
        match step(
            self.active(),
            ctl.pipeline.is_some(),
            core.last_media.load(Ordering::Relaxed),
            wall_ns(),
            self.after.as_nanos() as u64,
            allowed,
        ) {
            Step::Nothing => {}
            Step::Stop => {
                teardown(&mut ctl);
                info!("fallback stopped");
            }
            Step::Start => self.start(core, &mut ctl),
        }
    }

    fn start(&self, core: &Core, ctl: &mut Ctl) {
        let Some(fmt) = core.output.format() else {
            // No input format known yet: behave as without a fallback.
            return;
        };
        if let Err(e) = check(fmt.codec) {
            error!(err = %format!("{e:#}"), "fallback picture disabled");
            ctl.broken = true;
            return;
        }
        let image = match (self.mode, &self.image) {
            (FallbackMode::Image, Some(p)) => match image_decoder(p) {
                Ok(dec) => Some((p.as_path(), dec)),
                Err(e) => {
                    if !ctl.image_warned {
                        ctl.image_warned = true;
                        error!(err = %format!("{e:#}"), "fallback image unusable; sending black");
                    }
                    None
                }
            },
            _ => None,
        };
        let p = match build(&fmt, image, &self.gate, &core.bridge) {
            Ok(p) => p,
            Err(e) => {
                error!(err = %format!("{e:#}"), "fallback pipeline build failed; retrying later");
                ctl.retry_at = Some(Instant::now() + RETRY_AFTER);
                return;
            }
        };
        {
            let mut g = lock(&self.gate.codec);
            core.bridge.reset_without_hold();
            self.gate.set(&mut g, Some(fmt.codec));
        }
        if let Err(e) = p.set_state(gst::State::Playing) {
            error!(err = %e, "fallback pipeline failed to start; retrying later");
            self.gate.deactivate();
            let _ = p.set_state(gst::State::Null);
            ctl.retry_at = Some(Instant::now() + RETRY_AFTER);
            return;
        }
        ctl.pipeline = Some(p);
        ctl.retry_at = None;
        inc(&self.counters.fallback_activations);
        let silent_ms =
            wall_ns().saturating_sub(core.last_media.load(Ordering::Relaxed)) / 1_000_000;
        info!(silent_ms, codec = ?fmt.codec, caps = %fmt.video, "input gone: sending the fallback picture");
    }

    /// Stops feeding and tears the fallback down (media shutdown).
    pub fn shutdown(&self) {
        self.gate.deactivate();
        teardown(&mut lock(&self.ctl));
    }
}

fn teardown(ctl: &mut Ctl) {
    if let Some(p) = ctl.pipeline.take() {
        let _ = p.set_state(gst::State::Null);
    }
}

fn encoder_for(codec: Codec) -> (&'static str, &'static str) {
    match codec {
        // openh264 (BSD, gst-plugins-bad): x264enc is GPL and in -ugly.
        Codec::H264 => ("openh264enc", "h264parse"),
        Codec::Hevc => ("x265enc", "h265parse"),
    }
}

/// Every element the fallback needs for `codec` (the image decoders are
/// optional: black is used without them).
fn check(codec: Codec) -> Result<()> {
    let (enc, parse) = encoder_for(codec);
    let need = [
        enc,
        parse,
        "videotestsrc",
        "videoconvert",
        "videoscale",
        "audiotestsrc",
        "audioconvert",
        "audioresample",
        "avenc_aac",
        "aacparse",
        "capsfilter",
        "appsink",
    ];
    let missing: Vec<&str> = need
        .into_iter()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing GStreamer elements {} (openh264enc and x265enc are in gst-plugins-bad, \
             avenc_aac in gst-libav); outputs stay idle while the input is gone",
            missing.join(", ")
        );
    }
    Ok(())
}

/// `pngdec` or `jpegdec`, from the file's first bytes.
fn image_decoder(path: &Path) -> Result<&'static str> {
    use std::io::Read;
    let mut head = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .with_context(|| format!("cannot read {}", path.display()))?;
    let dec = match head {
        [0x89, b'P', b'N', b'G'] => "pngdec",
        [0xff, 0xd8, ..] => "jpegdec",
        _ => bail!("{} is not a PNG or JPEG file", path.display()),
    };
    if gst::ElementFactory::find(dec).is_none()
        || gst::ElementFactory::find("imagefreeze").is_none()
    {
        bail!("{dec} or imagefreeze missing (gst-plugins-good)");
    }
    Ok(dec)
}

fn el(f: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(f)
        .build()
        .with_context(|| format!("missing GStreamer element {f}"))
}

fn capsfilter(caps: &gst::Caps) -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()?)
}

fn appsink(gate: &Arc<Gate>, bridge: &Arc<Bridge>, stream: usize) -> gst_app::AppSink {
    let s = gst_app::AppSink::builder()
        .sync(false)
        .async_(false)
        .max_buffers(30)
        .drop(true)
        .build();
    let (gate, bridge) = (gate.clone(), bridge.clone());
    s.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let Ok(sample) = s.pull_sample() else {
                    return Err(gst::FlowError::Eos);
                };
                gate.push_fallback(&bridge, stream, &sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    s
}

/// Encoder parameters from the last input's parsed video caps.
struct VideoParams {
    width: i32,
    height: i32,
    fps: gst::Fraction,
    profile: Option<String>,
}

fn video_params(caps: &gst::Caps) -> Result<VideoParams> {
    let s = caps.structure(0).context("empty video caps")?;
    let width = s.get::<i32>("width").context("no width in video caps")?;
    let height = s.get::<i32>("height").context("no height in video caps")?;
    let fps = s
        .get::<gst::Fraction>("framerate")
        .ok()
        .filter(|f| f.numer() > 0 && f.denom() > 0)
        .unwrap_or_else(|| gst::Fraction::new(FALLBACK_FPS.0, FALLBACK_FPS.1));
    Ok(VideoParams {
        width,
        height,
        fps,
        profile: s.get::<String>("profile").ok(),
    })
}

/// Keyframe interval: one second of frames.
fn keyint(fps: gst::Fraction) -> u32 {
    let n = (f64::from(fps.numer()) / f64::from(fps.denom())).round();
    (n as u32).max(1)
}

fn build(
    fmt: &Format,
    image: Option<(&Path, &str)>,
    gate: &Arc<Gate>,
    bridge: &Arc<Bridge>,
) -> Result<gst::Pipeline> {
    let vp = video_params(&fmt.video)?;
    let p = gst::Pipeline::with_name("fallback");
    let mut v: Vec<gst::Element> = match image {
        Some((path, dec)) => {
            let src = el("filesrc")?;
            src.set_property("location", path.to_string_lossy().as_ref());
            let freeze = el("imagefreeze")?;
            freeze.set_property("is-live", true);
            vec![src, el(dec)?, el("videoconvert")?, freeze]
        }
        None => {
            let src = el("videotestsrc")?;
            src.set_property("is-live", true);
            src.set_property_from_str("pattern", "black");
            vec![src]
        }
    };
    v.push(el("videoconvert")?);
    v.push(el("videoscale")?);
    v.push(capsfilter(
        &gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", vp.width)
            .field("height", vp.height)
            .field("framerate", vp.fps)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build(),
    )?);
    let k = keyint(vp.fps);
    let (enc_f, parse_f) = encoder_for(fmt.codec);
    let enc = el(enc_f)?;
    let mut out = match fmt.codec {
        Codec::H264 => {
            enc.set_property("gop-size", k);
            enc.set_property("bitrate", BITRATE);
            enc.set_property_from_str("complexity", "low");
            gst::Caps::builder("video/x-h264")
        }
        Codec::Hevc => {
            enc.set_property_from_str("tune", "zerolatency");
            enc.set_property_from_str("speed-preset", "ultrafast");
            enc.set_property("key-int-max", k as i32);
            enc.set_property("bitrate", BITRATE / 1000);
            enc.set_property(
                "option-string",
                format!("bframes=0:keyint={k}:min-keyint={k}:scenecut=0:repeat-headers=1"),
            );
            gst::Caps::builder("video/x-h265")
        }
    };
    v.push(enc);
    // openh264 can signal these profiles; others keep its default.
    if fmt.codec == Codec::H264
        && let Some(prof) = vp.profile.as_deref().filter(|p| {
            matches!(
                *p,
                "constrained-baseline" | "baseline" | "main" | "high" | "constrained-high"
            )
        })
    {
        v.push(capsfilter(
            &gst::Caps::builder("video/x-h264")
                .field("profile", prof)
                .build(),
        )?);
    }
    v.push(el(parse_f)?);
    out = out
        .field("stream-format", "byte-stream")
        .field("alignment", "au");
    v.push(capsfilter(&out.build())?);
    v.push(appsink(gate, bridge, VIDEO).upcast());
    p.add_many(v.iter())?;
    gst::Element::link_many(v.iter())?;

    // Silence in the input's AAC format. Other audio codecs get no audio:
    // switching the muxer's audio stream type mid-stream is not safe.
    let audio = fmt.audio.as_ref().and_then(|c| {
        let s = c.structure(0)?;
        let aac =
            s.name() == "audio/mpeg" && matches!(s.get::<i32>("mpegversion").ok(), Some(2 | 4));
        aac.then(|| (s.get::<i32>("rate").ok(), s.get::<i32>("channels").ok()))
    });
    match audio {
        Some((Some(rate), Some(channels))) => {
            let src = el("audiotestsrc")?;
            src.set_property("is-live", true);
            src.set_property_from_str("wave", "silence");
            src.set_property("samplesperbuffer", 1024i32);
            let a = [
                src,
                el("audioconvert")?,
                el("audioresample")?,
                capsfilter(
                    &gst::Caps::builder("audio/x-raw")
                        .field("rate", rate)
                        .field("channels", channels)
                        .build(),
                )?,
                // FFmpeg's native AAC encoder (LGPL); never fdk-aac.
                el("avenc_aac")?,
                el("aacparse")?,
                capsfilter(
                    &gst::Caps::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "adts")
                        .build(),
                )?,
                appsink(gate, bridge, AUDIO).upcast(),
            ];
            p.add_many(a.iter())?;
            gst::Element::link_many(a.iter())?;
        }
        _ => info!("input audio is not AAC (or unknown); the fallback sends video only"),
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn switch_timing() {
        let after = 1000 * MS;
        let t0 = 50_000 * MS;
        // Never saw input: never starts.
        assert_eq!(step(false, false, 0, t0, after, true), Step::Nothing);
        // Input silent for less than the timeout.
        assert_eq!(
            step(false, false, t0, t0 + 999 * MS, after, true),
            Step::Nothing
        );
        assert_eq!(
            step(false, false, t0, t0 + 1000 * MS, after, true),
            Step::Start
        );
        // Mode off, broken or waiting to retry.
        assert_eq!(
            step(false, false, t0, t0 + 5000 * MS, after, false),
            Step::Nothing
        );
        // Active: keeps running, whatever the input does (the switch back
        // happens at the input's keyframe, not here).
        assert_eq!(
            step(true, true, t0, t0 + 5000 * MS, after, true),
            Step::Nothing
        );
        assert_eq!(step(true, true, t0 + 10, t0, after, true), Step::Nothing);
        // Switched back: tear down, even though input is fresh.
        assert_eq!(step(false, true, t0, t0, after, true), Step::Stop);
        // Clock going backwards is not "silent".
        assert_eq!(step(false, false, t0, t0 - MS, after, true), Step::Nothing);
    }

    #[test]
    fn keyframes_by_nal_type() {
        // H.264: SPS, PPS, IDR slice.
        let idr = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9];
        let p = [0, 0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x41, 9, 9];
        assert!(is_keyframe(Codec::H264, &idr));
        assert!(!is_keyframe(Codec::H264, &p));
        assert!(!is_keyframe(Codec::H264, &[0, 0, 1]));
        // HEVC: VPS (32), IDR_W_RADL (19); TRAIL_R (1).
        let hidr = [0, 0, 1, 0x40, 1, 0, 0, 1, 19 << 1, 1];
        let htrail = [0, 0, 1, 0x46, 1, 0, 0, 1, 1 << 1, 1];
        assert!(is_keyframe(Codec::Hevc, &hidr));
        assert!(!is_keyframe(Codec::Hevc, &htrail));
    }

    /// Builds the fallback pipeline and runs it for a moment: it must
    /// negotiate and produce encoded video and audio without bus errors.
    fn runs(codec: Codec, image: Option<(&Path, &str)>) -> Result<()> {
        gst::init()?;
        let caps = gst::Caps::builder(match codec {
            Codec::H264 => "video/x-h264",
            Codec::Hevc => "video/x-h265",
        })
        .field("width", 320i32)
        .field("height", 180i32)
        .field("framerate", gst::Fraction::new(25, 1))
        .field("profile", "high")
        .build();
        let audio = gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("rate", 44_100i32)
            .field("channels", 1i32)
            .build();
        let fmt = Format {
            codec,
            video: caps,
            audio: Some(audio),
        };
        let gate = Arc::new(Gate::new());
        let bridge = Arc::new(Bridge::new(Arc::new(Counters::default())));
        let p = build(&fmt, image, &gate, &bridge)?;
        p.set_state(gst::State::Playing)?;
        let bus = p.bus().context("bus")?;
        let end = Instant::now() + Duration::from_millis(1500);
        let mut err = None;
        while Instant::now() < end && err.is_none() {
            if let Some(m) = bus.timed_pop(gst::ClockTime::from_mseconds(50))
                && let gst::MessageView::Error(e) = m.view()
            {
                err = Some(format!("{} {:?}", e.error(), e.debug()));
            }
        }
        let v = p
            .iterate_sinks()
            .into_iter()
            .flatten()
            .filter_map(|e| e.static_pad("sink")?.current_caps())
            .map(|c| c.to_string())
            .collect::<Vec<_>>();
        p.set_state(gst::State::Null)?;
        assert_eq!(err, None);
        assert_eq!(v.len(), 2, "sink caps {v:?}");
        assert!(v.iter().any(|c| c.contains("width=(int)320")), "{v:?}");
        assert!(v.iter().any(|c| c.contains("rate=(int)44100")), "{v:?}");
        Ok(())
    }

    #[test]
    fn black_h264_and_hevc_negotiate() -> Result<()> {
        runs(Codec::H264, None)?;
        runs(Codec::Hevc, None)
    }

    #[test]
    fn image_slate_negotiates() -> Result<()> {
        gst::init()?;
        let dir = std::env::temp_dir().join(format!("multi-fb-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        // A small PNG written by GStreamer itself.
        let png = dir.join("slate.png");
        let enc = gst::parse::launch(&format!(
            "videotestsrc num-buffers=1 ! video/x-raw,width=64,height=48 ! videoconvert ! pngenc ! filesink location={}",
            png.display()
        ))?;
        enc.set_state(gst::State::Playing)?;
        let _ = enc
            .bus()
            .context("bus")?
            .timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos]);
        enc.set_state(gst::State::Null)?;
        assert_eq!(image_decoder(&png)?, "pngdec");
        runs(Codec::H264, Some((&png, "pngdec")))?;
        let bad = dir.join("x.png");
        std::fs::write(&bad, b"GIF89a")?;
        assert!(image_decoder(&bad).is_err());
        assert!(image_decoder(&dir.join("missing.png")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn keyint_is_one_second() {
        assert_eq!(keyint(gst::Fraction::new(30, 1)), 30);
        assert_eq!(keyint(gst::Fraction::new(30000, 1001)), 30);
        assert_eq!(keyint(gst::Fraction::new(1, 2)), 1);
    }

    #[test]
    fn video_params_from_parser_caps() -> Result<()> {
        gst::init()?;
        let c = gst::Caps::builder("video/x-h264")
            .field("width", 1280i32)
            .field("height", 720i32)
            .field("profile", "high")
            .build();
        let vp = video_params(&c)?;
        assert_eq!((vp.width, vp.height), (1280, 720));
        assert_eq!(vp.fps, gst::Fraction::new(30, 1));
        assert_eq!(vp.profile.as_deref(), Some("high"));
        assert!(video_params(&gst::Caps::builder("video/x-h264").build()).is_err());
        Ok(())
    }
}
