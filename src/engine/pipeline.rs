use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use oxideav_core::{ColorSignal, Demuxer, Error, MediaType, Packet, PixelFormat, StreamInfo};

use super::{
    captions::{self, Cue, Sidecar},
    decode::{AudioDecoder, Picture, Samples, VideoDecoder, seconds},
};
use crate::source::{self, MediaSource};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecodeMode {
    #[default]
    Auto,
    Software,
    Vaapi,
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub position: f64,
    pub video: Option<u32>,
    pub audio: Option<u32>,
    pub mode: DecodeMode,
    pub track_fallback: bool,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pixel_aspect: f32,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub video: Option<VideoDetails>,
}

#[derive(Clone, Debug)]
pub struct VideoDetails {
    pub frame_rate: Option<f64>,
    pub bit_rate: Option<u64>,
    pub profile: Option<String>,
    pub bit_depth: Option<u8>,
    pub format: Option<PixelFormat>,
    pub color: ColorSignal,
}

impl From<&StreamInfo> for Track {
    fn from(stream: &StreamInfo) -> Self {
        let parse = |key| stream.params.options.get(key)?.parse::<f32>().ok();
        let pixel_aspect = parse("pasp_h_spacing")
            .zip(parse("pasp_v_spacing"))
            .map(|(width, height)| width / height)
            .filter(|v| (0.05..=20.0).contains(v))
            .unwrap_or(1.0);
        let params = &stream.params;
        let video = (params.media_type == MediaType::Video).then(|| {
            let (profile, bit_depth) = super::metadata::codec_configuration(params);
            VideoDetails {
                frame_rate: params
                    .frame_rate
                    .filter(|r| r.num > 0 && r.den > 0)
                    .map(|r| r.num as f64 / r.den as f64),
                bit_rate: params.bit_rate.filter(|&rate| rate > 0),
                profile,
                bit_depth,
                format: params.pixel_format,
                color: params.color_signal,
            }
        });
        Self {
            index: stream.index,
            codec: stream.params.codec_id.to_string(),
            language: stream.params.language.clone(),
            width: stream.params.width,
            height: stream.params.height,
            pixel_aspect,
            sample_rate: params.sample_rate.filter(|&value| value > 0),
            channels: params.resolved_channels().filter(|&value| value > 0),
            video,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Info {
    pub title: String,
    pub container: String,
    pub container_label: String,
    pub file_size: Option<u64>,
    pub duration: Option<f64>,
    pub start: f64,
    pub video: Vec<Track>,
    pub audio: Vec<Track>,
    pub captions: Vec<Track>,
    pub available_audio: Vec<Track>,
    pub available_captions: Vec<Track>,
    pub current_video: Option<u32>,
    pub current_audio: Option<u32>,
    pub external_audio: Option<Track>,
    pub backend: Option<&'static str>,
}

pub enum Event {
    Info(Box<Info>),
    Video(TimedPicture),
    Audio(AudioChunk),
    Caption(u32, Cue),
    Sidecar(Sidecar),
    Notice(String),
    End(f64),
}

pub struct TimedPicture {
    pub time: f64,
    pub picture: Picture,
}

pub struct AudioChunk {
    pub time: f64,
    pub samples: Samples,
}

impl AudioChunk {
    pub fn end(&self) -> f64 {
        self.time
            + self.samples.data.len() as f64
                / self.samples.channels as f64
                / f64::from(self.samples.rate)
    }
}

#[derive(Debug)]
pub struct HardwareError(pub anyhow::Error);
impl std::fmt::Display for HardwareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VA-API decode failed: {:#}", self.0)
    }
}
impl std::error::Error for HardwareError {}

pub fn run(
    source: &dyn MediaSource,
    options: Options,
    cancel: &AtomicBool,
    emit: impl FnMut(Event) -> bool,
) -> Result<()> {
    run_session(source, None, options, cancel, true, emit)
}

pub(super) fn run_session(
    source: &dyn MediaSource,
    external_audio: Option<&dyn MediaSource>,
    options: Options,
    cancel: &AtomicBool,
    load_sidecar: bool,
    mut emit: impl FnMut(Event) -> bool,
) -> Result<()> {
    guarded_decode(
        source,
        external_audio,
        options,
        cancel,
        false,
        load_sidecar,
        &mut emit,
    )
}

pub fn preview(
    source: &dyn MediaSource,
    position: f64,
    video: Option<u32>,
    cancel: &AtomicBool,
) -> Result<Option<TimedPicture>> {
    sample_preview(source, position, video, cancel).map(|(_, picture)| picture)
}

pub fn sample_preview(
    source: &dyn MediaSource,
    position: f64,
    video: Option<u32>,
    cancel: &AtomicBool,
) -> Result<(Option<f64>, Option<TimedPicture>)> {
    let mut frame = None;
    let mut duration = None;
    let options = Options {
        position,
        video,
        mode: DecodeMode::Software,
        ..Options::default()
    };
    guarded_decode(source, None, options, cancel, true, false, &mut |event| {
        match event {
            Event::Info(info) => duration = info.duration,
            Event::Video(picture) => {
                frame = Some(picture);
                return false;
            }
            _ => {}
        }
        true
    })?;
    Ok((duration, frame))
}

fn guarded_decode(
    source: &dyn MediaSource,
    external_audio: Option<&dyn MediaSource>,
    options: Options,
    cancel: &AtomicBool,
    video_only: bool,
    load_sidecar: bool,
    emit: &mut dyn FnMut(Event) -> bool,
) -> Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode(
            source,
            external_audio,
            options,
            cancel,
            video_only,
            load_sidecar,
            emit,
        )
    }))
    .map_err(|_| anyhow::anyhow!("Decoder stopped on malformed media"))?
}

fn decode(
    source: &dyn MediaSource,
    external_audio: Option<&dyn MediaSource>,
    options: Options,
    cancel: &AtomicBool,
    video_only: bool,
    load_sidecar: bool,
    emit: &mut dyn FnMut(Event) -> bool,
) -> Result<()> {
    let preview_deadline = video_only.then(|| Instant::now() + Duration::from_secs(4));
    let ctx = source::runtime();
    let (mut demux, container_label) = source.open_described(&ctx)?;
    let tracks = demux.streams().to_vec();
    let framed_dts = container_label == "matroska";
    let is_video = |s: &&StreamInfo| {
        s.params.media_type == MediaType::Video
            && VideoDecoder::supported(s.params.codec_id.as_str())
    };
    let is_audio = |s: &&StreamInfo| {
        let codec = s.params.codec_id.as_str();
        s.params.media_type == MediaType::Audio
            && AudioDecoder::supported(codec)
            // The DTS adapter expects one Core access unit at the head
            // of each packet. Transport demuxers can emit partial PES.
            && (codec != "dts" || framed_dts)
    };
    let is_text = |s: &&StreamInfo| {
        s.params.media_type == MediaType::Subtitle
            && captions::supported(s.params.codec_id.as_str())
    };
    let find_video = || {
        tracks
            .iter()
            .filter(is_video)
            .find(|s| Some(s.index) == options.video)
            .or_else(|| tracks.iter().find(is_video))
    };
    let video = find_video();
    let audio = tracks
        .iter()
        .filter(is_audio)
        .find(|s| Some(s.index) == options.audio)
        .or_else(|| tracks.iter().find(is_audio));
    if let Some(index) = options.video
        && !options.track_fallback
    {
        anyhow::ensure!(
            video.is_some_and(|s| s.index == index),
            "Video track #{index} is unavailable"
        );
    }
    if let Some(index) = options.audio
        && !options.track_fallback
    {
        anyhow::ensure!(
            audio.is_some_and(|s| s.index == index),
            "Audio track #{index} is unavailable"
        );
    }
    let main = video.or(audio).with_context(|| {
        let found = tracks
            .iter()
            .map(|s| s.params.codec_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("No supported audio or video track ({found})")
    })?;
    let mut initial_packets = VecDeque::new();
    let packet_timestamps = matches!(demux.format_name(), "mpegts" | "mpegps");
    let start = if packet_timestamps {
        let mut origins = Vec::new();
        let chosen: Vec<_> = video
            .into_iter()
            .chain(audio)
            .map(|stream| stream.index)
            .collect();
        let mut bytes = 0;
        for _ in 0..48 {
            let packet = match demux.next_packet() {
                Ok(packet) => packet,
                Err(Error::Eof) => break,
                Err(err) => return Err(err).context("Reading MPEG timestamps"),
            };
            if chosen.contains(&packet.stream_index)
                && !origins.iter().any(|(i, _)| *i == packet.stream_index)
                && let Some(time) = packet_time(&packet)
            {
                origins.push((packet.stream_index, time));
            }
            bytes += packet.data.len();
            initial_packets.push_back(packet);
            if origins.len() == chosen.len() || bytes >= 8 * 1024 * 1024 {
                break;
            }
        }
        origins
            .into_iter()
            .map(|(_, time)| time)
            .min_by(f64::total_cmp)
    } else {
        video
            .into_iter()
            .chain(audio)
            .filter_map(|s| s.start_time.map(|v| seconds(v, s.time_base)))
            .min_by(f64::total_cmp)
    }
    .unwrap_or_default();
    let duration = demux
        .duration_micros()
        .filter(|&n| n > 0)
        .map(|n| n as f64 / 1_000_000.0)
        .or_else(|| {
            video
                .into_iter()
                .chain(audio)
                .filter_map(|s| s.duration.map(|v| seconds(v, s.time_base)))
                .max_by(f64::total_cmp)
        })
        .filter(|&n| n > 0.0);
    let mut video_decoder = video
        .map(|s| VideoDecoder::open(&ctx, &s.params, options.mode))
        .transpose()?;
    let backend = video_decoder.as_ref().map(VideoDecoder::backend);
    if options.mode == DecodeMode::Vaapi && backend != Some("VA-API") {
        bail!("VA-API could not initialize for this stream");
    }
    let target = options.position.max(0.0);
    let mut external_error = None;
    let external = external_audio
        .filter(|_| video.is_some())
        .and_then(
            |source| match ExternalAudio::open(source, &ctx, target, cancel) {
                Ok(track) => Some(track),
                Err(err) => {
                    external_error = Some(format!("External audio: {err:#}"));
                    None
                }
            },
        );
    if external_audio.is_some() && video.is_none() {
        external_error = Some("External audio requires a video track".into());
    }
    let (mut external, imported_decoder) = match external {
        Some((track, decoder)) => (Some(track), Some(decoder)),
        None => (None, None),
    };
    let external_track = external.as_ref().map(|external| external.track.clone());
    let audio = audio.filter(|_| !video_only && external_track.is_none());
    let selected_audio = external_track.as_ref().or(audio);
    let mut audio_error = None;
    let mut audio_decoder = match if imported_decoder.is_some() {
        Ok(imported_decoder)
    } else {
        audio.map(|s| AudioDecoder::open(&s.params)).transpose()
    } {
        Ok(decoder) => decoder,
        Err(err) if video.is_some() => {
            audio_error = Some(format!("Audio unavailable: {err:#}"));
            None
        }
        Err(err) => return Err(err),
    };
    let active: Vec<_> = video
        .into_iter()
        .chain(audio.filter(|_| audio_decoder.is_some()))
        .chain(tracks.iter().filter(is_text).filter(|_| !video_only))
        .map(|s| s.index)
        .collect();
    demux.set_active_streams(&active);
    if target > 0.0 && (!video_only || video.is_some()) {
        let base = main.time_base.as_rational();
        let ticks = ((target + start) * base.den as f64 / base.num as f64) as i64;
        // FLV without a keyframe table can report the requested time while
        // positioning at EOF. Exact reports from this demuxer need a scan.
        let flv = demux.format_name() == "flv";
        let ps = demux.format_name() == "mpegps";
        if !ps
            && matches!(demux.seek_to(main.index, ticks), Ok(actual) if actual < ticks || (!flv && actual == ticks))
        {
            initial_packets.clear();
        } else {
            demux = source.open_demuxer(&ctx)?;
            demux.set_active_streams(&active);
            initial_packets.clear();
            if (flv || ps) && video.is_some() {
                match scan_to_keyframe(
                    demux.as_mut(),
                    main.index,
                    target + start,
                    ps,
                    &active,
                    || {
                        cancel.load(Ordering::Relaxed)
                            || preview_deadline.is_some_and(|end| Instant::now() >= end)
                    },
                ) {
                    Ok(Some(packets)) => initial_packets = packets,
                    _ if cancel.load(Ordering::Relaxed)
                        || preview_deadline.is_some_and(|end| Instant::now() >= end) =>
                    {
                        return Ok(());
                    }
                    _ => {
                        demux = source.open_demuxer(&ctx)?;
                        demux.set_active_streams(&active);
                    }
                }
            }
        }
    }
    let info = Info {
        title: source.title(),
        container: demux.format_name().to_owned(),
        container_label,
        file_size: source.byte_len(),
        duration,
        start,
        video: tracks.iter().filter(is_video).map(Track::from).collect(),
        audio: tracks.iter().filter(is_audio).map(Track::from).collect(),
        captions: tracks.iter().filter(is_text).map(Track::from).collect(),
        available_audio: tracks
            .iter()
            .filter(|s| s.params.media_type == MediaType::Audio)
            .map(Track::from)
            .collect(),
        available_captions: tracks
            .iter()
            .filter(|s| s.params.media_type == MediaType::Subtitle)
            .map(Track::from)
            .collect(),
        current_video: video.map(|s| s.index),
        current_audio: audio.filter(|_| audio_decoder.is_some()).map(|s| s.index),
        external_audio: external_track
            .as_ref()
            .filter(|_| audio_decoder.is_some())
            .map(Track::from),
        backend,
    };
    if !emit(Event::Info(Box::new(info))) {
        return Ok(());
    }
    if let Some(err) = external_error
        && !emit(Event::Notice(err))
    {
        return Ok(());
    }
    if video_only && video.is_none() {
        return Ok(());
    }
    if !video_only
        && video.is_none()
        && let Some(track) = tracks
            .iter()
            .find(|s| s.params.media_type == MediaType::Video)
        && !emit(Event::Notice(format!(
            "Video unavailable: {}",
            track.params.codec_id
        )))
    {
        return Ok(());
    }
    if let Some(err) = audio_error {
        if !emit(Event::Notice(err)) {
            return Ok(());
        }
    } else if !video_only
        && selected_audio.is_none()
        && tracks
            .iter()
            .any(|s| s.params.media_type == MediaType::Audio)
        && !emit(Event::Notice("This audio codec is unsupported".to_owned()))
    {
        return Ok(());
    }
    if load_sidecar && let Some(path) = source.sidecar() {
        let event = match captions::load_sidecar(&path) {
            Ok(sidecar) => Event::Sidecar(sidecar),
            Err(err) => Event::Notice(format!("Subtitle file: {err:#}")),
        };
        if !emit(event) {
            return Ok(());
        }
    }
    let video_step = video
        .and_then(|s| s.params.frame_rate)
        .filter(|r| r.num > 0 && r.den > 0)
        .map_or(1.0 / 30.0, |r| r.den as f64 / r.num as f64);
    let mut last_video = None;
    let mut audio_end = None;
    let mut prior_frame: Option<TimedPicture> = None;
    let mut has_video = false;
    let mut has_audio = false;
    let mut incomplete_audio_reported = false;
    let mut primary_packet = None;
    loop {
        if cancel.load(Ordering::Relaxed)
            || preview_deadline.is_some_and(|end| Instant::now() >= end)
        {
            return Ok(());
        }
        if primary_packet.is_none() {
            primary_packet = match initial_packets
                .pop_front()
                .map_or_else(|| demux.next_packet(), Ok)
            {
                Ok(packet) => Some(packet),
                Err(Error::Eof) => None,
                Err(err) => return Err(err).context("Reading media packets"),
            };
        }
        let mut external_packet = None;
        if let (Some(ext), Some(primary)) = (&mut external, &primary_packet)
            && audio_decoder.is_some()
        {
            match ext.before(
                decode_time(primary).map_or(target, |time| time - start) + 0.12,
                cancel,
            ) {
                Ok(packet) => external_packet = packet,
                Err(err) => {
                    external = None;
                    if !emit(Event::Notice(format!("External audio: {err:#}"))) {
                        return Ok(());
                    }
                }
            }
        }
        let imported = external_packet.is_some();
        let packet = external_packet.or_else(|| primary_packet.take());
        if !imported
            && let Some(s) = video
            && packet.as_ref().is_none_or(|p| {
                p.stream_index == s.index && !(p.flags.header && !s.params.extradata.is_empty())
            })
            && let Some(decoder) = video_decoder.as_mut()
        {
            let pictures = decoder.decode(packet.as_ref()).map_err(|err| {
                if backend == Some("VA-API") {
                    HardwareError(err).into()
                } else {
                    err.context("Decoding video")
                }
            })?;
            for picture in pictures {
                let time = picture.timestamp.map_or_else(
                    || last_video.map_or(0.0, |last| last + video_step),
                    |pts| seconds(pts, s.time_base) - start,
                );
                last_video = Some(time);
                let timed = TimedPicture { time, picture };
                if time + 0.001 < target {
                    prior_frame = Some(timed);
                    continue;
                }
                if let Some(mut prior) = prior_frame.take()
                    && time > target + 0.001
                {
                    prior.time = target;
                    if !emit(Event::Video(prior)) {
                        return Ok(());
                    }
                }
                has_video = true;
                if !emit(Event::Video(timed)) {
                    return Ok(());
                }
            }
        }
        let Some(packet) = packet else {
            break;
        };
        if packet.flags.header || packet.data.is_empty() {
            continue;
        }
        if let Some(s) = if imported {
            external_track.as_ref()
        } else {
            audio
        } && s.index == packet.stream_index
            && let Some(decoder) = audio_decoder.as_mut()
        {
            // The MPEG demuxer flushes its partial access unit at EOF. Keep
            // playing the complete packets if the file ends inside a frame.
            let incomplete = match s.params.codec_id.as_str() {
                "mp2" => oxideav_mpegts::parse_mpeg_audio_header(&packet.data)
                    .map(|header| ("MP2", header.frame_len)),
                "ac3" | "eac3" => oxideav_mpegts::parse_ac3_frame(&packet.data)
                    .map(|header| ("Dolby", header.frame_len)),
                _ => None,
            };
            if let Some((codec, length)) = incomplete
                && length > packet.data.len()
            {
                if !incomplete_audio_reported
                    && !emit(Event::Notice(format!("Incomplete {codec} frame skipped")))
                {
                    return Ok(());
                }
                incomplete_audio_reported = true;
                continue;
            }
            let origin = if imported {
                external.as_ref().map_or(start, |track| track.origin)
            } else {
                start
            };
            let mut time =
                packet_time(&packet).map_or_else(|| audio_end.unwrap_or(0.0), |v| v - origin);
            // Millisecond container timestamps round audio packet boundaries.
            if let Some(end) = audio_end
                && (time - end).abs() <= 0.003
            {
                time = end;
            }
            let samples = match decoder.decode(&packet) {
                Ok(samples) => samples,
                Err(err)
                    if imported
                        || (video.is_some()
                            && matches!(s.params.codec_id.as_str(), "ac3" | "eac3" | "dts")) =>
                {
                    audio_decoder = None;
                    if !emit(Event::Notice(format!("Audio unavailable: {err:#}"))) {
                        return Ok(());
                    }
                    continue;
                }
                Err(err) => return Err(err).context("Decoding audio"),
            };
            if samples.data.is_empty() || samples.channels == 0 || samples.rate == 0 {
                continue;
            }
            let mut chunk = AudioChunk { time, samples };
            audio_end = Some(chunk.end());
            if !packet.flags.discard && chunk.end() > target {
                let rate = f64::from(chunk.samples.rate);
                let skip = ((target - chunk.time) * rate).ceil().max(0.0) as usize;
                let discard = (skip * chunk.samples.channels).min(chunk.samples.data.len());
                chunk.samples.data.drain(..discard);
                chunk.time += skip as f64 / rate;
                if chunk.samples.data.is_empty() {
                    continue;
                }
                has_audio = true;
                if !emit(Event::Audio(chunk)) {
                    return Ok(());
                }
            }
        } else if !imported
            && !video_only
            && let Some(s) = tracks
                .iter()
                .filter(is_text)
                .find(|s| s.index == packet.stream_index)
            && let Some(pts) = packet_time(&packet)
        {
            let position = pts - start;
            let length = packet
                .duration
                .map_or(3.0, |d| seconds(d, packet.time_base));
            if let Some(cue) = captions::embedded(
                s.params.codec_id.as_str(),
                &packet.data,
                position,
                position + length,
            ) && !emit(Event::Caption(s.index, cue))
            {
                return Ok(());
            }
        }
    }
    if let Some(mut frame) = prior_frame {
        frame.time = target;
        has_video = true;
        if !emit(Event::Video(frame)) {
            return Ok(());
        }
    }
    if !has_audio && !has_video && target == 0.0 {
        if backend == Some("VA-API") {
            return Err(HardwareError(anyhow::anyhow!("No decoded frames")).into());
        }
        bail!("The file contains no decodable frames");
    }
    let end = audio_end
        .filter(|_| external_track.is_none())
        .unwrap_or_default()
        .max(last_video.map_or(0.0, |v| v + video_step));
    emit(Event::End(end));
    Ok(())
}

struct ExternalAudio {
    demux: Box<dyn Demuxer>,
    track: StreamInfo,
    origin: f64,
    target: f64,
    next: Option<Packet>,
    ended: bool,
}

impl ExternalAudio {
    fn open(
        source: &dyn MediaSource,
        ctx: &oxideav_core::RuntimeContext,
        target: f64,
        cancel: &AtomicBool,
    ) -> Result<(Self, AudioDecoder)> {
        let (mut demux, container) = source.open_described(ctx)?;
        let track = demux
            .streams()
            .iter()
            .find(|s| {
                let codec = s.params.codec_id.as_str();
                s.params.media_type == MediaType::Audio
                    && AudioDecoder::supported(codec)
                    && (codec != "dts" || container == "matroska")
            })
            .cloned()
            .context("No supported audio track in the selected file")?;
        let decoder = AudioDecoder::open(&track.params)?;
        demux.set_active_streams(&[track.index]);
        let mut next = Self::read_audio(demux.as_mut(), track.index, cancel)?;
        let first = next.as_ref().context("The audio track is empty")?;
        let origin = track
            .start_time
            .map(|t| seconds(t, track.time_base))
            .or_else(|| packet_time(first))
            .unwrap_or_default();
        if target > 0.0 && !cancel.load(Ordering::Relaxed) {
            let base = track.time_base.as_rational();
            let preroll = (target - 0.25).max(0.0);
            let ticks = ((preroll + origin) * base.den as f64 / base.num as f64) as i64;
            if !matches!(demux.seek_to(track.index, ticks), Ok(actual) if actual <= ticks) {
                demux = source.open_demuxer(ctx)?;
                demux.set_active_streams(&[track.index]);
            }
            next = None;
        }
        Ok((
            Self {
                demux,
                track,
                origin,
                target,
                next,
                ended: false,
            },
            decoder,
        ))
    }

    fn read_audio(
        demux: &mut dyn Demuxer,
        index: u32,
        cancel: &AtomicBool,
    ) -> Result<Option<Packet>> {
        while !cancel.load(Ordering::Relaxed) {
            match demux.next_packet() {
                Ok(packet)
                    if packet.stream_index == index
                        && !packet.flags.header
                        && !packet.data.is_empty() =>
                {
                    return Ok(Some(packet));
                }
                Ok(_) => {}
                Err(Error::Eof) => break,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(None)
    }

    // Keep the independent audio reader close to the primary decoder timeline.
    fn before(&mut self, time: f64, cancel: &AtomicBool) -> Result<Option<Packet>> {
        while !self.ended {
            if self.next.is_none() {
                self.next = Self::read_audio(self.demux.as_mut(), self.track.index, cancel)?;
            }
            let Some(packet) = &self.next else {
                self.ended = true;
                break;
            };
            if let Some(pts) = packet_time(packet).map(|pts| pts - self.origin) {
                if pts > time {
                    break;
                }
                let packet_end = packet
                    .duration
                    .filter(|&duration| duration > 0)
                    .map(|duration| pts + seconds(duration, packet.time_base));
                if packet_end.is_some_and(|end| end + 0.25 < self.target) {
                    self.next = None;
                    continue;
                }
            }
            return Ok(self.next.take());
        }
        Ok(None)
    }
}

fn decode_time(packet: &Packet) -> Option<f64> {
    packet
        .dts
        .or(packet.pts)
        .map(|t| seconds(t, packet.time_base))
}

/// For unindexed files, scan compressed packets and hand the decoder only the
/// last usable GOP. MPEG-PS needs a sequence header to start a fresh decoder.
fn scan_to_keyframe(
    demux: &mut dyn Demuxer,
    video: u32,
    target: f64,
    needs_sequence: bool,
    active: &[u32],
    cancelled: impl Fn() -> bool,
) -> Result<Option<VecDeque<Packet>>> {
    let mut pending = VecDeque::new();
    let mut anchor = None;
    let mut bytes = 0;
    while !cancelled() {
        let packet = match demux.next_packet() {
            Ok(packet) => packet,
            Err(Error::Eof) => break,
            Err(err) => return Err(err.into()),
        };
        let time = (packet.stream_index == video)
            .then(|| packet_time(&packet))
            .flatten();
        if let Some(time) = time
            && time <= target
            && (anchor.is_none() || time <= target - 0.10)
            && packet.flags.keyframe
            && !packet.flags.header
            && (!needs_sequence || packet.data.windows(4).any(|code| code == [0, 0, 1, 0xb3]))
        {
            pending.clear();
            bytes = 0;
            anchor = Some(time);
        }
        if anchor.is_some() && active.contains(&packet.stream_index) {
            bytes += packet.data.len();
            if bytes > 16 * 1024 * 1024 || pending.len() >= 8192 {
                return Ok(None);
            }
            pending.push_back(packet);
        }
        if time.is_some_and(|t| t > target + 0.25) {
            break;
        }
    }
    Ok((!pending.is_empty() && !cancelled()).then_some(pending))
}

fn packet_time(packet: &Packet) -> Option<f64> {
    packet
        .pts
        .or(packet.dts)
        .map(|pts| seconds(pts, packet.time_base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::LocalFile;
    use std::path::PathBuf;

    #[test]
    #[ignore = "run scripts/check-containers.sh to fetch the public video fixture"]
    fn external_audio_trims_decoded_flac_block_at_seek() -> Result<()> {
        let fixtures = PathBuf::from(std::env::var_os("RIVPLAYER_FIXTURES").unwrap());
        let video = LocalFile(fixtures.join("bbb-320x240-2video-2audio.mp4"));
        let audio = LocalFile(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/external-long-block.flac"),
        );
        // At 8 kHz, this fixture's 4096-sample blocks last 512 ms.
        let position = 1.375;
        let mut first_audio = None;
        run_session(
            &video,
            Some(&audio),
            Options {
                position,
                mode: DecodeMode::Software,
                ..Options::default()
            },
            &AtomicBool::new(false),
            false,
            |event| {
                if let Event::Audio(chunk) = event {
                    first_audio = Some(chunk);
                    return false;
                }
                true
            },
        )?;
        let chunk =
            first_audio.context("Expected audio from the block containing the seek target")?;
        assert!(
            (position..=position + 1.0 / 8000.0).contains(&chunk.time),
            "{}",
            chunk.time
        );
        assert!(chunk.samples.data.iter().any(|sample| sample.abs() > 0.01));
        Ok(())
    }

    #[test]
    #[ignore = "run scripts/check-containers.sh to fetch public media fixtures"]
    fn external_audio_seeks_with_video_and_keeps_the_video_end() -> Result<()> {
        let fixtures = PathBuf::from(std::env::var_os("RIVPLAYER_FIXTURES").unwrap());
        let video = LocalFile(fixtures.join("bear-1280x720-avt_subt_frag.mp4"));
        let audio = LocalFile(fixtures.join("bbb-320x240-2video-2audio.mp4"));
        for position in [0.0, 1.25] {
            let mut info = None;
            let mut first_audio = None;
            let mut first_video = None;
            let mut end = None;
            run_session(
                &video,
                Some(&audio),
                Options {
                    position,
                    mode: DecodeMode::Software,
                    ..Options::default()
                },
                &AtomicBool::new(false),
                false,
                |event| {
                    match event {
                        Event::Info(value) => info = Some(value),
                        Event::Audio(chunk) => {
                            first_audio.get_or_insert(chunk.time);
                        }
                        Event::Video(frame) => {
                            first_video.get_or_insert(frame.time);
                        }
                        Event::End(time) => end = Some(time),
                        _ => {}
                    }
                    true
                },
            )?;
            let info = info.unwrap();
            assert!(info.current_audio.is_none() && !info.audio.is_empty());
            assert_eq!(
                info.external_audio
                    .as_ref()
                    .map(|track| track.codec.as_str()),
                Some("aac")
            );
            for first in [first_audio.unwrap(), first_video.unwrap()] {
                assert!(
                    (position - 0.002..position + 0.12).contains(&first),
                    "{position} -> {first}"
                );
            }
            assert!((2.55..3.0).contains(&end.unwrap()), "{end:?}");
        }
        let missing = LocalFile(fixtures.join("missing-external.flac"));
        let mut restored = false;
        let mut warned = false;
        run_session(
            &video,
            Some(&missing),
            Options {
                mode: DecodeMode::Software,
                ..Options::default()
            },
            &AtomicBool::new(false),
            false,
            |event| {
                match event {
                    Event::Info(info) => {
                        restored = info.current_audio.is_some() && info.external_audio.is_none()
                    }
                    Event::Notice(message) => warned = message.starts_with("External audio:"),
                    _ => {}
                }
                !(restored && warned)
            },
        )?;
        assert!(restored && warned);
        Ok(())
    }
}
