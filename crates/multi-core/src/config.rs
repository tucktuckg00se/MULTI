//! MULTI's configuration: one TOML file, every setting optional with the
//! defaults from PRD §5 (`docs/prd/05-configuration-and-tuning.md`).
//!
//! [`Config::validate`] reports every problem with the dotted path of the
//! setting, so the CLI and the web GUI can point at the exact field.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Top-level configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub input: Input,
    pub outputs: Vec<Output>,
    pub video: Video,
    pub captions: Captions,
    pub languages: Vec<Language>,
    pub asr: Asr,
    pub vad: Vad,
    pub translate: Translate,
    pub filter: Filter,
    pub audio: Audio,
    pub srt: Srt,
    pub gpu: Gpu,
    pub web: Web,
    pub degrade: Degrade,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            input: Input::default(),
            outputs: vec![Output::new("srt://0.0.0.0:9001?mode=listener")],
            video: Video::default(),
            captions: Captions::default(),
            languages: default_languages(),
            asr: Asr::default(),
            vad: Vad::default(),
            translate: Translate::default(),
            filter: Filter::default(),
            audio: Audio::default(),
            srt: Srt::default(),
            gpu: Gpu::default(),
            web: Web::default(),
            degrade: Degrade::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    /// `srt://`, `udp://` or `rtp://` URL.
    pub url: String,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            url: "srt://0.0.0.0:9000?mode=listener".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// `srt://`, `udp://`, `rtmp(s)://` or `hls://<name>` URL. An HLS
    /// output takes `?segment_s=2&window=6` (segment length in seconds,
    /// segments in the live playlist).
    pub url: String,
    /// Label shown in the GUI and logs (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A stopped output keeps its settings but sends nothing.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// HLS outputs only: `/watch/<name>` and `/hls/<name>/…` need no sign-in.
    /// Everything else (the GUI, the API, other outputs) still does.
    #[serde(default, skip_serializing_if = "is_false")]
    pub public: bool,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Longest HLS output name (`hls://<name>`).
pub const HLS_NAME_MAX: usize = 32;

/// Whether `name` is a valid HLS output name: 1–32 of `a-z 0-9 _ -`. It is
/// used as a directory name and in URLs.
pub fn hls_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= HLS_NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Longest output name accepted.
pub const OUTPUT_NAME_MAX: usize = 64;

impl Output {
    /// An enabled output with no name.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            name: None,
            enabled: true,
            public: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Video {
    /// Hold video back so captions line up with speech. 0 = pass-through.
    pub delay_ms: u32,
    /// What the outputs carry while the input is gone.
    pub fallback: FallbackMode,
    /// PNG or JPEG shown when `fallback = "image"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_image: Option<PathBuf>,
    /// Input silence before the fallback picture starts.
    pub fallback_after_ms: u32,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            delay_ms: 0,
            fallback: FallbackMode::Black,
            fallback_image: None,
            fallback_after_ms: 1000,
        }
    }
}

/// Fallback picture sent while the input is gone (with silent audio).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FallbackMode {
    /// Black frames.
    #[default]
    Black,
    /// `video.fallback_image`, scaled to the stream's size.
    Image,
    /// Send nothing (outputs stay connected but idle).
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptionMode {
    RollUp,
    PopOn,
    PaintOn,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Captions {
    pub offset_ms: i32,
    pub mode: CaptionMode,
    pub rows: u8,
    pub max_chars_per_line: u8,
    pub clear_after_ms: u32,
}

impl Default for Captions {
    fn default() -> Self {
        Self {
            offset_ms: 0,
            mode: CaptionMode::RollUp,
            rows: 3,
            max_chars_per_line: 32,
            clear_after_ms: 4000,
        }
    }
}

/// CEA-608 caption channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cc608 {
    Cc1,
    Cc2,
    Cc3,
    Cc4,
}

/// One caption language and where it is carried.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Language {
    /// ISO 639-1 code, e.g. `en`.
    pub code: String,
    /// The spoken language; exactly one language must be the source.
    #[serde(default)]
    pub source: bool,
    /// CEA-608 channel, if any.
    #[serde(default)]
    pub cc608: Option<Cc608>,
    /// CEA-708 service number (1–6), if any.
    #[serde(default)]
    pub cea708_service: Option<u8>,
    /// Shown as a WebVTT subtitle rendition on HLS outputs (any script).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub webvtt: bool,
    /// Lower numbers are kept longest when shedding load.
    #[serde(default)]
    pub priority: u8,
}

/// EN on CC1 + 708 service 1, ES on CC3 + service 2, FR and DE on 708 only
/// (the layout proven in M0 spike S6).
pub fn default_languages() -> Vec<Language> {
    let lang = |code: &str, source, cc608, svc, priority| Language {
        code: code.into(),
        source,
        cc608,
        cea708_service: Some(svc),
        webvtt: true,
        priority,
    };
    vec![
        lang("en", true, Some(Cc608::Cc1), 1, 0),
        lang("es", false, Some(Cc608::Cc3), 2, 1),
        lang("fr", false, None, 3, 2),
        lang("de", false, None, 4, 3),
    ]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Asr {
    pub model: String,
    pub chunk_ms: u32,
    pub stability_passes: u8,
}

impl Default for Asr {
    fn default() -> Self {
        Self {
            model: "nemotron-3.5-streaming".into(),
            chunk_ms: 560,
            stability_passes: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vad {
    pub threshold: f32,
}

impl Default for Vad {
    fn default() -> Self {
        Self { threshold: 0.5 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Segment {
    Word,
    Clause,
    Sentence,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Translate {
    pub segment: Segment,
    pub max_wait_ms: u32,
}

impl Default for Translate {
    fn default() -> Self {
        Self {
            segment: Segment::Clause,
            max_wait_ms: 800,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaskStyle {
    Asterisks,
    FirstLetter,
    Bleep,
    Drop,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    /// Built-in explicit-language filter, applied in every language.
    pub profanity: bool,
    pub mask_style: MaskStyle,
    /// Extra words to mask, in any caption language. `*` matches any letters.
    pub blocklist: Vec<String>,
    /// Words never masked, even if a list matches them.
    pub allowlist: Vec<String>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            profanity: true,
            mask_style: MaskStyle::FirstLetter,
            blocklist: Vec::new(),
            allowlist: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// Audio track index in the input (0 = first).
    pub track: u32,
    /// Channel to transcribe; `None` downmixes all channels.
    pub channel: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Srt {
    pub latency_ms: u32,
}

impl Default for Srt {
    fn default() -> Self {
        Self { latency_ms: 120 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gpu {
    /// CUDA device index; `None` picks automatically and falls back to CPU.
    pub device: Option<u32>,
    /// Upper limit on GPU memory for models; `None` means no limit.
    pub max_vram_mb: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Web {
    pub bind: IpAddr,
    pub port: u16,
    /// API token for scripts (`Authorization: Bearer <token>`). Prefer the
    /// `MULTI_WEB_TOKEN` environment variable over storing it here.
    pub token: Option<String>,
    /// `multi serve` starts the pipeline right away instead of waiting for Start.
    pub autostart: bool,
    /// Sign-in name for the web GUI.
    pub username: String,
    /// Argon2id PHC string (`$argon2id$...`), set by `multi passwd` or the
    /// first-run page. Never a plain-text password.
    pub password_hash: Option<String>,
    /// HTTPS: `auto` (on unless `bind` is a loopback address), `on` or `off`.
    pub tls: TlsMode,
    /// Own certificate chain (PEM); without it a self-signed one is made.
    pub tls_cert: Option<PathBuf>,
    /// Private key (PEM) for `tls_cert`.
    pub tls_key: Option<PathBuf>,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            bind: IpAddr::from([127, 0, 0, 1]),
            port: 8480,
            token: None,
            autostart: false,
            username: "admin".into(),
            password_hash: None,
            tls: TlsMode::Auto,
            tls_cert: None,
            tls_key: None,
        }
    }
}

impl Web {
    /// Whether the server speaks HTTPS.
    pub fn tls_enabled(&self) -> bool {
        match self.tls {
            TlsMode::On => true,
            TlsMode::Off => false,
            TlsMode::Auto => !self.bind.is_loopback(),
        }
    }

    /// Whether a password is set.
    pub fn has_password(&self) -> bool {
        self.password_hash.as_deref().is_some_and(|h| !h.is_empty())
    }
}

/// When the web GUI uses HTTPS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DegradeStep {
    Languages,
    Model,
    SourceOnly,
    PassThrough,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Degrade {
    pub max_lag_ms: u32,
    pub order: Vec<DegradeStep>,
}

impl Default for Degrade {
    fn default() -> Self {
        Self {
            max_lag_ms: 5000,
            order: vec![
                DegradeStep::Languages,
                DegradeStep::Model,
                DegradeStep::SourceOnly,
                DegradeStep::PassThrough,
            ],
        }
    }
}

/// Languages written in non-Latin scripts, which CEA-608 can't carry and
/// MULTI's 708 path doesn't support.
const NON_LATIN: &[&str] = &[
    "am", "ar", "be", "bg", "bn", "el", "fa", "gu", "he", "hi", "hy", "ja", "ka", "kk", "km", "kn",
    "ko", "lo", "mk", "ml", "mr", "my", "ne", "pa", "ru", "si", "sr", "ta", "te", "th", "uk", "ur",
    "yi", "zh",
];

/// Latin-script languages with letters outside CEA-608's character sets.
const LATIN_EXTENDED: &[&str] = &[
    "cs", "et", "hr", "hu", "lt", "lv", "pl", "ro", "sk", "sl", "tr", "vi",
];

/// A single validation problem, keyed by the setting's dotted path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

impl Config {
    /// Parses TOML text. Missing settings take their defaults; unknown keys are errors.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: shown.clone(),
            source,
        })?;
        Self::from_toml(&text).map_err(|source| LoadError::Parse {
            path: shown,
            source,
        })
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Checks ranges and cross-field rules. Empty means valid.
    pub fn validate(&self) -> Vec<Issue> {
        let mut v = Validator::default();

        v.url("input.url", &self.input.url, &["srt", "udp", "rtp"]);
        if self.outputs.is_empty() {
            v.push("outputs", "at least one output is required");
        }
        let mut hls_names = Vec::new();
        for (i, o) in self.outputs.iter().enumerate() {
            let at = format!("outputs[{i}].url");
            v.url(&at, &o.url, &["srt", "udp", "rtmp", "rtmps", "hls"]);
            if let Some(rest) = o.url.strip_prefix("hls://") {
                let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
                if !hls_name_ok(name) {
                    v.push(
                        &at,
                        &format!("HLS output name: 1–{HLS_NAME_MAX} of a-z, 0-9, _ and -"),
                    );
                } else if hls_names.contains(&name) {
                    v.push(&at, "another HLS output has this name");
                }
                hls_names.push(name);
                for kv in query.split('&').filter(|kv| !kv.is_empty()) {
                    let (k, val) = kv.split_once('=').unwrap_or((kv, ""));
                    let n: Option<u32> = val.parse().ok();
                    match (k, n) {
                        ("segment_s", Some(n)) if (1..=10).contains(&n) => {}
                        ("window", Some(n)) if (3..=30).contains(&n) => {}
                        ("segment_s", _) => v.push(&at, "segment_s must be 1–10 seconds"),
                        ("window", _) => v.push(&at, "window must be 3–30 segments"),
                        _ => v.push(&at, &format!("unknown HLS option {k}")),
                    }
                }
            } else if o.public {
                v.push(
                    &format!("outputs[{i}].public"),
                    "only HLS outputs can be public",
                );
            }
            if let Some(n) = &o.name
                && (n.chars().count() > OUTPUT_NAME_MAX || n.chars().any(char::is_control))
            {
                v.push(
                    &format!("outputs[{i}].name"),
                    &format!("at most {OUTPUT_NAME_MAX} characters, no control characters"),
                );
            }
        }

        v.range("video.delay_ms", self.video.delay_ms, 0, 10_000);
        v.range(
            "video.fallback_after_ms",
            self.video.fallback_after_ms,
            200,
            10_000,
        );
        if self.video.fallback == FallbackMode::Image {
            let ext = self
                .video
                .fallback_image
                .as_deref()
                .and_then(Path::extension)
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            match ext.as_deref() {
                None if self.video.fallback_image.is_none() => v.push(
                    "video.fallback_image",
                    "required when video.fallback = \"image\"",
                ),
                Some("png" | "jpg" | "jpeg") => {}
                _ => v.push("video.fallback_image", "must be a .png, .jpg or .jpeg file"),
            }
        }
        v.range("captions.offset_ms", self.captions.offset_ms, -5_000, 5_000);
        v.range("captions.rows", self.captions.rows, 1, 4);
        v.range(
            "captions.max_chars_per_line",
            self.captions.max_chars_per_line,
            20,
            42,
        );
        if self.captions.max_chars_per_line > 32 && self.languages.iter().any(|l| l.cc608.is_some())
        {
            v.push(
                "captions.max_chars_per_line",
                "CEA-608 allows at most 32 characters per line",
            );
        }
        v.range(
            "captions.clear_after_ms",
            self.captions.clear_after_ms,
            1_000,
            30_000,
        );

        self.validate_languages(&mut v);

        if self.asr.model.trim().is_empty() {
            v.push("asr.model", "a model name is required");
        }
        v.range("asr.chunk_ms", self.asr.chunk_ms, 160, 3_000);
        v.range("asr.stability_passes", self.asr.stability_passes, 1, 3);
        v.range("vad.threshold", self.vad.threshold, 0.1, 0.9);
        v.range(
            "translate.max_wait_ms",
            self.translate.max_wait_ms,
            200,
            3_000,
        );
        v.range("srt.latency_ms", self.srt.latency_ms, 20, 8_000);
        if self.web.port == 0 {
            v.push("web.port", "port must be between 1 and 65535");
        }
        if self.web.username.trim().is_empty() {
            v.push("web.username", "a user name is required");
        }
        if let Some(h) = self.web.password_hash.as_deref()
            && !h.is_empty()
            && !h.starts_with("$argon2id$")
        {
            v.push(
                "web.password_hash",
                "must be an argon2id hash: set the password with `multi passwd`",
            );
        }
        if self.web.tls_cert.is_some() != self.web.tls_key.is_some() {
            let path = if self.web.tls_cert.is_some() {
                "web.tls_key"
            } else {
                "web.tls_cert"
            };
            v.push(path, "set both web.tls_cert and web.tls_key, or neither");
        }
        for (name, list) in [
            ("blocklist", &self.filter.blocklist),
            ("allowlist", &self.filter.allowlist),
        ] {
            for (i, e) in list.iter().enumerate() {
                if let Some(msg) = crate::filter::entry_problem(e.trim()) {
                    v.push(&format!("filter.{name}[{i}]"), msg);
                }
            }
        }
        v.range("degrade.max_lag_ms", self.degrade.max_lag_ms, 1_000, 30_000);
        let mut seen = Vec::new();
        for step in &self.degrade.order {
            if seen.contains(step) {
                v.push("degrade.order", "each step may appear only once");
            }
            seen.push(*step);
        }
        v.issues
    }

    /// Advice that doesn't block saving or starting: settings that will work
    /// but probably not as the user expects.
    pub fn warnings(&self) -> Vec<Issue> {
        let mut out = Vec::new();
        let has_hls = self.outputs.iter().any(|o| o.url.starts_with("hls://"));
        for (i, l) in self.languages.iter().enumerate() {
            if l.cc608.is_none() && l.cea708_service.is_none() {
                if !has_hls {
                    out.push(Issue {
                        path: format!("languages[{i}].webvtt"),
                        message: "this language is carried only as WebVTT, which only an hls:// output shows; add one".into(),
                    });
                }
                continue;
            }
            let code = l.code.as_str();
            let message = if NON_LATIN.contains(&code) {
                "this language's script can't be carried in CEA-608/708 captions; carry it as WebVTT only (no 608 channel or 708 service) and add an hls:// output"
            } else if LATIN_EXTENDED.contains(&code) && l.cc608.is_some() {
                "CEA-608 can't show every letter of this language; some accented letters will be replaced with plain ones (708 is unaffected)"
            } else {
                continue;
            };
            out.push(Issue {
                path: format!("languages[{i}].code"),
                message: message.into(),
            });
        }
        out
    }

    fn validate_languages(&self, v: &mut Validator) {
        if self.languages.is_empty() {
            v.push("languages", "at least one language is required");
            return;
        }
        let sources = self.languages.iter().filter(|l| l.source).count();
        if sources != 1 {
            v.push(
                "languages",
                "exactly one language must be marked as the source",
            );
        }
        let mut codes = Vec::new();
        let mut channels = Vec::new();
        let mut services = Vec::new();
        for (i, l) in self.languages.iter().enumerate() {
            let at = |field: &str| format!("languages[{i}].{field}");
            let code_ok = l.code.len() == 2 && l.code.chars().all(|c| c.is_ascii_lowercase());
            if !code_ok {
                v.push(
                    &at("code"),
                    "use a two-letter lowercase ISO 639-1 code, e.g. \"en\"",
                );
            } else if codes.contains(&l.code) {
                v.push(&at("code"), "language listed twice");
            }
            codes.push(l.code.clone());
            if l.cc608.is_none() && l.cea708_service.is_none() && !l.webvtt {
                v.push(
                    &at("cc608"),
                    "carry the language on a 608 channel, a 708 service or WebVTT (at least one)",
                );
            }
            if let Some(ch) = l.cc608 {
                if matches!(ch, Cc608::Cc2 | Cc608::Cc4) {
                    v.push(
                        &at("cc608"),
                        "CC2 and CC4 are not supported: they share a field with CC1/CC3, and \
                         GStreamer's encoders write CC1/CC3 only; use a 708 service instead",
                    );
                }
                if channels.contains(&ch) {
                    v.push(&at("cc608"), "channel already used by another language");
                }
                channels.push(ch);
            }
            if let Some(svc) = l.cea708_service {
                if !(1..=6).contains(&svc) {
                    v.push(&at("cea708_service"), "708 service must be 1–6");
                } else if services.contains(&svc) {
                    v.push(
                        &at("cea708_service"),
                        "service already used by another language",
                    );
                }
                services.push(svc);
            }
        }
    }
}

#[derive(Default)]
struct Validator {
    issues: Vec<Issue>,
}

impl Validator {
    fn push(&mut self, path: &str, message: &str) {
        self.issues.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }

    fn range<T: PartialOrd + std::fmt::Display + Copy>(&mut self, path: &str, x: T, lo: T, hi: T) {
        if x < lo || x > hi {
            self.push(path, &format!("must be between {lo} and {hi} (is {x})"));
        }
    }

    fn url(&mut self, path: &str, url: &str, schemes: &[&str]) {
        match url.split_once("://") {
            Some((scheme, rest)) if schemes.contains(&scheme) && !rest.is_empty() => {}
            _ => self.push(
                path,
                &format!("expected a URL starting with {}://", schemes.join("://, ")),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_paths(c: &Config) -> Vec<String> {
        c.validate().into_iter().map(|i| i.path).collect()
    }

    #[test]
    fn defaults_are_valid() {
        assert_eq!(Config::default().validate(), Vec::new());
    }

    #[test]
    fn defaults_match_prd() {
        let c = Config::default();
        assert_eq!(c.asr.chunk_ms, 560);
        assert_eq!(c.translate.max_wait_ms, 800);
        assert_eq!(c.web.port, 8480);
        assert_eq!(c.srt.latency_ms, 120);
        assert_eq!(c.degrade.max_lag_ms, 5000);
        assert_eq!(c.captions.mode, CaptionMode::RollUp);
    }

    #[test]
    fn empty_file_gives_defaults() -> Result<(), toml::de::Error> {
        assert_eq!(Config::from_toml("")?, Config::default());
        Ok(())
    }

    #[test]
    fn toml_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let c = Config::default();
        assert_eq!(Config::from_toml(&c.to_toml()?)?, c);
        Ok(())
    }

    #[test]
    fn partial_file_overrides_only_what_it_sets() -> Result<(), toml::de::Error> {
        let c = Config::from_toml("[asr]\nchunk_ms = 160\n[web]\nport = 9000\n")?;
        assert_eq!(c.asr.chunk_ms, 160);
        assert_eq!(c.asr.model, Asr::default().model);
        assert_eq!(c.web.port, 9000);
        Ok(())
    }

    #[test]
    fn fallback_defaults_and_validation() -> Result<(), toml::de::Error> {
        let c = Config::default();
        assert_eq!(c.video.fallback, FallbackMode::Black);
        assert_eq!(c.video.fallback_after_ms, 1000);
        let c = Config::from_toml("[video]\nfallback = \"off\"\nfallback_after_ms = 150\n")?;
        assert_eq!(c.video.fallback, FallbackMode::Off);
        assert_eq!(issue_paths(&c), vec!["video.fallback_after_ms"]);
        let c = Config::from_toml("[video]\nfallback = \"image\"\n")?;
        assert_eq!(issue_paths(&c), vec!["video.fallback_image"]);
        let c =
            Config::from_toml("[video]\nfallback = \"image\"\nfallback_image = \"slate.gif\"\n")?;
        assert_eq!(issue_paths(&c), vec!["video.fallback_image"]);
        let c = Config::from_toml(
            "[video]\nfallback = \"image\"\nfallback_image = \"/srv/Slate.JPG\"\nfallback_after_ms = 10000\n",
        )?;
        assert!(c.validate().is_empty());
        assert_eq!(Config::from_toml(&c.to_toml().unwrap_or_default())?, c);
        assert!(Config::from_toml("[video]\nfallback = \"blue\"\n").is_err());
        Ok(())
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::from_toml("[asr]\nchunk = 1\n").is_err());
    }

    #[test]
    fn out_of_range_values_are_reported_by_path() {
        let mut c = Config::default();
        c.captions.rows = 9;
        c.vad.threshold = 2.0;
        let paths = issue_paths(&c);
        assert!(paths.contains(&"captions.rows".to_string()));
        assert!(paths.contains(&"vad.threshold".to_string()));
    }

    #[test]
    fn language_rules() {
        let mut c = Config::default();
        c.languages[1].source = true;
        c.languages[2].cea708_service = Some(1);
        c.languages[3].cea708_service = None;
        c.languages[3].webvtt = false;
        let paths = issue_paths(&c);
        assert!(paths.contains(&"languages".to_string()));
        assert!(paths.contains(&"languages[2].cea708_service".to_string()));
        assert!(paths.contains(&"languages[3].cc608".to_string()));
    }

    #[test]
    fn webvtt_only_language_and_hls_outputs() {
        let mut c = Config::default();
        c.languages[3].cea708_service = None;
        c.languages[3].code = "ja".into();
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        // Without an HLS output a WebVTT-only language is shown nowhere.
        assert!(c.warnings().iter().any(|i| i.path == "languages[3].webvtt"));
        c.outputs.push(Output {
            public: true,
            ..Output::new("hls://web?segment_s=2&window=6")
        });
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        assert!(c.warnings().is_empty());
        c.outputs.push(Output::new("hls://web"));
        c.outputs.push(Output::new("hls://../x"));
        c.outputs.push(Output::new("hls://ok?window=99"));
        c.outputs.push(Output {
            public: true,
            ..Output::new("udp://127.0.0.1:5000")
        });
        let paths = issue_paths(&c);
        for i in 2..=4 {
            assert!(paths.contains(&format!("outputs[{i}].url")), "{paths:?}");
        }
        assert!(paths.contains(&"outputs[5].public".to_string()));
        assert!(hls_name_ok("web-1_a") && !hls_name_ok("Web") && !hls_name_ok(""));
    }

    #[test]
    fn bad_urls_are_reported() {
        let mut c = Config::default();
        c.input.url = "http://x".into();
        c.outputs = vec![Output::new("rtmp://")];
        let paths = issue_paths(&c);
        assert!(paths.contains(&"input.url".to_string()));
        assert!(paths.contains(&"outputs[0].url".to_string()));
    }

    #[test]
    fn cc2_cc4_and_bad_filter_entries_are_reported() {
        let mut c = Config::default();
        c.languages[2].cc608 = Some(Cc608::Cc2);
        c.languages[3].cc608 = Some(Cc608::Cc4);
        c.filter.blocklist = vec!["ok*".into(), "*".into()];
        c.filter.allowlist = vec!["a$$".into()];
        let paths = issue_paths(&c);
        assert!(paths.contains(&"languages[2].cc608".to_string()));
        assert!(paths.contains(&"languages[3].cc608".to_string()));
        assert!(paths.contains(&"filter.blocklist[1]".to_string()));
        assert!(!paths.contains(&"filter.blocklist[0]".to_string()));
        assert!(paths.contains(&"filter.allowlist[0]".to_string()));
    }

    #[test]
    fn default_config_has_no_warnings() {
        assert_eq!(Config::default().warnings(), Vec::new());
    }

    #[test]
    fn script_warnings() {
        let mut c = Config::default();
        c.languages[2].code = "ja".into();
        c.languages[3].code = "pl".into();
        c.languages[3].cc608 = Some(Cc608::Cc4);
        let w: Vec<String> = c.warnings().into_iter().map(|i| i.path).collect();
        assert_eq!(
            w,
            vec![
                "languages[2].code".to_string(),
                "languages[3].code".to_string()
            ]
        );
        // Warnings never block: validity is judged separately.
        assert!(!c.validate().iter().any(|i| i.path == "languages[2].code"));
    }

    #[test]
    fn output_enabled_defaults_true_and_name_is_optional() -> Result<(), Box<dyn std::error::Error>>
    {
        let c = Config::from_toml("[[outputs]]\nurl = \"udp://127.0.0.1:5000\"\n")?;
        assert!(c.outputs[0].enabled);
        assert_eq!(c.outputs[0].name, None);
        let c = Config::from_toml(
            "[[outputs]]\nurl = \"udp://127.0.0.1:5000\"\nname = \"Studio\"\nenabled = false\n",
        )?;
        assert!(!c.outputs[0].enabled);
        assert_eq!(c.outputs[0].name.as_deref(), Some("Studio"));
        assert_eq!(Config::from_toml(&c.to_toml()?)?, c);
        Ok(())
    }

    #[test]
    fn bad_output_names_are_reported() {
        let mut c = Config::default();
        c.outputs[0].name = Some("x".repeat(OUTPUT_NAME_MAX + 1));
        assert!(issue_paths(&c).contains(&"outputs[0].name".to_string()));
        c.outputs[0].name = Some("a\nb".into());
        assert!(issue_paths(&c).contains(&"outputs[0].name".to_string()));
        c.outputs[0].name = Some("Main".into());
        assert!(!issue_paths(&c).contains(&"outputs[0].name".to_string()));
    }

    #[test]
    fn wide_lines_rejected_when_608_in_use() {
        let mut c = Config::default();
        c.captions.max_chars_per_line = 40;
        assert!(issue_paths(&c).contains(&"captions.max_chars_per_line".to_string()));
    }
}
