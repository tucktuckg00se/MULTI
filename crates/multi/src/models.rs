//! `multi models`: the models directory, downloads (resumable, SHA-256
//! checked, atomic), opus-mt conversion, verification, and the lookups the
//! default workers use.
//!
//! Layout inside the models directory: each registry entry's `dir`
//! (`sherpa/...`, `ct2/opus-mt-en-es`), plus `.tmp/` for downloads and
//! conversions in progress and `.venv/` for the conversion tools.

use anyhow::{Context, Result, bail};
use multi_core::config::{Config, Issue};
use multi_core::models::{self, File, MANIFEST, Model, Origin, Registry, Source};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// Conversion script, embedded so installed binaries don't need the source tree.
const CONVERT_SCRIPT: &str = include_str!("../../../scripts/convert-opus-mt.sh");
const ATTEMPTS: u32 = 4;

/// `$MULTI_MODELS`, else `$XDG_DATA_HOME/multi/models`, else
/// `~/.local/share/multi/models`.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MULTI_MODELS").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("multi/models");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/share/multi/models")
}

/// `--models-dir` if given, else [`default_dir`].
pub fn resolve_dir(cli: Option<&Path>) -> PathBuf {
    cli.map_or_else(default_dir, Path::to_path_buf)
}

// ---------------------------------------------------------------- catalogue

static CATALOGUE: OnceLock<PathBuf> = OnceLock::new();

/// Sets the user catalogue file for this process (`--catalogue`); first call wins.
pub fn set_catalogue(path: PathBuf) {
    let _ = CATALOGUE.set(path);
}

/// The user catalogue file: `--catalogue`, else `$MULTI_CATALOGUE`, else
/// `$XDG_CONFIG_HOME/multi/models.toml`, else `~/.config/multi/models.toml`.
/// The second value says whether it was named explicitly (then a missing
/// file is reported).
pub fn catalogue_path() -> (PathBuf, bool) {
    if let Some(p) = CATALOGUE.get() {
        return (p.clone(), true);
    }
    if let Some(p) = std::env::var_os("MULTI_CATALOGUE").filter(|p| !p.is_empty()) {
        return (PathBuf::from(p), true);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        });
    (base.join("multi/models.toml"), false)
}

/// The built-in registry merged with the user catalogue at `path` (or
/// [`catalogue_path`]), plus a message per skipped user entry.
pub fn load_catalogue(path: Option<&Path>) -> Result<(Registry, Vec<String>)> {
    let mut reg = Registry::builtin().map_err(anyhow::Error::msg)?;
    let (path, explicit) = match path {
        Some(p) => (p.to_path_buf(), true),
        None => catalogue_path(),
    };
    let warnings = match fs::read_to_string(&path) {
        Ok(text) => reg
            .merge_user(&text)
            .into_iter()
            .map(|w| format!("{}: {w}", path.display()))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound && !explicit => Vec::new(),
        Err(e) => vec![format!("user catalogue {} not read: {e}", path.display())],
    };
    Ok((reg, warnings))
}

fn registry() -> Result<Registry> {
    let (reg, warnings) = load_catalogue(None)?;
    for w in warnings {
        tracing::warn!("{w}");
    }
    Ok(reg)
}

fn pull_hint(id: &str) -> String {
    format!("run `multi models pull {id}`")
}

// ---------------------------------------------------------------- workers

/// What the default ASR worker runs (M2-5: the engine comes from the
/// catalogue entry).
#[derive(Debug, Clone)]
pub struct AsrPaths {
    pub id: String,
    pub engine: models::AsrEngine,
    /// `multi-asr --model`: the export folder, or a Whisper model's file.
    pub model: PathBuf,
    /// `multi-asr --chunk-ms`: a Nemotron export's chunk, or the Whisper
    /// pass interval (`asr.chunk_ms`).
    pub chunk_ms: u32,
    pub vad: PathBuf,
}

/// The ASR model for `asr.model` (and `asr.chunk_ms`), falling back to its
/// CPU variant when only that is installed, plus the VAD model.
pub fn asr_paths(config: &Config, root: &Path) -> Result<AsrPaths> {
    let reg = registry()?;
    asr_paths_in(&reg, config, root)
}

pub fn asr_paths_in(reg: &Registry, config: &Config, root: &Path) -> Result<AsrPaths> {
    let (model, chunk) = (&config.asr.model, config.asr.chunk_ms);
    let Some(m) = reg.asr(model, chunk) else {
        bail!(
            "ASR model `{model}` ({chunk} ms) is not in the model registry; see `multi models list`"
        );
    };
    let Some(engine) = m.asr_engine() else {
        bail!("ASR model {} has no engine in the registry", m.id);
    };
    let src = source_lang(config);
    if !m.languages.contains(&src) {
        bail!(
            "ASR model {} cannot transcribe `{src}` (it supports {})",
            m.id,
            m.languages.join(", ")
        );
    }
    let chosen = if m.installed(root) {
        m
    } else {
        match m.cpu_variant.as_deref().and_then(|v| reg.get(v)) {
            Some(v) if v.installed(root) => v,
            _ => bail!(
                "ASR model {} is not installed in {}; {}",
                m.id,
                root.display(),
                pull_hint(&m.id)
            ),
        }
    };
    let Some(vad) = reg.vad() else {
        bail!("the model registry has no VAD model");
    };
    if !vad.installed(root) {
        bail!(
            "VAD model {} is not installed in {}; {}",
            vad.id,
            root.display(),
            pull_hint(&vad.id)
        );
    }
    let vad_file = vad
        .files
        .first()
        .map(|f| f.path.as_str())
        .unwrap_or_default();
    let chunk_ms = match engine {
        models::AsrEngine::SherpaStreaming => chosen.chunk_ms.unwrap_or(chunk),
        models::AsrEngine::Whisper => chunk.clamp(200, 3000),
    };
    Ok(AsrPaths {
        id: chosen.id.clone(),
        engine: chosen.asr_engine().unwrap_or(engine),
        model: chosen.asr_model_path(root),
        chunk_ms,
        vad: vad.path(root).join(vad_file),
    })
}

fn source_lang(config: &Config) -> String {
    config
        .languages
        .iter()
        .find(|l| l.source)
        .map_or_else(|| "en".into(), |l| l.code.clone())
}

/// One lane of the translation worker: a target language and its model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtModel {
    pub lang: String,
    pub dir: PathBuf,
    /// `>>id<<` token to put before each sentence.
    pub prefix: Option<String>,
    /// Translates from English: the source is first translated by the
    /// `en` lane (pivot, M2-2).
    pub pivot: bool,
}

/// The translation lanes for every target language (plus an `en` pivot
/// lane when a target goes through English and English is not a target).
pub fn mt_paths(config: &Config, root: &Path) -> Result<Vec<MtModel>> {
    let reg = registry()?;
    mt_paths_in(&reg, config, root)
}

/// How the source reaches `target`, preferring installed models.
fn mt_route<'a>(
    reg: &'a Registry,
    src: &str,
    target: &str,
    root: &Path,
) -> Option<models::Route<'a>> {
    reg.route(src, target, |m| m.installed(root))
}

fn lane(lang: &str, m: &Model, root: &Path, pivot: bool) -> MtModel {
    MtModel {
        lang: lang.into(),
        dir: m.path(root),
        prefix: m.target_token.clone(),
        pivot,
    }
}

pub fn mt_paths_in(reg: &Registry, config: &Config, root: &Path) -> Result<Vec<MtModel>> {
    let src = source_lang(config);
    let mut out = Vec::new();
    let mut to_en = None;
    for l in config.languages.iter().filter(|l| !l.source) {
        let Some(route) = mt_route(reg, &src, &l.code, root) else {
            bail!(
                "no translation model for {src}->{} (direct or through English) in the model registry; remove the language or see `multi models list`",
                l.code
            );
        };
        if let Some(m) = route.models().into_iter().find(|m| !m.installed(root)) {
            bail!(
                "translation model {} ({}->{}) is not installed in {}; {}",
                m.id,
                m.source.as_deref().unwrap_or("?"),
                m.targets.join(","),
                root.display(),
                pull_hint(&m.id)
            );
        }
        match route {
            models::Route::Direct(m) => out.push(lane(&l.code, m, root, false)),
            models::Route::Pivot(a, b) => {
                to_en = Some(a);
                out.push(lane(&l.code, b, root, true));
            }
        }
    }
    if let Some(a) = to_en
        && !out.iter().any(|m| m.lang == models::PIVOT)
    {
        out.push(lane(models::PIVOT, a, root, false));
    }
    Ok(out)
}

/// Ids of the installed models the default workers would use for `config`
/// (ASR or its CPU variant, VAD, the translation models of every target,
/// both halves of a pivot).
pub fn in_use(reg: &Registry, config: &Config, root: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(m) = reg.asr(&config.asr.model, config.asr.chunk_ms) {
        let variant = m.cpu_variant.as_deref().and_then(|v| reg.get(v));
        if m.installed(root) {
            ids.push(m.id.clone());
        } else if let Some(v) = variant.filter(|v| v.installed(root)) {
            ids.push(v.id.clone());
        }
    }
    if let Some(v) = reg.vad() {
        ids.push(v.id.clone());
    }
    let src = source_lang(config);
    for l in config.languages.iter().filter(|l| !l.source) {
        if let Some(r) = mt_route(reg, &src, &l.code, root) {
            for m in r.models() {
                if !ids.contains(&m.id) {
                    ids.push(m.id.clone());
                }
            }
        }
    }
    ids
}

/// One warning per model the configuration needs that is missing from
/// `models_dir` (paths `asr.model` or `languages[i].code`), and per
/// language with no model in the registry. Presence check only, no hashing.
pub fn missing_models(config: &Config, models_dir: &Path) -> Vec<Issue> {
    match registry() {
        Ok(reg) => missing_models_in(&reg, config, models_dir),
        Err(e) => vec![Issue {
            path: "asr.model".into(),
            message: format!("{e:#}"),
        }],
    }
}

pub fn missing_models_in(reg: &Registry, config: &Config, root: &Path) -> Vec<Issue> {
    let mut v = Vec::new();
    let mut push = |path: String, message: String| v.push(Issue { path, message });
    let src = source_lang(config);
    match reg.asr(&config.asr.model, config.asr.chunk_ms) {
        None => push(
            "asr.model".into(),
            format!(
                "`{}` ({} ms) is not in the model registry",
                config.asr.model, config.asr.chunk_ms
            ),
        ),
        Some(m) => {
            let variant = m.cpu_variant.as_deref().and_then(|id| reg.get(id));
            if !m.installed(root) && !variant.is_some_and(|x| x.installed(root)) {
                push(
                    "asr.model".into(),
                    format!("{} is not installed; {}", m.id, pull_hint(&m.id)),
                );
            }
            // Built-in ASR models: `Config::validate` reports this as an error.
            if let Some(i) = config.languages.iter().position(|l| l.source)
                && !m.languages.contains(&src)
                && m.origin == Origin::User
            {
                push(
                    format!("languages[{i}].code"),
                    format!("{} cannot transcribe `{src}`", m.id),
                );
            }
        }
    }
    if let Some(vad) = reg.vad()
        && !vad.installed(root)
    {
        push(
            "asr.model".into(),
            format!(
                "{} (voice activity) is not installed; {}",
                vad.id,
                pull_hint(&vad.id)
            ),
        );
    }
    for (i, l) in config.languages.iter().enumerate() {
        if l.source {
            continue;
        }
        let path = format!("languages[{i}].code");
        let Some(route) = mt_route(reg, &src, &l.code, root) else {
            push(
                path,
                format!(
                    "no translation model for {src}->{} (direct or through English) in the registry",
                    l.code
                ),
            );
            continue;
        };
        let via = match route {
            models::Route::Direct(_) => String::new(),
            models::Route::Pivot(..) => " (translating through English)".into(),
        };
        for m in route.models().into_iter().filter(|m| !m.installed(root)) {
            push(
                path.clone(),
                format!("{} is not installed{via}; {}", m.id, pull_hint(&m.id)),
            );
        }
    }
    v
}

// ---------------------------------------------------------------- list

/// `installed`, `partial` (a folder without a finished model) or `missing`.
pub fn status(m: &Model, root: &Path) -> &'static str {
    if m.installed(root) {
        "installed"
    } else if m.path(root).exists() && !matches!(m.source(), Source::Files) {
        "partial"
    } else {
        "missing"
    }
}

/// Bytes on disk of an installed model: its registry files, or every file
/// in a converted model's folder.
pub fn size_on_disk(m: &Model, root: &Path) -> u64 {
    let dir = m.path(root);
    match m.source() {
        Source::Convert(_) => fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .filter(|md| md.is_file())
                    .map(|md| md.len())
                    .sum()
            })
            .unwrap_or(0),
        _ => m
            .files
            .iter()
            .filter_map(|f| fs::metadata(dir.join(&f.path)).ok())
            .map(|md| md.len())
            .sum(),
    }
}

pub fn list(reg: &Registry, root: &Path) {
    println!("models directory: {}", root.display());
    println!("user catalogue: {}", catalogue_path().0.display());
    println!(
        "{:<36} {:<5} {:<7} {:<9} {:>8} {:>8}  {:<12} languages",
        "id", "kind", "from", "status", "disk MB", "VRAM MB", "licence"
    );
    for m in &reg.models {
        let status = status(m, root);
        let from = match m.origin {
            Origin::Builtin => "builtin",
            Origin::User => "user",
        };
        let kind = format!("{:?}", m.kind).to_lowercase();
        let mark = if m.default { "*" } else { " " };
        println!(
            "{:<36} {:<5} {:<7} {:<9} {:>8} {:>8}  {:<12} {}",
            format!("{}{mark}", m.id),
            kind,
            from,
            status,
            m.disk_mb,
            m.vram_mb,
            m.licence,
            m.languages_label()
        );
    }
    println!("* = default set (`multi models pull` with no ids)");
}

// ---------------------------------------------------------------- pull

fn select<'a>(reg: &'a Registry, ids: &[String]) -> Result<Vec<&'a Model>> {
    if ids.is_empty() {
        return Ok(reg.defaults().collect());
    }
    ids.iter()
        .map(|id| {
            reg.get(id)
                .with_context(|| format!("unknown model `{id}`; see `multi models list`"))
        })
        .collect()
}

/// Downloads (or converts) the given models, or the default set.
pub fn pull(reg: &Registry, ids: &[String], root: &Path) -> Result<()> {
    let models = select(reg, ids)?;
    fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    let mut failed = Vec::new();
    for m in models {
        println!(
            "{} ({}, ~{} MB)",
            m.id,
            format!("{:?}", m.kind).to_lowercase(),
            m.disk_mb
        );
        println!("  licence: {}", m.licence);
        println!("  attribution: {}", m.attribution);
        if m.installed(root) {
            println!(
                "  already installed (check with `multi models verify {}`)",
                m.id
            );
            continue;
        }
        match pull_one(m, root) {
            Ok(()) => println!("  installed in {}", m.path(root).display()),
            Err(e) => {
                eprintln!("  FAILED: {e:#}");
                failed.push(m.id.clone());
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("could not install: {}", failed.join(", "))
    }
}

/// What a pull is doing, for progress reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Download,
    Extract,
    Convert,
    Verify,
}

/// Progress callback: phase, bytes done, bytes total (0 when unknown).
pub type Report<'a> = &'a (dyn Fn(Phase, u64, u64) + Sync);

fn quiet(_: Phase, _: u64, _: u64) {}

fn tmp_dir(root: &Path) -> Result<PathBuf> {
    let t = root.join(".tmp");
    fs::create_dir_all(&t).with_context(|| format!("cannot create {}", t.display()))?;
    Ok(t)
}

fn pull_one(m: &Model, root: &Path) -> Result<()> {
    pull_one_with(m, root, &quiet)
}

/// Downloads (or converts) one model, reporting progress. The CLI prints as
/// well; the web GUI (`web_models`) uses the reports.
pub fn pull_one_with(m: &Model, root: &Path, report: Report<'_>) -> Result<()> {
    let dir = m.path(root);
    match m.source() {
        Source::Files => {
            fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
            let total: u64 = m.files.iter().map(|f| f.size).sum();
            let mut base = 0;
            for f in &m.files {
                let dest = dir.join(&f.path);
                report(Phase::Verify, base, total);
                if !file_ok(&dest, f)? {
                    let url = m.file_url(f).context("file without a URL")?;
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p)?;
                    }
                    let offset = |p: Phase, d: u64, _: u64| report(p, base + d, total);
                    download_with(&url, &dest, &f.sha256, f.size, &offset)?;
                }
                base += f.size;
            }
            report(Phase::Verify, total, total);
            Ok(())
        }
        Source::Archive(a) => {
            let tmp = tmp_dir(root)?;
            let file = tmp.join(format!("{}.tar.bz2", m.id));
            if !file.is_file() {
                download_with(&a.url, &file, &a.sha256, a.size, report)?;
            }
            let staging = tmp.join(format!("{}.extract", m.id));
            remove_path(&staging)?;
            println!("  extracting");
            report(Phase::Extract, 0, 0);
            extract_tar_bz2(&file, &staging)?;
            report(Phase::Verify, 0, 0);
            for f in &m.files {
                if !file_ok(&staging.join(&f.path), f)? {
                    bail!("{} is missing or wrong in the archive", f.path);
                }
            }
            install_dir(&staging, &dir)?;
            fs::remove_file(&file).ok();
            Ok(())
        }
        Source::Convert(c) => {
            let tmp = tmp_dir(root)?;
            let out = tmp.join(&m.id);
            remove_path(&out)?;
            report(Phase::Convert, 0, 0);
            convert(root, &tmp, &c.repo, &c.revision, &out)?;
            report(Phase::Verify, 0, 0);
            write_manifest(m, &c.repo, &c.revision, &out)?;
            install_dir(&out, &dir)
        }
    }
}

/// Moves a finished folder into place (replacing a partial one).
fn install_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(p) = to.parent() {
        fs::create_dir_all(p)?;
    }
    remove_path(to)?;
    fs::rename(from, to)
        .with_context(|| format!("cannot move {} to {}", from.display(), to.display()))
}

fn remove_path(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => return Ok(()),
    }
    .with_context(|| format!("cannot remove {}", p.display()))
}

/// Size and SHA-256 match (false when missing).
fn file_ok(path: &Path, f: &File) -> Result<bool> {
    match fs::metadata(path) {
        Ok(md) if md.is_file() && md.len() == f.size => Ok(sha256_file(path)? == f.sha256),
        _ => Ok(false),
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn url_allowed(url: &str) -> bool {
    url.starts_with("https://")
        || url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://localhost:")
}

/// Downloads `url` to `dest` via `dest.part`: resumes a partial `.part`
/// (HTTP Range), retries broken transfers, checks size and SHA-256, and
/// renames into place. On a checksum mismatch nothing is left behind.
pub fn download(url: &str, dest: &Path, sha256: &str, size: u64) -> Result<()> {
    download_with(url, dest, sha256, size, &quiet)
}

/// [`download`] with progress reports.
pub fn download_with(
    url: &str,
    dest: &Path,
    sha256: &str,
    size: u64,
    report: Report<'_>,
) -> Result<()> {
    if !url_allowed(url) {
        bail!("refusing non-HTTPS download {url}");
    }
    let part = part_path(dest);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .build();
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("  downloading {name} ({:.1} MB)", size as f64 / 1e6);
    let mut attempt = 1;
    loop {
        match fetch(&agent, url, &part, size, report) {
            Ok(()) => break,
            Err(e) if attempt < ATTEMPTS => {
                eprintln!("  {name}: {e:#}; retrying ({attempt}/{})", ATTEMPTS - 1);
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
                attempt += 1;
            }
            Err(e) => return Err(e.context(format!("downloading {url}"))),
        }
    }
    report(Phase::Verify, size, size);
    let got = sha256_file(&part)?;
    if got != sha256 {
        fs::remove_file(&part).ok();
        bail!("checksum mismatch for {name}: expected {sha256}, got {got} (file deleted)");
    }
    fs::rename(&part, dest).with_context(|| format!("cannot rename {}", part.display()))?;
    Ok(())
}

fn fetch(agent: &ureq::Agent, url: &str, part: &Path, size: u64, report: Report<'_>) -> Result<()> {
    let mut have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if have > size {
        fs::remove_file(part)?;
        have = 0;
    }
    if have == size {
        return Ok(());
    }
    let mut req = agent.get(url);
    if have > 0 {
        req = req.set("Range", &format!("bytes={have}-"));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(416, _)) => {
            fs::remove_file(part).ok();
            bail!("server refused to resume; starting again");
        }
        Err(e) => return Err(e.into()),
    };
    let resumed = resp.status() == 206
        && resp
            .header("Content-Range")
            .is_some_and(|r| r.starts_with(&format!("bytes {have}-")));
    if resp.status() == 206 && !resumed {
        fs::remove_file(part).ok();
        bail!("unexpected Content-Range; starting again");
    }
    let mut file = if resumed {
        fs::OpenOptions::new().append(true).open(part)?
    } else {
        have = 0;
        fs::File::create(part)?
    };
    let mut reader = resp.into_reader().take(size - have);
    let mut writer = Progress::new(
        io::BufWriter::with_capacity(1 << 20, &mut file),
        have,
        size,
        report,
    );
    let copied = io::copy(&mut reader, &mut writer);
    writer.flush()?;
    drop(writer);
    file.sync_all()?;
    copied?;
    let got = fs::metadata(part)?.len();
    if got != size {
        bail!("incomplete download ({got} of {size} bytes)");
    }
    Ok(())
}

/// Prints a line every 10% for large files, and reports every write.
struct Progress<'a, W> {
    inner: W,
    done: u64,
    total: u64,
    next: u64,
    report: Report<'a>,
}

impl<'a, W: Write> Progress<'a, W> {
    fn new(inner: W, done: u64, total: u64, report: Report<'a>) -> Self {
        let step = total / 10;
        report(Phase::Download, done, total);
        Self {
            inner,
            done,
            total,
            report,
            next: if total < 50_000_000 {
                u64::MAX
            } else {
                (done / step.max(1) + 1) * step
            },
        }
    }
}

impl<W: Write> Write for Progress<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.done += n as u64;
        (self.report)(Phase::Download, self.done, self.total);
        if self.done >= self.next {
            eprintln!("    {}%", self.done * 100 / self.total.max(1));
            self.next += self.total / 10;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Extracts regular files and folders, dropping the top-level folder.
fn extract_tar_bz2(archive: &Path, into: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut tar = tar::Archive::new(bzip2::read::BzDecoder::new(io::BufReader::new(file)));
    fs::create_dir_all(into)?;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let rel: PathBuf = path.components().skip(1).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            bail!("unsafe path in archive: {}", path.display());
        }
        let kind = entry.header().entry_type();
        let dest = into.join(&rel);
        if kind.is_dir() {
            fs::create_dir_all(&dest)?;
        } else if kind.is_file() {
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p)?;
            }
            entry.unpack(&dest)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- convert

fn python() -> String {
    std::env::var("MULTI_PYTHON").unwrap_or_else(|_| "python3".into())
}

fn convert(root: &Path, tmp: &Path, repo: &str, revision: &str, out: &Path) -> Result<()> {
    let py = python();
    let found = Command::new(&py)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        bail!(
            "converting opus-mt models needs Python 3 (`{py}` not found): install python3 with venv \
             support, set MULTI_PYTHON, or copy an already converted models folder from another machine"
        );
    }
    let script = tmp.join("convert-opus-mt.sh");
    fs::write(&script, CONVERT_SCRIPT)?;
    println!("  converting {repo} @ {revision} to CTranslate2 (float16)");
    let status = Command::new("bash")
        .arg(&script)
        .args([repo, revision])
        .arg(out)
        .env("MULTI_VENV", root.join(".venv"))
        .env("MULTI_PYTHON", &py)
        .status()
        .context("cannot run bash")?;
    if !status.success() {
        bail!("conversion failed ({status})");
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub repo: String,
    pub revision: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// Hashes every file in `dir` (flat) into `manifest.json`.
pub fn write_manifest(m: &Model, repo: &str, revision: &str, dir: &Path) -> Result<()> {
    let mut files = Vec::new();
    let mut names: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != MANIFEST)
        .collect();
    names.sort();
    for n in names {
        let p = dir.join(&n);
        files.push(ManifestFile {
            sha256: sha256_file(&p)?,
            size: fs::metadata(&p)?.len(),
            path: n,
        });
    }
    let manifest = Manifest {
        id: m.id.clone(),
        repo: repo.into(),
        revision: revision.into(),
        files,
    };
    fs::write(
        dir.join(MANIFEST),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    Ok(())
}

// ---------------------------------------------------------------- verify

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Ok,
    Missing,
    /// Present, but nothing to check against (converted without a manifest).
    Unverified(String),
    Failed(Vec<String>),
}

/// Checks the given models (or every model with a folder present) against
/// the registry, or against `manifest.json` for converted models.
pub fn verify(reg: &Registry, ids: &[String], root: &Path) -> Result<Vec<(String, Check)>> {
    let models: Vec<&Model> = if ids.is_empty() {
        reg.models
            .iter()
            .filter(|m| {
                m.installed(root) || m.files.iter().any(|f| m.path(root).join(&f.path).exists())
            })
            .collect()
    } else {
        select(reg, ids)?
    };
    let mut out = Vec::new();
    for m in models {
        out.push((m.id.clone(), check_model(m, root)?));
    }
    Ok(out)
}

pub fn check_model(m: &Model, root: &Path) -> Result<Check> {
    let dir = m.path(root);
    if let Source::Convert(c) = m.source() {
        if !dir.is_dir() {
            return Ok(Check::Missing);
        }
        let Ok(text) = fs::read_to_string(dir.join(MANIFEST)) else {
            return Ok(Check::Unverified(format!(
                "no {MANIFEST} (not installed by `multi models pull`)"
            )));
        };
        let man: Manifest = serde_json::from_str(&text).context("bad manifest.json")?;
        let mut bad = Vec::new();
        if man.repo != c.repo || man.revision != c.revision {
            bad.push(format!(
                "converted from {}@{}, registry wants {}@{}",
                man.repo, man.revision, c.repo, c.revision
            ));
        }
        for f in &man.files {
            if !models::safe_relative(&f.path) {
                bad.push(format!("{}: bad path", f.path));
                continue;
            }
            let rf = File {
                path: f.path.clone(),
                url: None,
                sha256: f.sha256.clone(),
                size: f.size,
            };
            if !file_ok(&dir.join(&f.path), &rf)? {
                bad.push(format!("{}: missing or changed", f.path));
            }
        }
        if !man.files.iter().any(|f| f.path == "model.bin") {
            bad.push("model.bin not in manifest".into());
        }
        return Ok(if bad.is_empty() {
            Check::Ok
        } else {
            Check::Failed(bad)
        });
    }
    let mut bad = Vec::new();
    let mut missing = 0;
    for f in &m.files {
        let p = dir.join(&f.path);
        match fs::metadata(&p) {
            Err(_) => {
                missing += 1;
                bad.push(format!("{}: missing", f.path));
            }
            Ok(md) if md.len() != f.size => {
                bad.push(format!(
                    "{}: size {} (expected {})",
                    f.path,
                    md.len(),
                    f.size
                ));
            }
            Ok(_) if sha256_file(&p)? != f.sha256 => {
                bad.push(format!("{}: SHA-256 mismatch", f.path))
            }
            Ok(_) => {}
        }
    }
    Ok(if missing == m.files.len() {
        Check::Missing
    } else if bad.is_empty() {
        Check::Ok
    } else {
        Check::Failed(bad)
    })
}

// ---------------------------------------------------------------- remove

pub fn remove(reg: &Registry, id: &str, root: &Path) -> Result<()> {
    let m = reg
        .get(id)
        .with_context(|| format!("unknown model `{id}`; see `multi models list`"))?;
    let dir = m.path(root);
    if !dir.exists() {
        bail!("{id} is not installed in {}", root.display());
    }
    // A folder shared with other models (e.g. `sherpa/` for the VAD file)
    // loses only this model's files.
    let shared = reg
        .models
        .iter()
        .any(|o| o.id != m.id && o.path(root).starts_with(&dir));
    if matches!(m.source(), Source::Files) && shared {
        for f in &m.files {
            for p in [dir.join(&f.path), part_path(&dir.join(&f.path))] {
                if p.exists() {
                    fs::remove_file(&p)
                        .with_context(|| format!("cannot remove {}", p.display()))?;
                }
            }
        }
    } else {
        remove_path(&dir)?;
    }
    println!("removed {id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_models_names_each_model() -> Result<()> {
        let reg = Registry::builtin().map_err(anyhow::Error::msg)?;
        let mut c = Config::default();
        let mut xx = c.languages.last().cloned().context("no languages")?;
        xx.code = "xx".into();
        xx.source = false;
        c.languages.push(xx);
        let dir = std::env::temp_dir().join(format!("multi-missing-{}", std::process::id()));
        let issues = missing_models_in(&reg, &c, &dir);
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"asr.model"));
        let last = format!("languages[{}].code", c.languages.len() - 1);
        let xx = issues
            .iter()
            .find(|i| i.path == last)
            .map(|i| i.message.as_str());
        assert!(
            xx.is_some_and(|m| m.contains("no translation model")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("multi models pull opus-mt-en-es"))
        );
        assert!(asr_paths_in(&reg, &c, &dir).is_err());
        // A Portuguese speaker: French goes through English (ROMANCE-en,
        // then en-fr), both halves named with their pull command.
        let mut pt = Config::default();
        pt.languages[0].code = "pt".into();
        let issues = missing_models_in(&reg, &pt, &dir);
        let fr = |i: &&Issue| i.path == "languages[2].code";
        let msgs: Vec<_> = issues.iter().filter(fr).map(|i| &i.message).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("multi models pull opus-mt-ROMANCE-en")
                    && m.contains("through English")),
            "{issues:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("multi models pull opus-mt-en-fr"))
        );
        let err = mt_paths_in(&reg, &c, &dir)
            .map(|_| ())
            .map_err(|e| e.to_string());
        assert!(err.is_err_and(|e| e.contains("multi models pull")));
        Ok(())
    }

    fn fake_install(reg: &Registry, root: &Path, ids: &[&str]) -> Result<()> {
        for id in ids {
            let dir = reg.get(id).context("unknown id")?.path(root);
            fs::create_dir_all(&dir)?;
            fs::write(dir.join("model.bin"), b"x")?;
            fs::write(dir.join("config.json"), b"{}")?;
        }
        Ok(())
    }

    /// Registry files at their full size (sparse, no content).
    fn fake_files(reg: &Registry, root: &Path, ids: &[&str]) -> Result<()> {
        for id in ids {
            let m = reg.get(id).context("unknown id")?;
            let dir = m.path(root);
            for f in &m.files {
                let p = dir.join(&f.path);
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::File::create(&p)?.set_len(f.size)?;
            }
        }
        Ok(())
    }

    #[test]
    fn asr_model_resolves_engine_path_and_chunk() -> Result<()> {
        let reg = Registry::builtin().map_err(anyhow::Error::msg)?;
        let root = std::env::temp_dir().join(format!("multi-asr-pick-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fake_files(
            &reg,
            &root,
            &[
                "silero-vad",
                "whisper-small",
                "nemotron-3.5-streaming-160ms-int8",
            ],
        )?;
        let mut c = Config::default();
        // Nemotron 160 ms by id: the chunk follows the model, and only the
        // int8 variant is installed.
        c.asr.model = "nemotron-3.5-streaming-160ms".into();
        let p = asr_paths_in(&reg, &c, &root)?;
        assert_eq!(p.id, "nemotron-3.5-streaming-160ms-int8");
        assert_eq!(p.engine, models::AsrEngine::SherpaStreaming);
        assert_eq!(p.chunk_ms, 160);
        assert!(
            p.model
                .ends_with("sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-160ms-int8-2026-06-11")
        );
        assert_eq!(p.vad, root.join("sherpa/silero_vad.onnx"));
        // Whisper small: the file, the pass interval from asr.chunk_ms, and a
        // language Nemotron doesn't have.
        c.asr.model = "whisper-small".into();
        c.asr.chunk_ms = 1000;
        lang_of(&mut c, 0, "pl", true);
        let p = asr_paths_in(&reg, &c, &root)?;
        assert_eq!(p.engine, models::AsrEngine::Whisper);
        assert_eq!(p.model, root.join("whisper/ggml-small.bin"));
        assert_eq!(p.chunk_ms, 1000);
        assert!(
            missing_models_in(&reg, &c, &root)
                .iter()
                .all(|i| i.path != "asr.model")
        );
        // Not installed, or the wrong spoken language: refused.
        c.asr.model = "whisper-large-v3-turbo".into();
        let err = asr_paths_in(&reg, &c, &root)
            .map(|_| ())
            .map_err(|e| e.to_string());
        assert!(err.is_err_and(|e| e.contains("multi models pull whisper-large-v3-turbo")));
        assert!(
            missing_models_in(&reg, &c, &root)
                .iter()
                .any(|i| i.path == "asr.model" && i.message.contains("not installed"))
        );
        c.asr.model = "nemotron-3.5-streaming-160ms".into();
        let err = asr_paths_in(&reg, &c, &root)
            .map(|_| ())
            .map_err(|e| e.to_string());
        assert!(err.is_err_and(|e| e.contains("cannot transcribe `pl`")));
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }

    fn lang_of(c: &mut Config, i: usize, code: &str, source: bool) {
        c.languages[i].code = code.into();
        c.languages[i].source = source;
    }

    #[test]
    fn translation_lanes_direct_and_pivot() -> Result<()> {
        let reg = Registry::builtin().map_err(anyhow::Error::msg)?;
        let root = std::env::temp_dir().join(format!("multi-route-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fake_install(
            &reg,
            &root,
            &[
                "opus-mt-es-en",
                "opus-mt-es-de",
                "opus-mt-en-fr",
                "opus-mt-ROMANCE-en",
                "opus-mt-tc-big-en-pt",
            ],
        )?;
        // Spanish speaker; EN and DE direct, FR through English.
        let mut c = Config::default();
        lang_of(&mut c, 0, "es", true);
        lang_of(&mut c, 1, "en", false);
        let lanes = mt_paths_in(&reg, &c, &root)?;
        let got: Vec<_> = lanes
            .iter()
            .map(|m| {
                (
                    m.lang.as_str(),
                    m.dir.file_name().and_then(|f| f.to_str()).unwrap_or(""),
                    m.pivot,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("en", "opus-mt-es-en", false),
                ("fr", "opus-mt-en-fr", true),
                ("de", "opus-mt-es-de", false)
            ]
        );
        let used = in_use(&reg, &c, &root);
        for id in ["opus-mt-es-en", "opus-mt-en-fr", "opus-mt-es-de"] {
            assert!(used.iter().any(|u| u == id), "{id} not in {used:?}");
        }
        // Portuguese speaker, English not a target: EN-ES missing is named;
        // then FR and DE both go through an extra `en` lane (ROMANCE-en).
        let mut c = Config::default();
        c.languages.truncate(3);
        lang_of(&mut c, 0, "pt", true);
        lang_of(&mut c, 1, "fr", false);
        lang_of(&mut c, 2, "es", false);
        let err = mt_paths_in(&reg, &c, &root).map_err(|e| e.to_string());
        assert!(
            err.as_ref()
                .is_err_and(|e| e.contains("multi models pull opus-mt-en-es")),
            "{err:?}"
        );
        lang_of(&mut c, 2, "de", false);
        fake_install(&reg, &root, &["opus-mt-en-de"])?;
        let lanes = mt_paths_in(&reg, &c, &root)?;
        let got: Vec<_> = lanes.iter().map(|m| (m.lang.as_str(), m.pivot)).collect();
        assert_eq!(got, [("fr", true), ("de", true), ("en", false)]);
        assert!(lanes[2].dir.ends_with("opus-mt-ROMANCE-en"));
        // The EN->PT model keeps its target token on a pivot lane.
        lang_of(&mut c, 0, "es", true);
        lang_of(&mut c, 1, "pt", false);
        let lanes = mt_paths_in(&reg, &c, &root)?;
        assert_eq!(lanes[0].prefix.as_deref(), Some(">>por<<"));
        assert!(lanes[0].pivot);
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }
}
