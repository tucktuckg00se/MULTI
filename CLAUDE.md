# MULTI — agent guide

MULTI is a Rust service that captions live video streams with local AI and embeds the captions without re-encoding. Current stage: **M0 spikes** (throwaway experiments in `spikes/`); there is no product code yet.

## Reading the docs

Docs use progressive disclosure. **Start at [docs/INDEX.md](docs/INDEX.md)**: every doc is listed with a type, a one-line title and its read cost in ~tokens. Open only what the task needs; go from summary pages (L2) to detail files (L3), and read raw evidence (L4, `docs/**/evidence/`) only when a finding is in doubt.

## Writing docs

- One topic per file. Every detail file opens with `> **Summary:**` in 2–3 lines.
- Add a row to `docs/INDEX.md` for every new doc or evidence folder, then run `scripts/docs-index.sh` (fills in ~Tokens). `scripts/docs-index.sh --check` must pass before committing.
- Type tags: `summary`, `spec`, `decision`, `finding`, `benchmark`, `gotcha`, `how-it-works`, `guide`, `evidence`.
- Evidence folders hold small text only (logs, CSV, ffprobe output). Media goes in gitignored `spikes/harness/media/`.

## Working rules

- **Reliability first, speed second.** Never let caption work interrupt video. No `unwrap`/`expect`/panics on the video path.
- GPU benchmarks run under `flock /tmp/multi-gpu.lock <cmd>` so parallel work doesn't skew numbers.
- Never commit secrets (stream keys, SRT passphrases); pass them via environment variables.
- Licences matter: no GPL-only/nonfree FFmpeg parts in shipped code, no non-commercial models (NLLB-200, SeamlessM4T). Record every model's licence.
- Linux x86-64 only during M0.
