//! MULTI's configuration: one TOML file, every setting optional with the
//! defaults from PRD §5 (`docs/prd/05-configuration-and-tuning.md`).
//!
//! [`Config::validate`] reports every problem with the dotted path of the
//! setting, so the CLI and the web GUI can point at the exact field.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::Path;

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
            outputs: vec![Output {
                url: "srt://0.0.0.0:9001?mode=listener".into(),
            }],
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
    /// `srt://`, `udp://` or `rtmp(s)://` URL.
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Video {
    /// Hold video back so captions line up with speech. 0 = pass-through.
    pub delay_ms: u32,
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
    /// Required when `bind` is not a loopback address. Prefer the
    /// `MULTI_WEB_TOKEN` environment variable over storing it here.
    pub token: Option<String>,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            bind: IpAddr::from([127, 0, 0, 1]),
            port: 8480,
            token: None,
        }
    }
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
        for (i, o) in self.outputs.iter().enumerate() {
            v.url(
                &format!("outputs[{i}].url"),
                &o.url,
                &["srt", "udp", "rtmp", "rtmps"],
            );
        }

        v.range("video.delay_ms", self.video.delay_ms, 0, 10_000);
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
            if l.cc608.is_none() && l.cea708_service.is_none() {
                v.push(
                    &at("cc608"),
                    "carry the language on a 608 channel, a 708 service, or both",
                );
            }
            if let Some(ch) = l.cc608 {
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
        let paths = issue_paths(&c);
        assert!(paths.contains(&"languages".to_string()));
        assert!(paths.contains(&"languages[2].cea708_service".to_string()));
        assert!(paths.contains(&"languages[3].cc608".to_string()));
    }

    #[test]
    fn bad_urls_are_reported() {
        let mut c = Config::default();
        c.input.url = "http://x".into();
        c.outputs = vec![Output {
            url: "rtmp://".into(),
        }];
        let paths = issue_paths(&c);
        assert!(paths.contains(&"input.url".to_string()));
        assert!(paths.contains(&"outputs[0].url".to_string()));
    }

    #[test]
    fn wide_lines_rejected_when_608_in_use() {
        let mut c = Config::default();
        c.captions.max_chars_per_line = 40;
        assert!(issue_paths(&c).contains(&"captions.max_chars_per_line".to_string()));
    }
}
