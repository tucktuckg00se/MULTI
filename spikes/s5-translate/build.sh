#!/bin/bash
# Builds s5-translate with CUDA 13 (RTX 3090 = sm_86). Pass extra cargo args, e.g. `clippy --all-targets`.
set -euo pipefail
cd "$(dirname "$0")/.."
export CUDA_PATH=${CUDA_PATH:-/opt/cuda} CUDA_ARCH_LIST=${CUDA_ARCH_LIST:-8.6} CUDAARCHS=${CUDAARCHS:-86}
export CMAKE_PARALLEL=${CMAKE_PARALLEL:-$(nproc)} CMAKE_BUILD_PARALLEL_LEVEL=${CMAKE_BUILD_PARALLEL_LEVEL:-$(nproc)}
cmd=${1:-build}; shift || true
cargo "$cmd" --release -p s5-translate "$@"
