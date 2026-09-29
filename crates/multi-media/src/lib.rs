//! MULTI's media pipeline (GStreamer), ported from M0 spikes S2, S2b and S6.
//!
//! ```text
//! input (SRT/UDP/RTP, MPEG-TS) -> tsdemux -> bridge -> h26xparse -> [caption lanes] -> h26xccinserter -> mpegtsmux -> outputs (SRT/UDP/RTMP)
//!   (streams entering mpegtsmux also feed RTMP (flvmux) and HLS (hlssink2 + WebVTT, hls.rs))
//!                                    `-> audio tap: avdec_aac -> 16 kHz mono i16 -> AudioCallback
//! ```
//!
//! Video is never decoded. The input pipeline is rebuilt on errors, EOS or
//! silence; the bridge re-stamps everything onto one continuous output
//! timeline, so a source restarting at PTS 0 is seamless downstream. Each
//! output is its own pipeline, restarted on its own with backoff.
//!
//! [`Media::start`] runs it on a control thread; [`Media::captions`] pushes
//! text into the caption lanes; [`Media::stats`] snapshots the counters.
//! Nothing here panics or blocks a streaming thread on caption work.

mod bridge;
mod captions;
mod fallback;
mod gstcc;
pub mod hls;
mod input;
mod output;
mod stats;
pub mod url;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use multi_core::Config;
use multi_core::config::{Captions, FallbackMode, Language};
use tracing::{error, info, warn};

pub use captions::{CaptionHandle, LaneSpec, lane_specs};
pub use output::{Codec, OutputSpec};
pub use stats::{LaneStats, OutputStats, SILENCE_DBFS, Stats, level_dbfs};

use crate::bridge::Bridge;
use crate::captions::Captioner;
use crate::output::{OutputManager, OutputSet};
use crate::stats::{Counters, LaneCounters, get, inc};

/// 16 kHz mono PCM from the audio tap. `start_ms` is the input audio PTS of
/// the first sample (the audio stream's own timeline; it jumps when the source
/// restarts).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioChunk {
    pub start_ms: u64,
    pub samples: Vec<i16>,
}

/// Receives tap audio on a GStreamer streaming thread. Must not block.
pub type AudioCallback = Arc<dyn Fn(AudioChunk) + Send + Sync>;

/// What the media pipeline needs from the configuration.
#[derive(Clone, Debug)]
pub struct MediaConfig {
    pub input_url: String,
    pub outputs: Vec<OutputSpec>,
    pub srt_latency_ms: u32,
    pub languages: Vec<Language>,
    pub captions: Captions,
    /// Audio track (0 = first) for the ASR tap. The output always carries the first.
    pub audio_track: u32,
    /// Channel to transcribe; `None` downmixes.
    pub audio_channel: Option<u32>,
    /// Restart the input when data stops for this long (after it has flowed).
    pub watchdog: Duration,
    /// Picture sent while the input is gone.
    pub fallback: FallbackMode,
    pub fallback_image: Option<std::path::PathBuf>,
    /// Input silence before the fallback starts.
    pub fallback_after: Duration,
}

impl MediaConfig {
    pub fn from_config(c: &Config) -> Self {
        Self {
            input_url: c.input.url.clone(),
            outputs: c
                .outputs
                .iter()
                .map(|o| OutputSpec {
                    url: o.url.clone(),
                    name: o.name.clone(),
                    enabled: o.enabled,
                })
                .collect(),
            srt_latency_ms: c.srt.latency_ms,
            languages: c.languages.clone(),
            captions: c.captions.clone(),
            audio_track: c.audio.track,
            audio_channel: c.audio.channel,
            watchdog: Duration::from_secs(2),
            fallback: c.video.fallback,
            fallback_image: c.video.fallback_image.clone(),
            fallback_after: Duration::from_millis(u64::from(c.video.fallback_after_ms)),
        }
    }
}

pub(crate) fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Initialises GStreamer and checks that every element this configuration
/// needs is installed, naming the missing ones.
pub fn check_elements(cfg: &MediaConfig) -> Result<()> {
    gst::init().context("GStreamer failed to initialise")?;
    let mut need = vec![
        "tsdemux",
        "h264parse",
        "h265parse",
        "h264ccinserter",
        "h265ccinserter",
        "mpegtsmux",
        "tttocea708",
        "tttocea608",
        "cea708mux",
        "ccconverter",
        "capssetter",
        "appsrc",
        "appsink",
        "queue",
        "tee",
        "aacparse",
        "mpegaudioparse",
        "ac3parse",
        "audioconvert",
        "audioresample",
        "fakesink",
        "capsfilter",
    ];
    let mut schemes = vec![url::input_kind(&cfg.input_url).map(|k| format!("{k:?}"))?];
    for o in &cfg.outputs {
        schemes.push(format!("out-{:?}", url::output_kind(&o.url)?));
    }
    for s in &schemes {
        match s.as_str() {
            "Srt" => need.push("srtsrc"),
            "Udp" => need.push("udpsrc"),
            "Rtp" => need.extend(["udpsrc", "rtpmp2tdepay"]),
            "out-Srt" => need.push("srtsink"),
            "out-Udp" => need.push("udpsink"),
            "out-Rtmp" => need.extend(["flvmux", "rtmp2sink"]),
            "out-Hls" => need.push("hlssink2"),
            _ => {}
        }
    }
    let missing: Vec<&str> = need
        .into_iter()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    if gst::ElementFactory::find("avdec_aac").is_none() {
        bail!(
            "GStreamer element avdec_aac is missing: install gst-libav (the audio tap uses \
             FFmpeg's LGPL AAC decoder only)"
        );
    }
    if !missing.is_empty() {
        bail!("missing GStreamer elements: {}", missing.join(", "));
    }
    Ok(())
}

/// Checks that the elements an output URL needs are installed (for outputs
/// added while running; [`check_elements`] covers the ones at start).
fn check_output_elements(u: &str) -> Result<()> {
    let need: &[&str] = match url::output_kind(u)? {
        url::OutputKind::Srt => &["srtsink"],
        url::OutputKind::Udp => &["udpsink"],
        url::OutputKind::Rtmp => &["flvmux", "rtmp2sink", "h264parse", "aacparse"],
        url::OutputKind::Hls => &["hlssink2"],
    };
    let missing: Vec<&str> = need
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    if !missing.is_empty() {
        bail!("missing GStreamer elements: {}", missing.join(", "));
    }
    Ok(())
}

/// Live control of the outputs of a running [`Media`]: add, remove, start
/// and stop one output without touching the others. Cheap to clone. Ids are
/// stable while the pipeline runs; the outputs from [`MediaConfig`] get ids
/// `0..n` in order.
#[derive(Clone)]
pub struct OutputControl {
    set: Arc<OutputSet>,
}

impl OutputControl {
    /// Adds an output at the end (started at once if enabled) and returns its id.
    pub fn add(&self, spec: &OutputSpec) -> Result<u64> {
        check_output_elements(&spec.url)?;
        self.set.add(spec)
    }

    /// Removes an output, tearing its pipeline down. False if unknown.
    pub fn remove(&self, id: u64) -> bool {
        self.set.remove(id)
    }

    /// Starts or stops an output. False if unknown.
    pub fn set_enabled(&self, id: u64, on: bool) -> bool {
        self.set.set_enabled(id, on)
    }

    /// Renames an output. False if unknown.
    pub fn set_name(&self, id: u64, name: Option<String>) -> bool {
        self.set.set_name(id, name)
    }

    /// Orders the outputs (and their stats) as `ids`.
    pub fn arrange(&self, ids: &[u64]) {
        self.set.arrange(ids);
    }

    /// Current ids, in order.
    pub fn ids(&self) -> Vec<u64> {
        self.set.ids()
    }

    pub fn stats(&self) -> Vec<OutputStats> {
        self.set.stats()
    }
}

/// State shared by the control thread and streaming-thread callbacks.
pub(crate) struct Core {
    pub cfg: MediaConfig,
    pub counters: Arc<Counters>,
    pub bridge: Arc<Bridge>,
    pub output: OutputManager,
    pub audio: AudioCallback,
    /// Wall ns of the last buffer from the current input (0 = none yet).
    pub last_data: AtomicU64,
    /// Wall ns of the last input buffer from any input (0 = never); not
    /// reset when the input is rebuilt.
    pub last_media: AtomicU64,
    /// Fallback picture while the input is gone.
    pub fallback: fallback::Fallback,
    /// Input audio level, from the audio tap.
    pub level: stats::LevelMeter,
}

/// A running media pipeline. Dropping it stops it.
pub struct Media {
    core: Arc<Core>,
    captions: CaptionHandle,
    lane_counters: Arc<Vec<LaneCounters>>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Media {
    /// Checks elements, builds the caption lanes and the input, starts the
    /// outputs, and runs the control loop on its own thread. Errors here are
    /// configuration or installation problems; later failures are retried.
    pub fn start(cfg: MediaConfig, audio: AudioCallback) -> Result<Self> {
        check_elements(&cfg)?;
        let specs = lane_specs(&cfg.languages, &cfg.captions)?;
        let hls = Arc::new(hls::HlsCtx::new(&cfg.languages, &cfg.captions));
        let (captioner, captions) = Captioner::new(specs, &cfg.captions, &hls);
        let lane_counters = captioner.counters();
        let counters = Arc::new(Counters::default());
        let bridge = Arc::new(Bridge::new(counters.clone()));
        let output = OutputManager::new(
            bridge.clone(),
            captioner,
            counters.clone(),
            &cfg.outputs,
            cfg.srt_latency_ms,
            hls,
        )?;
        let fallback = fallback::Fallback::new(&cfg, counters.clone());
        let core = Arc::new(Core {
            cfg,
            counters,
            bridge,
            output,
            audio,
            last_data: AtomicU64::new(0),
            last_media: AtomicU64::new(0),
            fallback,
            level: stats::LevelMeter::default(),
        });
        core.output.sinks.poll();
        let first = input::build(&core)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (core, stop) = (core.clone(), stop.clone());
            std::thread::Builder::new()
                .name("media".into())
                .spawn(move || control(&core, &stop, first))?
        };
        info!(input = %url::redact(&core.cfg.input_url), outputs = core.cfg.outputs.len(), "media pipeline started");
        Ok(Self {
            core,
            captions,
            lane_counters,
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Handle for adding, removing, starting and stopping outputs live.
    pub fn outputs(&self) -> OutputControl {
        OutputControl {
            set: self.core.output.sinks.clone(),
        }
    }

    /// Handle for pushing caption text; cheap to clone.
    pub fn captions(&self) -> CaptionHandle {
        self.captions.clone()
    }

    pub fn stats(&self) -> Stats {
        let c = &self.core.counters;
        let last = self.core.last_data.load(Ordering::Relaxed);
        let (audio_rms_dbfs, audio_peak_dbfs, audio_silent_s) = self.core.level.snapshot(wall_ns());
        Stats {
            frames_in: get(&c.frames_in),
            frames_out: get(&c.frames_out),
            audio_in: get(&c.audio_in),
            sessions: get(&c.sessions),
            input_restarts: get(&c.input_restarts),
            input_errors: get(&c.input_errors),
            output_pipeline_errors: get(&c.output_pipeline_errors),
            bridge_drops: get(&c.bridge_drops),
            caption_frames: get(&c.caption_frames),
            caption_errors: get(&c.caption_errors),
            audio_chunks: get(&c.audio_chunks),
            audio_drops: get(&c.audio_drops),
            fallback_active: self.core.fallback.active(),
            fallback_activations: get(&c.fallback_activations),
            input_live: last > 0
                && wall_ns().saturating_sub(last) < self.core.cfg.watchdog.as_nanos() as u64,
            audio_rms_dbfs,
            audio_peak_dbfs,
            audio_silent_s,
            outputs: self.core.output.sinks.stats(),
            lanes: captions::lane_stats(self.captions.languages(), &self.lane_counters),
        }
    }

    /// Stops the input, the output pipeline and every output. Idempotent.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let t = self.thread.lock().ok().and_then(|mut t| t.take());
        if let Some(t) = t {
            let _ = t.join();
            info!("media pipeline stopped");
        }
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sleeps up to `d` in small steps; returns true if stopping.
fn nap(stop: &AtomicBool, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.load(Ordering::Acquire)
}

const INPUT_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
const INPUT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// The control loop: runs the input, rebuilds it on error, EOS or silence
/// (srtsrc as caller can go silent without an error, S2), and polls the
/// output side and every output.
fn control(core: &Arc<Core>, stop: &AtomicBool, first: input::Input) {
    let c = &core.counters;
    let mut backoff = INPUT_BACKOFF_INITIAL;
    let mut next = Some(first);
    let watchdog_ns = core.cfg.watchdog.as_nanos() as u64;
    'outer: while !stop.load(Ordering::Acquire) {
        // While the fallback feeds the bridge its session carries on; the
        // switch back to the input resets the bridge itself.
        if !core.fallback.active() {
            core.bridge.reset();
        }
        let inp = match next.take().map_or_else(|| input::build(core), Ok) {
            Ok(i) => i,
            Err(e) => {
                error!(err = %format!("{e:#}"), "input rebuild failed; retrying");
                inc(&c.input_errors);
                if nap(stop, backoff) {
                    break;
                }
                backoff = (backoff * 2).min(INPUT_BACKOFF_MAX);
                continue;
            }
        };
        let started = inp.pipeline.set_state(gst::State::Playing);
        if let Err(e) = &started {
            warn!(err = %e, "input failed to start");
            inc(&c.input_errors);
        }
        let in_bus = inp.pipeline.bus();
        let mut restart = started.is_err();
        while !restart {
            if stop.load(Ordering::Acquire) {
                let _ = inp.pipeline.set_state(gst::State::Null);
                break 'outer;
            }
            if let Some(bus) = &in_bus {
                while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            warn!(src = ?msg.src().map(|s| s.path_string()), err = %e.error(), dbg = ?e.debug(), "input error");
                            inc(&c.input_errors);
                            restart = true;
                        }
                        gst::MessageView::Eos(_) => {
                            warn!("input EOS");
                            restart = true;
                        }
                        gst::MessageView::Warning(w) => warn!(err = %w.error(), "input warning"),
                        _ => {}
                    }
                    if restart {
                        break;
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(100));
            }
            let last = core.last_data.load(Ordering::Relaxed);
            if !restart && last > 0 && wall_ns().saturating_sub(last) > watchdog_ns {
                warn!(
                    silent_ms = wall_ns().saturating_sub(last) / 1_000_000,
                    "input silent; restarting input"
                );
                restart = true;
            }
            if last > 0 {
                backoff = INPUT_BACKOFF_INITIAL;
            }
            core.output.poll();
            core.output.sinks.poll();
            core.fallback.tick(core);
        }
        let _ = inp.pipeline.set_state(gst::State::Null);
        core.last_data.store(0, Ordering::Relaxed);
        inc(&c.input_restarts);
        core.fallback.tick(core);
        if nap(stop, backoff) {
            break;
        }
        backoff = (backoff * 2).min(INPUT_BACKOFF_MAX);
    }
    core.fallback.shutdown();
    core.bridge.set_targets(None);
    core.output.stop();
}
