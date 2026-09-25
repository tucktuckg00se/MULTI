# ADR-0001: Build MULTI in Rust

> **Summary:** MULTI is written in Rust. The product priority is reliability first, speed second, and Rust's memory safety and compile-time data-race checks fit a 24/7 service that parses untrusted network streams.

- **Status:** accepted
- **Date:** 2026-09-22
- **Evidence:** discussion only; see [PRD §7](../prd/07-proposed-architecture-and-technology.md)

## Context

The core dependencies (FFmpeg, whisper.cpp, llama.cpp, ONNX Runtime, CUDA) are C/C++. The choice was between C++ (direct calls) and Rust (through bindings).

## Options considered

| Option | For | Against |
|---|---|---|
| C++ | Calls every dependency directly; familiar to broadcast engineers | Memory bugs in stream parsers become crashes or security holes; data races across many worker threads; heavier web/API and build tooling |
| Rust | Memory safety and data-race checks; no GC pauses; cargo; easy web/API (axum); GStreamer caption elements already in Rust | Bindings to C/C++ libraries can lag upstream; C/C++ deps still need building per platform |

## Decision

Rust, with thin bindings to the C/C++ inference and media libraries. If a binding crate lags, we maintain our own small `-sys` wrapper.

## Consequences

Crash isolation still needs separate worker processes, because Rust cannot contain a segfault inside a C++ library. Revisit only if M0 shows binding friction that blocks the pipeline.
