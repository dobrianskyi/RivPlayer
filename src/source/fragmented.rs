use std::{
    collections::{HashMap, HashSet},
    io::SeekFrom,
    ops::Range,
};

use oxideav_core::{Demuxer, Error, MediaType, Packet, ReadSeek, Result, StreamInfo, TimeBase};
use oxideav_mov::{
    atom::{self, AtomHeader},
    edit::{
        Edit, EditedTiming, edited_timing_for_sample, never_presented_timing_for_sample,
        resolve_edit_segments,
    },
    fragment::{self, TFHD_BASE_DATA_OFFSET_PRESENT, TFHD_DEFAULT_BASE_IS_MOOF},
};
use oxideav_mp4::demux::Mp4Demuxer;

const MAX_PACKET_SIZE: u32 = 64 * 1024 * 1024;
const MAX_SAMPLES: usize = 10_000_000;

#[derive(Clone, Copy)]
struct Sample {
    offset: u64,
    size: u32,
    pts: i64,
    dts: i64,
    duration: i64,
    key: bool,
    discard: bool,
}

struct Track {
    samples: Vec<Sample>,
    next: usize,
}

pub struct Fragmented {
    reader: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    tracks: Vec<Track>,
    active: HashSet<u32>,
    duration: Option<i64>,
}

impl Fragmented {
    pub fn open(mp4: &Mp4Demuxer, mut reader: Box<dyn ReadSeek>) -> Result<Option<Self>> {
        let len = reader.seek(SeekFrom::End(0))?;
        reader.rewind()?;
        let mut defaults = HashMap::new();
        let mut fragments = Vec::new();
        let mut payloads = Vec::new();
        atom::walk_children(reader.as_mut(), Some(len), |reader, header| {
            match header.fourcc {
                atom::MOOV => {
                    let end = end_offset(header, len);
                    atom::walk_children(reader, Some(end), |reader, child| {
                        if child.fourcc == atom::MVEX {
                            let (_, trex, _) = fragment::parse_mvex(reader, child)?;
                            defaults.extend(trex.into_iter().map(|t| (t.track_id, t)));
                        }
                        Ok(())
                    })?;
                }
                atom::MOOF => fragments.push(*header),
                atom::MDAT => payloads.push(header.payload_offset..end_offset(header, len)),
                _ => {}
            }
            Ok(())
        })?;
        let Some(first) = fragments.first().map(start_offset) else {
            return Ok(None);
        };
        let streams = mp4.streams().to_vec();
        let ids: HashMap<_, _> = streams
            .iter()
            .enumerate()
            .filter_map(|(i, stream)| {
                mp4.track_header(stream.index)
                    .map(|header| (header.track_id, i))
            })
            .collect();

        let mut tracks: Vec<_> = streams
            .iter()
            .map(|stream| {
                let samples = mp4
                    .samples(stream.index)
                    .into_iter()
                    .filter(|sample| sample.offset < first)
                    .map(|sample| Sample {
                        offset: sample.offset,
                        size: sample.size,
                        pts: sample.composition_time,
                        dts: sample.decode_time,
                        duration: sample.duration,
                        key: sample.sync,
                        discard: false,
                    })
                    .collect();
                Track { samples, next: 0 }
            })
            .collect();
        let mut clocks: Vec<_> = tracks
            .iter()
            .map(|track| {
                track
                    .samples
                    .last()
                    .map_or(0, |s| s.dts.saturating_add(s.duration).max(0) as u64)
            })
            .collect();
        let mut total = tracks.iter().map(|t| t.samples.len()).sum::<usize>();
        for (i, header) in fragments.iter().enumerate() {
            let start = start_offset(header);
            let next = fragments.get(i + 1).map_or(len, start_offset);
            let payload_start = payloads.partition_point(|p| p.end <= start);
            let payload_end = payloads.partition_point(|p| p.start < next);
            let local = &payloads[payload_start..payload_end];
            let (_, trafs) = fragment::parse_moof(reader.as_mut(), header)?;
            let mut previous_end = start;
            for traf in trafs {
                let track_id = traf.tfhd.track_id;
                let index = *ids
                    .get(&track_id)
                    .ok_or_else(|| Error::invalid("Fragment refers to an unknown track"))?;
                let track = &mut tracks[index];
                let default = defaults.get(&track_id);
                let count = track.samples.len() as u32;
                let time = traf.tfdt.unwrap_or(clocks[index]);
                let (mut entries, mut end, mut next_time) = fragment::resolve_traf_samples(
                    &traf,
                    default,
                    start,
                    previous_end,
                    time,
                    count,
                )?;
                // Some writers leave default-base-is-moof clear while encoding
                // explicit run offsets relative to the moof. Only accept that
                // interpretation when it places the samples in this fragment.
                if !local.is_empty()
                    && !entries.iter().all(|s| in_payload(local, s.offset, s.size))
                    && traf.tfhd.tf_flags
                        & (TFHD_BASE_DATA_OFFSET_PRESENT | TFHD_DEFAULT_BASE_IS_MOOF)
                        == 0
                    && let Ok((alternative, alt_end, alt_time)) =
                        fragment::resolve_traf_samples(&traf, default, start, start, time, count)
                    && alternative
                        .iter()
                        .all(|s| in_payload(local, s.offset, s.size))
                {
                    entries = alternative;
                    end = alt_end;
                    next_time = alt_time;
                }
                if !entries
                    .iter()
                    .all(|s| in_payload(&payloads, s.offset, s.size))
                {
                    return Err(Error::invalid("Fragment references data outside mdat"));
                }
                previous_end = end;
                clocks[index] = next_time;
                total = total.saturating_add(entries.len());
                if total > MAX_SAMPLES {
                    return Err(Error::invalid("Too many MP4 samples"));
                }
                track.samples.extend(entries.into_iter().map(|s| Sample {
                    pts: s.pts(),
                    dts: s.dts.min(i64::MAX as u64) as i64,
                    duration: i64::from(s.duration),
                    offset: s.offset,
                    size: s.size,
                    key: s.keyframe,
                    discard: false,
                }));
            }
        }

        let mut duration = 0;
        let mut streams = streams;
        for (stream, track) in streams.iter_mut().zip(&mut tracks) {
            let edits = mp4.edit_list(stream.index);
            let scale = stream.time_base.as_rational().den as u32;
            // A common hybrid encode retains an edit for the initial sample
            // table even when fragments extend that same continuous timeline.
            if !edits.is_empty() && scale > 0 {
                let edits = edits
                    .iter()
                    .map(|edit| Edit {
                        track_duration: edit.segment_duration,
                        media_time: edit.media_time,
                        media_rate: (i32::from(edit.media_rate_integer) << 16)
                            | i32::from(edit.media_rate_fraction as u16),
                    })
                    .collect();
                let segments = resolve_edit_segments(&edits, None);
                let single_linear = segments
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.kind,
                            oxideav_mov::edit::EditSegmentKind::Media {
                                media_rate: 65536,
                                ..
                            }
                        )
                    })
                    .count()
                    == 1;
                for sample in &mut track.samples {
                    let time = |map: fn(&[_], i64, i64, i64, u32, u32) -> Option<EditedTiming>| {
                        map(
                            &segments,
                            sample.pts,
                            sample.dts,
                            sample.duration,
                            mp4.movie_timescale(),
                            scale,
                        )
                    };
                    let visible = time(edited_timing_for_sample);
                    let discard = visible.is_none() && !single_linear;
                    if let Some(timing) =
                        visible.or_else(|| time(never_presented_timing_for_sample))
                    {
                        sample.pts = timing.pts;
                        sample.dts = timing.dts;
                        sample.duration = timing.duration;
                    }
                    sample.discard = discard;
                }
            }
            let end = track
                .samples
                .iter()
                .filter(|s| !s.discard)
                .map(|s| s.pts.saturating_add(s.duration))
                .max()
                .unwrap_or(0);
            stream.duration = Some(end);
            if matches!(
                stream.params.media_type,
                MediaType::Audio | MediaType::Video
            ) {
                duration = duration.max((seconds(end, stream.time_base) * 1_000_000.0) as i64);
            }
        }
        Ok(Some(Self {
            reader,
            streams,
            tracks,
            active: HashSet::new(),
            duration: (duration > 0).then_some(duration),
        }))
    }
}

impl Demuxer for Fragmented {
    fn format_name(&self) -> &str {
        "mp4"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn duration_micros(&self) -> Option<i64> {
        self.duration
    }

    fn set_active_streams(&mut self, indices: &[u32]) {
        self.active = indices.iter().copied().collect();
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let (index, sample) = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                self.active.is_empty() || self.active.contains(&self.streams[*i].index)
            })
            .filter_map(|(i, track)| track.samples.get(track.next).map(|s| (i, *s)))
            .min_by(|(a, left), (b, right)| {
                seconds(left.dts, self.streams[*a].time_base)
                    .total_cmp(&seconds(right.dts, self.streams[*b].time_base))
            })
            .ok_or(Error::Eof)?;
        self.tracks[index].next += 1;
        if sample.size > MAX_PACKET_SIZE {
            return Err(Error::invalid("MP4 sample exceeds 64 MiB"));
        }
        self.reader.seek(SeekFrom::Start(sample.offset))?;
        let mut bytes = vec![0; sample.size as usize];
        self.reader.read_exact(&mut bytes)?;
        let stream = &self.streams[index];
        Ok(Packet::new(stream.index, stream.time_base, bytes)
            .with_pts(sample.pts)
            .with_dts(sample.dts)
            .with_duration(sample.duration)
            .with_keyframe(sample.key)
            .with_discard(sample.discard))
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let index = self
            .streams
            .iter()
            .position(|s| s.index == stream_index)
            .ok_or_else(|| Error::invalid("Unknown MP4 stream"))?;
        let primary = &self.tracks[index];
        let wanted = primary
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.key && s.pts <= pts)
            .max_by_key(|(_, s)| s.pts)
            .map_or(0, |(i, _)| i);
        let sample = *primary.samples.get(wanted).ok_or(Error::Eof)?;
        let real_time = seconds(sample.dts, self.streams[index].time_base);
        for (i, track) in self.tracks.iter_mut().enumerate() {
            if i == index {
                track.next = wanted;
                continue;
            }
            let stream = &self.streams[i];
            let base = stream.time_base;
            track.next = track.samples.partition_point(|s| {
                let tail = s.pts.saturating_add(s.duration);
                let preroll = if stream.params.media_type == MediaType::Subtitle {
                    10.0
                } else {
                    0.08
                };
                seconds(tail, base) < real_time - preroll
            });
        }
        Ok(sample.pts)
    }
}

fn start_offset(atom: &AtomHeader) -> u64 {
    atom.payload_offset - atom.header_len
}
fn end_offset(atom: &AtomHeader, file_end: u64) -> u64 {
    atom.payload_len()
        .map_or(file_end, |len| atom.payload_offset.saturating_add(len))
}

fn in_payload(payloads: &[Range<u64>], offset: u64, size: u32) -> bool {
    let i = payloads.partition_point(|span| span.end <= offset);
    payloads.get(i).is_some_and(|span| {
        offset >= span.start && offset.saturating_add(u64::from(size)) <= span.end
    })
}

fn seconds(timestamp: i64, base: TimeBase) -> f64 {
    let base = base.as_rational();
    timestamp as f64 * base.num as f64 / base.den as f64
}
