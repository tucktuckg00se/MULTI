# Licensing, distribution and business model

> **Summary:** GPL-3.0 code plus trademarked name; free source, paid official binaries with support and a limited warranty (draft, needs lawyer review).

The model works: source code free under an open-source licence, with paid official binaries, support and a warranty. Open-source licences allow selling binaries; what customers pay for is convenience, tested builds, updates and someone accountable.

**Repository:** [github.com/tucktuckg00se/MULTI](https://github.com/tucktuckg00se/MULTI)

**Licence recommendation: GPL-3.0 for the MULTI code, with the MULTI name and logo trademarked.**

| Option | Effect | Fit |
| --- | --- | --- |
| GPL-3.0 | Anyone can use, build and sell it, but forks must stay open source | Best: stops closed commercial forks; compatible with LGPL FFmpeg and MIT/MPL deps |
| AGPL-3.0 | Like GPL, plus network-service users must get the source | Stronger against hosted SaaS clones; may scare some institutional users |
| Apache-2.0 / MIT | Anyone can take it closed-source | Maximum adoption, weakest protection of the paid offering |
| Source-available (BSL, etc.) | Not open source; restricts commercial use | Conflicts with "free to use"; not recommended |

A GPL licence cannot stop others from redistributing binaries, so the trademark is what protects the paid offering: only official builds may be called MULTI. A contributor licence agreement (CLA) or DCO sign-off keeps the option to relicense or dual-license later.

**Distribution**

| Channel | Price | Includes |
| --- | --- | --- |
| Source on GitHub | Free | Full code, build docs, community support via issues/discussions |
| Community Docker image | Free | CPU build, or GPU build without warranty (decision pending) |
| Official binaries (Linux x64/ARM64, Windows) | Paid, per-instance yearly subscription | Signed installers, tested GPU builds, auto-update, email support, warranty |
| Pro/Broadcast tier | Paid, higher tier | Priority support SLA, multi-stream licences, long-term-support branch |

**Warranty policy (draft outline — have a lawyer review before selling)**

- The open-source code stays "as is" with no warranty, as the licence already says.
- Paid binaries carry a limited warranty: they perform substantially as documented on supported hardware and OS for the subscription term.
- Remedy: we fix the defect, provide a workaround, or refund the current term's fee.
- Explicit exclusions: caption accuracy (AI output is never guaranteed), missed filtered words, unsupported hardware, modified builds, third-party models.
- Liability capped at fees paid in the last 12 months; no liability for consequential losses such as broadcast fines or lost revenue.
- Customers stay responsible for their own regulatory caption compliance.

**Third-party licence hygiene:** keep a machine-generated dependency licence list in the repo, ship LGPL libraries dynamically linked with notices, and let models download at runtime under their own licences.

