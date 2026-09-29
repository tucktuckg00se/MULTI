//! The model registry (`data/models.toml`, embedded): which models MULTI can
//! use, where they come from, their licences, and where they live inside the
//! models directory. Downloading and verifying is done by the `multi` binary.
//!
//! The catalogue is this built-in registry merged with an optional user
//! file of the same schema ([`Registry::merge_user`]): user entries add
//! models or replace built-in ones by `id`; a bad user entry is skipped.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// The registry shipped with MULTI.
pub const BUILTIN: &str = include_str!("../data/models.toml");

/// Manifest written next to a converted model (`multi models pull`).
pub const MANIFEST: &str = "manifest.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Asr,
    Vad,
    Mt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    SherpaOnnx,
    Ctranslate2,
}

/// Where a catalogue entry comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    #[default]
    Builtin,
    User,
}

/// A caption format a language's script can go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Format {
    #[serde(rename = "608")]
    Cea608,
    #[serde(rename = "708")]
    Cea708,
    #[serde(rename = "webvtt")]
    WebVtt,
}

/// Note for Latin languages with letters CEA-608 lacks (as in
/// `Config::warnings`).
pub const LATIN_EXTENDED_NOTE: &str = "CEA-608 can't show every letter of this language; some accented letters will be replaced with plain ones (708 is unaffected)";

/// A language code used by the catalogue, with its script.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageInfo {
    pub code: String,
    pub name: String,
    /// `Latin`, `Cyrillic`, `Arabic`, `CJK`…
    pub script: String,
    /// Latin letters outside CEA-608's character sets.
    #[serde(default)]
    pub latin_extended: bool,
}

impl LanguageInfo {
    /// CEA-608 and (in MULTI today) 708 carry Latin only; WebVTT any script.
    pub fn formats(&self) -> Vec<Format> {
        if self.script == "Latin" {
            vec![Format::Cea608, Format::Cea708, Format::WebVtt]
        } else {
            vec![Format::WebVtt]
        }
    }

    pub fn note(&self) -> Option<&'static str> {
        (self.script == "Latin" && self.latin_extended).then_some(LATIN_EXTENDED_NOTE)
    }
}

/// One file of a model: downloaded directly or found inside its archive.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// Relative to the model's `dir`.
    pub path: String,
    /// Download URL; defaults to `base_url/path`. Unused for archives.
    #[serde(default)]
    pub url: Option<String>,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// A Hugging Face model converted locally to CTranslate2.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Convert {
    pub repo: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub kind: Kind,
    pub backend: Backend,
    /// Part of the default set that `multi models pull` fetches.
    #[serde(default)]
    pub default: bool,
    /// Spoken languages (ASR).
    #[serde(default)]
    pub languages: Vec<String>,
    /// Translation direction (MT).
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    /// `>>id<<` token multi-target opus-mt models need before each sentence.
    #[serde(default)]
    pub target_token: Option<String>,
    pub licence: String,
    pub attribution: String,
    pub vram_mb: u32,
    pub disk_mb: u32,
    /// A smaller variant used when this one is not installed.
    #[serde(default)]
    pub cpu_variant: Option<String>,
    /// Relative to the models directory.
    pub dir: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub archive: Option<Archive>,
    #[serde(default)]
    pub convert: Option<Convert>,
    /// Built-in or from the user's catalogue file (not in the TOML).
    #[serde(skip)]
    pub origin: Origin,
}

/// How a model is fetched.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    Files,
    Archive(&'a Archive),
    Convert(&'a Convert),
}

impl Model {
    pub fn source(&self) -> Source<'_> {
        match (&self.archive, &self.convert) {
            (Some(a), _) => Source::Archive(a),
            (None, Some(c)) => Source::Convert(c),
            (None, None) => Source::Files,
        }
    }

    /// The model's folder inside `root`.
    pub fn path(&self, root: &Path) -> PathBuf {
        root.join(&self.dir)
    }

    /// Download URL of a direct file.
    pub fn file_url(&self, f: &File) -> Option<String> {
        f.url.clone().or_else(|| {
            self.base_url
                .as_ref()
                .map(|b| format!("{}/{}", b.trim_end_matches('/'), f.path))
        })
    }

    /// Cheap presence check (no hashing): every registry file exists with
    /// the right size, or a converted model has its `model.bin`.
    pub fn installed(&self, root: &Path) -> bool {
        let dir = self.path(root);
        match self.source() {
            Source::Convert(_) => {
                dir.join("model.bin").is_file() && dir.join("config.json").is_file()
            }
            Source::Files | Source::Archive(_) => self.files.iter().all(|f| {
                std::fs::metadata(dir.join(&f.path)).is_ok_and(|m| m.is_file() && m.len() == f.size)
            }),
        }
    }

    /// Checks one entry on its own (no cross-references).
    pub fn check(&self) -> Result<(), String> {
        let m = self;
        let err = |msg: &str| Err(format!("{}: {msg}", m.id));
        if m.id.is_empty() {
            return Err("model with an empty id".into());
        }
        if m.licence.trim().is_empty() || m.attribution.trim().is_empty() {
            return err("licence and attribution are required");
        }
        if !safe_relative(&m.dir) {
            return err("dir must be a relative path without `..`");
        }
        match m.source() {
            Source::Convert(c) => {
                if !m.files.is_empty() || c.repo.is_empty() || c.revision.is_empty() {
                    return err("convert needs repo and revision, and no files");
                }
            }
            Source::Archive(a) => {
                if !is_sha256(&a.sha256) || m.files.is_empty() {
                    return err("archive needs a SHA-256 and the files it must contain");
                }
            }
            Source::Files => {
                if m.files.is_empty() {
                    return err("needs files, archive or convert");
                }
                if m.files.iter().any(|f| m.file_url(f).is_none()) {
                    return err("every file needs a url (or base_url)");
                }
            }
        }
        if m.files
            .iter()
            .any(|f| !is_sha256(&f.sha256) || !safe_relative(&f.path))
        {
            return err("file with a bad SHA-256 or path");
        }
        if let Some(t) = &m.target_token
            && !(t.starts_with(">>") && t.ends_with("<<") && t.len() > 4 && !t.contains(' '))
        {
            return err("target_token must look like >>id<<");
        }
        match m.kind {
            Kind::Mt if m.source.is_none() || m.targets.is_empty() => {
                err("translation models need source and targets")
            }
            Kind::Asr if m.languages.is_empty() => err("ASR models need languages"),
            _ => Ok(()),
        }
    }

    /// Languages this model handles, for display.
    pub fn languages_label(&self) -> String {
        match (self.kind, &self.source) {
            (Kind::Mt, Some(s)) => format!("{s}->{}", self.targets.join(",")),
            _ if self.languages.is_empty() => "-".into(),
            _ => self.languages.join(","),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(rename = "language", default)]
    pub languages: Vec<LanguageInfo>,
    #[serde(rename = "model", default)]
    pub models: Vec<Model>,
}

impl Registry {
    /// The embedded registry.
    pub fn builtin() -> Result<Self, String> {
        Self::parse(BUILTIN)
    }

    /// Parses and checks a registry.
    pub fn parse(text: &str) -> Result<Self, String> {
        let reg: Registry = toml::from_str(text).map_err(|e| format!("model registry: {e}"))?;
        reg.check()?;
        Ok(reg)
    }

    fn check(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        for m in &self.models {
            m.check().map_err(|e| format!("model registry: {e}"))?;
            if !ids.insert(m.id.as_str()) {
                return Err(format!("model registry: {}: duplicate id", m.id));
            }
        }
        for m in &self.models {
            if let Some(v) = &m.cpu_variant
                && self.get(v).is_none()
            {
                return Err(format!("model registry: {}: unknown cpu_variant {v}", m.id));
            }
        }
        Ok(())
    }

    /// Merges a user catalogue (same schema) into this one: an entry with a
    /// known `id` replaces it, a new one is added. Returns one message per
    /// entry (or file) that was skipped; nothing here is fatal.
    pub fn merge_user(&mut self, text: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        let table: toml::Table = match toml::from_str(text) {
            Ok(t) => t,
            Err(e) => return vec![format!("user catalogue ignored: {e}")],
        };
        for key in table.keys() {
            if key != "model" && key != "language" {
                warnings.push(format!("user catalogue: unknown key `{key}` ignored"));
            }
        }
        let mut items = |key: &str| match table.get(key) {
            None => Vec::new(),
            Some(toml::Value::Array(a)) => a.clone(),
            Some(_) => {
                warnings.push(format!("user catalogue: `{key}` must be an array; ignored"));
                Vec::new()
            }
        };
        let (languages, models) = (items("language"), items("model"));
        for (i, v) in languages.into_iter().enumerate() {
            match v.try_into::<LanguageInfo>() {
                Ok(l) if l.code.is_empty() || l.script.is_empty() => warnings
                    .push(format!("user catalogue: language #{}: code and script are required; skipped", i + 1)),
                Ok(l) => {
                    self.languages.retain(|o| o.code != l.code);
                    self.languages.push(l);
                }
                Err(e) => warnings.push(format!("user catalogue: language #{}: {e}; skipped", i + 1)),
            }
        }
        let mut seen = HashSet::new();
        for (i, v) in models.into_iter().enumerate() {
            let mut m = match v.try_into::<Model>() {
                Ok(m) => m,
                Err(e) => {
                    warnings.push(format!("user catalogue: model #{}: {e}; skipped", i + 1));
                    continue;
                }
            };
            if let Err(e) = m.check() {
                warnings.push(format!("user catalogue: {e}; skipped"));
                continue;
            }
            if !seen.insert(m.id.clone()) {
                warnings.push(format!("user catalogue: {}: duplicate id; skipped", m.id));
                continue;
            }
            m.origin = Origin::User;
            match self.models.iter_mut().find(|o| o.id == m.id) {
                Some(slot) => *slot = m,
                None => self.models.push(m),
            }
        }
        // Drop user entries whose cpu_variant points nowhere (repeat: a
        // dropped entry may be another's variant).
        loop {
            let bad = self.models.iter().position(|m| {
                m.origin == Origin::User
                    && m.cpu_variant.as_ref().is_some_and(|v| self.get(v).is_none())
            });
            let Some(i) = bad else { break };
            let m = self.models.remove(i);
            warnings.push(format!(
                "user catalogue: {}: unknown cpu_variant {}; skipped",
                m.id,
                m.cpu_variant.unwrap_or_default()
            ));
        }
        warnings
    }

    /// Script and name of a language code.
    pub fn language(&self, code: &str) -> Option<&LanguageInfo> {
        self.languages.iter().find(|l| l.code == code)
    }

    pub fn get(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The default set for `multi models pull`.
    pub fn defaults(&self) -> impl Iterator<Item = &Model> {
        self.models.iter().filter(|m| m.default)
    }

    /// ASR model for the config's `asr.model`: an exact id, or
    /// `<model>-<chunk_ms>ms` (`nemotron-3.5-streaming` + 560).
    pub fn asr(&self, model: &str, chunk_ms: u32) -> Option<&Model> {
        let pick = |id: &str| self.get(id).filter(|m| m.kind == Kind::Asr);
        pick(model).or_else(|| pick(&format!("{model}-{chunk_ms}ms")))
    }

    /// The first voice-activity model.
    pub fn vad(&self) -> Option<&Model> {
        self.models.iter().find(|m| m.kind == Kind::Vad)
    }

    /// Translation model for `source -> target`, preferring the default set.
    pub fn mt(&self, source: &str, target: &str) -> Option<&Model> {
        self.mt_with(source, target, |_| false)
    }

    /// Translation model for `source -> target`: an installed default, else
    /// any installed one, else a default, else the first in the catalogue.
    pub fn mt_with(
        &self,
        source: &str,
        target: &str,
        installed: impl Fn(&Model) -> bool,
    ) -> Option<&Model> {
        let found: Vec<&Model> = self
            .models
            .iter()
            .filter(|m| {
                m.kind == Kind::Mt
                    && m.source.as_deref() == Some(source)
                    && m.targets.iter().any(|t| t == target)
            })
            .collect();
        let inst: Vec<&Model> = found.iter().copied().filter(|m| installed(m)).collect();
        inst.iter()
            .find(|m| m.default)
            .or(inst.first())
            .or(found.iter().find(|m| m.default))
            .or(found.first())
            .copied()
    }
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A relative path with only normal components.
pub fn safe_relative(p: &str) -> bool {
    !p.is_empty()
        && Path::new(p)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_parses_and_is_complete() -> Result<(), String> {
        let r = Registry::builtin()?;
        for m in &r.models {
            assert!(!m.licence.is_empty(), "{}", m.id);
            assert!(!m.files.is_empty() || m.convert.is_some(), "{}", m.id);
        }
        let defaults: Vec<_> = r.defaults().map(|m| m.id.as_str()).collect();
        for id in [
            "nemotron-3.5-streaming-560ms",
            "nemotron-3.5-streaming-560ms-int8",
            "silero-vad",
            "opus-mt-en-es",
            "opus-mt-en-fr",
            "opus-mt-en-de",
            "opus-mt-tc-big-en-pt",
        ] {
            assert!(defaults.contains(&id), "{id} not in the default set");
        }
        Ok(())
    }

    #[test]
    fn lookups() -> Result<(), String> {
        let r = Registry::builtin()?;
        let asr = r.asr("nemotron-3.5-streaming", 560).map(|m| m.id.as_str());
        assert_eq!(asr, Some("nemotron-3.5-streaming-560ms"));
        assert!(r.asr("nemotron-3.5-streaming", 160).is_none());
        assert_eq!(
            r.mt("en", "pt").map(|m| m.id.as_str()),
            Some("opus-mt-tc-big-en-pt")
        );
        assert!(r.mt("en", "xx").is_none());
        assert_eq!(r.vad().map(|m| m.id.as_str()), Some("silero-vad"));
        Ok(())
    }

    #[test]
    fn every_entry_has_a_licence_and_every_language_a_script() -> Result<(), String> {
        let r = Registry::builtin()?;
        assert!(r.models.len() > 100, "{} models", r.models.len());
        let mut codes = HashSet::new();
        for m in &r.models {
            assert!(
                ["Apache-2.0", "CC-BY-4.0", "MIT", "OpenMDW-1.1"].contains(&m.licence.as_str()),
                "{}: {}",
                m.id,
                m.licence
            );
            assert_eq!(m.origin, Origin::Builtin);
            codes.extend(m.languages.iter().cloned());
            codes.extend(m.source.iter().cloned());
            codes.extend(m.targets.iter().cloned());
            if m.kind == Kind::Mt {
                let c = m.convert.as_ref().ok_or(format!("{}: not convert", m.id))?;
                assert_eq!(c.revision.len(), 40, "{}: revision not pinned", m.id);
                if m.licence == "CC-BY-4.0" {
                    assert!(m.attribution.contains("CC-BY-4.0"), "{}", m.id);
                }
            }
        }
        for c in &codes {
            let l = r.language(c).ok_or(format!("language {c} has no script"))?;
            assert!(!l.script.is_empty() && !l.name.is_empty(), "{c}");
            let latin = l.script == "Latin";
            assert_eq!(!latin, crate::config::NON_LATIN.contains(&c.as_str()), "{c}");
            assert_eq!(
                latin && l.latin_extended,
                crate::config::LATIN_EXTENDED.contains(&c.as_str()),
                "{c}"
            );
            assert!(l.formats().contains(&Format::WebVtt));
            assert_eq!(l.formats().contains(&Format::Cea608), latin);
        }
        assert_eq!(
            r.language("ja").map(|l| l.formats()),
            Some(vec![Format::WebVtt])
        );
        assert!(r.language("pl").and_then(|l| l.note()).is_some());
        assert_eq!(
            r.get("opus-mt-en-zh").and_then(|m| m.target_token.as_deref()),
            Some(">>cmn_Hans<<")
        );
        Ok(())
    }

    #[test]
    fn user_catalogue_merges() -> Result<(), String> {
        let mut r = Registry::builtin()?;
        let n = r.models.len();
        let user = r#"
language = [{ code = "xx", name = "Test", script = "Latin" }, { code = "", name = "bad", script = "" }]

[[model]]
id = "opus-mt-en-es"
kind = "mt"
backend = "ctranslate2"
source = "en"
targets = ["es"]
licence = "Apache-2.0"
attribution = "mine"
vram_mb = 1
disk_mb = 1
dir = "ct2/my-en-es"
convert = { repo = "me/en-es", revision = "abc" }

[[model]]
id = "my-en-xx"
kind = "mt"
backend = "ctranslate2"
source = "en"
targets = ["xx"]
licence = "Apache-2.0"
attribution = "mine"
vram_mb = 1
disk_mb = 1
dir = "ct2/my-en-xx"
convert = { repo = "me/en-xx", revision = "abc" }

[[model]]
id = "escape"
kind = "mt"
backend = "ctranslate2"
source = "en"
targets = ["yy"]
licence = "Apache-2.0"
attribution = "a"
vram_mb = 1
disk_mb = 1
dir = "../../etc"
convert = { repo = "me/x", revision = "abc" }

[[model]]
id = "no-licence"
kind = "vad"
backend = "sherpa-onnx"
licence = ""
attribution = "a"
vram_mb = 0
disk_mb = 0
dir = "x"
convert = { repo = "me/x", revision = "abc" }

[[model]]
id = "typo"
kind = "mt"
backend = "ctranslate2"
sorce = "en"
"#;
        let warnings = r.merge_user(user);
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("escape")));
        assert!(warnings.iter().any(|w| w.contains("no-licence")));
        assert_eq!(r.models.len(), n + 1);
        let es = r.get("opus-mt-en-es").ok_or("missing")?;
        assert_eq!((es.origin, es.dir.as_str()), (Origin::User, "ct2/my-en-es"));
        assert_eq!(r.get("my-en-xx").map(|m| m.origin), Some(Origin::User));
        assert!(r.get("escape").is_none() && r.get("no-licence").is_none());
        assert_eq!(r.language("xx").map(|l| l.name.as_str()), Some("Test"));
        assert_eq!(r.merge_user("not [valid").len(), 1);
        Ok(())
    }

    #[test]
    fn mt_prefers_installed() -> Result<(), String> {
        let r = Registry::builtin()?;
        let pick = |inst: &str| r.mt_with("en", "es", |m| m.id == inst).map(|m| m.id.clone());
        assert_eq!(pick("none").as_deref(), Some("opus-mt-en-es"));
        assert_eq!(
            pick("opus-mt-tc-big-en-es").as_deref(),
            Some("opus-mt-tc-big-en-es")
        );
        Ok(())
    }

    #[test]
    fn rejects_bad_entries() {
        let base = "id = \"x\"\nkind = \"vad\"\nbackend = \"sherpa-onnx\"\nlicence = \"MIT\"\n\
                    attribution = \"a\"\nvram_mb = 0\ndisk_mb = 0\n";
        let files = "files = [{ path = \"f\", size = 1, sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\", url = \"https://x/f\" }]\n";
        let ok = format!("[[model]]\n{base}dir = \"d\"\n{files}");
        assert!(Registry::parse(&ok).is_ok());
        let no_source = format!("[[model]]\n{base}dir = \"d\"\n");
        assert!(Registry::parse(&no_source).is_err());
        let escape = format!("[[model]]\n{base}dir = \"../d\"\n{files}");
        assert!(Registry::parse(&escape).is_err());
        let no_licence = ok.replace("licence = \"MIT\"", "licence = \"\"");
        assert!(Registry::parse(&no_licence).is_err());
        let dup = format!("{ok}{ok}");
        assert!(Registry::parse(&dup).is_err());
    }
}
