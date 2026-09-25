//! Library side of the S2 spike (reused by S6): input, bridge, output and
//! caption stages.

pub mod bridge;
pub mod captions;
pub mod gstcc;
pub mod input;
pub mod output;
pub mod stamps;

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CaptionMode {
    /// Pass-through only (baseline).
    None,
    /// (a) appsrc text -> tttocea708/tttocea608 -> cccombiner.
    Gst,
    /// (b) spikes/cc CcMux -> GstVideoCaptionMeta per frame, keyed by PTS.
    Ours,
    /// (c, S2b) tttocea708 driven per frame from the video probe, no cccombiner.
    GstDirect,
}
