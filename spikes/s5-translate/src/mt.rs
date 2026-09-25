//! Dedicated MT models through CTranslate2 (ct2rs).
//!
//! - `opus`: one Marian model per language pair (`<dir>/opus-mt-en-<lang>` or
//!   `<dir>/opus-mt-tc-big-en-<lang>`); `threads` mode runs the pairs in parallel.
//! - `m2m`: M2M-100, one model, target chosen by a `__xx__` target prefix;
//!   `batch` mode translates all languages as one batch.
//! - `madlad`: MADLAD-400, one model, target chosen by a `<2xx>` source tag.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use ct2rs::tokenizers::sentencepiece::Tokenizer as SpTokenizer;
use ct2rs::{ComputeType, Config, Device, TranslationOptions, Translator};
use sentencepiece::SentencePieceProcessor;

use crate::common::{self, CommonArgs, Job, Out};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Opus,
    M2m,
    Madlad,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Languages one after another.
    Single,
    /// One thread per language (opus: one model each; m2m/madlad: shared model).
    Threads,
    /// All languages as one batch (m2m/madlad only).
    Batch,
}

#[derive(clap::Args, Debug)]
pub struct MtArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[arg(long, value_enum)]
    pub kind: Kind,
    /// opus: directory holding opus-mt-* models; m2m/madlad: the model directory.
    #[arg(long)]
    pub model: PathBuf,
    #[arg(long, value_enum, default_value = "threads")]
    pub mode: Mode,
    #[arg(long, default_value_t = 1)]
    pub beam: usize,
    /// Run on CPU (int8).
    #[arg(long)]
    pub cpu: bool,
    /// CPU threads per translator.
    #[arg(long, default_value_t = 4)]
    pub threads: usize,
    /// Split multi-sentence clauses and translate the sentences as one batch
    /// (Marian drops trailing sentences otherwise).
    #[arg(long)]
    pub split: bool,
    /// GPU compute type: float16, int8_float16, bfloat16, int8_bfloat16, int8.
    #[arg(long, default_value = "float16")]
    pub compute: String,
}

/// SentencePiece tokenizer with a fixed source prefix token (M2M-100: `__en__`).
struct PrefixSp {
    sp: SentencePieceProcessor,
    prefix: Option<String>,
}

impl ct2rs::Tokenizer for PrefixSp {
    fn encode(&self, input: &str) -> Result<Vec<String>> {
        let mut v: Vec<String> = self.prefix.iter().cloned().collect();
        v.extend(self.sp.encode(input)?.into_iter().map(|p| p.piece));
        v.push("</s>".into());
        Ok(v)
    }
    fn decode(&self, tokens: Vec<String>) -> Result<String> {
        Ok(self.sp.decode_pieces(&tokens)?)
    }
}

enum Tr {
    Opus(Vec<Translator<SpTokenizer>>),
    Multi(Translator<PrefixSp>),
}

fn config(a: &MtArgs) -> Result<Config> {
    let compute_type = if a.cpu {
        ComputeType::INT8
    } else {
        match a.compute.as_str() {
            "float16" => ComputeType::FLOAT16,
            "int8_float16" => ComputeType::INT8_FLOAT16,
            "int8" => ComputeType::INT8,
            "bfloat16" => ComputeType::BFLOAT16,
            "int8_bfloat16" => ComputeType::INT8_BFLOAT16,
            o => bail!("unknown compute type {o}"),
        }
    };
    Ok(Config {
        device: if a.cpu { Device::CPU } else { Device::CUDA },
        compute_type,
        num_threads_per_replica: a.threads,
        ..Default::default()
    })
}

pub fn opus_dir(root: &Path, lang: &str) -> Result<PathBuf> {
    for name in [format!("opus-mt-en-{lang}"), format!("opus-mt-tc-big-en-{lang}")] {
        let p = root.join(name);
        if p.is_dir() {
            return Ok(p);
        }
    }
    bail!("no opus-mt model for en-{lang} under {}", root.display())
}

pub fn options(beam: usize, src_words: usize) -> TranslationOptions<String, String> {
    TranslationOptions {
        beam_size: beam,
        // Cap decode length: ~3 subwords per source word + slack.
        max_decoding_length: (src_words * 4 + 16).min(256),
        repetition_penalty: 1.0,
        ..Default::default()
    }
}

/// Splits after . ! ? when followed by whitespace and an upper-case or
/// opening-punctuation character; skips common abbreviations.
pub fn sentences(text: &str) -> Vec<&str> {
    const ABBR: [&str; 8] = ["Mr.", "Mrs.", "Ms.", "Dr.", "St.", "Jr.", "a.m.", "p.m."];
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for w in 0..chars.len() {
        let (i, c) = chars[w];
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let Some(&(j, ws)) = chars.get(w + 1) else { continue };
        let Some(&(_, next)) = chars.get(w + 2) else { continue };
        if !ws.is_whitespace() || !(next.is_uppercase() || matches!(next, '¿' | '¡' | '"' | '“')) {
            continue;
        }
        let head = &text[start..=i];
        if ABBR.iter().any(|a| head.ends_with(a)) {
            continue;
        }
        out.push(head.trim());
        start = j;
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

pub fn run(a: MtArgs) -> Result<()> {
    let langs = a.common.langs.clone();
    let t_load = Instant::now();
    let cfg = config(&a)?;
    let tr = match a.kind {
        Kind::Opus => {
            let mut v = Vec::new();
            for l in &langs {
                let d = opus_dir(&a.model, l)?;
                v.push(Translator::with_tokenizer(&d, SpTokenizer::new(&d)?, &cfg)?);
            }
            Tr::Opus(v)
        }
        Kind::M2m => {
            let sp = SentencePieceProcessor::open(a.model.join("sentencepiece.bpe.model"))?;
            Tr::Multi(Translator::with_tokenizer(
                &a.model,
                PrefixSp { sp, prefix: Some("__en__".into()) },
                &cfg,
            )?)
        }
        Kind::Madlad => {
            let sp = SentencePieceProcessor::open(a.model.join("spiece.model"))?;
            Tr::Multi(Translator::with_tokenizer(&a.model, PrefixSp { sp, prefix: None }, &cfg)?)
        }
    };
    let load_ms = t_load.elapsed().as_secs_f64() * 1e3;
    if a.mode == Mode::Batch && matches!(tr, Tr::Opus(_)) {
        bail!("batch mode needs one multilingual model (m2m/madlad)");
    }
    if a.common.context > 0 {
        eprintln!("note: MT models take no context; --context is ignored");
    }
    let kind = a.kind;
    let beam = a.beam;
    let split = a.split;
    if split && a.mode == Mode::Batch {
        bail!("--split is implemented for single/threads modes");
    }

    // Translate `text` into `langs[idx]` (one sentence).
    let one = |idx: usize, text: &str, t0: Instant| -> Result<Out> {
        let words = text.split_whitespace().count();
        let opts = options(beam, words);
        let mut first: Option<f64> = None;
        let mut steps = 0usize;
        let mut cb = |_r: ct2rs::GenerationStepResult| -> Result<()> {
            if first.is_none() {
                first = Some(t0.elapsed().as_secs_f64() * 1e3);
            }
            steps += 1;
            Ok(())
        };
        let cb_opt: Option<&mut dyn FnMut(ct2rs::GenerationStepResult) -> Result<()>> =
            if beam == 1 { Some(&mut cb) } else { None };
        let lang = &langs[idx];
        let parts: Vec<&str> = if split { sentences(text) } else { vec![text] };
        let joined = |r: Vec<(String, Option<f32>)>| {
            let v: Vec<String> = r.into_iter().map(|x| x.0.trim().to_string()).collect();
            (!v.is_empty()).then(|| v.join(" "))
        };
        let out = match (&tr, kind) {
            (Tr::Opus(v), _) => joined(v[idx].translate_batch(&parts, &opts, cb_opt)?),
            (Tr::Multi(t), Kind::M2m) => {
                let pre = vec![vec![format!("__{lang}__")]; parts.len()];
                joined(t.translate_batch_with_target_prefix(&parts, &pre, &opts, cb_opt)?)
            }
            (Tr::Multi(t), _) => {
                let tagged: Vec<String> = parts.iter().map(|p| format!("<2{lang}> {p}")).collect();
                joined(t.translate_batch(&tagged, &opts, cb_opt)?)
            }
        };
        let out = out.ok_or_else(|| anyhow!("no output"))?;
        let n_out = steps;
        let cap = if n_out >= opts.max_decoding_length * parts.len() {
            "max_tokens".to_string()
        } else {
            String::new()
        };
        Ok(Out {
            lang: lang.clone(),
            text: out.trim().to_string(),
            ms: t0.elapsed().as_secs_f64() * 1e3,
            ttft_ms: first,
            n_out,
            cap,
        })
    };

    let summary = match a.mode {
        Mode::Single => common::run(&a.common, load_ms, |job: &Job| {
            (0..job.langs.len())
                .map(|i| one(i, job.text, Instant::now()))
                .collect()
        })?,
        Mode::Threads => common::run(&a.common, load_ms, |job: &Job| {
            let t0 = Instant::now();
            std::thread::scope(|s| {
                let hs: Vec<_> = (0..job.langs.len())
                    .map(|i| {
                        let one = &one;
                        let text = job.text;
                        s.spawn(move || one(i, text, t0))
                    })
                    .collect();
                hs.into_iter()
                    .map(|h| h.join().map_err(|_| anyhow!("worker panicked"))?)
                    .collect()
            })
        })?,
        Mode::Batch => {
            let Tr::Multi(t) = &tr else { unreachable!() };
            common::run(&a.common, load_ms, |job: &Job| {
                let t0 = Instant::now();
                let words = job.text.split_whitespace().count();
                let opts = options(beam, words);
                let res = if kind == Kind::M2m {
                    let src = vec![job.text; job.langs.len()];
                    let pre: Vec<Vec<String>> =
                        job.langs.iter().map(|l| vec![format!("__{l}__")]).collect();
                    t.translate_batch_with_target_prefix(&src, &pre, &opts, None)?
                } else {
                    let src: Vec<String> =
                        job.langs.iter().map(|l| format!("<2{l}> {}", job.text)).collect();
                    t.translate_batch(&src, &opts, None)?
                };
                let ms = t0.elapsed().as_secs_f64() * 1e3;
                Ok(job
                    .langs
                    .iter()
                    .zip(res)
                    .map(|(l, (text, _))| Out {
                        lang: l.clone(),
                        text: text.trim().to_string(),
                        ms,
                        ttft_ms: None,
                        n_out: 0,
                        cap: String::new(),
                    })
                    .collect())
            })?
        }
    };
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sentences;

    #[test]
    fn splits_sentences_not_abbreviations() {
        assert_eq!(
            sentences("Cold, is it, my darling? Bless your sweet face."),
            ["Cold, is it, my darling?", "Bless your sweet face."]
        );
        assert_eq!(
            sentences("Thank you, Mr. Chen. We'll ask staff."),
            ["Thank you, Mr. Chen.", "We'll ask staff."]
        );
        assert_eq!(sentences("Version 2.4 fixes it"), ["Version 2.4 fixes it"]);
        assert_eq!(sentences("and then we're going to"), ["and then we're going to"]);
    }
}
