mod inspect;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use rvideoplayer::engine::{DecodeMode, Options};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut paths = Vec::new();
    let mut options = Options::default();
    let mut check = false;
    let mut literal = false;
    let mut position_explicit = false;
    while let Some(arg) = args.next() {
        match arg.to_str().filter(|_| !literal) {
            Some("--help" | "-h") => {
                println!(
                    "RVideoPlayer [--software | --vaapi] [FILE]\nRVideoPlayer --check [--software | --vaapi] [--seek SECONDS] [--video-track ID] [--audio-track ID] FILE…"
                );
                return Ok(());
            }
            Some("--check") => check = true,
            Some("--video-track" | "--audio-track") => {
                let index = args
                    .next()
                    .and_then(|arg| arg.into_string().ok())
                    .context("Missing track index")?
                    .parse()
                    .context("Invalid track index")?;
                if arg == "--video-track" {
                    options.video = Some(index);
                } else {
                    options.audio = Some(index);
                }
            }
            Some("--software") => options.mode = DecodeMode::Software,
            Some("--vaapi") => options.mode = DecodeMode::Vaapi,
            Some("--seek") => {
                position_explicit = true;
                let value = args
                    .next()
                    .and_then(|arg| arg.into_string().ok())
                    .context("--seek needs seconds")?;
                options.position = value.parse().context("Invalid seek position")?;
                ensure!(
                    options.position.is_finite() && options.position >= 0.0,
                    "Invalid seek position"
                );
            }
            Some("--") => literal = true,
            Some(arg) if arg.starts_with('-') => bail!("Unknown option {arg}"),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if check {
        ensure!(!paths.is_empty(), "--check requires at least one file");
        if options.mode == DecodeMode::Auto {
            options.mode = DecodeMode::Software;
        }
        let mut failures = 0;
        for path in paths {
            if let Err(err) = inspect::inspect(path, options) {
                eprintln!("{err:#}");
                failures += 1;
            }
        }
        ensure!(failures == 0, "{failures} file(s) failed");
        return Ok(());
    }
    ensure!(
        options.video.is_none() && options.audio.is_none(),
        "Track flags are for --check"
    );
    ensure!(paths.len() <= 1, "Open one file at a time");
    ui::launch(paths.pop(), options, position_explicit)
}
