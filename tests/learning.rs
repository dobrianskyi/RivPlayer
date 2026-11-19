use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rivplayer::{
    engine::{Captions, DecodeMode, Player, Repeat, active_cue},
    source::LocalFile,
};

fn fixture(name: &str) -> Result<Arc<LocalFile>> {
    Ok(Arc::new(LocalFile(
        PathBuf::from(std::env::var_os("RIVPLAYER_FIXTURES").context("Missing fixture directory")?)
            .join(name),
    )))
}

#[test]
#[ignore = "run scripts/check-containers.sh with its isolated ALSA device"]
fn transcripts_include_later_lines_and_seek_to_an_active_subtitle() -> Result<()> {
    let mut player = Player::new(|| {}, DecodeMode::Software);
    player.open_paused(fixture("bear-1280x720-avt_subt_frag.mp4")?, 0.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        player.tick();
        player.read_transcript();
        if player.info.is_some() && !player.transcript_loading() {
            break;
        }
        ensure!(Instant::now() < deadline, "Transcript indexing timed out");
        thread::sleep(Duration::from_millis(5));
    }
    let info = player.info.as_ref().context("Media info missing")?;
    let mut tested = 0;
    for index in info.captions.iter().map(|t| t.index).collect::<Vec<_>>() {
        let choice = Captions::Track(index);
        let cues = player.cues(choice);
        assert!(cues.windows(2).all(|pair| pair[0].start <= pair[1].start));
        if let Some(cue) = cues.last() {
            let time = cue.start.max(0.0) + 0.005;
            let text = cue.text.clone();
            assert_eq!(active_cue(cues, time), Some(cues.len() - 1));
            player.captions = choice;
            player.seek(time);
            assert!((player.position() - time).abs() < 0.0001);
            assert!(
                player
                    .caption_at(time)
                    .is_some_and(|txt| txt.contains(&text))
            );
            tested += 1;
        }
    }
    assert!(tested > 0);
    Ok(())
}

#[test]
#[ignore = "run scripts/check-containers.sh with its isolated ALSA device"]
fn repeat_prepares_three_passes_at_two_speeds_with_audio() -> Result<()> {
    for (name, rate) in [
        ("bbb-320x240-2video-2audio.mp4", 1.25),
        ("bear-vp9-opus.webm", 0.75),
    ] {
        let mut player = Player::new(|| {}, DecodeMode::Software);
        player.set_rate(rate);
        player.open(fixture(name)?, 0.45);
        let range = Repeat::new(0.45, 1.13).unwrap();
        player.set_repeat(Some(range));
        let mut previous = 0.45;
        let mut wraps = 0;
        let mut shown = 0;
        let deadline = Instant::now() + Duration::from_secs(8);
        while wraps < 3 && Instant::now() < deadline {
            let frame = player.tick();
            let time = player.position();
            if time < previous - 0.3 {
                wraps += 1;
                assert!(
                    previous > range.end - 0.09 && time < range.start + 0.09,
                    "{name}: {previous} -> {time}"
                );
                assert!(
                    !player.is_loading(),
                    "Prerolled loop fell back to a cold seek in {name}"
                );
                assert!(player.is_playing());
            }
            if let Some(frame) = frame {
                assert!(frame.time >= range.start - 0.01 && frame.time <= range.end + 0.01);
                shown += 1;
            }
            assert!(player.error.is_none(), "{name}: {:?}", player.error);
            assert!(
                player
                    .notice
                    .as_ref()
                    .is_none_or(|notice| !notice.starts_with("Audio output")),
                "{name}: {:?}",
                player.notice
            );
            previous = time;
            thread::sleep(Duration::from_millis(3));
        }
        assert_eq!(wraps, 3, "{name}");
        assert!(shown > 20, "{name}");
        assert_eq!(player.rate(), rate);
    }
    Ok(())
}
