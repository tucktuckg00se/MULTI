# MULTI — Product Requirements Document

**M**ultilingual **U**nified **L**ive **T**ranscription & **I**nsertion

Last updated: 2026-09-22

MULTI is a self-hosted, open-source inline box: a live SRT/RTP stream comes in, local AI models caption its audio in several languages at once, the captions are embedded into the video without re-encoding, and the stream goes back out. It runs on your own GPU or CPU, so captioning costs nothing per hour. **Stability and reliability come first; speed second.**

This page is the summary. Each section below is a separate file; open only the ones you need. Read costs (~tokens) for every doc are in [docs/INDEX.md](docs/INDEX.md).

| # | Section | In one line |
|---|---|---|
| 1 | [Overview](docs/prd/01-overview.md) | Problem, solution, why local beats cloud and human captioning |
| 2 | [Goals and non-goals](docs/prd/02-goals-and-non-goals.md) | Reliability-first principle; seven v1.0 goals; what's out of scope |
| 3 | [Target users](docs/prd/03-target-users-and-use-cases.md) | Six user groups and four key user stories |
| 4 | [Functional requirements](docs/prd/04-functional-requirements.md) | Every requirement by ID and priority, incl. caption formats and AI backends |
| 5 | [Configuration and tuning](docs/prd/05-configuration-and-tuning.md) | Presets and the table of settings with defaults and ranges |
| 6 | [Non-functional requirements](docs/prd/06-non-functional-requirements.md) | Video delay, latency budget, platforms, reliability rules, security |
| 7 | [Architecture and technology](docs/prd/07-proposed-architecture-and-technology.md) | Rust single binary, pipeline diagram, candidate libraries and crates |
| 8 | [Licensing and business model](docs/prd/08-licensing-distribution-and-business-model.md) | GPL-3.0 + trademark, free source, paid binaries, draft warranty |
| 9 | [Milestones](docs/prd/09-milestones.md) | M0 spikes through v1.0 launch |
| 10 | [Risks and open questions](docs/prd/10-risks-and-open-questions.md) | Seven risks with mitigations; open decisions |

**Decided so far:** Rust; GPL-3.0; Linux x64 first. See [docs/decisions/](docs/decisions/README.md).
