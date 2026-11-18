use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use oxideav_core::{CodecParameters, Packet, StreamInfo, TimeBase};
use rvideoplayer::{
    engine::{self, Captions, DecodeMode, Event, Options, Player},
    source::{LocalFile, MediaSource, runtime},
};

#[derive(Default)]
struct Playback {
    container: String,
    video: Option<String>,
    audio: Option<String>,
    video_count: usize,
    first_video: Option<f64>,
    last_video: Option<f64>,
    audio_count: usize,
    first_audio: Option<f64>,
    last_audio: Option<f64>,
    peak: f32,
    incomplete_audio: bool,
}

fn decode(path: &Path, position: f64) -> Result<Playback> {
    let mut result = Playback::default();
    engine::run(
        &LocalFile(path.to_owned()),
        Options {
            position,
            mode: DecodeMode::Software,
            ..Options::default()
        },
        &AtomicBool::new(false),
        |event| {
            match event {
                Event::Info(info) => {
                    result.container = info.container;
                    result.video = info
                        .video
                        .into_iter()
                        .find(|t| Some(t.index) == info.current_video)
                        .map(|t| t.codec);
                    result.audio = info
                        .audio
                        .into_iter()
                        .find(|t| Some(t.index) == info.current_audio)
                        .map(|t| t.codec);
                }
                Event::Video(frame) => {
                    assert!(
                        result
                            .last_video
                            .is_none_or(|prev| prev <= frame.time + 0.001)
                    );
                    let picture = frame.picture;
                    assert!(picture.width > 0 && picture.height > 0);
                    assert!(picture.frame.image_planes()[0].data.iter().any(|&y| y != 0));
                    result.video_count += 1;
                    result.first_video.get_or_insert(frame.time);
                    result.last_video = Some(frame.time);
                }
                Event::Audio(chunk) => {
                    assert!(chunk.samples.rate > 0 && chunk.samples.channels > 0);
                    assert!(
                        result
                            .last_audio
                            .is_none_or(|prev| prev <= chunk.time + 0.08)
                    );
                    result.audio_count += chunk.samples.data.len() / chunk.samples.channels;
                    result.first_audio.get_or_insert(chunk.time);
                    result.last_audio = Some(chunk.end());
                    for sample in chunk.samples.data {
                        assert!(sample.is_finite());
                        result.peak = result.peak.max(sample.abs());
                    }
                }
                Event::Notice(text) => {
                    result.incomplete_audio |= text == "Incomplete Dolby frame skipped"
                }
                _ => {}
            }
            true
        },
    )?;
    Ok(result)
}

#[test]
#[ignore = "run scripts/check-codecs.sh to fetch public media fixtures"]
fn codec_playback_and_seeking() -> Result<()> {
    let dir = PathBuf::from(
        std::env::var_os("RVIDEOPLAYER_CODEC_FIXTURES").context("Missing fixture directory")?,
    );
    let dts = dir.join("bear-dts-core.mkv");
    let film = std::fs::read(dir.join("lotr_5.1_768.dts"))?;
    mux_dts(&dts, &film, Some(&dir.join("bear-320x240.webm")), 250)?;
    for (name, container, video, audio, seek) in [
        ("bear-320x240.webm", "matroska", Some("vp8"), "vorbis", 0.85),
        ("bear.mpeg", "mpeg1ps", Some("mpeg1video"), "mp2", 0.64),
        ("zelda.mpeg", "mpeg1ps", Some("mpeg1video"), "mp2", 12.8),
        ("bear.avi", "avi", Some("mpeg4video"), "ac3", 0.64),
        ("bear-ac3-only-frag.mp4", "mp4", None, "ac3", 0.42),
        ("bear-eac3-only-frag.mp4", "mp4", None, "eac3", 0.42),
        ("bear.ac3", "ac3", None, "ac3", 0.42),
        ("bear.eac3", "ac3", None, "eac3", 0.42),
        ("ALAC_24bits2.mov", "mov", None, "alac", 8.52),
        ("ALAC_6ch.mov", "mov", None, "alac", 8.52),
        ("luckynight.m4a", "mp4", None, "alac", 23.4),
        ("bear-dts-core.mkv", "matroska", Some("vp8"), "dts", 1.2),
        (
            "TITLE01-ANGLE1.VOB",
            "mpegps",
            Some("mpeg2video"),
            "ac3",
            4.25,
        ),
    ] {
        let path = dir.join(name);
        let original = decode(&path, 0.0).with_context(|| format!("Playing {name}"))?;
        assert_eq!(original.container, container, "{name}");
        assert_eq!(original.video.as_deref(), video, "{name}");
        assert_eq!(original.audio.as_deref(), Some(audio), "{name}");
        assert!(
            original.audio_count > 10_000 && original.peak > 0.001,
            "{name}"
        );
        if audio == "dts" {
            assert!(original.peak < 0.3, "DTS Core PCM clipped");
        }
        if name.ends_with(".VOB") {
            assert!(original.incomplete_audio && original.video_count > 150);
        }
        let resumed = decode(&path, seek).with_context(|| format!("Seeking {name}"))?;
        let a = resumed.first_audio.context("No audio after seek")?;
        assert!(
            (a - seek).abs() < 0.085,
            "{name}: audio at {a}, seek {seek}"
        );
        assert!(resumed.audio_count > 1_000 && resumed.audio_count < original.audio_count);
        if video.is_some() {
            assert!(original.video_count >= 25 && resumed.video_count < original.video_count);
            let v = resumed.first_video.context("No video after seek")?;
            assert!(
                (v - seek).abs() < 0.045,
                "{name}: video at {v}, seek {seek}"
            );
            assert!((v - a).abs() < 0.1, "{name}: audio/video start difference");
            assert!(
                (resumed.last_video.unwrap() - resumed.last_audio.unwrap()).abs() < 0.32,
                "{name}: audio/video end difference"
            );
        }
    }
    Ok(())
}

fn mux_dts(path: &Path, input: &[u8], video: Option<&Path>, max: usize) -> Result<()> {
    let mut streams = Vec::new();
    let mut packets = Vec::new();
    if let Some(path) = video {
        let mut input = LocalFile(path.to_owned()).open_demuxer(&runtime())?;
        let video = input
            .streams()
            .first()
            .context("No video in mux fixture")?
            .clone();
        while let Ok(packet) = input.next_packet() {
            if packet.stream_index == video.index {
                packets.push(packet);
            }
        }
        streams.push(video);
    }
    let index = streams.len() as u32;
    let base = TimeBase::new(1, 48_000);
    let mut params = CodecParameters::audio("dts".into());
    params.channels = Some(6);
    params.sample_rate = Some(48_000);
    streams.push(StreamInfo {
        index,
        params,
        time_base: base,
        duration: None,
        start_time: Some(0),
    });
    for (i, frame) in oxideav_dts::iter_frames(input).take(max).enumerate() {
        let frame = frame?;
        assert_eq!(frame.header.sample_rate_hz(), Some(48_000));
        packets.push(
            Packet::new(index, base, frame.data.to_vec())
                .with_pts(i as i64 * 512)
                .with_duration(512)
                .with_keyframe(true),
        );
    }
    packets.sort_by_key(|p| {
        p.time_base.rescale(
            p.dts.or(p.pts).unwrap_or_default(),
            TimeBase::new(1, 1_000_000),
        )
    });
    let mut mux = oxideav_mkv::mux::open(Box::new(BufWriter::new(File::create(path)?)), &streams)?;
    mux.write_header()?;
    for packet in &packets {
        mux.write_packet(packet)?;
    }
    mux.write_trailer()?;
    Ok(())
}

#[test]
#[ignore = "run scripts/check-codecs.sh to fetch public DTS and PCM fixtures"]
fn dts_core_matches_reference_pcm() -> Result<()> {
    let dir = PathBuf::from(
        std::env::var_os("RVIDEOPLAYER_CODEC_FIXTURES").context("Missing fixture directory")?,
    );
    let reference = std::fs::read(dir.join("core_51_24_48_768_0.f32"))?;
    let source = std::fs::read(dir.join("core_51_24_48_768_0.dtshd"))?;
    let path = dir.join("dts-reference.mka");
    mux_dts(&path, &source, None, 6)?;
    let mut pcm = Vec::new();
    engine::run(
        &LocalFile(path),
        Options {
            mode: DecodeMode::Software,
            ..Options::default()
        },
        &AtomicBool::new(false),
        |event| {
            if let Event::Audio(audio) = event {
                assert_eq!(audio.samples.channels, 6);
                pcm.extend(audio.samples.data);
            }
            true
        },
    )?;
    let (reference, tail) = reference.as_chunks::<4>();
    assert!(tail.is_empty());
    assert_eq!(pcm.len(), reference.len());
    let mean_square = pcm
        .iter()
        .zip(reference)
        .map(|(&actual, &expected)| {
            (f64::from(actual) - f64::from(f32::from_le_bytes(expected).clamp(-1.0, 1.0))).powi(2)
        })
        .sum::<f64>()
        / pcm.len() as f64;
    assert!(mean_square.sqrt() < 0.01, "DTS PCM deviation {mean_square}");
    Ok(())
}

#[test]
#[ignore = "run scripts/check-codecs.sh to fetch public subtitle fixtures"]
fn external_subtitles_follow_the_timeline() -> Result<()> {
    let dir = PathBuf::from(
        std::env::var_os("RVIDEOPLAYER_CODEC_FIXTURES").context("Missing fixture directory")?,
    );
    for name in [
        "SubRip_capability_tester.srt",
        "1ededcbd7b.ass",
        "a9-misc.ssa",
    ] {
        let cues = engine::read_sidecar(&dir.join(name))?;
        assert!(
            cues.len() > 2 && cues.iter().all(|cue| cue.end > cue.start),
            "{name}"
        );
    }

    let mut player = Player::new(|| {}, DecodeMode::Software);
    for (name, time, canvas) in [
        ("pgs_sub.sup", 0.15, (1920, 1080)),
        ("vobsub.idx", 48.55, (720, 480)),
    ] {
        player.captions = Captions::Off;
        player.load_captions(&dir.join(name));
        let deadline = Instant::now() + Duration::from_secs(3);
        while player.captions == Captions::Off && Instant::now() < deadline {
            player.tick();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(player.bitmap_at(0.0).is_none(), "{name}");
        let frame = player
            .bitmap_at(time)
            .with_context(|| format!("No bitmap at {time} from {name}"))?;
        assert_eq!((frame.width, frame.height), canvas);
        assert!(
            frame.compositions.iter().any(|sprite| sprite
                .rgba
                .iter()
                .skip(3)
                .step_by(4)
                .any(|&a| a > 0))
        );
        assert!(player.bitmap_at(time + 30.0).is_none() || name == "vobsub.idx");
        assert!(
            player.bitmap_at(time).is_some(),
            "{name} after seeking backward"
        );
    }
    Ok(())
}
