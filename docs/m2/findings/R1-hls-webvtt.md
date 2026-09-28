# R1: HLS with WebVTT subtitle renditions

> **Summary:** No GStreamer 1.28 / gst-plugins-rs 0.15.4 element writes HLS WebVTT subtitle renditions: `hlssink3`, `hlscmafsink` and `hlsmultivariantsink` accept only audio and video. FFmpeg 9's HLS muxer does (`-var_stream_map … sgroup:`), and a stream-copy proof produced a master playlist with `en` and `es` SUBTITLES renditions that ffprobe reads. Expected latency is about 3 × segment duration.

## GStreamer

Evidence: [gst-hls-elements.txt](../evidence/R1/gst-hls-elements.txt)

| Element | Installed here | Arch package | Subtitle input? |
|---|---|---|---|
| `hlssink2` (bad) | yes | gst-plugins-bad | no: `audio`/`video` pads only |
| `hlssink3`, `hlscmafsink` | no | `gst-plugin-hlssink3` (needs `gst-plugin-isobmff`) | no: pads `audio`/`video`; `hlscmafsink` caps are H.264/H.265/AAC |
| `hlsmultivariantsink` (the correct name) | no | `gst-plugin-hlsmultivariantsink` | no: pads `audio_%u`/`video_%u` only. Its `alternate-rendition` struct is for audio/video. |
| `cmafmux`, `isofmp4mux` | no | `gst-plugin-isobmff` | no text/wvtt caps |
| `mpegtsmux` | yes | gst-plugins-bad | carries no WebVTT for HLS |
| `webvttenc` | yes | gst-plugins-base (subenc) | text → WebVTT |
| `jsontovtt` | yes | `gst-plugin-rsclosedcaption` | cea608 JSON → `application/x-subtitle-vtt-fragmented` |

rs elements were checked by extracting the Arch packages into a scratch `GST_PLUGIN_PATH` (no sudo).

A GStreamer-only route needs our own code: per-language `webvttenc`/`jsontovtt` segments and hand-written playlists.

## FFmpeg proof (works)

Evidence: [hls-ffmpeg-proof.txt](../evidence/R1/hls-ffmpeg-proof.txt)

The input was a pre-encoded H.264/AAC `.ts` plus `en.srt` and `es.srt`, with video and audio stream-copied (`-c:v copy -c:a copy`), `-c:s webvtt`, `-hls_time 2`, and
`-var_stream_map "v:0,a:0,s:0,sgroup:subs,language:en,name:en v:1,a:1,s:1,sgroup:subs,language:es,name:es"`.

- The master playlist has `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs"` for `en` (DEFAULT=YES) and `es`, and every `EXT-X-STREAM-INF` carries `SUBTITLES="subs"`.
- `ffprobe master.m3u8` lists h264, aac and webvtt streams tagged `language=en` / `es`, and the `es` VTT playlist decodes back to the cues.

**Gotchas found:**
1. FFmpeg puts each subtitle stream in its own variant, and a subtitle-only variant fails with "Could not write header". So video and audio have to be mapped once per language. That duplicates the A/V segments on disk (copy only, no re-encode). The fix is to write the master playlist ourselves: one video variant and N subtitle renditions, using only one copy of the A/V.
2. The VTT segments carry no `X-TIMESTAMP-MAP`. Safari and hls.js need it to align cues with MPEG-TS PTS once the stream no longer starts at 0. Plan to write it ourselves or verify it in a player.
3. Live, MULTI's captions would reach FFmpeg as a piped WebVTT input per language.

## Latency

A player starts about 3 target durations behind live. With `hls_time 2` that is ≈6 s plus encode and upload, so ≈6–8 s glass-to-glass. MULTI's ~1 s caption lag fits inside that window.

## Recommendation

Use the **FFmpeg HLS muxer** for M2's web output. Stream-copy the A/V from the tee, feed one WebVTT input per language, and write the master playlist ourselves to avoid the duplicate-variant problem. Alternatively, and simpler to control, have MULTI write the `.vtt` segments and playlists in Rust next to an A/V-only HLS from FFmpeg or `hlssink3`. The VTT format is trivial and doesn't touch the video path.
