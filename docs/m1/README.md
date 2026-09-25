# M1 — First real version

> **Summary:** Turns the M0 spikes into product code in `crates/`: SRT/UDP in, SRT/UDP/RTMP out, English speech captioned in EN plus ES/FR/DE translations on 608/708, word filters, ASR and translation in supervised worker processes, a TOML config, and a web GUI for settings, status and live captions. CPU-only CI on GitHub.

## Work packages

One branch and PR per package; CI must pass before merge.

| WP | Contents | Status |
|---|---|---|
| 1 Skeleton | Workspace, `multi-core` (config with PRD defaults, text types, worker IPC framing), `multi` CLI stub, CI | in progress |
| 2 Media | `multi-media`: GStreamer input, restamping bridge, caption lanes, SRT/UDP/RTMP outputs, reconnect; fake-worker integration test | not started |
| 3 Workers | `multi-asr`, `multi-mt` worker binaries; supervisor with heartbeats and restart | not started |
| 4 Caption quality | Segmenter, word filters, text cleaning, backlog cap, lane restart, degrade policy, CC2/CC4 option | not started |
| 5 Web GUI | Control layer, API + live events, embedded page: settings, status, live captions | not started |
| 6 Validate | B-frames, 4-hour soak, live OBS test from the GUI, YouTube RTMP test, findings | not started |

## Exit criteria

- 4-hour live run: no video interruptions, memory flat, no errors.
- EN caption lag P95 ≤ 1.5 s; translated ≤ 2.5 s.
- Killing the ASR or translation worker never interrupts video; captions return within 5 s.
- A blocklisted word never appears in any caption language.
- Captions visible on YouTube via RTMP; EN/ES in VLC; FR/DE via the CC2/CC4 option.
- Set up, start and monitor a stream using only the web GUI.
- CI green on `main`.

## Code layout

| Path | What |
|---|---|
| `crates/multi-core` | Config (`Config::validate` reports issues by setting path), `Word`/`Clause`/`Translation`, worker IPC framing |
| `crates/multi` | The `multi` binary: `multi run`, `multi config default`, `multi config check` |
| `spikes/` | M0 reference code; not built by the root workspace |

## Log

Newest first.

- 2026-09-25 — M1 started: WP1 skeleton.
