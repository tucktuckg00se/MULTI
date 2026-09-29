//! `multi models` against a local HTTP server serving tiny fake files (no
//! network): pull, checksum mismatch, resume, archives, verify, remove.

use anyhow::{Context, Result};
use multi::models::{self, Check};
use multi_core::models::{MANIFEST, Registry};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `(path, Range header)` of every request.
type Log = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Serves `files` by path with Range support. A path listed in `cut` has
/// its first response cut off halfway (headers promise the full body).
struct Server {
    port: u16,
    log: Log,
}

impl Server {
    fn start(files: HashMap<String, Vec<u8>>, cut: &[&str]) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let log: Log = Arc::default();
        let log2 = log.clone();
        let cut: Arc<Mutex<Vec<String>>> =
            Arc::new(Mutex::new(cut.iter().map(|s| s.to_string()).collect()));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => continue,
                });
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut range = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("range")
                    {
                        range = Some(v.trim().to_string());
                    }
                }
                if let Ok(mut l) = log2.lock() {
                    l.push((path.clone(), range.clone()));
                }
                let Some(body) = files.get(&path) else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                let start: usize = range
                    .as_deref()
                    .and_then(|r| r.strip_prefix("bytes="))
                    .and_then(|r| r.trim_end_matches('-').parse().ok())
                    .unwrap_or(0);
                let rest = &body[start.min(body.len())..];
                let head = if start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        rest.len(),
                        start,
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        rest.len()
                    )
                };
                let mut send = rest;
                if let Ok(mut c) = cut.lock()
                    && let Some(i) = c.iter().position(|p| *p == path)
                {
                    c.remove(i);
                    send = &rest[..rest.len() / 2];
                }
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(send);
                let _ = stream.flush();
            }
        });
        Ok(Self { port, log })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn ranges(&self, path: &str) -> Vec<Option<String>> {
        self.log
            .lock()
            .map(|l| {
                l.iter()
                    .filter(|(p, _)| p == path)
                    .map(|(_, r)| r.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

static N: AtomicUsize = AtomicUsize::new(0);

fn temp_dir() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!(
        "multi-models-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn blob(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn files_model(id: &str, dir: &str, base: &str, files: &[(&str, &[u8], Option<&str>)]) -> String {
    let entries: Vec<String> = files
        .iter()
        .map(|(p, data, sum)| {
            format!(
                "{{ path = \"{p}\", size = {}, sha256 = \"{}\" }}",
                data.len(),
                sum.map_or_else(|| sha(data), str::to_string)
            )
        })
        .collect();
    format!(
        "[[model]]\nid = \"{id}\"\nkind = \"vad\"\nbackend = \"sherpa-onnx\"\ndefault = true\nlicence = \"MIT\"\n\
         attribution = \"test\"\nvram_mb = 0\ndisk_mb = 1\ndir = \"{dir}\"\nbase_url = \"{base}\"\nfiles = [{}]\n",
        entries.join(", ")
    )
}

fn parts_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "part") {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn pull_verify_remove() -> Result<()> {
    let (a, b) = (blob(10_000, 1), blob(333, 2));
    let srv = Server::start(
        HashMap::from([
            ("/m/a.bin".into(), a.clone()),
            ("/m/b.txt".into(), b.clone()),
        ]),
        &[],
    )?;
    let base = format!("{}/m", srv.url());
    let reg = Registry::parse(&files_model(
        "fake",
        "fake/dir",
        &base,
        &[("a.bin", &a, None), ("b.txt", &b, None)],
    ))
    .map_err(anyhow::Error::msg)?;
    let root = temp_dir()?;
    models::pull(&reg, &[], &root)?;
    assert_eq!(std::fs::read(root.join("fake/dir/a.bin"))?, a);
    assert_eq!(std::fs::read(root.join("fake/dir/b.txt"))?, b);
    assert!(parts_in(&root).is_empty());
    let m = reg.get("fake").context("fake")?;
    assert!(m.installed(&root));
    assert_eq!(
        models::verify(&reg, &[], &root)?,
        vec![("fake".into(), Check::Ok)]
    );

    // A changed byte fails verify.
    let mut bad = b.clone();
    bad[0] ^= 1;
    std::fs::write(root.join("fake/dir/b.txt"), &bad)?;
    assert!(matches!(models::check_model(m, &root)?, Check::Failed(_)));

    models::remove(&reg, "fake", &root)?;
    assert!(!root.join("fake/dir").exists());
    assert_eq!(models::check_model(m, &root)?, Check::Missing);
    Ok(())
}

#[test]
fn checksum_mismatch_leaves_nothing() -> Result<()> {
    let a = blob(5_000, 3);
    let srv = Server::start(HashMap::from([("/a.bin".into(), a.clone())]), &[])?;
    let wrong = sha(b"something else");
    let reg = Registry::parse(&files_model(
        "fake",
        "d",
        &srv.url(),
        &[("a.bin", &a, Some(&wrong))],
    ))
    .map_err(anyhow::Error::msg)?;
    let root = temp_dir()?;
    let err = models::pull(&reg, &["fake".into()], &root).map_err(|e| format!("{e:#}"));
    assert!(err.is_err());
    assert!(!root.join("d/a.bin").exists());
    assert!(parts_in(&root).is_empty(), "{:?}", parts_in(&root));
    Ok(())
}

#[test]
fn interrupted_download_resumes() -> Result<()> {
    let a = blob(200_000, 4);
    let srv = Server::start(HashMap::from([("/a.bin".into(), a.clone())]), &["/a.bin"])?;
    let reg = Registry::parse(&files_model(
        "fake",
        "d",
        &srv.url(),
        &[("a.bin", &a, None)],
    ))
    .map_err(anyhow::Error::msg)?;
    let root = temp_dir()?;
    models::pull(&reg, &[], &root)?;
    assert_eq!(std::fs::read(root.join("d/a.bin"))?, a);
    let ranges = srv.ranges("/a.bin");
    assert_eq!(ranges.len(), 2, "{ranges:?}");
    assert_eq!(ranges[1], Some(format!("bytes={}-", a.len() / 2)));
    Ok(())
}

#[test]
fn partial_file_from_an_earlier_run_resumes() -> Result<()> {
    let a = blob(50_000, 5);
    let srv = Server::start(HashMap::from([("/a.bin".into(), a.clone())]), &[])?;
    let reg = Registry::parse(&files_model(
        "fake",
        "d",
        &srv.url(),
        &[("a.bin", &a, None)],
    ))
    .map_err(anyhow::Error::msg)?;
    let root = temp_dir()?;
    std::fs::create_dir_all(root.join("d"))?;
    std::fs::write(root.join("d/a.bin.part"), &a[..12_345])?;
    models::pull(&reg, &[], &root)?;
    assert_eq!(std::fs::read(root.join("d/a.bin"))?, a);
    assert_eq!(srv.ranges("/a.bin"), vec![Some("bytes=12345-".into())]);
    Ok(())
}

#[test]
fn archive_is_extracted_and_checked() -> Result<()> {
    let (x, y) = (blob(4_000, 6), blob(10, 7));
    let mut tar = tar::Builder::new(bzip2::write::BzEncoder::new(
        Vec::new(),
        bzip2::Compression::fast(),
    ));
    for (name, data) in [("pkg/x.onnx", &x), ("pkg/sub/y.txt", &y)] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, name, data.as_slice())?;
    }
    let archive = tar.into_inner()?.finish()?;
    let srv = Server::start(
        HashMap::from([("/pkg.tar.bz2".into(), archive.clone())]),
        &[],
    )?;
    let toml = format!(
        "[[model]]\nid = \"arc\"\nkind = \"asr\"\nbackend = \"sherpa-onnx\"\nlanguages = [\"en\"]\nlicence = \"MIT\"\n\
         attribution = \"test\"\nvram_mb = 0\ndisk_mb = 1\ndir = \"sherpa/arc\"\n\
         archive = {{ url = \"{}/pkg.tar.bz2\", size = {}, sha256 = \"{}\" }}\n\
         files = [{{ path = \"x.onnx\", size = {}, sha256 = \"{}\" }}, {{ path = \"sub/y.txt\", size = {}, sha256 = \"{}\" }}]\n",
        srv.url(),
        archive.len(),
        sha(&archive),
        x.len(),
        sha(&x),
        y.len(),
        sha(&y)
    );
    let reg = Registry::parse(&toml).map_err(anyhow::Error::msg)?;
    let root = temp_dir()?;
    models::pull(&reg, &["arc".into()], &root)?;
    assert_eq!(std::fs::read(root.join("sherpa/arc/sub/y.txt"))?, y);
    assert!(!root.join(".tmp/arc.tar.bz2").exists());
    assert_eq!(
        models::verify(&reg, &[], &root)?,
        vec![("arc".into(), Check::Ok)]
    );
    Ok(())
}

#[test]
fn verify_converted_model_against_manifest() -> Result<()> {
    let toml = "[[model]]\nid = \"opus-mt-en-xx\"\nkind = \"mt\"\nbackend = \"ctranslate2\"\nsource = \"en\"\n\
                targets = [\"xx\"]\nlicence = \"CC-BY-4.0\"\nattribution = \"test\"\nvram_mb = 1\ndisk_mb = 1\n\
                dir = \"ct2/opus-mt-en-xx\"\nconvert = { repo = \"Helsinki-NLP/opus-mt-en-xx\", revision = \"abc\" }\n";
    let reg = Registry::parse(toml).map_err(anyhow::Error::msg)?;
    let m = reg.get("opus-mt-en-xx").context("model")?;
    let root = temp_dir()?;
    assert_eq!(models::check_model(m, &root)?, Check::Missing);
    let dir = root.join("ct2/opus-mt-en-xx");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("model.bin"), blob(1000, 8))?;
    std::fs::write(dir.join("config.json"), "{}")?;
    assert!(m.installed(&root));
    // Copied in by hand: present but unverifiable.
    assert!(matches!(
        models::check_model(m, &root)?,
        Check::Unverified(_)
    ));
    models::write_manifest(m, "Helsinki-NLP/opus-mt-en-xx", "abc", &dir)?;
    assert_eq!(models::check_model(m, &root)?, Check::Ok);
    std::fs::write(dir.join("config.json"), "{\"x\":1}")?;
    assert!(matches!(models::check_model(m, &root)?, Check::Failed(_)));
    // Converted from another revision.
    models::write_manifest(m, "Helsinki-NLP/opus-mt-en-xx", "old", &dir)?;
    assert!(matches!(models::check_model(m, &root)?, Check::Failed(_)));
    assert!(dir.join(MANIFEST).is_file());
    Ok(())
}

#[test]
fn plain_http_is_refused() -> Result<()> {
    let root = temp_dir()?;
    let err = models::download("http://example.com/x", &root.join("x"), &sha(b""), 0);
    assert!(err.is_err());
    Ok(())
}

// ---------------------------------------------------------------- web API

mod api {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use multi::service::{Service, Workers};
    use multi::web::{AppState, router};
    use multi_core::Config;
    use tower::ServiceExt;

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        write_header: bool,
    ) -> Result<(StatusCode, serde_json::Value)> {
        let peer: std::net::SocketAddr = "127.0.0.1:50000".parse()?;
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .extension(axum::extract::ConnectInfo(peer))
            .header("host", "localhost");
        if write_header {
            req = req.header("x-multi", "1");
        }
        let resp = app.clone().oneshot(req.body(Body::empty())?).await?;
        let code = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        Ok((code, serde_json::from_slice(&bytes).unwrap_or_default()))
    }

    fn entry<'a>(list: &'a serde_json::Value, id: &str) -> Option<&'a serde_json::Value> {
        list["models"].as_array()?.iter().find(|m| m["id"] == id)
    }

    #[test]
    fn list_pull_verify_remove() -> Result<()> {
        let a = blob(200_000, 3);
        let srv = Server::start(HashMap::from([("/m/a.bin".into(), a.clone())]), &[])?;
        let root = temp_dir()?;
        let user = root.join("user-models.toml");
        let model = files_model(
            "fake",
            "fake/dir",
            &format!("{}/m", srv.url()),
            &[("a.bin", &a, None)],
        );
        // One good entry and one that escapes the models directory.
        let bad = model
            .replace("\"fake\"", "\"bad\"")
            .replace("fake/dir", "../x");
        std::fs::write(&user, format!("{model}\n{bad}"))?;
        let workers = Workers {
            asr: None,
            mt: None,
            models_dir: Some(root.clone()),
        };
        let cfg = Config::default();
        let service = Service::new(cfg.clone(), workers);
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let app = router(
            AppState::new(service, cfg, root.join("multi.toml"), None, rx).with_catalogue(user),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let (code, list) = call(&app, "GET", "/api/models", false).await?;
            assert_eq!(code, StatusCode::OK);
            let fake = entry(&list, "fake").context("fake not listed")?;
            assert_eq!(
                (fake["origin"].as_str(), fake["status"].as_str()),
                (Some("user"), Some("missing"))
            );
            assert!(entry(&list, "bad").is_none());
            assert_eq!(
                list["warnings"].as_array().map(Vec::len),
                Some(1),
                "{}",
                list["warnings"]
            );
            let es = entry(&list, "opus-mt-en-es").context("en-es")?;
            assert_eq!(es["formats"], serde_json::json!(["608", "708", "webvtt"]));
            assert_eq!(es["origin"], "builtin");
            let zh = entry(&list, "opus-mt-en-zh").context("en-zh")?;
            assert_eq!(zh["formats"], serde_json::json!(["webvtt"]));

            // Writes need X-Multi; DELETE too.
            let (code, _) = call(&app, "DELETE", "/api/models/fake", false).await?;
            assert_eq!(code, StatusCode::FORBIDDEN);
            let (code, _) = call(&app, "POST", "/api/models/nope/pull", true).await?;
            assert_eq!(code, StatusCode::NOT_FOUND);

            let (code, _) = call(&app, "POST", "/api/models/fake/pull", true).await?;
            assert_eq!(code, StatusCode::ACCEPTED);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let (_, list) = call(&app, "GET", "/api/models", false).await?;
                let fake = entry(&list, "fake").context("fake")?;
                if fake["status"] == "installed" && list["active"].is_null() {
                    assert_eq!(fake["size"], a.len());
                    break;
                }
                assert!(fake["error"].is_null(), "{}", fake["error"]);
                assert!(std::time::Instant::now() < deadline, "pull did not finish");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let (code, _) = call(&app, "POST", "/api/models/fake/pull", true).await?;
            assert_eq!(code, StatusCode::CONFLICT, "already installed");

            let (code, v) = call(&app, "POST", "/api/models/fake/verify", true).await?;
            assert_eq!((code, v["result"].as_str()), (StatusCode::OK, Some("ok")));
            std::fs::write(root.join("fake/dir/a.bin"), b"changed")?;
            let (_, v) = call(&app, "POST", "/api/models/fake/verify", true).await?;
            assert_eq!(v["result"], "failed");

            let (code, _) = call(&app, "DELETE", "/api/models/fake", true).await?;
            assert_eq!(code, StatusCode::OK);
            assert!(!root.join("fake/dir/a.bin").exists());
            let (code, _) = call(&app, "DELETE", "/api/models/fake", true).await?;
            assert_eq!(code, StatusCode::CONFLICT, "not installed");
            anyhow::Ok(())
        })
    }

    /// The models the running pipeline uses (what `DELETE` refuses): an
    /// installed translation model wins over the catalogue default.
    #[test]
    fn in_use_names_the_installed_models() -> Result<()> {
        let reg = Registry::builtin().map_err(anyhow::Error::msg)?;
        let root = temp_dir()?;
        let big = root.join("ct2/opus-mt-tc-big-en-es");
        std::fs::create_dir_all(&big)?;
        std::fs::write(big.join("model.bin"), b"x")?;
        std::fs::write(big.join("config.json"), b"{}")?;
        let used = models::in_use(&reg, &Config::default(), &root);
        assert!(
            used.contains(&"opus-mt-tc-big-en-es".to_string()),
            "{used:?}"
        );
        assert!(!used.contains(&"opus-mt-en-es".to_string()));
        assert!(used.contains(&"silero-vad".to_string()));
        assert!(used.contains(&"opus-mt-en-fr".to_string()));
        Ok(())
    }
}
