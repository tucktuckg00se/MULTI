//! Small instruction/translation LLMs through llama.cpp (llama-cpp-2, CUDA).
//!
//! Modes for N target languages per clause:
//! - `single`: one context, languages one after another (each timed alone).
//! - `batched`: one context, N sequences decoded together in one llama_batch per step.
//! - `threads`: N contexts on N threads sharing the model weights.
//! - `oneprompt`: one request asking for all N languages as `XX: ...` lines.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use crate::common::{self, CommonArgs, Job, Out, lang_name};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum Family {
    /// Tencent Hy-MT2 (hunyuan-dense), official translation prompt.
    Hymt,
    /// Qwen3.5 (ChatML, thinking disabled).
    Qwen35,
    /// Gemma 4 E2B/E4B (<|turn> format, thinking off).
    Gemma4,
    /// Gemma 4 26B/31B: template pre-fills an empty thought channel.
    Gemma4big,
    /// EuroLLM Instruct (ChatML with empty system).
    Eurollm,
    /// TranslateGemma (Gemma 3 format, fixed translator prompt).
    Tgemma,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Single,
    Batched,
    Threads,
    Oneprompt,
}

#[derive(clap::Args, Debug)]
pub struct LlmArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// GGUF file.
    #[arg(long)]
    pub model: PathBuf,
    #[arg(long, value_enum)]
    pub family: Family,
    #[arg(long, value_enum, default_value = "batched")]
    pub mode: Mode,
    /// Layers on GPU (0 = CPU only).
    #[arg(long, default_value_t = 999)]
    pub ngl: u32,
    /// CPU threads (for --ngl 0).
    #[arg(long, default_value_t = 8)]
    pub threads: i32,
    /// Use the generic prompt instead of a model's own recommended one.
    #[arg(long)]
    pub generic_prompt: bool,
    /// Print the first prompt and its tokens, then exit.
    #[arg(long)]
    pub show_prompt: bool,
}

// ---------- prompts ----------

fn instruction(family: Family, generic: bool, lang: &str, text: &str, prev: &[&str]) -> String {
    let name = lang_name(lang);
    let hy = family == Family::Hymt && !generic;
    match (hy, prev.is_empty()) {
        // Hy-MT2 README "Default Translation" prompt.
        (true, true) => format!(
            "Translate the following text into {name}. Note that you should only output the translated result without any additional explanation:\n\n{text}"
        ),
        // Hy-MT2 README "Structured Data 2" (background information) prompt.
        (true, false) => format!(
            "[Background Information]\n{}\n\nPlease translate the following text into {name}, taking the provided background information into consideration. Only output the translated result without any additional explanation.\n\n[Source Text]\n{text}",
            prev.join(" ")
        ),
        (false, true) => format!(
            "Translate the following English text into {name}. Output only the translation, with no explanations, notes or quotation marks. If the text is an unfinished fragment, translate it as a fragment and do not complete it.\n\n{text}"
        ),
        (false, false) => format!(
            "Previous sentences, for context only (do not translate them):\n{}\n\nTranslate the following English text into {name}. Output only the translation, with no explanations, notes or quotation marks. If the text is an unfinished fragment, translate it as a fragment and do not complete it.\n\n{text}",
            prev.join(" ")
        ),
    }
}

fn oneprompt_instruction(langs: &[String], text: &str, prev: &[&str]) -> String {
    let names: Vec<&str> = langs.iter().map(|l| lang_name(l)).collect();
    let fmt: Vec<String> = langs
        .iter()
        .map(|l| format!("{}: <{}>", l.to_uppercase(), lang_name(l)))
        .collect();
    let ctx = if prev.is_empty() {
        String::new()
    } else {
        format!(
            "Previous sentences, for context only (do not translate them):\n{}\n\n",
            prev.join(" ")
        )
    };
    format!(
        "{ctx}Translate the following English text into {}. Output exactly {} lines and nothing else, in this format:\n{}\nIf the text is an unfinished fragment, translate it as a fragment and do not complete it.\n\nText: {text}",
        names.join(", "),
        langs.len(),
        fmt.join("\n")
    )
}

/// Wraps a user message in the model's chat format (BOS is added by the tokenizer).
fn chat(family: Family, user: &str) -> String {
    match family {
        Family::Hymt => format!("<｜hy_begin▁of▁sentence｜><｜hy_User｜>{user}<｜hy_Assistant｜>"),
        Family::Qwen35 => format!(
            "<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
        ),
        Family::Gemma4 => format!("<|turn>user\n{user}<turn|>\n<|turn>model\n"),
        Family::Gemma4big => {
            format!("<|turn>user\n{user}<turn|>\n<|turn>model\n<|channel>thought\n<channel|>")
        }
        Family::Eurollm => format!(
            "<|im_start|>system\n<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
        ),
        Family::Tgemma => format!("<start_of_turn>user\n{user}<end_of_turn>\n<start_of_turn>model\n"),
    }
}

fn tgemma_user(lang: &str, text: &str) -> String {
    let n = lang_name(lang);
    format!(
        "You are a professional English (en) to {n} ({lang}) translator. Your goal is to accurately convey the meaning and nuances of the original English text while adhering to {n} grammar, vocabulary, and cultural sensitivities.\nProduce only the {n} translation, without any additional explanations or commentary. Please translate the following English text into {n}:\n\n\n{text}"
    )
}

// ---------- generation ----------

/// One sequence to generate.
struct Req {
    tokens: Vec<LlamaToken>,
    max_new: usize,
    /// Stop after this many completed non-empty lines.
    max_lines: usize,
}

#[derive(Default, Clone)]
struct Gen {
    bytes: Vec<u8>,
    n_out: usize,
    ttft_ms: Option<f64>,
    done_ms: f64,
    /// Time each completed line ended (for oneprompt).
    line_ms: Vec<f64>,
    cap: String,
}

struct SeqState {
    g: Gen,
    pos: i32,
    logit_idx: i32,
    done: bool,
    lines: usize,
    line_has_text: bool,
}

fn piece(model: &LlamaModel, t: LlamaToken) -> Vec<u8> {
    match model.token_to_piece_bytes(t, 64, false, None) {
        Ok(b) => b,
        Err(_) => model
            .token_to_piece_bytes(t, 1024, false, None)
            .unwrap_or_default(),
    }
}

/// Decodes several independent sequences together (one sequence = plain decode).
fn generate(
    ctx: &mut LlamaContext,
    model: &LlamaModel,
    reqs: &[Req],
    max_ms: u64,
    t0: Instant,
) -> Result<Vec<Gen>> {
    ctx.clear_kv_cache();
    let total: usize = reqs.iter().map(|r| r.tokens.len()).sum();
    let n_batch = ctx.n_batch() as usize;
    if total > n_batch {
        bail!("prompts ({total} tokens) exceed n_batch {n_batch}");
    }
    let mut batch = LlamaBatch::new(n_batch.max(reqs.len()), reqs.len() as i32);
    let mut st: Vec<SeqState> = Vec::with_capacity(reqs.len());
    for (s, r) in reqs.iter().enumerate() {
        let last = r.tokens.len() - 1;
        for (i, &t) in r.tokens.iter().enumerate() {
            batch.add(t, i as i32, &[s as i32], i == last)?;
        }
        st.push(SeqState {
            g: Gen::default(),
            pos: r.tokens.len() as i32,
            logit_idx: batch.n_tokens() - 1,
            done: false,
            lines: 0,
            line_has_text: false,
        });
    }
    ctx.decode(&mut batch).context("prefill decode")?;
    let mut sampler = LlamaSampler::greedy();
    loop {
        let now_ms = t0.elapsed().as_secs_f64() * 1e3;
        let over = now_ms > max_ms as f64;
        batch.clear();
        for (s, (state, r)) in st.iter_mut().zip(reqs).enumerate() {
            if state.done {
                continue;
            }
            if over {
                state.done = true;
                state.g.cap = "deadline".into();
                state.g.done_ms = now_ms;
                continue;
            }
            let tok = sampler.sample(ctx, state.logit_idx);
            let t_ms = t0.elapsed().as_secs_f64() * 1e3;
            if state.g.ttft_ms.is_none() {
                state.g.ttft_ms = Some(t_ms);
            }
            if model.is_eog_token(tok) {
                state.done = true;
                state.g.done_ms = t_ms;
                continue;
            }
            state.g.n_out += 1;
            let p = piece(model, tok);
            for &b in &p {
                if b == b'\n' {
                    if state.line_has_text {
                        state.lines += 1;
                        state.g.line_ms.push(t_ms);
                    }
                    state.line_has_text = false;
                } else if !b.is_ascii_whitespace() {
                    state.line_has_text = true;
                }
            }
            state.g.bytes.extend_from_slice(&p);
            if state.lines >= r.max_lines {
                state.done = true;
                state.g.done_ms = t_ms;
                continue;
            }
            if state.g.n_out >= r.max_new {
                state.done = true;
                state.g.cap = "max_tokens".into();
                state.g.done_ms = t_ms;
                continue;
            }
            batch.add(tok, state.pos, &[s as i32], true)?;
            state.pos += 1;
            state.logit_idx = batch.n_tokens() - 1;
        }
        if batch.n_tokens() == 0 {
            break;
        }
        ctx.decode(&mut batch).context("step decode")?;
    }
    Ok(st.into_iter().map(|s| s.g).collect())
}

fn clean(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    // Strip one layer of wrapping quotes if the model added them.
    let s = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s);
    s.trim().to_string()
}

/// Output token cap: generous for real translations, short enough to stop runaways.
fn max_new(src_tokens: usize, n_langs: usize) -> usize {
    (src_tokens * 3 + 16).min(200) * n_langs
}

// ---------- entry point ----------

pub fn run(a: LlmArgs) -> Result<()> {
    let mut backend = LlamaBackend::init()?;
    backend.void_logs();
    let t_load = Instant::now();
    let mparams = LlamaModelParams::default().with_n_gpu_layers(a.ngl);
    let model = LlamaModel::load_from_file(&backend, &a.model, &mparams)
        .map_err(|e| anyhow!("load {}: {e}", a.model.display()))?;
    let langs = a.common.langs.clone();
    let n_seq: u32 = match a.mode {
        Mode::Batched => langs.len() as u32,
        _ => 1,
    };
    let cparams = || {
        LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(1024 * n_seq))
            .with_n_batch(2048)
            .with_n_ubatch(512)
            .with_n_seq_max(n_seq)
            .with_n_threads(a.threads)
            .with_n_threads_batch(a.threads)
    };
    let family = a.family;
    let generic = a.generic_prompt;

    let tokenize = |s: &str| -> Result<Vec<LlamaToken>> {
        Ok(model.str_to_token(s, AddBos::Always)?)
    };
    let build = |lang: &str, job: &Job| -> Result<Req> {
        let user = if family == Family::Tgemma {
            tgemma_user(lang, job.text)
        } else {
            instruction(family, generic, lang, job.text, &job.prev)
        };
        let src_n = model.str_to_token(job.text, AddBos::Never)?.len();
        Ok(Req {
            tokens: tokenize(&chat(family, &user))?,
            max_new: max_new(src_n, 1),
            max_lines: 1,
        })
    };

    if a.show_prompt {
        let clauses = common::read_clauses(&a.common.input)?;
        let prev: Vec<&str> = if a.common.context > 0 {
            vec!["Previous clause one.", "Previous clause two."]
        } else {
            vec![]
        };
        let job = Job {
            text: &clauses[0].text,
            prev,
            langs: &langs,
            max_ms: a.common.max_ms,
        };
        let r = build(&langs[0], &job)?;
        let shown: Vec<String> = r
            .tokens
            .iter()
            .map(|&t| String::from_utf8_lossy(&model.token_to_piece_bytes(t, 256, true, None).unwrap_or_default()).into_owned())
            .collect();
        println!("{} tokens: {:?}", r.tokens.len(), shown);
        return Ok(());
    }

    let mut ctx = model
        .new_context(&backend, cparams())
        .map_err(|e| anyhow!("context: {e}"))?;
    let load_ms = t_load.elapsed().as_secs_f64() * 1e3;

    let summary = match a.mode {
        Mode::Single => common::run(&a.common, load_ms, |job| {
            let mut outs = Vec::new();
            for l in job.langs {
                let r = build(l, job)?;
                let t0 = Instant::now();
                let g = generate(&mut ctx, &model, std::slice::from_ref(&r), job.max_ms, t0)?;
                outs.push(to_out(l, &g[0]));
            }
            Ok(outs)
        })?,
        Mode::Batched => common::run(&a.common, load_ms, |job| {
            let reqs = job
                .langs
                .iter()
                .map(|l| build(l, job))
                .collect::<Result<Vec<_>>>()?;
            let t0 = Instant::now();
            let gens = generate(&mut ctx, &model, &reqs, job.max_ms, t0)?;
            Ok(job.langs.iter().zip(&gens).map(|(l, g)| to_out(l, g)).collect())
        })?,
        Mode::Oneprompt => common::run(&a.common, load_ms, |job| {
            let user = oneprompt_instruction(job.langs, job.text, &job.prev);
            let src_n = model.str_to_token(job.text, AddBos::Never)?.len();
            let r = Req {
                tokens: tokenize(&chat(family, &user))?,
                max_new: max_new(src_n + 3, job.langs.len()),
                max_lines: job.langs.len(),
            };
            let t0 = Instant::now();
            let g = generate(&mut ctx, &model, std::slice::from_ref(&r), job.max_ms, t0)?;
            Ok(split_oneprompt(job.langs, &g[0]))
        })?,
        Mode::Threads => {
            drop(ctx);
            std::thread::scope(|s| -> Result<common::Summary> {
                let mut txs = Vec::new();
                let (rtx, rrx) = mpsc::channel::<(usize, Result<Gen>)>();
                for w in 0..langs.len() {
                    let (tx, rx) = mpsc::channel::<(Req, u64, Instant)>();
                    txs.push(tx);
                    let rtx = rtx.clone();
                    let model = &model;
                    let backend = &backend;
                    let p = cparams();
                    s.spawn(move || {
                        let mut ctx = match model.new_context(backend, p) {
                            Ok(c) => c,
                            Err(e) => {
                                let _ = rtx.send((w, Err(anyhow!("context: {e}"))));
                                return;
                            }
                        };
                        while let Ok((r, max_ms, t0)) = rx.recv() {
                            let g = generate(&mut ctx, model, std::slice::from_ref(&r), max_ms, t0)
                                .map(|mut v| v.remove(0));
                            if rtx.send((w, g)).is_err() {
                                return;
                            }
                        }
                    });
                }
                let res = common::run(&a.common, load_ms, |job| {
                    let reqs = job
                        .langs
                        .iter()
                        .map(|l| build(l, job))
                        .collect::<Result<Vec<_>>>()?;
                    let t0 = Instant::now();
                    for (tx, r) in txs.iter().zip(reqs) {
                        tx.send((r, job.max_ms, t0)).map_err(|_| anyhow!("worker gone"))?;
                    }
                    let mut gens = vec![Gen::default(); job.langs.len()];
                    for _ in 0..job.langs.len() {
                        let (w, g) = rrx.recv()?;
                        gens[w] = g?;
                    }
                    Ok(job.langs.iter().zip(&gens).map(|(l, g)| to_out(l, g)).collect())
                });
                drop(txs);
                res
            })?
        }
    };
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

fn to_out(lang: &str, g: &Gen) -> Out {
    Out {
        lang: lang.to_string(),
        text: clean(&g.bytes),
        ms: g.done_ms,
        ttft_ms: g.ttft_ms,
        n_out: g.n_out,
        cap: g.cap.clone(),
    }
}

/// Splits `ES: ...\nFR: ...` output; each language's latency is when its line ended.
fn split_oneprompt(langs: &[String], g: &Gen) -> Vec<Out> {
    let text = String::from_utf8_lossy(&g.bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    langs
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let tag = format!("{}:", l.to_uppercase());
            let line = lines
                .iter()
                .find(|x| x.trim_start().starts_with(&tag))
                .map(|x| x.trim_start()[tag.len()..].trim().to_string());
            let ms = g.line_ms.get(i).copied().unwrap_or(g.done_ms);
            Out {
                lang: l.clone(),
                text: line.unwrap_or_default(),
                ms,
                ttft_ms: g.ttft_ms,
                n_out: if i == 0 { g.n_out } else { 0 },
                cap: g.cap.clone(),
            }
        })
        .collect()
}
