mod bitmap;

use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, ensure};
use oxideav_core::{Error, MediaType};

use super::decode::seconds;
use crate::source::{self, MediaSource};

pub use bitmap::BitmapFrame;
use bitmap::BitmapTrack;

pub enum Sidecar {
    Text(Vec<Cue>),
    Bitmap(Box<BitmapTrack>),
}

impl Sidecar {
    pub(super) fn cues(&self) -> &[Cue] {
        match self {
            Self::Text(cues) => cues,
            Self::Bitmap(track) => track.cues(),
        }
    }
}

pub(super) fn transcript(
    source: &dyn MediaSource,
    origin: f64,
    cancel: &AtomicBool,
) -> Result<HashMap<u32, Vec<Cue>>> {
    let mut demux = source.open_demuxer(&source::runtime())?;
    let codecs: HashMap<_, _> = demux
        .streams()
        .iter()
        .filter(|s| {
            s.params.media_type == MediaType::Subtitle && supported(s.params.codec_id.as_str())
        })
        .map(|s| (s.index, s.params.codec_id.to_string()))
        .collect();
    if codecs.is_empty() {
        return Ok(HashMap::new());
    }
    demux.set_active_streams(&codecs.keys().copied().collect::<Vec<_>>());
    let mut tracks: HashMap<u32, Vec<Cue>> = HashMap::new();
    let mut entries = 0;
    let mut bytes = 0;
    while !cancel.load(Ordering::Relaxed) {
        let packet = match demux.next_packet() {
            Ok(packet) => packet,
            Err(Error::Eof) => break,
            Err(err) => return Err(err.into()),
        };
        if packet.flags.header || packet.flags.discard {
            continue;
        }
        if let Some(codec) = codecs.get(&packet.stream_index)
            && let Some(pts) = packet.pts.or(packet.dts)
        {
            let time = seconds(pts, packet.time_base) - origin;
            let length = packet
                .duration
                .map_or(3.0, |d| seconds(d, packet.time_base));
            ensure!(
                packet.data.len() <= 1024 * 1024,
                "Subtitle packet is larger than 1 MiB"
            );
            if let Some(cue) = embedded(codec, &packet.data, time, time + length) {
                entries += 1;
                bytes += cue.text.len();
                ensure!(
                    entries <= 200_000 && bytes <= 32 * 1024 * 1024,
                    "Subtitle transcript is too large"
                );
                tracks.entry(packet.stream_index).or_default().push(cue);
            }
        }
    }
    for cues in tracks.values_mut() {
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    }
    Ok(tracks)
}

pub fn active_cue(cues: &[Cue], time: f64) -> Option<usize> {
    let until = cues.partition_point(|cue| cue.start <= time);
    (0..until).rev().find(|&i| cues[i].end > time)
}

pub fn load_sidecar(path: &Path) -> Result<Sidecar> {
    if let Some(track) = BitmapTrack::open(path)? {
        Ok(Sidecar::Bitmap(Box::new(track)))
    } else {
        read_sidecar(path).map(Sidecar::Text)
    }
}

#[derive(Clone, Debug)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

pub fn read_sidecar(path: &Path) -> Result<Vec<Cue>> {
    let mut text = String::new();
    File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() <= 8 * 1024 * 1024,
        "Subtitle file is larger than 8 MiB"
    );
    let text = text.trim_start_matches('\u{feff}');
    let mut cues = if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ass") || ext.eq_ignore_ascii_case("ssa"))
    {
        ass_sidecar(text)
    } else {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .split("\n\n")
            .filter_map(|block| {
                let mut lines = block.lines();
                let timing = lines.find(|line| line.contains("-->"))?;
                let (start, end) = timing.split_once("-->")?;
                cue(
                    parse_time(start.trim())?,
                    parse_time(end.split_whitespace().next()?)?,
                    plain(&lines.collect::<Vec<_>>().join("\n")),
                )
            })
            .collect()
    };
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    ensure!(!cues.is_empty(), "No timed subtitles in {}", path.display());
    Ok(cues)
}

fn ass_sidecar(text: &str) -> Vec<Cue> {
    // ASS and SSA share these positions in the default dialogue format.
    let (mut start, mut end, mut fields) = (1, 2, 10);
    let mut events = false;
    let mut cues = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            events = line.eq_ignore_ascii_case("[Events]");
        }
        let Some((kind, value)) = line.split_once(':').filter(|_| events) else {
            continue;
        };
        if kind.eq_ignore_ascii_case("Format") {
            let names: Vec<_> = value.split(',').map(str::trim).collect();
            if let Some(first) = names.iter().position(|s| s.eq_ignore_ascii_case("Start"))
                && let Some(last) = names.iter().position(|s| s.eq_ignore_ascii_case("End"))
                && names.last().is_some_and(|s| s.eq_ignore_ascii_case("Text"))
                && first < names.len() - 1
                && last < names.len() - 1
            {
                (start, end, fields) = (first, last, names.len());
            }
        } else if kind.eq_ignore_ascii_case("Dialogue") {
            let values: Vec<_> = value.splitn(fields, ',').collect();
            if let [.., body] = values.as_slice()
                && values.len() == fields
                && let Some(first) = parse_time(values[start].trim())
                && let Some(last) = parse_time(values[end].trim())
                && let Some(cue) = cue(first, last, ass_text(body))
            {
                cues.push(cue);
            }
        }
    }
    cues
}

fn parse_time(value: &str) -> Option<f64> {
    let mut parts = value.rsplit(':');
    let seconds: f64 = parts.next()?.replace(',', ".").parse().ok()?;
    let minutes: u32 = parts.next()?.parse().ok()?;
    let hours: u32 = parts.next().map_or(Some(0), |v| v.parse().ok())?;
    (parts.next().is_none() && (0.0..60.0).contains(&seconds))
        .then_some(f64::from(hours) * 3600.0 + f64::from(minutes) * 60.0 + seconds)
}

pub fn supported(id: &str) -> bool {
    matches!(
        id,
        "subrip" | "webvtt" | "mov_text" | "text" | "ass" | "ssa" | "sbtt" | "stxt"
    )
}

pub fn embedded(id: &str, bytes: &[u8], start: f64, end: f64) -> Option<Cue> {
    let text = match id {
        "mov_text" | "text" => {
            let size = u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]) as usize;
            let data = bytes.get(2..2 + size)?;
            if data.starts_with(&[0xfe, 0xff]) || data.starts_with(&[0xff, 0xfe]) {
                let be = data[0] == 0xfe;
                let words: Vec<_> = data[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| {
                        if be {
                            u16::from_be_bytes([b[0], b[1]])
                        } else {
                            u16::from_le_bytes([b[0], b[1]])
                        }
                    })
                    .collect();
                String::from_utf16_lossy(&words)
            } else {
                String::from_utf8_lossy(data).into_owned()
            }
        }
        "ass" | "ssa" => {
            let text = String::from_utf8_lossy(bytes);
            return cue(start, end, ass_text(text.splitn(9, ',').nth(8)?));
        }
        "webvtt" if bytes.get(4..8) == Some(b"vttc") || bytes.get(4..8) == Some(b"vtte") => {
            atoms(bytes)
                .filter(|(tag, _)| *tag == b"vttc")
                .flat_map(|(_, body)| atoms(body))
                .filter(|(tag, _)| *tag == b"payl")
                .map(|(_, payload)| String::from_utf8_lossy(payload))
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    };
    cue(start, end, plain(&text))
}

fn ass_text(text: &str) -> String {
    plain(
        &text
            .replace("\\N", "\n")
            .replace("\\n", "\n")
            .replace("\\h", " "),
    )
}

fn atoms(mut bytes: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        let tag = bytes.get(4..8)?;
        let body = bytes.get(8..size)?;
        bytes = bytes.get(size..)?;
        Some((tag, body))
    })
}

fn plain(text: &str) -> String {
    let mut end = None;
    let mut output = String::new();
    for c in text.chars() {
        match (end, c) {
            (None, '<') => end = Some('>'),
            (None, '{') => end = Some('}'),
            (Some(stop), c) if stop == c => end = None,
            (None, c) if !c.is_control() || c == '\n' => output.push(c),
            _ => {}
        }
    }
    output
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn cue(start: f64, end: f64, text: String) -> Option<Cue> {
    let text = text.trim();
    (end > start && !text.is_empty()).then(|| Cue {
        start,
        end,
        text: text.to_owned(),
    })
}
