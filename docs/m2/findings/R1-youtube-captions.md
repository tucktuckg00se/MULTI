# R1: Multi-language live captions on YouTube

> **Summary:** A YouTube live broadcast shows **one** caption track, whichever way captions arrive. Embedded 608/708 gives one track, HTTP POST gives one feed per stream, and HLS ingest takes no WebVTT. The only route to live multi-language captions is **one broadcast per language**. After the stream, per-language tracks can be uploaded to the archived video.

## Answer per path

| Path | Multi-language live? | Evidence |
|---|---|---|
| Embedded 608/708 (RTMP/SRT/HLS, H.264) | **No.** One track only. | Help Center: "The 608/708 standard supports up to 4 language tracks, YouTube currently only supports one track of captions." Matches M1's observation that only the English track shows. Which track is read (CC1 or 708 service 1) isn't documented; M1's working setup puts English in CC1 and service 1. |
| Embedded 608/708 over HLS ingest with HEVC | **No.** Not supported at all. | HLS ingest guide: embedded 608/708 "work with HLS ingestions that use the H264 video codec but not ... HEVC". |
| HTTP POST caption ingestion | **No.** One feed per stream. | Help Center: "Each stream entry point can have one caption feed only." The API models captions as a single enum per broadcast: `closedCaptionsType` = disabled / httpPost / embedded. Google doesn't publish the POST format (it's gated behind a vendor form). Third-party senders document `?cid=…&seq=N` with a `text/plain` body of `timestamp region:…` lines. A `lang` query parameter appears only in third-party write-ups, and nothing official says it opens a second track. |
| WebVTT / side-channel captions over HLS ingest | **No.** | The HLS ingest guide lists only two caption methods: HTTP POST ("works for all HLS ingestions") and embedded 608/708. It says nothing about subtitle renditions or WebVTT. |
| YouTube auto-captions / auto-translate | English only | Help Center: "Automatic captions for live streams are available in English only." Viewer-side auto-translate for live isn't documented. |

## Routes that do work

1. **One broadcast per language.** Each language runs its own `liveBroadcast` + `liveStream` with its own 608 CC1 or POST track. The video is stream-copied, so the extra cost is N× upload bandwidth and N watch pages. Viewers pick a language by picking a link, not from the CC menu. Chat and view counts are split across the broadcasts.
2. **After the stream:** upload one caption track per language to the archived video with `captions.insert` (Data API). The archive then gets a real CC-menu language picker. MULTI can build these tracks from the captions it already produced (WebVTT/SRT).
3. **Our own player:** for a real live language picker, serve HLS with WebVTT renditions (see [R1-hls-webvtt.md](R1-hls-webvtt.md)) and embed that player next to or instead of YouTube.

## Recommendation

- Keep embedded 608 CC1 carrying **one** language per YouTube output. It is the source language by default, and a setting picks any target language instead.
- Let an output list several YouTube destinations, each with its own caption language. That is route 1: already possible with M1 outputs plus a per-output language choice.
- Add an optional "upload caption tracks to the VOD after the stream" job (`captions.insert`, OAuth). This is the only way to get a YouTube CC-menu picker.
- Don't build the HTTP POST sender for multi-language. It gives no extra tracks and its format isn't officially documented.

## Sources opened

- Live caption requirements (Help Center): https://support.google.com/youtube/answer/3068031
- HLS ingestion guide: https://developers.google.com/youtube/v3/live/guides/hls-ingestion
- liveBroadcasts resource (`closedCaptionsType`): https://developers.google.com/youtube/v3/live/docs/liveBroadcasts
- Automatic captions (Help Center): https://support.google.com/youtube/answer/6373554
- Third-party POST sender (format only, not official): https://github.com/jsilvanus/live-captions-yt
