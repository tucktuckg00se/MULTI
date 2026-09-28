//! `multi models`: the models directory, downloads (resumable, SHA-256
//! checked, atomic), opus-mt conversion, verification, and the lookups the
//! default workers use.
//!
//! Layout inside the models directory: each registry entry's `dir`
//! (`sherpa/...`, `ct2/opus-mt-en-es`), plus `.tmp/` for downloads and
//! conversions in progress and `.venv/` for the conversion tools.

use anyhow::{Context, Result, bail};
use multi_core::config::{Config, Issue};
use multi_core::models::{self, File, MANIFEST, Model, Registry, Source};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
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

fn registry() -> Result<Registry> {
    Registry::builtin().map_err(anyhow::Error::msg)
}

fn pull_hint(id: &str) -> String {
    format!("run `multi models pull {id}`")
}

// ---------------------------------------------------------------- workers

/// Model folders for the default ASR worker.
#[derive(Debug, Clone)]
pub struct AsrPaths {
    pub id: String,
    pub model_dir: PathBuf,
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
    Ok(AsrPaths {
        id: chosen.id.clone(),
        model_dir: chosen.path(root),
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

/// `(language, model folder)` for every target language.
pub fn mt_paths(config: &Config, root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let reg = registry()?;
    mt_paths_in(&reg, config, root)
}

pub fn mt_paths_in(reg: &Registry, config: &Config, root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let src = source_lang(config);
    let mut out = Vec::new();
    for l in config.languages.iter().filter(|l| !l.source) {
        let Some(m) = reg.mt(&src, &l.code) else {
            bail!(
                "no translation model for {src}->{} in the model registry; remove the language or see `multi models list`",
                l.code
            );
        };
        if !m.installed(root) {
            bail!(
                "translation model {} ({src}->{}) is not installed in {}; {}",
                m.id,
                l.code,
                root.display(),
                pull_hint(&m.id)
            );
        }
        out.push((l.code.clone(), m.path(root)));
    }
    Ok(out)
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
            if let Some(i) = config.languages.iter().position(|l| l.source)
                && !m.languages.contains(&src)
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
        match reg.mt(&src, &l.code) {
            None => push(
                path,
                format!("no translation model for {src}->{} in the registry", l.code),
            ),
            Some(m) if !m.installed(root) => push(
                path,
                format!("{} is not installed; {}", m.id, pull_hint(&m.id)),
            ),
            Some(_) => {}
        }
    }
    v
}

// ---------------------------------------------------------------- list

pub fn list(reg: &Registry, root: &Path) {
    println!("models directory: {}", root.display());
    println!(
        "{:<36} {:<5} {:<9} {:>8} {:>8}  {:<12} languages",
        "id", "kind", "status", "disk MB", "VRAM MB", "licence"
    );
    for m in &reg.models {
        let status = if m.installed(root) {
            "installed"
        } else if m.path(root).exists() && !matches!(m.source(), Source::Files) {
            "partial"
        } else {
            "missing"
        };
        let kind = format!("{:?}", m.kind).to_lowercase();
        let mark = if m.default { "*" } else { " " };
        println!(
            "{:<36} {:<5} {:<9} {:>8} {:>8}  {:<12} {}",
            format!("{}{mark}", m.id),
            kind,
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
        println!("{} ({}, ~{} MB)", m.id, format!("{:?}", m.kind).to_lowercase(), m.disk_mb);
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

fn tmp_dir(root: &Path) -> Result<PathBuf> {
    let t = root.join(".tmp");
    fs::create_dir_all(&t).with_context(|| format!("cannot create {}", t.display()))?;
    Ok(t)
}

fn pull_one(m: &Model, root: &Path) -> Result<()> {
    let dir = m.path(root);
    match m.source() {
        Source::Files => {
            fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
            for f in &m.files {
                let dest = dir.join(&f.path);
                if file_ok(&dest, f)? {
                    continue;
                }
                let url = m.file_url(f).context("file without a URL")?;
                if let Some(p) = dest.parent() {
                    fs::create_dir_all(p)?;
                }
                download(&url, &dest, &f.sha256, f.size)?;
            }
            Ok(())
        }
        Source::Archive(a) => {
            let tmp = tmp_dir(root)?;
            let file = tmp.join(format!("{}.tar.bz2", m.id));
            if !file.is_file() {
                download(&a.url, &file, &a.sha256, a.size)?;
            }
            let staging = tmp.join(format!("{}.extract", m.id));
            remove_path(&staging)?;
            println!("  extracting");
            extract_tar_bz2(&file, &staging)?;
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
            convert(root, &tmp, &c.repo, &c.revision, &out)?;
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
        match fetch(&agent, url, &part, size) {
            Ok(()) => break,
            Err(e) if attempt < ATTEMPTS => {
                eprintln!("  {name}: {e:#}; retrying ({attempt}/{})", ATTEMPTS - 1);
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
                attempt += 1;
            }
            Err(e) => return Err(e.context(format!("downloading {url}"))),
        }
    }
    let got = sha256_file(&part)?;
    if got != sha256 {
        fs::remove_file(&part).ok();
        bail!("checksum mismatch for {name}: expected {sha256}, got {got} (file deleted)");
    }
    fs::rename(&part, dest).with_context(|| format!("cannot rename {}", part.display()))?;
    Ok(())
}

fn fetch(agent: &ureq::Agent, url: &str, part: &Path, size: u64) -> Result<()> {
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
    let mut writer = Progress::new(io::BufWriter::with_capacity(1 << 20, &mut file), have, size);
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

/// Prints a line every 10% for large files.
struct Progress<W> {
    inner: W,
    done: u64,
    total: u64,
    next: u64,
}

impl<W: Write> Progress<W> {
    fn new(inner: W, done: u64, total: u64) -> Self {
        let step = total / 10;
        Self {
            inner,
            done,
            total,
            next: if total < 50_000_000 {
                u64::MAX
            } else {
                (done / step.max(1) + 1) * step
            },
        }
    }
}

impl<W: Write> Write for Progress<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.done += n as u64;
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
        let err = mt_paths_in(&reg, &c, &dir)
            .map(|_| ())
            .map_err(|e| e.to_string());
        assert!(err.is_err_and(|e| e.contains("multi models pull")));
        Ok(())
    }
}
