# S3: Pure-Rust CEA-608/708 encoder and SEI builder

> **Summary:** It works. The `spikes/cc` encoder's SEI, placed in x264/x265 streams without re-encoding, decodes correctly in FFmpeg, ccextractor and libcaption: CC1–CC4 and 708 services 1–6 in one stream, H.264 and HEVC, 25/29.97/30/59.94 fps. No external dependencies. 608 can carry Spanish, French, German and Portuguese in full, at about 44 characters per second per field (32-character lines). Non-Latin scripts need 708 with P16 or another format.

## What was built (`spikes/cc`)

- **`Cc608Encoder`**: supports roll-up (RU2/3/4), pop-on (RCL/ENM/EOC) and paint-on (RDC).
  - Each row gets a PAC; also implements CR, EDM, BS and DER.
  - Wraps at 32 columns at word boundaries, hard-splitting over-long words.
  - Every control, special and extended code is sent twice. Odd parity is applied.
  - CC2 and CC4 codes carry the 0x08 bit. Misc control codes use 0x14/0x1C/0x15/0x1D.
  - Covers the basic, special and both extended sets. An extended character is sent after its unaccented base letter, so decoders without the extended sets still show `A` for `Ä`.
  - Other characters are transliterated where possible (`œ`→`oe`, `ł`→`l`). Anything with no mapping (emoji, CJK) is dropped.
  - The queue is capped at 900 pairs; when full, the oldest whole lines are dropped.
- **`Cc708Encoder`** (services 1–6): each line is sent as `CR` + text + `ETX`.
  - Before the first line, and again every 4 lines for late joiners, it sends `DefineWindow 0` + `SetWindowAttributes` + `SetPenAttributes` + `SetPenColor`. The window is 3 rows × 32 columns, anchored bottom-centre, roll-up style.
  - Text uses G0, G1 (Latin-1) and G2 via EXT1.
  - Commands and characters never cross a service block or packet.
- **`CcMux`** (the API S1 and S6 call): `CcMux::new(FrameRate)`, `add_608`, `add_708`, `push_text_608(Channel, &str)`, `push_text_708(service, &str)`, `next_frame() -> Vec<CcTriple>`.
  - Call `next_frame()` once per frame **in display order**, then pass the result to `h264_sei_nal` / `hevc_sei_nal`.
  - `cc_count` is fixed per rate: 25 at 24, 24 at 25, 20 at 29.97/30, 12 at 50, 10 at 59.94/60. That is 9600 bit/s.
  - Each frame starts with a field-1 triple and a field-2 triple. At 50/60 fps the fields alternate frames, keeping 608 at 2 bytes per field per 1/30 s.
  - After that comes at most one DTVCC packet per frame (≤36 bytes at 30 fps, ≤16 at 60), with services round-robined, followed by `fa 00 00` padding.
  - CC1 and CC2 (likewise CC3 and CC4) take turns on their field one line at a time.
- **Test tooling:** `annexb` inserts the SEI (with start code) before each picture's first VCL NAL. `cc_dump` example prints pairs and SEI hex.
- **Tests:** 31 unit tests + doctest (incl. empty, 100k-char, control-char, emoji, CJK input); 8 FFmpeg round trips in `cargo test`; 3 ccextractor round trips, `#[ignore]`d (`cargo test -p cc --test ccextractor_roundtrip -- --ignored`; build steps in [tools-build](../evidence/S3/tools-build.txt)).

## Validation

Streams were made with `ffmpeg … -c:v libx264|libx265 -bf 0 -f h264|hevc`, had the SEI inserted, and were remuxed to TS with `-c copy`. Evidence: [test log](../evidence/S3/test-log.txt).

| Decoder | What was checked | Result |
|---|---|---|
| FFmpeg 9.0.1 `ccaption` (lavfi `subcc`) | RU3 and RU2 at H.264 30 fps and HEVC 30 fps; pop-on at 29.97; paint-on at 25; RU2 at 59.94; CC3 via `-data_field second`; es/fr/de/pt accents; `èè`; `*_~\|{}\^` | all text exact |
| ccextractor 0.96.5 (built from source) | One stream carrying CC1–CC4 and 708 services 1–6 (es/fr/de/pt), in three variants: H.264 30 fps, H.264 59.94 fps and HEVC 29.97 fps | all 4 channels and all 6 services exact (see 708 note below) |
| libcaption `ts2srt` | Same CC1 streams as the FFmpeg tests | all text exact, including accents and `èè` |

ffprobe shows the frames' "ATSC A53 Part 4 Closed Captions" side data. There were no decoder warnings.

**The `a53_payload` / `h264_sei_nal` / `hevc_sei_nal` builders needed no fixes.** Their layout matches libcaption byte for byte: `b5 0031 GA94 03`, `0x40|cc_count`, `em_data ff`, then the triples and a trailing `ff`. See the [hex dump](../evidence/S3/sei-hexdump.txt).

## Limits by language and script

| | 608 | 708 as built | 708 full |
|---|---|---|---|
| English, es, fr, de, pt | Yes. Missing: `œ æ ÿ` (transliterated), `€`→`EUR` | Yes: all of Latin-1 plus `œ Œ š Š Ÿ … “ ” ™` | Yes |
| Other Latin (pl, cs, tr, ro, hu…) | Diacritics stripped (`ł`→`l`) | Same (G1 has no `ł ş ő`) | Needs P16 |
| Cyrillic, Greek, Arabic, Hebrew, CJK, Hindi… | No; text is dropped | No; text is dropped | P16 (C0 `0x18` + 2 bytes). CEA-708 leaves the 16-bit set to the region (e.g. KS X 1001 in Korea), and receivers must be told the charset out of band. ccextractor accepts `--service 1[EUC-KR]`. Consumer support is patchy, so plan WebVTT/TTML/DVB for these scripts (PRD CI-1). |
| Bandwidth | 30 pairs/s per field. A 32-character roll-up line takes 6 control pairs + 16 text pairs, about 0.73 s. Two channels sharing a field get half each. | About 1000 bytes/s shared by all services. A 32-character line costs about 34 bytes, plus 19 bytes every 4 lines for the window. | |

## Gotchas

- **Repeat filtering differs between decoders.** FFmpeg and libcaption drop *every* repeat of the last control, special or extended code until a *different non-null* code arrives. ccextractor drops only the first repeat. A null pair does not reset FFmpeg or libcaption. So `èè` sent as `è è è è` renders as `è` in two of the three decoders. Fix: put a doubled DER (`14 24`) between identical codes; it is harmless because we always write at the end of the row. libcaption itself puts RCL there, which only works in pop-on.
- 608 redefines ASCII `` * \ ^ _ ` { | } ~ ``. They must go through the extended sets (`12 28`, `13 2B`, and so on). ccextractor shows `|` (`13 2E`) as `¦`.
- ccextractor writes 708 output as raw Latin-1, not UTF-8. It writes G2 characters as its internal byte (code − 0x20, e.g. `…`→`\x05`) and `♪` as `&l`. These are decoder bugs, not encoder bugs; the tests assert the exact bytes ccextractor produces.
- FFmpeg's decoder merges CC1 and CC2 (it has no channel filter) and only selects the field. Use ccextractor `--cc2` to check CC2 and CC4.
- `ffmpeg -i x.h264` without `-framerate` assumes 25 fps, which puts caption timing off by 20%.
- Late joiners see nothing until the next line, so 608 resends mode + PAC per line and 708 resends the window every 4 lines.

## Recommendation for M1

1. Promote `spikes/cc` to a product crate largely as it is. It has no dependencies and no panics on the data path, and the queues are bounded.
2. **B-frames:** SEI must carry the captions for the picture's *display* time. The pipeline (S1) must attach `next_frame()` output in PTS order, not decode order. This is untested here because every test used `-bf 0`, so S1 should test it next.
3. Default to roll-up RU3 on 608 and 708 together. Map languages as: source → CC1 + service 1; translations → CC3 (then CC2/CC4) + services 2–6. Add a queue-latency metric: pairs pending ÷ 30 = seconds behind.
4. Check real players in S6: VLC, YouTube ingest, and a hardware set-top box if one is available. None of them have been validated yet.
5. Non-Latin scripts: treat 708 P16 as out of scope for v1. Deliver them through WebVTT/TTML/DVB.
6. Licences: the code is ours (GPL-3.0). ccextractor (GPL-2.0) and libcaption (MIT) are test tools only; nothing links them.
