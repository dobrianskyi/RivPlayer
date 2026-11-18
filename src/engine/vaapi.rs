use anyhow::{Context, Result, bail, ensure};
use oxideav_core::{CodecParameters, ColorSignal, Packet, PixelFormat, VideoFrame, VideoPlane};
use oxideav_h264::{
    avcc::AvcDecoderConfigurationRecord,
    nal::{AnnexBSplitter, AvccSplitter},
};
use oxideav_vaapi::{
    Display,
    av1::Av1Decoder,
    h264::H264Decoder,
    hevc::HevcDecoder,
    session::{DecodeError, Planes},
    vp9::Vp9Decoder,
};

use super::{av1::config_obus, decode::Picture};

enum Codec {
    H264(Box<H264Decoder>),
    Hevc(Box<HevcDecoder>),
    Vp9(Box<Vp9Decoder>),
    Av1(Box<Av1Decoder>),
}

pub struct VaDecoder {
    codec: Codec,
    length_size: u8,
    color: ColorSignal,
    reorder: Vec<Picture>,
}

impl VaDecoder {
    pub fn open(params: &CodecParameters) -> Result<Self> {
        let id = params.codec_id.as_str();
        let family = if id == "h265" { "hevc" } else { id };
        let mut display = None;
        for index in 0..16 {
            let Ok(path) = oxideav_vaapi::device_path_for_index(index) else {
                break;
            };
            if let Ok(candidate) = Display::open_drm(&path)
                && let Ok(matrix) = candidate.entrypoint_matrix()
                && oxideav_vaapi::codec_decode_supported(&matrix, family)
            {
                display = Some(candidate);
                break;
            }
        }
        let display = display.context("No VA-API device exposes this codec")?;
        let extra = &params.extradata;
        let mut length_size = 0;
        let codec = match id {
            "h264" => {
                let mut codec = H264Decoder::new(display);
                if extra.first() == Some(&1) {
                    let config = AvcDecoderConfigurationRecord::parse(extra)
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    length_size = config.length_size;
                    codec.decode(config.sps.iter().chain(&config.pps).map(Vec::as_slice))?;
                    codec.prepare()?;
                }
                Codec::H264(Box::new(codec))
            }
            "hevc" | "h265" => {
                let mut codec = HevcDecoder::new(display);
                if extra.first() == Some(&1) {
                    let (size, nals) = hevc_config(extra)?;
                    length_size = size;
                    codec.decode(nals)?;
                    codec.prepare()?;
                }
                Codec::Hevc(Box::new(codec))
            }
            "vp9" => Codec::Vp9(Box::new(Vp9Decoder::new(display))),
            "av1" => {
                let mut codec = Av1Decoder::new(display);
                if let Some(obus) = config_obus(extra) {
                    codec.decode(obus)?;
                }
                Codec::Av1(Box::new(codec))
            }
            _ => bail!("No VA-API adapter for {id}"),
        };
        Ok(Self {
            codec,
            length_size,
            color: params.color_signal,
            reorder: Vec::new(),
        })
    }

    pub fn decode(&mut self, packet: Option<&Packet>) -> Result<Vec<Picture>> {
        let Some(packet) = packet else {
            self.reorder.sort_by_key(|frame| frame.timestamp);
            return Ok(std::mem::take(&mut self.reorder));
        };
        let pts = packet.pts.or(packet.dts);
        let color = self.color;
        match &mut self.codec {
            Codec::Vp9(decoder) => {
                let Some(p) = decoder.decode(&packet.data)? else {
                    return Ok(Vec::new());
                };
                return Ok(vec![read(
                    p.width,
                    p.height,
                    p.bit_depth,
                    pts,
                    color,
                    |out| decoder.read(&p, out),
                )?]);
            }
            Codec::Av1(decoder) => {
                let Some(p) = decoder.decode(&packet.data)? else {
                    return Ok(Vec::new());
                };
                return Ok(vec![read(
                    p.width,
                    p.height,
                    p.bit_depth,
                    pts,
                    color,
                    |out| decoder.read(&p, out),
                )?]);
            }
            Codec::H264(decoder) => {
                let mut pictures = decoder.decode(sample_nals(&packet.data, self.length_size)?)?;
                pictures.extend(decoder.flush()?);
                for p in pictures {
                    let (_, _, w, h) = p.crop;
                    self.reorder
                        .push(read(w, h, p.bit_depth, pts, color, |out| {
                            decoder.read(&p, out)
                        })?);
                }
            }
            Codec::Hevc(decoder) => {
                let mut pictures = decoder.decode(sample_nals(&packet.data, self.length_size)?)?;
                pictures.extend(decoder.flush()?);
                for p in pictures.into_iter().filter(|p| p.output) {
                    let (_, _, w, h) = p.crop;
                    self.reorder
                        .push(read(w, h, p.bit_depth, pts, color, |out| {
                            decoder.read(&p, out)
                        })?);
                }
            }
        }

        self.reorder.sort_by_key(|p| p.timestamp);
        // MP4 supplies a decode timeline. Matroska can omit DTS, so keep
        // a bounded presentation queue for those streams.
        let ready = packet
            .dts
            .filter(|dts| Some(*dts) != packet.pts)
            .map_or(0, |dts| {
                self.reorder
                    .partition_point(|p| p.timestamp.is_none_or(|pts| pts <= dts))
            });
        let count = ready.max(self.reorder.len().saturating_sub(6));
        Ok(self.reorder.drain(..count).collect())
    }
}

fn sample_nals(data: &[u8], length_size: u8) -> Result<Vec<&[u8]>> {
    if length_size == 0 {
        return Ok(AnnexBSplitter::new(data).collect());
    }
    AvccSplitter::new(data, length_size)
        .and_then(Iterator::collect)
        .map_err(|e| anyhow::anyhow!("Invalid NAL sample: {e:?}"))
}

fn hevc_config(data: &[u8]) -> Result<(u8, Vec<&[u8]>)> {
    let header = data.get(..23).context("Truncated hvcC header")?;
    let mut remaining = &data[23..];
    let mut nals = Vec::new();
    for _ in 0..header[22] {
        let (array, rest) = remaining
            .split_at_checked(3)
            .context("Truncated hvcC array")?;
        remaining = rest;
        let count = u16::from_be_bytes([array[1], array[2]]);
        for _ in 0..count {
            let (size, rest) = remaining
                .split_at_checked(2)
                .context("Truncated hvcC NAL length")?;
            let (nal, rest) = rest
                .split_at_checked(usize::from(u16::from_be_bytes([size[0], size[1]])))
                .context("Truncated hvcC NAL")?;
            nals.push(nal);
            remaining = rest;
        }
    }
    Ok(((header[21] & 3) + 1, nals))
}

fn read(
    width: u32,
    height: u32,
    bits: u8,
    pts: Option<i64>,
    color: ColorSignal,
    copy: impl FnOnce(&mut Planes<'_>) -> std::result::Result<(), DecodeError>,
) -> Result<Picture> {
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 8192 * 4320,
        "VA-API frame exceeds size limit"
    );
    ensure!(
        matches!(bits, 8 | 10),
        "Unsupported VA-API bit depth {bits}"
    );
    let bytes = if bits == 8 { 1 } else { 2 };
    let y_stride = width as usize * bytes;
    let uv_stride = width.div_ceil(2) as usize * bytes * 2;
    let mut y = vec![0; y_stride * height as usize];
    let mut uv = vec![0; uv_stride * height.div_ceil(2) as usize];
    copy(&mut Planes::new(&mut y, y_stride, &mut uv, uv_stride))?;
    let mut planes = Vec::with_capacity(3);
    let format = if bits == 8 {
        planes.push(VideoPlane {
            stride: y_stride,
            data: y,
        });
        planes.push(VideoPlane {
            stride: uv_stride,
            data: uv,
        });
        PixelFormat::Nv12
    } else {
        // VA-API P010 stores MSB-aligned samples. The renderer accepts
        // the same LSB-aligned planar format as the software decoder.
        for word in y.as_chunks_mut::<2>().0 {
            let sample = u16::from_le_bytes([word[0], word[1]]) >> 6;
            word.copy_from_slice(&sample.to_le_bytes());
        }
        let mut u = Vec::with_capacity(uv.len() / 2);
        let mut v = Vec::with_capacity(uv.len() / 2);
        for pair in uv.as_chunks::<4>().0 {
            u.extend_from_slice(&(u16::from_le_bytes([pair[0], pair[1]]) >> 6).to_le_bytes());
            v.extend_from_slice(&(u16::from_le_bytes([pair[2], pair[3]]) >> 6).to_le_bytes());
        }
        planes.push(VideoPlane {
            stride: y_stride,
            data: y,
        });
        planes.push(VideoPlane {
            stride: uv_stride / 2,
            data: u,
        });
        planes.push(VideoPlane {
            stride: uv_stride / 2,
            data: v,
        });
        PixelFormat::Yuv420P10Le
    };
    Ok(Picture {
        timestamp: pts,
        width,
        height,
        format,
        color,
        frame: VideoFrame { pts, planes },
    })
}
