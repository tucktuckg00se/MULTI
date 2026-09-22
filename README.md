# MULTI

**M**ultilingual **U**nified **L**ive **T**ranscription & **I**nsertion

MULTI takes a live video stream, generates closed captions from its audio in several languages at once, embeds them back into the stream, and sends it on, all on your own hardware.

> **Status: planning.** There is no working code yet. See the [product requirements document](PRD.md) for scope, design and roadmap.

## What it will do

- **Ingest and output live streams** over SRT, RTP and UDP MPEG-TS (RTMP later), passing video through without re-encoding.
- **Transcribe speech in real time** with small, fast local AI models on an NVIDIA GPU or CPU.
- **Translate into multiple languages at once**, each on its own caption track.
- **Insert standard caption formats**: CEA-608/708 first, then WebVTT, SRT, DVB Teletext and more.
- **Filter captions** with a built-in profanity filter and your own word blocklist, in every language.
- **Let the engineer tune everything**, including video delay, caption timing, model choice and web UI port, with sensible defaults.
- **Stay up.** Reliability comes first: a caption failure never interrupts the video.

Because everything runs locally, there are no per-minute cloud fees and no audio leaves your building.

## Platforms

Planned: Linux x86-64 and ARM64 (including NVIDIA Jetson) first, then Windows x86-64.

## Built with

Rust, with FFmpeg or GStreamer for media handling and whisper.cpp / ONNX Runtime / llama.cpp for AI inference. The pipeline base is decided in the first prototype milestone.

## License

MULTI is free software, licensed under the [GNU General Public License v3.0](LICENSE). You can use, study, modify and build it yourself.

Official prebuilt binaries with support and a warranty are planned as a paid option. "MULTI" as the name of official builds is intended to be protected as a trademark; forks should use a different name.
