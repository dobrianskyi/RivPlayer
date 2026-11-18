#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

chromium=https://chromium.googlesource.com/chromium/src/+/24db33777572825c8dc4e1fa37ece2affaa7636d/media/test/data
for name in bear-320x240.webm bear.mpeg bear.avi bear-ac3-only-frag.mp4 bear-eac3-only-frag.mp4 bear.ac3 bear.eac3; do
    curl -fsSL --retry 2 --max-time 30 "$chromium/$name?format=TEXT" | base64 -d > "$work/$name"
done
samples=https://samples.ffmpeg.org
curl -fsSL --retry 2 --max-time 30 "$samples/MPEG1/zelda%20first%20commercial.mpeg" -o "$work/zelda.mpeg"
for name in ALAC_24bits2.mov ALAC_6ch.mov; do
    curl -fsSL --retry 2 --max-time 30 "$samples/A-codecs/lossless/ALAC/$name" -o "$work/$name"
done
curl -fsSL --retry 2 --max-time 30 "$samples/A-codecs/lossless/luckynight.m4a" -o "$work/luckynight.m4a"
curl -fsSL --retry 2 --max-time 30 "$samples/MPEG2/TITLE01-ANGLE1.VOB" -o "$work/TITLE01-ANGLE1.VOB"
curl -fsSL --retry 2 --max-time 30 "$samples/A-codecs/DTS/lotr_5.1_768.dts" -o "$work/lotr_5.1_768.dts"
for name in core_51_24_48_768_0.dtshd core_51_24_48_768_0.f32; do
    curl -fsSL --retry 2 --max-time 30 "https://fate-suite.ffmpeg.org/dts/dcadec-suite/$name" -o "$work/$name"
done
for name in 1ededcbd7b.ass a9-misc.ssa SubRip_capability_tester.srt pgs_sub.sup; do
    curl -fsSL --retry 2 --max-time 30 "https://fate-suite.ffmpeg.org/sub/$name" -o "$work/$name"
done
vobsub=https://raw.githubusercontent.com/altqx/libbitsub/73c7e795808c35f4e63965e5351228d7437627f6/crates/core/src/testfiles
for name in vobsub.idx vobsub.sub; do
    curl -fsSL --retry 2 --max-time 30 "$vobsub/$name" -o "$work/$name"
done

RVIDEOPLAYER_CODEC_FIXTURES="$work" cargo test --locked --release --test codecs -- --ignored
