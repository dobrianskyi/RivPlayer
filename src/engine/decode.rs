mod dts;

use anyhow::{Context, Result, bail};
use oxideav_core::{
    CodecParameters, ExecutionContext, Frame, Packet, PixelFormat, RuntimeContext, SampleFormat,
    VideoFrame,
};
use oxideav_mpeg12video::{decoder::frame_buffer_to_video_frame, video_sequence::SequenceDecoder};
use symphonia::core::{
    audio::{Channels, Position},
    codecs::audio::{
        AudioCodecId, AudioCodecParameters, AudioDecoder as SymDecoder, AudioDecoderOptions,
        well_known::*,
    },
    packet::PacketRef,
    units::{Duration, Timestamp},
};

use super::{av1::Av1, pipeline::DecodeMode};

pub fn seconds(pts: i64, base: oxideav_core::TimeBase) -> f64 {
    let base = base.as_rational();
    pts as f64 * base.num as f64 / base.den as f64
}

pub struct Picture {
    pub timestamp: Option<i64>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub color: oxideav_core::ColorSignal,
    pub frame: VideoFrame,
}

pub enum VideoDecoder {
    Oxide(Box<dyn oxideav_core::Decoder>, Box<CodecParameters>),
    Mpeg(Box<MpegVideo>),
    Av1(Av1),
    #[cfg(feature = "vaapi")]
    Vaapi(Box<super::vaapi::VaDecoder>),
}

impl VideoDecoder {
    pub fn supported(id: &str) -> bool {
        matches!(
            id,
            "h264"
                | "h265"
                | "hevc"
                | "vp8"
                | "vp9"
                | "av1"
                | "mpeg1video"
                | "mpeg2video"
                | "theora"
                | "mpeg4video"
                | "avi:FMP4"
        )
    }

    pub fn open(ctx: &RuntimeContext, params: &CodecParameters, mode: DecodeMode) -> Result<Self> {
        #[cfg(feature = "vaapi")]
        if mode != DecodeMode::Software {
            match super::vaapi::VaDecoder::open(params) {
                Ok(decoder) => return Ok(Self::Vaapi(Box::new(decoder))),
                Err(err) if mode == DecodeMode::Vaapi => {
                    return Err(err).context("Initializing VA-API");
                }
                Err(_) => {}
            }
        }
        #[cfg(not(feature = "vaapi"))]
        if mode == DecodeMode::Vaapi {
            bail!("This build has no VA-API support");
        }
        if params.codec_id.as_str() == "av1" {
            return Ok(Self::Av1(Av1::open(&params.extradata)?));
        }

        let mut params = params.clone();
        if params.codec_id.as_str() == "avi:FMP4" {
            params.codec_id = "mpeg4video".into();
        }
        if matches!(params.codec_id.as_str(), "mpeg1video" | "mpeg2video") {
            return Ok(Self::Mpeg(Box::new(MpegVideo {
                decoder: SequenceDecoder::with_concealment(),
                clock: MpegClock::default(),
                color: params.color_signal,
                frame_index: 0,
            })));
        }
        if params.codec_id.as_str() == "theora" && params.extradata.starts_with(&[2]) {
            params.extradata =
                theora_headers(&params.extradata).context("Invalid Theora headers")?;
        }
        if params.codec_id.as_str() == "mpeg4video" {
            params.options.insert("ecosystem-compat", "true");
        }
        params.limits.max_pixels_per_frame = 8192 * 4320;
        let mut decoder = ctx.codecs.first_decoder(&params)?;
        decoder.set_execution_context(&ExecutionContext::with_threads(decode_threads()));
        Ok(Self::Oxide(decoder, Box::new(params)))
    }

    pub fn backend(&self) -> &'static str {
        match self {
            Self::Oxide(..) | Self::Mpeg(_) => "OxideAV",
            Self::Av1(_) => "rav1d",
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => "VA-API",
        }
    }

    pub fn decode(&mut self, packet: Option<&Packet>) -> Result<Vec<Picture>> {
        match self {
            Self::Av1(decoder) => decoder.decode(packet),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(decoder) => decoder.decode(packet),
            Self::Mpeg(decoder) => decoder.decode(packet),
            Self::Oxide(decoder, input) => {
                if let Some(packet) = packet {
                    decoder.send_packet(packet)?;
                } else {
                    decoder.flush()?;
                }
                let mut output = Vec::new();
                loop {
                    match decoder.receive_frame() {
                        Ok(Frame::Video(frame)) => {
                            let reported = decoder.output_params();
                            let params = reported.as_ref().unwrap_or(input);
                            let inferred =
                                if matches!(input.codec_id.as_str(), "mpeg4video" | "theora") {
                                    planar_format(&frame)
                                } else {
                                    None
                                };
                            let (width, height, format) = if let Some(layout) = inferred {
                                layout
                            } else {
                                (
                                    params.width.context("Missing decoded frame width")?,
                                    params.height.context("Missing decoded frame height")?,
                                    params
                                        .pixel_format
                                        .or_else(|| decoder.output_pixel_format())
                                        .context("Missing decoded pixel format")?,
                                )
                            };
                            let color = frame
                                .color_signal()
                                .unwrap_or(params.color_signal)
                                .or(input.color_signal);
                            output.push(Picture {
                                timestamp: frame.pts,
                                width,
                                height,
                                format,
                                color,
                                frame,
                            });
                        }
                        Ok(_) => {}
                        Err(oxideav_core::Error::Eof | oxideav_core::Error::NeedMore) => break,
                        Err(err) => return Err(err.into()),
                    }
                }
                Ok(output)
            }
        }
    }
}

pub struct MpegVideo {
    decoder: SequenceDecoder,
    clock: MpegClock,
    color: oxideav_core::ColorSignal,
    frame_index: i64,
}

impl MpegVideo {
    fn decode(&mut self, packet: Option<&Packet>) -> Result<Vec<Picture>> {
        if let Some(packet) = packet {
            self.clock.read(packet);
            self.decoder.push(&packet.data);
        } else {
            self.decoder.finish();
        }
        let mut frames = Vec::new();
        while let Some(decoded) = self
            .decoder
            .next_frame()
            .map_err(|err| anyhow::anyhow!("MPEG video: {err}"))?
        {
            let frame = frame_buffer_to_video_frame(&decoded.frame);
            let (width, height, format) =
                planar_format(&frame).context("Missing MPEG video planes")?;
            frames.push(Picture {
                timestamp: self.clock.timestamp(Some(self.frame_index)),
                width,
                height,
                format,
                color: self.color,
                frame,
            });
            self.frame_index += 1;
        }
        Ok(frames)
    }
}

/// The older codec adapters hand back tightly packed planes without a
/// stream-format event. Their luma/chroma strides are the visible widths.
fn planar_format(frame: &VideoFrame) -> Option<(u32, u32, PixelFormat)> {
    let [y, u, ..] = frame.image_planes() else {
        return None;
    };
    let width = u32::try_from(y.stride).ok()?;
    let height = u32::try_from(y.data.len().checked_div(y.stride)?).ok()?;
    let chroma_width = u32::try_from(u.stride).ok()?;
    let chroma_height = u32::try_from(u.data.len().checked_div(u.stride)?).ok()?;
    let format = match (chroma_width == width, chroma_height == height) {
        (true, true) => PixelFormat::Yuv444P,
        (false, true) => PixelFormat::Yuv422P,
        _ => PixelFormat::Yuv420P,
    };
    Some((width, height, format))
}

fn theora_headers(data: &[u8]) -> Option<Vec<u8>> {
    let mut body = data.get(1..)?;
    let mut lens = [0_usize; 2];
    for len in &mut lens {
        loop {
            let (&part, rest) = body.split_first()?;
            body = rest;
            *len = len.checked_add(usize::from(part))?;
            if part < 255 {
                break;
            }
        }
    }
    let (ident, rest) = body.split_at_checked(lens[0])?;
    let (comment, setup) = rest.split_at_checked(lens[1])?;
    let mut encoded = Vec::with_capacity(body.len() + 6);
    for header in [ident, comment, setup] {
        encoded.extend(u16::try_from(header.len()).ok()?.to_be_bytes());
        encoded.extend(header);
    }
    Some(encoded)
}

#[derive(Default)]
struct MpegClock {
    anchor: Option<i64>,
    ticks_per_second: f64,
    fps: f64,
}

impl MpegClock {
    fn read(&mut self, packet: &Packet) {
        self.anchor = self.anchor.or(packet.pts).or(packet.dts);
        let base = packet.time_base.as_rational();
        self.ticks_per_second = base.den as f64 / base.num as f64;
        if let Some(at) = packet
            .data
            .windows(4)
            .position(|window| window == [0, 0, 1, 0xb3])
            && let Some(code) = packet.data.get(at + 7)
        {
            let rates = [
                0.0,
                24000.0 / 1001.0,
                24.0,
                25.0,
                30000.0 / 1001.0,
                30.0,
                50.0,
                60000.0 / 1001.0,
                60.0,
            ];
            if let Some(&fps) = rates.get(usize::from(code & 0xf))
                && fps > 0.0
            {
                self.fps = fps;
            }
        }
    }

    fn timestamp(&self, index: Option<i64>) -> Option<i64> {
        let index = index?;
        let fps = if self.fps > 0.0 { self.fps } else { 30.0 };
        Some(
            self.anchor
                .unwrap_or_default()
                .saturating_add((index as f64 * self.ticks_per_second / fps).round() as i64),
        )
    }
}

pub fn decode_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(6)
}

pub struct Samples {
    pub rate: u32,
    pub channels: usize,
    pub layout: DialogueLayout,
    pub data: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DialogueLayout {
    pub front: Option<(usize, usize)>,
    pub center: Option<usize>,
}

pub enum Opus {
    Standard(Box<rusty_opus::OpusDecoder>),
    Multistream(Box<opus_decoder::OpusMultistreamDecoder>),
}

pub enum AudioDecoder {
    Symphonia(Box<dyn SymDecoder>, AudioCodecId),
    Dolby(Box<dyn oxideav_core::Decoder>),
    Dts(Box<dyn oxideav_core::Decoder>),
    Opus {
        decoder: Opus,
        channels: usize,
        layout: DialogueLayout,
        gain: f32,
    },
}

fn audio_codec(id: &str) -> Option<AudioCodecId> {
    Some(match id {
        "aac" => CODEC_ID_AAC,
        "alac" => CODEC_ID_ALAC,
        "flac" => CODEC_ID_FLAC,
        "mp3" => CODEC_ID_MP3,
        "mp2" => CODEC_ID_MP2,
        "vorbis" => CODEC_ID_VORBIS,
        "pcm_s16le" => CODEC_ID_PCM_S16LE,
        "pcm_s16be" => CODEC_ID_PCM_S16BE,
        "pcm_s24le" => CODEC_ID_PCM_S24LE,
        "pcm_s24be" => CODEC_ID_PCM_S24BE,
        "pcm_s32le" => CODEC_ID_PCM_S32LE,
        "pcm_s32be" => CODEC_ID_PCM_S32BE,
        "pcm_f32le" => CODEC_ID_PCM_F32LE,
        "pcm_f32be" => CODEC_ID_PCM_F32BE,
        _ => return None,
    })
}

impl AudioDecoder {
    pub fn supported(id: &str) -> bool {
        matches!(id, "opus" | "ac3" | "eac3" | "dts") || audio_codec(id).is_some()
    }

    pub fn open(params: &CodecParameters) -> Result<Self> {
        let id = params.codec_id.as_str();
        if id == "dts" {
            return Ok(Self::Dts(oxideav_dts::make_decoder(params)?));
        }
        if matches!(id, "ac3" | "eac3") {
            let mut params = params.clone();
            // Preserve the dialogue channel until the output stage.
            params.channels = None;
            params.sample_format = Some(SampleFormat::F32);
            return Ok(Self::Dolby(oxideav_ac3::decoder::make_decoder(&params)?));
        }
        if id == "opus" {
            let head = &params.extradata;
            let num = usize::from(params.channels.unwrap_or(2));
            let channels = if head.starts_with(b"OpusHead") {
                usize::from(*head.get(9).context("Missing Opus channel count")?)
            } else {
                num
            };
            let (streams, coupled, map): (usize, usize, Vec<u8>) = match head.get(18) {
                Some(0) | None if (1..=2).contains(&channels) => {
                    (1, channels - 1, (0..channels as u8).collect())
                }
                Some(1 | 255) => (
                    usize::from(*head.get(19).context("Missing Opus stream count")?),
                    usize::from(*head.get(20).context("Missing Opus coupled stream count")?),
                    head.get(21..21 + channels)
                        .context("Missing Opus channel map")?
                        .to_vec(),
                ),
                _ => bail!("Unsupported Opus channel mapping"),
            };
            let gain = head.get(16..18).map_or(1.0, |g| {
                10f32.powf(f32::from(i16::from_le_bytes([g[0], g[1]])) / 5120.0)
            });
            let decoder = if matches!(head.get(18), Some(0) | None) {
                Opus::Standard(Box::new(rusty_opus::OpusDecoder::new(48000, channels)?))
            } else {
                Opus::Multistream(Box::new(opus_decoder::OpusMultistreamDecoder::new(
                    48000, channels, streams, coupled, &map,
                )?))
            };
            return Ok(Self::Opus {
                decoder,
                channels,
                layout: if head.get(18) == Some(&1) && matches!(channels, 3 | 5..=8) {
                    DialogueLayout {
                        front: Some((0, 2)),
                        center: Some(1),
                    }
                } else {
                    DialogueLayout::default()
                },
                gain,
            });
        }
        let codec = audio_codec(id).with_context(|| format!("Unsupported audio codec {id}"))?;
        let mut sym = AudioCodecParameters::new();
        sym.for_codec(codec);
        sym.sample_rate = params.sample_rate;
        sym.channels = params.channels.map(Channels::Discrete);
        if !params.extradata.is_empty() {
            let extra = if id == "flac" && params.extradata.len() >= 38 {
                let raw = params
                    .extradata
                    .strip_prefix(b"fLaC")
                    .unwrap_or(&params.extradata);
                raw.get(4..38).context("Missing FLAC STREAMINFO")?
            } else {
                &params.extradata
            };
            sym.with_extra_data(extra.into());
        }
        Ok(Self::Symphonia(
            symphonia::default::get_codecs()
                .make_audio_decoder(&sym, &AudioDecoderOptions::default())?,
            codec,
        ))
    }

    pub fn decode(&mut self, packet: &Packet) -> Result<Samples> {
        match self {
            Self::Dts(decoder) => dts::decode(decoder.as_mut(), packet),
            Self::Dolby(decoder) => {
                let rate = oxideav_mpegts::parse_ac3_frame(&packet.data)
                    .context("Invalid Dolby syncframe header")?
                    .sample_rate;
                let acmod = if packet.data[5] >> 3 <= 10 {
                    packet.data.get(6).map_or(2, |mode| mode >> 5)
                } else {
                    (packet.data[4] >> 1) & 7
                };
                decoder.send_packet(packet)?;
                let mut output = None;
                loop {
                    match decoder.receive_frame() {
                        Ok(Frame::Audio(frame)) => {
                            let bytes = frame.data.first().context("Missing Dolby PCM data")?;
                            let channels = bytes.len() / (frame.samples as usize * 4).max(1);
                            anyhow::ensure!(
                                (1..=16).contains(&channels)
                                    && bytes.len() == frame.samples as usize * channels * 4
                                    && rate > 0,
                                "Invalid Dolby PCM layout"
                            );
                            let result = output.get_or_insert_with(|| Samples {
                                rate,
                                channels,
                                layout: DialogueLayout {
                                    front: (acmod == 1).then_some((0, 0)),
                                    center: (channels >= 3 && matches!(acmod, 3 | 5 | 7))
                                        .then_some(2),
                                },
                                data: Vec::new(),
                            });
                            anyhow::ensure!(
                                result.rate == rate && result.channels == channels,
                                "Dolby PCM layout changed within a packet"
                            );
                            result.data.extend(
                                bytes
                                    .as_chunks::<4>()
                                    .0
                                    .iter()
                                    .map(|&sample| f32::from_le_bytes(sample)),
                            );
                        }
                        Ok(_) => {}
                        Err(oxideav_core::Error::NeedMore | oxideav_core::Error::Eof) => break,
                        Err(err) => return Err(err.into()),
                    }
                }
                Ok(output.unwrap_or(Samples {
                    rate: 0,
                    channels: 0,
                    layout: DialogueLayout::default(),
                    data: Vec::new(),
                }))
            }
            Self::Symphonia(decoder, codec) => {
                let data = if *codec == CODEC_ID_AAC {
                    adts_payload(&packet.data).unwrap_or(&packet.data)
                } else {
                    &packet.data
                };
                let info = PacketRef::new(
                    packet.stream_index,
                    Timestamp::new(packet.pts.unwrap_or_default()),
                    Duration::new(packet.duration.unwrap_or_default().max(0) as u64),
                    data,
                );
                let audio = decoder.decode_ref(&info)?;
                let rate = audio.spec().rate();
                let positions = audio.spec().channels();
                let channels = positions.count();
                let index = |position| match positions {
                    Channels::Positioned(mask) if mask.contains(position) => {
                        positions.get_canonical_index_for_positioned_channel(position)
                    }
                    _ => None,
                };
                let mut data = Vec::new();
                audio.copy_to_vec_interleaved(&mut data);
                Ok(Samples {
                    rate,
                    channels,
                    layout: DialogueLayout {
                        front: index(Position::FRONT_LEFT).zip(index(Position::FRONT_RIGHT)),
                        center: index(Position::FRONT_CENTER),
                    },
                    data,
                })
            }
            Self::Opus {
                decoder,
                channels,
                layout,
                gain,
            } => {
                let mut data = vec![0.0; 5760 * *channels];
                let count = match decoder {
                    Opus::Standard(decoder) => decoder.decode(&packet.data, 5760, &mut data)?,
                    Opus::Multistream(decoder) => {
                        decoder.decode_float(&packet.data, &mut data, false)?
                    }
                };
                data.truncate(count * *channels);
                data.iter_mut().for_each(|sample| *sample *= *gain);
                Ok(Samples {
                    rate: 48000,
                    channels: *channels,
                    layout: *layout,
                    data,
                })
            }
        }
    }
}

fn adts_payload(bytes: &[u8]) -> Option<&[u8]> {
    let header = bytes.get(..7)?;
    if header[0] != 0xff || header[1] & 0xf6 != 0xf0 || header[6] & 3 != 0 {
        return None;
    }
    let size = (usize::from(header[3] & 3) << 11)
        | (usize::from(header[4]) << 3)
        | (usize::from(header[5]) >> 5);
    if size != bytes.len() {
        return None;
    }
    let head_len = if header[1] & 1 == 1 { 7 } else { 9 };
    bytes.get(head_len..)
}
