//! ISO/IEC 11172-1 program streams use different pack and PES headers
//! from MPEG-2 PS. Veloce indexes the access units and reads them on demand.

use std::{
    cell::RefCell,
    io::{self, SeekFrom},
};

use anyhow::{Context, Result, ensure};
use oxideav_core::{
    CodecParameters, Demuxer, Error, MediaType, Packet, ReadSeek, StreamInfo, TimeBase,
};
use oxideav_mpeg12video::sequence_header::Mpeg2SequenceHeader;
use oxideav_mpegts::{AudioFrameKind, parse_mpeg_audio_header};
use veloce_mpegts::{ByteSource, Codec, Unit};

const TIME_BASE: TimeBase = TimeBase::new(1, 90_000);

struct Reader {
    input: RefCell<Box<dyn ReadSeek>>,
    len: u64,
}

impl ByteSource for Reader {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut input = self.input.borrow_mut();
        input.seek(SeekFrom::Start(offset))?;
        input.read_exact(buf)
    }
}

pub struct Mpeg1Ps {
    file: veloce_mpegts::File,
    reader: Reader,
    streams: Vec<StreamInfo>,
    positions: Vec<usize>,
    active: Vec<u32>,
}

pub fn open(mut input: Box<dyn ReadSeek>) -> Result<Mpeg1Ps> {
    let reader = Reader {
        len: input.seek(SeekFrom::End(0))?,
        input: RefCell::new(input),
    };
    let file = veloce_mpegts::open(&reader).context("Indexing MPEG-1 program stream")?;
    let mut streams = Vec::new();
    for (index, stream) in file.streams.iter().enumerate() {
        if stream.units.is_empty() {
            continue;
        }
        let params = match stream.codec {
            Codec::Mpeg1Video => {
                let mut params = CodecParameters::video("mpeg1video".into());
                let data = file.read_unit(&reader, index, 0)?;
                if let Some(header) = data
                    .windows(4)
                    .position(|p| p == [0, 0, 1, 0xb3])
                    .and_then(|at| Mpeg2SequenceHeader::parse(&data[at..]).ok())
                {
                    params.width = Some(u32::from(header.width));
                    params.height = Some(u32::from(header.height));
                }
                params
            }
            Codec::MpegAudio => {
                let data = file.read_unit(&reader, index, 0)?;
                let Some(header) = parse_mpeg_audio_header(&data) else {
                    continue;
                };
                let codec = match header.kind {
                    AudioFrameKind::MpegLayer2 => "mp2",
                    AudioFrameKind::MpegLayer3 => "mp3",
                    _ => continue,
                };
                let mut params = CodecParameters::audio(codec.into());
                params.sample_rate = Some(header.sample_rate);
                params.channels = Some(header.channels);
                params
            }
            _ => continue,
        };
        let range = stream.pts_range();
        streams.push(StreamInfo {
            index: index as u32,
            time_base: TIME_BASE,
            duration: range.map(|(first, last)| last - first),
            start_time: range.map(|(first, _)| first),
            params,
        });
    }
    ensure!(
        !streams.is_empty(),
        "No supported streams in MPEG-1 program stream"
    );
    Ok(Mpeg1Ps {
        positions: vec![0; file.streams.len()],
        active: streams.iter().map(|s| s.index).collect(),
        file,
        reader,
        streams,
    })
}

fn is_access_point(unit: &Unit) -> bool {
    unit.key && unit.picture.is_some_and(|p| p.sequence_header)
}

impl Demuxer for Mpeg1Ps {
    fn format_name(&self) -> &str {
        "mpeg1ps"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn set_active_streams(&mut self, streams: &[u32]) {
        self.active = streams.to_vec();
    }

    fn next_packet(&mut self) -> oxideav_core::Result<Packet> {
        let (index, unit) = self
            .active
            .iter()
            .filter_map(|&index| {
                let index = index as usize;
                let position = *self.positions.get(index)?;
                self.file
                    .streams
                    .get(index)?
                    .units
                    .get(position)
                    .map(|unit| (index, unit))
            })
            .min_by_key(|(_, unit)| unit.dts.or(unit.pts).unwrap_or(i64::MIN))
            .ok_or(Error::Eof)?;
        let mut data = self
            .file
            .read_unit(&self.reader, index, self.positions[index])
            .map_err(|err| Error::invalid(format!("MPEG-1 PS: {err}")))?;
        self.positions[index] += 1;
        let mut duration = None;
        if self.file.streams[index].codec == Codec::MpegAudio
            && let Some(header) = parse_mpeg_audio_header(&data)
        {
            // The index can attach the remaining PES padding to the last unit.
            // The Layer II/III decoder expects precisely one complete frame.
            data.truncate(header.frame_len);
            duration = Some(header.duration_90k() as i64);
        }
        let mut packet = Packet::new(index as u32, TIME_BASE, data).with_keyframe(unit.key);
        packet.pts = unit.pts;
        packet.dts = unit.dts;
        packet.duration = duration;
        Ok(packet)
    }

    fn seek_to(&mut self, stream: u32, pts: i64) -> oxideav_core::Result<i64> {
        let main = self
            .streams
            .iter()
            .find(|s| s.index == stream)
            .ok_or_else(|| Error::invalid("Unknown MPEG-1 PS stream"))?;
        let units = &self.file.streams[stream as usize].units;
        let video = main.params.media_type == MediaType::Video;
        let (position, landed) = units
            .iter()
            .enumerate()
            .filter_map(|(i, unit)| {
                let time = unit.pts?;
                (time <= pts && (!video || is_access_point(unit))).then_some((i, time))
            })
            .next_back()
            .ok_or_else(|| Error::unsupported("No preceding MPEG-1 PS access point"))?;
        for s in &self.streams {
            let index = s.index as usize;
            self.positions[index] = if s.index == stream {
                position
            } else {
                self.file.streams[index]
                    .units
                    .iter()
                    .rposition(|u| u.pts.is_some_and(|t| t <= landed - 9_000))
                    .unwrap_or(0)
            };
        }
        Ok(landed)
    }
}
