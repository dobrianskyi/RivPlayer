use std::{path::PathBuf, sync::atomic::AtomicBool};

use anyhow::{Context, Result, ensure};
use rivplayer::{
    engine::{self, Event, Options},
    source::LocalFile,
};

pub fn inspect(path: PathBuf, options: Options) -> Result<()> {
    let mut frames = 0_u64;
    let mut sample_frames = 0_u64;
    let mut captions = 0;
    let mut checksum = 0_u64;
    let mut peak = 0_f32;
    let mut first_video = None;
    let mut last_video = f64::NEG_INFINITY;
    let mut reordered = 0;
    let mut has_video = false;
    let mut has_audio = false;
    let mut track_warning = None;
    let start = std::time::Instant::now();
    let title = path.display().to_string();
    engine::run(
        &LocalFile(path),
        options,
        &AtomicBool::new(false),
        |event| {
            match event {
                Event::Info(info) => {
                    println!("{title}  [{}]", info.container);
                    for track in &info.video {
                        println!(
                            "  video #{} {} {}x{}{}",
                            track.index,
                            track.codec,
                            track.width.unwrap_or_default(),
                            track.height.unwrap_or_default(),
                            if Some(track.index) == info.current_video {
                                " (selected)"
                            } else {
                                ""
                            }
                        );
                    }
                    for track in &info.audio {
                        println!(
                            "  audio #{} {}{}",
                            track.index,
                            track.codec,
                            if Some(track.index) == info.current_audio {
                                " (selected)"
                            } else {
                                ""
                            }
                        );
                    }
                    has_video = info.current_video.is_some();
                    has_audio = info.current_audio.is_some();
                    if let Some(backend) = info.backend {
                        println!("  Decoder {backend}");
                    }
                }
                Event::Video(video) => {
                    frames += 1;
                    if first_video.is_none() {
                        first_video = Some(video.time);
                    }
                    if video.time + 0.001 < last_video {
                        reordered += 1;
                    }
                    last_video = video.time;
                    let picture = &video.picture;
                    if let Some(plane) = picture.frame.image_planes().first() {
                        for &sample in plane.data.iter().step_by(101) {
                            checksum = checksum
                                .wrapping_mul(1_000_003)
                                .wrapping_add(u64::from(sample));
                        }
                    }
                }
                Event::Audio(audio) => {
                    sample_frames += (audio.samples.data.len() / audio.samples.channels) as u64;
                    for sample in &audio.samples.data {
                        peak = peak.max(sample.abs());
                    }
                }
                Event::Caption(_, _) => captions += 1,
                Event::End(duration) => println!("  Decoded timeline {duration:.3} s"),
                Event::Notice(message) => {
                    eprintln!("  {message}");
                    if message.starts_with("Audio") || message.starts_with("Video") {
                        track_warning = Some(message);
                    }
                }
                Event::Sidecar(_) => {}
            }
            true
        },
    )
    .with_context(|| title)?;
    println!(
        "  {frames} video frames, {sample_frames} audio sample frames, {captions} subtitle cues in {:.3} s",
        start.elapsed().as_secs_f64(),
    );
    if let Some(first) = first_video {
        println!(
            "  Video PTS {first:.3}..{last_video:.3} s, {reordered} out of order, luma {checksum:016x}"
        );
    }
    if sample_frames > 0 {
        println!("  Audio peak {peak:.5}");
    }
    ensure!(
        track_warning.is_none(),
        "{}",
        track_warning.unwrap_or_default()
    );
    ensure!(!has_video || frames > 0, "Expected video frames");
    ensure!(!has_audio || sample_frames > 0, "Expected audio samples");
    ensure!(
        reordered == 0,
        "Decoded video was not in presentation order"
    );
    Ok(())
}
