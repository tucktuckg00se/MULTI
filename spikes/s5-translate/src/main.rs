//! S5 spike: live clause translation with small local models.
//! See docs/m0/findings/S5-translation.md.

mod asrload;
mod llm;

use s5_translate::{common, mt};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "S5: translate caption clauses with llama.cpp or CTranslate2 and time it")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Small LLM via llama.cpp (GGUF).
    Llm(llm::LlmArgs),
    /// Dedicated MT model via CTranslate2.
    Mt(mt::MtArgs),
    /// Synthetic ASR GPU load (Whisper via CTranslate2) until killed.
    Asrload(asrload::AsrArgs),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Llm(a) => llm::run(a),
        Cmd::Mt(a) => mt::run(a),
        Cmd::Asrload(a) => asrload::run(a),
    }
}
