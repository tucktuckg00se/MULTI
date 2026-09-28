//! The model registry (`data/models.toml`, embedded): which models MULTI can
//! use, where they come from, their licences, and where they live inside the
//! models directory. Downloading and verifying is done by the `multi` binary.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// The registry shipped with MULTI.
pub const BUILTIN: &str = include_str!("../data/models.toml");

/// Manifest written next to a converted model (`multi models pull`).
pub const MANIFEST: &str = "manifest.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
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
            let err = |msg: &str| Err(format!("model registry: {}: {msg}", m.id));
            if m.id.is_empty() || !ids.insert(m.id.as_str()) {
                return err("empty or duplicate id");
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
            match m.kind {
                Kind::Mt if m.source.is_none() || m.targets.is_empty() => {
                    return err("translation models need source and targets");
                }
                Kind::Asr if m.languages.is_empty() => return err("ASR models need languages"),
                _ => {}
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
        let mut found = self.models.iter().filter(|m| {
            m.kind == Kind::Mt
                && m.source.as_deref() == Some(source)
                && m.targets.iter().any(|t| t == target)
        });
        let first = found.next()?;
        if first.default {
            return Some(first);
        }
        Some(found.find(|m| m.default).unwrap_or(first))
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
