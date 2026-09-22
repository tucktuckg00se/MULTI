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
| P-02 | spec | [Goals, non-goals, reliability-first principle](prd/02-goals-and-non-goals.md) | current | ~455 |
| P-03 | spec | [Target users and key user stories](prd/03-target-users-and-use-cases.md) | current | ~410 |
| P-04 | spec | [Functional requirements by ID and priority](prd/04-functional-requirements.md) | current | ~1540 |
| P-05 | spec | [Configuration settings, defaults and presets](prd/05-configuration-and-tuning.md) | current | ~764 |
| P-06 | spec | [Latency budget, platforms, reliability, security](prd/06-non-functional-requirements.md) | current | ~886 |
| P-07 | spec | [Architecture, candidate libraries and Rust crates](prd/07-proposed-architecture-and-technology.md) | current | ~1004 |
| P-08 | spec | [Licensing, distribution, paid binaries, warranty draft](prd/08-licensing-distribution-and-business-model.md) | current | ~752 |
| P-09 | spec | [Milestones M0 to v1.0](prd/09-milestones.md) | current | ~399 |
| P-10 | spec | [Risks, mitigations and open questions](prd/10-risks-and-open-questions.md) | current | ~478 |

## Decisions

| ID | Type | Title | Status | ~Tokens |
|---|---|---|---|---|
| D-00 | summary | [All decisions, newest first](decisions/README.md) | current | ~137 |
| D-01 | decision | [ADR-0001: build in Rust](decisions/ADR-0001-rust.md) | accepted | ~355 |
| D-02 | decision | [ADR-0002: GPL-3.0 with trademarked name](decisions/ADR-0002-gpl-3.md) | accepted | ~270 |
| D-T | guide | [ADR template](decisions/TEMPLATE.md) | current | ~132 |

## M0 spikes

| ID | Type | Title | Status | ~Tokens |
|---|---|---|---|---|
| M-00 | summary | [M0 overview: spikes, harness, log](m0/README.md) | current | ~447 |
