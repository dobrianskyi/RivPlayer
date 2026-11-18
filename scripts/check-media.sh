#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
player=${RVIDEOPLAYER_BIN:-"$root/target/release/rvideoplayer"}
if [[ ! -x $player ]]; then
    printf 'Build first with cargo build --release --locked\n' >&2
    exit 1
fi

case ${1:-} in
    '') backend=--software ;;
    --vaapi) backend=--vaapi ;;
    *) printf 'Usage: %s [--vaapi]\n' "$0" >&2; exit 2 ;;
esac

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
chromium=https://chromium.googlesource.com/chromium/src/+/24db33777572825c8dc4e1fa37ece2affaa7636d/media/test/data
files=(
    bear.mp4
    bear-1280x720-hevc-10bit.mp4
    bear-av1-opus.mp4
    bear-av1-320x180-10bit.webm
    bear-vp9-opus.webm
    testsrc-no-durations-h264.mkv
    bear-1280x720-avt_subt_frag.mp4
    bbb-320x240-2video-2audio.mp4
)
if [[ $backend == --software ]]; then
    files+=(bear-audio-10s-CBR-has-TOC.mp3 bear.flac bear-flac.mp4 bear_pcm_s16be.mov)
fi

pids=()
for name in "${files[@]}"; do
    (curl -fsSL --retry 2 --max-time 30 "$chromium/$name?format=TEXT" | base64 -d > "$work/$name") &
    pids+=("$!")
done
for pid in "${pids[@]}"; do wait "$pid"; done
mov=sample-video-portrait-720p.mov
curl -fsSL --retry 2 --max-time 30 "https://raw.githubusercontent.com/kodewright/dummy-data/ab8818629d31995c4de5b10460609b5c8d7baa94/videos/$mov" -o "$work/$mov"
files+=("$mov")

for name in "${files[@]}"; do
    output=$("$player" --check "$backend" "$work/$name")
    printf '%s\n' "$output"
    if [[ $backend == --vaapi ]]; then
        [[ $output == *"Decoder VA-API"* ]]
    fi
    case $name in
        bear-1280x720-avt_subt_frag.mp4) [[ $output == *"82 video frames"* && $output == *"5 subtitle cues"* ]] ;;
        bbb-320x240-2video-2audio.mp4) [[ $output == *"720 video frames"* ]] ;;
    esac
done
"$player" --check "$backend" --seek 13 "$work/bbb-320x240-2video-2audio.mp4"
