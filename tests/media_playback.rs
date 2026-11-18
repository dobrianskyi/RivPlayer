//! Reproducible checks using the public samples downloaded by check-containers.sh.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use oxideav_core::{Demuxer, Error, MediaType, Packet};
use oxideav_mp4::{BrandPreset, Mp4MuxerOptions};
use rvideoplayer::{
    engine::{self, DecodeMode, Event, Options, Player},
    source::{LocalFile, MediaSource, runtime},
};

#[test]
#[ignore = "use scripts/check-containers.sh to supply the media fixtures"]
fn container_and_transport_regressions() -> Result<()> {
    let directory = PathBuf::from(std::env::var_os("RVIDEOPLAYER_FIXTURES").unwrap());
    let base = directory.join("bear.mp4");
    let vfr = directory.join("bear-vfr.m4v");
    let flv = directory.join("bear-avc.flv");
    let ps = directory.join("bear-mpeg2.mpg");
    make_m4v(&base, &vfr)?;
    make_flv(&base, &flv)?;
    let transport = directory.join("bear.m2ts");
    make_program_stream(&transport, &ps, false)?;
    let truncated = directory.join("bear-mpeg2-truncated.mpg");
    make_program_stream(&transport, &truncated, true)?;

    let mut skipped_mp2 = false;
    engine::run(
        &LocalFile(truncated),
        software(0.0),
        &AtomicBool::new(false),
        |event| {
            if let Event::Notice(text) = event {
                skipped_mp2 |= text == "Incomplete MP2 frame skipped";
            }
            true
        },
    )?;
    assert!(skipped_mp2);

    for (name, format, needs_audio) in [
        ("bear-vfr.m4v", "mp4", false),
        ("bear-avc.flv", "flv", true),
        ("bear-mpeg2.mpg", "mpegps", true),
        ("bear_mpeg4_mp3.avi", "avi", true),
        ("bear_h264_aac.3gp", "mp4", true),
        ("bear.ogv", "ogg", true),
        ("bear.m2ts", "mpegts", true),
        ("bear-1280x720.ts", "mpegts", true),
    ] {
        let media = LocalFile(directory.join(name));
        let mut count = 0;
        let mut peak = 0.0_f32;
        engine::run(&media, software(0.0), &AtomicBool::new(false), |event| {
            match event {
                Event::Info(info) => assert_eq!(info.container, format, "{name}"),
                Event::Video(_) => count += 1,
                Event::Audio(chunk) => {
                    for value in chunk.samples.data {
                        peak = peak.max(value.abs());
                    }
                }
                _ => {}
            }
            true
        })?;
        assert!(count >= 25, "{name} decoded {count} frames");
        assert!(!needs_audio || peak > 0.001, "{name} produced silent audio");
        assert!(
            engine::preview(&media, 0.35, None, &AtomicBool::new(false))?.is_some(),
            "{name} seek preview"
        );
    }

    let media = Arc::new(LocalFile(vfr));
    let mut timeline = Vec::new();
    engine::run(
        media.as_ref(),
        software(0.0),
        &AtomicBool::new(false),
        |event| {
            if let Event::Video(frame) = event {
                timeline.push((frame.time, frame.picture.timestamp));
            }
            true
        },
    )?;
    assert!(
        timeline
            .windows(3)
            .any(|w| ((w[2].0 - w[1].0) - (w[1].0 - w[0].0)).abs() > 0.006)
    );
    let target = (timeline[8].0 + timeline[9].0) / 2.0;
    let frame = engine::preview(media.as_ref(), target, None, &AtomicBool::new(false))?.unwrap();
    assert_eq!(frame.picture.timestamp, timeline[8].1);
    assert!((frame.time - target).abs() < 0.001);

    let av = LocalFile(directory.join("bbb-320x240-2video-2audio.mp4"));
    let long_flv = directory.join("bbb-indexless.flv");
    make_flv(&av.0, &long_flv)?;
    let long_flv = LocalFile(long_flv);
    let target = 17.25;
    let mut expected = None;
    engine::run(&long_flv, software(0.0), &AtomicBool::new(false), |event| {
        if let Event::Video(frame) = event {
            if frame.time > target + 0.001 {
                return false;
            }
            expected = Some(frame.picture.frame.image_planes()[0].data.clone());
        }
        true
    })?;
    let preview = engine::preview(&long_flv, target, None, &AtomicBool::new(false))?
        .expect("preview in the latter half of an unindexed FLV");
    assert!((preview.time - target).abs() < 0.001);
    assert_eq!(
        preview.picture.frame.image_planes()[0].data,
        expected.unwrap()
    );

    let mut metadata_seen = false;
    engine::run(&av, software(0.0), &AtomicBool::new(false), |event| {
        let Event::Info(info) = event else {
            return true;
        };
        assert_eq!(info.container_label, "mp4");
        assert_eq!(
            info.file_size,
            Some(std::fs::metadata(&av.0).unwrap().len())
        );
        let video = info.video[0].video.as_ref().unwrap();
        assert_eq!(video.profile.as_deref(), Some("High"));
        assert_eq!(video.bit_depth, Some(8));
        assert_eq!(video.frame_rate, Some(30.0));
        assert_eq!(info.available_audio.len(), 2);
        assert_eq!(info.available_audio[0].sample_rate, Some(48_000));
        metadata_seen = true;
        false
    })?;
    assert!(metadata_seen);
    let webm = LocalFile(directory.join("bear-vp9-opus.webm"));
    let mut webm_identified = false;
    engine::run(&webm, software(0.0), &AtomicBool::new(false), |event| {
        if let Event::Info(info) = event {
            assert_eq!(info.container_label, "webm");
            webm_identified = true;
        }
        false
    })?;
    assert!(webm_identified);
    let target = 13.123;
    let mut video_start = None;
    let mut audio_start = None;
    engine::run(&av, software(target), &AtomicBool::new(false), |event| {
        match event {
            Event::Video(frame) => {
                video_start.get_or_insert(frame.time);
            }
            Event::Audio(chunk) => {
                audio_start.get_or_insert((chunk.time, 1.0 / f64::from(chunk.samples.rate)));
            }
            _ => {}
        }
        video_start.is_none() || audio_start.is_none()
    })?;
    assert!((video_start.unwrap() - target).abs() < 0.001);
    let (audio_start, sample_period) = audio_start.unwrap();
    assert!((audio_start - target).abs() <= sample_period);

    let mut player = Player::new(|| {}, DecodeMode::Software);
    player.open_paused(media.clone(), 0.17);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(frame) = player.tick() {
            assert!((frame.time - 0.17).abs() < 0.001);
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "Restored frame was not displayed"
        );
        thread::sleep(Duration::from_millis(4));
    }
    assert!(!player.is_playing());
    assert_eq!(player.position(), 0.17);

    player.open(media, 0.0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while player.position() < 0.16 {
        player.tick();
        ensure!(Instant::now() < deadline, "Player did not start");
        thread::sleep(Duration::from_millis(4));
    }
    player.play_pause();
    let paused = player.position();
    for _ in 0..12 {
        assert!(player.tick().is_none());
        assert_eq!(player.position(), paused);
        thread::sleep(Duration::from_millis(5));
    }
    player.play_pause();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut count = 0;
    while count < 8 {
        if let Some(frame) = player.tick() {
            assert!(frame.time >= paused - 0.001);
            assert!((frame.time - player.position()).abs() < 0.075);
            count += 1;
        }
        ensure!(Instant::now() < deadline, "Player did not resume");
        thread::sleep(Duration::from_millis(4));
    }
    Ok(())
}

fn software(position: f64) -> Options {
    Options {
        position,
        mode: DecodeMode::Software,
        ..Options::default()
    }
}

fn packets(demuxer: &mut dyn Demuxer, mut next: impl FnMut(Packet) -> Result<()>) -> Result<()> {
    loop {
        match demuxer.next_packet() {
            Ok(packet) => next(packet)?,
            Err(Error::Eof) => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
}

fn make_m4v(source: &Path, output: &Path) -> Result<()> {
    let mut demux = LocalFile(source.into()).open_demuxer(&runtime())?;
    let track = demux
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .unwrap()
        .clone();
    let mut mux = oxideav_mp4::muxer::open_with_options(
        Box::new(File::create(output)?),
        std::slice::from_ref(&track),
        Mp4MuxerOptions {
            brand: BrandPreset::Custom {
                major: *b"M4V ",
                compatible: vec![*b"M4V ", *b"isom"],
            },
            write_edit_list: false,
            ..Mp4MuxerOptions::default()
        },
    )?;
    mux.write_header()?;
    packets(demux.as_mut(), |mut packet| {
        if packet.stream_index != track.index {
            return Ok(());
        }
        let step = packet.duration.unwrap_or(1).max(1);
        let map = |t: i64| t + t.div_euclid(2 * step) * step / 2;
        if let Some(dts) = packet.dts {
            packet.duration = Some(map(dts + step) - map(dts));
            packet.dts = Some(map(dts));
        }
        packet.pts = packet.pts.map(map);
        mux.write_packet(&packet)?;
        Ok(())
    })?;
    mux.write_trailer()?;
    Ok(())
}

fn make_flv(source: &Path, output: &Path) -> Result<()> {
    let mut demux = LocalFile(source.into()).open_demuxer(&runtime())?;
    let mut out = BufWriter::new(File::create(output)?);
    out.write_all(b"FLV\x01\x05\0\0\0\x09\0\0\0\0")?;
    let video = demux
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "h264")
        .unwrap();
    let audio = demux
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "aac")
        .unwrap();
    let (video_id, audio_id) = (video.index, audio.index);
    write_flv_tag(&mut out, 9, 0, &[0x17, 0, 0, 0, 0], &video.params.extradata)?;
    write_flv_tag(&mut out, 8, 0, &[0xaf, 0], &audio.params.extradata)?;
    packets(demux.as_mut(), |packet| {
        let base = packet.time_base.as_rational();
        let ms = |ts: i64| (ts * base.num * 1000 / base.den) as i32;
        let dts = ms(packet.dts.or(packet.pts).unwrap_or_default());
        let clock = (dts + 100).max(0) as u32;
        if packet.stream_index == video_id {
            let delta = (ms(packet.pts.unwrap_or_default()) - dts).to_be_bytes();
            let kind = if packet.flags.keyframe { 0x17 } else { 0x27 };
            write_flv_tag(
                &mut out,
                9,
                clock,
                &[kind, 1, delta[1], delta[2], delta[3]],
                &packet.data,
            )?;
        } else if packet.stream_index == audio_id {
            write_flv_tag(&mut out, 8, clock, &[0xaf, 1], &packet.data)?;
        }
        Ok(())
    })?;
    Ok(())
}

fn write_flv_tag(
    out: &mut impl Write,
    tag: u8,
    time: u32,
    header: &[u8],
    data: &[u8],
) -> Result<()> {
    let size = (header.len() + data.len()) as u32;
    out.write_all(&[tag])?;
    out.write_all(&size.to_be_bytes()[1..])?;
    out.write_all(&time.to_be_bytes()[1..])?;
    out.write_all(&[time.to_be_bytes()[0], 0, 0, 0])?;
    out.write_all(header)?;
    out.write_all(data)?;
    out.write_all(&(size + 11).to_be_bytes())?;
    Ok(())
}

fn make_program_stream(source: &Path, output: &Path, partial_audio: bool) -> Result<()> {
    use oxideav_mpegts::pes::PesHeaderSpec;
    let mut demux = LocalFile(source.into()).open_demuxer(&runtime())?;
    let mut out = BufWriter::new(File::create(output)?);
    // MPEG-2 pack header at SCR zero, followed by video and audio PES.
    out.write_all(&[0, 0, 1, 0xba, 0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xc3, 0xf8])?;
    let mut partial = None;
    packets(demux.as_mut(), |packet| {
        let stream = if packet.stream_index == 0 { 0xe0 } else { 0xc0 };
        let spec = if let Some(pts) = packet.pts {
            PesHeaderSpec::with_timestamps(
                stream,
                pts as u64,
                packet.dts.filter(|&dts| dts != pts).map(|dts| dts as u64),
            )
        } else {
            PesHeaderSpec {
                stream_id: stream,
                ..Default::default()
            }
        };
        out.write_all(&spec.encode(&packet.data)?)?;
        if partial_audio && stream == 0xc0 && packet.data.len() > 12 {
            partial = Some(spec.encode(&packet.data[..12])?);
        }
        Ok(())
    })?;
    if let Some(partial) = partial {
        out.write_all(&partial)?;
    }
    out.write_all(&[0, 0, 1, 0xb9])?;
    Ok(())
}
