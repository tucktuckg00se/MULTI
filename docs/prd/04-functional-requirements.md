# Functional requirements

> **Summary:** All v1.0 requirements by ID with P0/P1/P2 priority: ingest/output (IO), transcription (TR), translation (TL), AI model backends (MD), caption formats and insertion (CI), filtering (FL), acceleration/control/monitoring (OP).

P0 = required for v1.0, P1 = targeted for v1.0, P2 = later release.

**Ingest and output**

| ID | Requirement | Priority |
| --- | --- | --- |
| IO-1 | Ingest SRT (caller and listener modes, passphrase encryption) | P0 |
| IO-2 | Ingest RTP and MPEG-TS over UDP, unicast and multicast | P0 |
| IO-3 | Output SRT, RTP and UDP MPEG-TS to any IP:port; multiple outputs per input | P0 |
| IO-4 | Pass H.264 and HEVC video through without re-encoding | P0 |
| IO-5 | Ingest RTMP and output RTMP (for YouTube/Facebook/Twitch) | P1 |
| IO-6 | Accept file input for testing and benchmarks | P1 |
| IO-7 | Output HLS with WebVTT caption renditions | P2 |
| IO-8 | Survive source drop-outs: keep output alive, reconnect automatically | P0 |

**Transcription**

| ID | Requirement | Priority |
| --- | --- | --- |
| TR-1 | Streaming speech-to-text from the selected audio track and channel | P0 |
| TR-2 | Swappable speech model (see AI model backends); ship a default small model tuned for speed | P0 |
| TR-3 | Voice-activity detection so silence and music do not produce junk captions | P0 |
| TR-4 | Automatic source-language detection, with a manual override | P1 |
| TR-5 | Custom vocabulary / prompt (names, places, jargon) per stream | P1 |
| TR-6 | Speaker-change markers (">>" convention) | P2 |

**Translation**

| ID | Requirement | Priority |
| --- | --- | --- |
| TL-1 | Translate the transcript into N target languages in parallel | P0 |
| TL-2 | Each language is its own caption track (CC1–CC4 / 708 services / DVB) with a language tag | P0 |
| TL-3 | Source-language track is always available alongside translations | P0 |
| TL-4 | Swappable translation model, dedicated MT model or small LLM (see AI model backends) | P1 |

**AI model backends**

Models improve every few months, so MULTI treats them as plug-ins behind a stable interface rather than building around one model.

| ID | Requirement | Priority |
| --- | --- | --- |
| MD-1 | One backend interface for speech-to-text and one for translation; new models need no pipeline changes | P0 |
| MD-2 | At least two speech backends at v1.0: whisper.cpp (GGML/GGUF Whisper models) and ONNX Runtime (Parakeet, Moonshine and similar) | P0 |
| MD-3 | Translation backends: llama.cpp (GGUF small LLMs) and CTranslate2 (opus-mt, M2M-100) | P0 |
| MD-4 | Model registry file listing each model's backend, languages, licence, VRAM need, download URL and checksum | P0 |
| MD-5 | Choose a model per stream and per language (e.g. accurate model for the source, fast model for translations) | P1 |
| MD-6 | Load a custom or fine-tuned model from a local path | P1 |
| MD-7 | `multi bench` command: runs candidate models on a test clip and reports real-time factor, caption lag and VRAM on this machine | P1 |
| MD-8 | Optional remote backend over an OpenAI-compatible HTTP API, e.g. a local Ollama or vLLM server on another box. Off by default; never required | P2 |

**Caption formats and insertion**

MULTI supports the formats most TV and web delivery chains actually use. Each output picks its formats, and each language maps to a track or service within them.

| Format | Where it's used | Carried in | Priority |
| --- | --- | --- | --- |
| CEA-608 (line 21 data) | US/Canada TV; accepted by YouTube, Twitch and Facebook live ingest | H.264/HEVC SEI (ATSC A/53), CC1–CC4 | P0 |
| CEA-708 (DTVCC) | US digital TV (ATSC), cable, most IP broadcast chains | H.264/HEVC SEI, up to 6 standard services | P0 |
| WebVTT | HLS, browsers, most web players | Sidecar file / HLS subtitle rendition | P1 |
| SubRip (.srt) + live text feed | Archive, CMS upload, custom overlays | File and WebSocket/JSON per language | P1 |
| DVB Teletext subtitles | UK, Europe, Australia legacy chains; most newer services use DVB Subtitles | MPEG-TS Teletext PID; no encoder in FFmpeg or GStreamer, so this is our own work | P2 (low) |
| DVB Subtitles (EN 300 743) | Europe and other DVB countries | MPEG-TS, bitmaps rendered from text | P2 |
| IMSC1 / TTML (EBU-TT-D) | MPEG-DASH, CMAF and broadcaster OTT apps | Fragmented MP4 / sidecar | P2 |
| YouTube live caption HTTP ingest | YouTube streams where embedded 608 is not used | HTTP POST to YouTube | P2 |
| SCC / MCC caption files | Archive, broadcast deliverables, caption editing tools | Sidecar file | P2 |

CEA-608 only covers Latin-alphabet languages. Other scripts need 708, WebVTT, TTML or DVB.

Burning captions into the picture is **not** part of MULTI: it needs a decode and re-encode, which breaks the pass-through design. It is planned as a separate companion tool that takes a captioned feed (e.g. MULTI's output) and burns the captions in.

| ID | Requirement | Priority |
| --- | --- | --- |
| CI-1 | Output any combination of the formats above on each output, with a language-to-track mapping | P0 |
| CI-2 | Roll-up, pop-on and paint-on caption modes; rows and line length configurable | P0 |
| CI-3 | Preserve any captions already in the source, or replace them (configurable) | P1 |

**Filtering**

| ID | Requirement | Priority |
| --- | --- | --- |
| FL-1 | User word blocklist, per language, whole-word and wildcard matching | P0 |
| FL-2 | Built-in explicit-language filter on by default, per language | P0 |
| FL-3 | Masking style: asterisks, first letter + asterisks, [bleep], or drop word | P0 |
| FL-4 | Filter runs after translation too, so no language leaks a blocked word | P0 |
| FL-5 | Allowlist to exempt words (e.g. place names that trip the filter) | P1 |
| FL-6 | Hot-reload lists without restarting the stream | P1 |

**Acceleration, control and monitoring**

| ID | Requirement | Priority |
| --- | --- | --- |
| OP-1 | NVIDIA CUDA acceleration; CPU fallback that still runs in real time at reduced accuracy | P0 |
| OP-2 | Apple/AMD/Intel acceleration (Vulkan, ROCm, OpenVINO) where the backend supports it | P2 |
| OP-3 | NVIDIA Jetson (ARM64 + CUDA) support | P1 |
| OP-4 | Config file (YAML/TOML) and CLI; every setting overridable by flag | P0 |
| OP-5 | Local web UI on a configurable address and port: stream status, live caption preview per language, lists editor | P1 |
| OP-6 | REST API and Prometheus metrics (caption lag, GPU %, drops, bitrate) | P1 |
| OP-7 | Multiple independent streams per instance, limited by hardware | P1 |
| OP-8 | Official Docker images, plus a systemd unit and Windows service | P1 |

