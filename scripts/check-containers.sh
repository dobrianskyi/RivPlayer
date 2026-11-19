#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
base=https://chromium.googlesource.com/chromium/src/+/24db33777572825c8dc4e1fa37ece2affaa7636d/media/test/data
files=(
    bear.mp4 bear.m2ts bear.ogv bear_h264_aac.3gp bear-1280x720.ts
    bbb-320x240-2video-2audio.mp4 bear-1280x720-avt_subt_frag.mp4 bear-vp9-opus.webm
    bear-320x240_corrupted_after_init_segment.webm
)
for name in "${files[@]}"; do
    curl -fsSL --retry 2 --max-time 30 "$base/$name?format=TEXT" | base64 -d > "$work/$name"
done
legacy=https://android.googlesource.com/platform/external/chromium_org/+/21a33e9/media/test/data
curl -fsSL --retry 2 --max-time 30 "$legacy/bear_mpeg4_mp3.avi?format=TEXT" \
    | base64 -d > "$work/bear_mpeg4_mp3.avi"

RIVPLAYER_FIXTURES="$work" cargo test --locked --release --test media_playback -- --ignored
RIVPLAYER_FIXTURES="$work" cargo test --locked --release --lib external_audio_ -- --ignored
printf 'pcm.!default { type null }\n' > "$work/alsa-null.conf"
ALSA_CONFIG_PATH="$work/alsa-null.conf" RIVPLAYER_FIXTURES="$work" \
    cargo test --locked --release --test learning -- --ignored
cargo build --locked --release
player=target/release/rivplayer
tracks=$("$player" --check --software --video-track 2 --audio-track 3 --seek 13 "$work/bbb-320x240-2video-2audio.mp4")
printf '%s\n' "$tracks"
[[ $tracks == *'video #2 h264 320x240 (selected)'* && $tracks == *'audio #3 aac (selected)'* ]]
[[ $tracks == *'Video PTS 13.000'* ]]
captions=$("$player" --check --software "$work/bear-1280x720-avt_subt_frag.mp4")
printf '%s\n' "$captions"
[[ $captions == *'5 subtitle cues'* ]]
status=0
"$player" --check "$work/bear-320x240_corrupted_after_init_segment.webm" || status=$?
test "$status" -eq 1
