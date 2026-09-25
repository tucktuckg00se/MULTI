# ADR-0002: License under GPL-3.0 with a trademarked name

> **Summary:** MULTI's code is GPL-3.0: free to use, modify and build, with forks required to stay open. The name "MULTI" for official builds is protected by trademark, which is what protects the paid binaries.

- **Status:** accepted
- **Date:** 2026-09-22
- **Evidence:** [PRD §8](../prd/08-licensing-distribution-and-business-model.md)

## Context

Source must be free to use and build; prebuilt binaries are a paid option with support and warranty.

## Options considered

| Option | For | Against |
|---|---|---|
| GPL-3.0 | Stops closed commercial forks; compatible with LGPL FFmpeg and MIT/MPL deps | Can't stop others redistributing binaries |
| AGPL-3.0 | Also covers hosted SaaS clones | May deter institutional users |
| Apache-2.0 / MIT | Maximum adoption | Anyone can ship a closed fork |

## Decision

GPL-3.0 for all MULTI code; trademark the name; take contributions under DCO sign-off (CLA to be decided).

## Consequences

Dependencies must be GPL-3.0-compatible. Revisit AGPL if hosted clones appear.
