# Docs index

Every doc in the repo, with its read cost. Scan this first, then open only what you need: summaries (L2) before detail (L3), raw evidence (L4) only when a finding is in doubt. `~Tokens` is maintained by `scripts/docs-index.sh`; don't edit it by hand.

Types: `summary` overview page · `spec` requirements · `decision` ADR · `finding` spike result · `benchmark` measured numbers · `gotcha` trap to avoid · `how-it-works` explanation · `guide` how-to · `evidence` raw logs/data.

## Start here

| ID | Type | Title | Status | ~Tokens |
|---|---|---|---|---|
| G-01 | guide | [Agent guide: rules and how to read docs](../CLAUDE.md) | current | ~404 |
| G-02 | summary | [README: what MULTI is, licence](../README.md) | current | ~472 |

## Product (PRD)

| ID | Type | Title | Status | ~Tokens |
|---|---|---|---|---|
| P-00 | summary | [PRD summary with links to every section](../PRD.md) | current | ~536 |
| P-01 | spec | [Overview: problem, solution, local vs cloud](prd/01-overview.md) | current | ~629 |
| P-02 | spec | [Goals, non-goals, reliability-first principle](prd/02-goals-and-non-goals.md) | current | ~483 |
| P-03 | spec | [Target users and key user stories](prd/03-target-users-and-use-cases.md) | current | ~410 |
| P-04 | spec | [Functional requirements by ID and priority](prd/04-functional-requirements.md) | current | ~1655 |
| P-05 | spec | [Configuration settings, defaults and presets](prd/05-configuration-and-tuning.md) | current | ~764 |
| P-06 | spec | [Latency budget, platforms, reliability, security](prd/06-non-functional-requirements.md) | current | ~886 |
| P-07 | spec | [Architecture, candidate libraries and Rust crates](prd/07-proposed-architecture-and-technology.md) | current | ~1004 |
| P-08 | spec | [Licensing, distribution, paid binaries, warranty draft](prd/08-licensing-distribution-and-business-model.md) | current | ~752 |
| P-09 | spec | [Milestones M0 to v1.0](prd/09-milestones.md) | current | ~409 |
| P-10 | spec | [Risks, mitigations and open questions](prd/10-risks-and-open-questions.md) | current | ~478 |

## Decisions

| ID | Type | Title | Status | ~Tokens |
|---|---|---|---|---|
| D-00 | summary | [All decisions, newest first](decisions/README.md) | current | ~237 |
| D-01 | decision | [ADR-0001: build in Rust](decisions/ADR-0001-rust.md) | accepted | ~355 |
| D-02 | decision | [ADR-0002: GPL-3.0 with trademarked name](decisions/ADR-0002-gpl-3.md) | accepted | ~270 |
| D-03 | decision | [ADR-0003: GStreamer caption encoders, SEI in display order](decisions/ADR-0003-caption-insertion.md) | accepted | ~839 |
| D-04 | decision | [ADR-0004: pipeline base GStreamer vs FFmpeg (proposed)](decisions/ADR-0004-pipeline-base.md) | proposed | ~1133 |
| D-05 | decision | [ADR-0005: Nemotron streaming ASR default, Whisper turbo option](decisions/ADR-0005-asr-backend.md) | accepted | ~466 |
| D-06 | decision | [ADR-0006: CTranslate2 + opus-mt translation, LLMs optional](decisions/ADR-0006-translation-backend.md) | accepted | ~431 |
| D-T | guide | [ADR template](decisions/TEMPLATE.md) | current | ~132 |

## M0 spikes

| ID | Type | Title | Status | ~Tokens |
|---|---|---|---|---|
| M-00 | summary | [M0 overview: spikes, harness, log](m0/README.md) | current | ~1074 |
| F-S3 | finding | [Rust 608/708 encoder decodes in FFmpeg, ccextractor, libcaption](m0/findings/S3-caption-encoder.md) | current | ~1894 |
| E-S3 | evidence | [S3 raw outputs](m0/evidence/S3) | current | ~4625 |
| H-01 | how-it-works | [How the latency tool measures video delay and caption lag](m0/findings/H-latency-tool.md) | current | ~1698 |
| E-H | evidence | [Latency tool self-test outputs](m0/evidence/H) | current | ~7196 |
| F-S2 | finding | [GStreamer SRT/UDP pass-through with SEI captions: works, 33 ms](m0/findings/S2-gstreamer-pipeline.md) | current | ~2331 |
| E-S2 | evidence | [S2 raw outputs](m0/evidence/S2) | current | ~16643 |
| F-S1 | finding | [FFmpeg pass-through with caption SEI: works, one-frame latency](m0/findings/S1-ffmpeg-pipeline.md) | current | ~2336 |
| E-S1 | evidence | [S1 raw outputs](m0/evidence/S1) | current | ~7902 |
| F-S2b | finding | [GStreamer caption encoders: per-line control, no added delay, use them](m0/findings/S2b-gstreamer-caption-encoders.md) | current | ~1667 |
| E-S2b | evidence | [S2b raw outputs](m0/evidence/S2b) | current | ~7395 |
| F-S5 | finding | [Local MT via CTranslate2 opus-mt: P95 34 ms, 4 languages](m0/findings/S5-translation.md) | current | ~1432 |
| E-S5 | evidence | [S5 raw outputs](m0/evidence/S5) | current | ~30755 |
| F-S4 | finding | [Nemotron 3.5 streaming beats Whisper turbo on lag and hallucinations](m0/findings/S4-streaming-asr.md) | current | ~1722 |
| E-S4 | evidence | [S4 raw outputs](m0/evidence/S4) | current | ~10875 |
