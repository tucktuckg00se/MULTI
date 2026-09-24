#!/usr/bin/env bash
# Mix noise or music under speech at a given SNR, with the FFmpeg CLI only.
#   mix.sh SPEECH.wav NOISE SNR_DB OUT.wav [OFFSET_S]
# NOISE is "pink" / "white" / "brown" (generated) or a WAV file (looped,
# starting OFFSET_S in). SNR uses whole-file RMS of each input, so pauses count
# as part of the speech level (speech-active SNR is ~1-2 dB higher).
set -euo pipefail
speech=$1 noise=$2 snr=$3 out=$4 off=${5:-0}
rms() { ffmpeg -hide_banner -nostats -i "$1" -af astats=measure_overall=RMS_level:measure_perchannel=none -f null - 2>&1 \
          | awk '/RMS level dB/ {v=$NF} END {print v}'; }
dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$speech")
tmp=$(mktemp --suffix .wav)
trap 'rm -f "$tmp"' EXIT
case $noise in
  pink|white|brown) ffmpeg -loglevel error -y -f lavfi -i "anoisesrc=color=$noise:sample_rate=16000:seed=42:duration=$dur" -ac 1 "$tmp" ;;
  *) ffmpeg -loglevel error -y -stream_loop -1 -ss "$off" -i "$noise" -t "$dur" -ar 16000 -ac 1 "$tmp" ;;
esac
s=$(rms "$speech"); n=$(rms "$tmp")
gain=$(awk -v s="$s" -v n="$n" -v r="$snr" 'BEGIN {printf "%.3f", s - r - n}')
ffmpeg -loglevel error -y -i "$speech" -i "$tmp" -filter_complex \
  "[1:a]volume=${gain}dB[n];[0:a][n]amix=inputs=2:duration=first:normalize=0,alimiter=limit=0.97:level=0[o]" \
  -map "[o]" -ar 16000 -ac 1 -c:a pcm_s16le "$out"
echo "$out speech_rms=${s}dB noise_rms=${n}dB gain=${gain}dB snr=${snr}dB"
