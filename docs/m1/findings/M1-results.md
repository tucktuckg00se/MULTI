# M1 results

> **Summary:** M1 is done (2026-09-28). MULTI runs as product code: SRT/UDP in, SRT/UDP/RTMP out, English speech captioned in EN plus ES/FR/DE, supervised workers, word filter, web GUI with login and HTTPS, per-output control and a fallback picture. A 4-hour soak, B-frame test, live OBS test and YouTube test all passed; YouTube shows only one caption track (English).

## Measured

| Check | Result | Target |
|---|---|---|
| Video delay, in → out (4 h) | p50 33.8 ms, p99 34.8 ms, drift 0 | < 250 ms |
| EN caption lag (first / last 10 min of 4 h) | p95 1.20 s / 1.28 s | ≤ 1.5 s |
| Translation per clause (ES/FR/DE) | ≈ 20 ms (WP3); S6 clause→translation p95 0.83 s | ≤ 2.5 s total |
| 4-hour soak | 0 errors, memory flat, video never interrupted | clean |
| Worker `kill -9` recovery | ASR 1.5 s, MT 0.8 s; video unaffected | ≤ 5 s |
| Stopping one of two outputs | other output: ~1 frame max gap (34–43 ms) | no interruption |
| Input loss (fallback picture) | ~1.0 s to fallback, 0.5 s back, PTS continuous | stay connected |
| B-frame source | CC1 order identical to no-B-frame control | in order |
| Blocklisted word | never on CC1/CC3 (CI test) | never |

Evidence: [WP6 soak summary](../evidence/WP6/soak-summary.txt), [WP3](../evidence/WP3), [WP2](../evidence/WP2), PR descriptions for WP10 (#15) and WP11 (#16).

## Live tests (user, 2026-09-28)

- Web GUI: login and first-run setup, dashboard with sidebar, collapsible settings, audio meter, silence warning — worked.
- Per-output start/stop and live add — worked without disturbing other outputs.
- Fallback picture: stopping OBS showed black instead of dropping; returned when OBS restarted.
- **YouTube (RTMP, embedded 608/708):** captions appeared; **only English was selectable**. Multi-language on YouTube needs another route (M2).

## Exit criteria

| Criterion | Status |
|---|---|
| 4-hour run clean | met |
| EN lag p95 ≤ 1.5 s; translated ≤ 2.5 s | met (1.20–1.28 s; ≈2 s) |
| Worker kill never interrupts video; captions back ≤ 5 s | met |
| Blocklisted word never appears | met |
| YouTube via RTMP; EN/ES in VLC | met (YouTube English only) |
| FR/DE via CC2/CC4 | **dropped**: GStreamer's encoders support CC1/CC3 only; FR/DE stay on 708 |
| Set up, start, monitor from the GUI | met |
| `multi models pull` on a clean machine | met (WP7 local check) |
| CI green on `main` | met |

## Gotchas worth remembering

- Remuxing a capture with ffmpeg `-copyts` shifts ffmpeg's real-time caption cue times by +1.4 s; record byte-exact with GStreamer when measuring caption lag.
- Before paid binaries: H.264/HEVC encoder patent licensing (openh264, x265) needs a decision; x265 is GPL-2.0-or-later (believed compatible with GPL-3.0 — confirm).
