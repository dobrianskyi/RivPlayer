use std::{mem::MaybeUninit, ptr::NonNull};

use anyhow::{Result, bail, ensure};
use oxideav_core::{
    ColorPrimaries, ColorRange, ColorSignal, MatrixCoefficients, Packet, PixelFormat,
    TransferCharacteristics, VideoFrame, VideoPlane,
};
use rav1d::{
    Dav1dResult,
    include::dav1d::{
        data::Dav1dData,
        dav1d::{Dav1dContext, Dav1dSettings},
        headers::*,
        picture::Dav1dPicture,
    },
    src::lib::{
        dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings,
        dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
    },
};

use super::decode::{Picture, decode_threads};

pub struct Av1(Option<Dav1dContext>);

impl Av1 {
    pub fn open(config: &[u8]) -> Result<Self> {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: rav1d initializes the entire settings struct.
        let mut settings = unsafe {
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        settings.n_threads = decode_threads() as i32;
        settings.max_frame_delay = 4;
        settings.frame_size_limit = 8192 * 4320;
        settings.all_layers = 0;
        let mut decoder = Self(None);
        // SAFETY: both storage locations are valid and settings are initialized.
        check(unsafe {
            dav1d_open(
                Some(NonNull::from(&mut decoder.0)),
                Some(NonNull::from(&mut settings)),
            )
        })?;
        if let Some(obus) = config_obus(config) {
            decoder.send(obus, None, &mut Vec::new())?;
        }
        Ok(decoder)
    }

    pub fn decode(&mut self, packet: Option<&Packet>) -> Result<Vec<Picture>> {
        let mut out = Vec::new();
        if let Some(packet) = packet {
            self.send(&packet.data, packet.pts, &mut out)?;
        } else {
            while let Some(picture) = self.receive()? {
                out.push(picture);
            }
        }
        Ok(out)
    }

    fn send(&mut self, bytes: &[u8], pts: Option<i64>, out: &mut Vec<Picture>) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut data = Input(Dav1dData::default());
        // SAFETY: rav1d owns this allocation until it consumes data or Input drops it.
        let dst = unsafe { dav1d_data_create(Some(NonNull::from(&mut data.0)), bytes.len()) };
        ensure!(!dst.is_null(), "rav1d packet allocation failed");
        // SAFETY: dst has bytes.len() writable bytes and does not alias bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len()) };
        data.0.m.timestamp = pts.unwrap_or(i64::MIN);
        loop {
            // SAFETY: the context remains open and data is initialized.
            let status = unsafe { dav1d_send_data(self.0, Some(NonNull::from(&mut data.0))) };
            if status.0 != -libc::EAGAIN {
                check(status)?;
                break;
            }
            out.push(
                self.receive()?.ok_or_else(|| {
                    anyhow::anyhow!("rav1d did not accept input or return a frame")
                })?,
            );
        }
        while let Some(picture) = self.receive()? {
            out.push(picture);
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<Picture>> {
        let mut raw = RawPicture(Dav1dPicture::default());
        // SAFETY: the context remains open and raw is valid output storage.
        let status = unsafe { dav1d_get_picture(self.0, Some(NonNull::from(&mut raw.0))) };
        if status.0 == -libc::EAGAIN {
            return Ok(None);
        }
        check(status)?;
        raw.copy().map(Some)
    }
}

pub fn config_obus(config: &[u8]) -> Option<&[u8]> {
    (config.first() == Some(&0x81))
        .then(|| config.get(4..))
        .flatten()
        .filter(|obus| !obus.is_empty())
}

fn check(result: Dav1dResult) -> Result<()> {
    ensure!(result.0 == 0, "rav1d returned {}", result.0);
    Ok(())
}

impl Drop for Av1 {
    fn drop(&mut self) {
        // SAFETY: this is the unique owning context slot and close clears it.
        unsafe { dav1d_close(Some(NonNull::from(&mut self.0))) };
    }
}

struct Input(Dav1dData);
impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: this struct is initialized and dav1d leaves consumed packets empty.
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut self.0))) };
    }
}

struct RawPicture(Dav1dPicture);
impl Drop for RawPicture {
    fn drop(&mut self) {
        // SAFETY: this initialized picture has not previously been unreferenced.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.0))) };
    }
}

impl RawPicture {
    fn copy(&self) -> Result<Picture> {
        let picture = &self.0;
        let width = u32::try_from(picture.p.w)?;
        let height = u32::try_from(picture.p.h)?;
        let format = match (picture.p.layout, picture.p.bpc) {
            (DAV1D_PIXEL_LAYOUT_I400, 8) => PixelFormat::Gray8,
            (DAV1D_PIXEL_LAYOUT_I400, 10) => PixelFormat::Gray10Le,
            (DAV1D_PIXEL_LAYOUT_I400, 12) => PixelFormat::Gray12Le,
            (DAV1D_PIXEL_LAYOUT_I420, 8) => PixelFormat::Yuv420P,
            (DAV1D_PIXEL_LAYOUT_I422, 8) => PixelFormat::Yuv422P,
            (DAV1D_PIXEL_LAYOUT_I444, 8) => PixelFormat::Yuv444P,
            (DAV1D_PIXEL_LAYOUT_I420, 10) => PixelFormat::Yuv420P10Le,
            (DAV1D_PIXEL_LAYOUT_I422, 10) => PixelFormat::Yuv422P10Le,
            (DAV1D_PIXEL_LAYOUT_I444, 10) => PixelFormat::Yuv444P10Le,
            (DAV1D_PIXEL_LAYOUT_I420, 12) => PixelFormat::Yuv420P12Le,
            (DAV1D_PIXEL_LAYOUT_I422, 12) => PixelFormat::Yuv422P12Le,
            (DAV1D_PIXEL_LAYOUT_I444, 12) => PixelFormat::Yuv444P12Le,
            _ => bail!("Unsupported AV1 pixel layout"),
        };
        let color = picture.seq_hdr.map_or_else(ColorSignal::default, |header| {
            // SAFETY: the picture retains the sequence header until unref.
            let header = unsafe { header.as_ref() };
            ColorSignal::new(
                ColorRange::from_full_range_flag(header.color_range != 0),
                ColorPrimaries(header.pri as u8),
                TransferCharacteristics(header.trc as u8),
                MatrixCoefficients(header.mtrx as u8),
            )
        });
        let timestamp = (picture.m.timestamp != i64::MIN).then_some(picture.m.timestamp);
        let mut planes = Vec::with_capacity(format.plane_count());
        for plane in 0..format.plane_count() {
            let (_, rows) = format.plane_dimensions(plane, width, height).unwrap();
            let row_bytes = format.plane_row_bytes(plane, width).unwrap();
            let stride = picture.stride[usize::from(plane != 0)];
            ensure!(stride.unsigned_abs() >= row_bytes, "Invalid AV1 stride");
            let src = picture.data[plane]
                .ok_or_else(|| anyhow::anyhow!("Missing AV1 plane"))?
                .cast::<u8>();
            let mut data = Vec::with_capacity(row_bytes * rows as usize);
            for y in 0..rows as isize {
                // SAFETY: rav1d supplies one readable row at each signed stride.
                let row = unsafe {
                    std::slice::from_raw_parts(src.as_ptr().offset(y * stride), row_bytes)
                };
                data.extend_from_slice(row);
            }
            planes.push(VideoPlane {
                stride: row_bytes,
                data,
            });
        }
        Ok(Picture {
            timestamp,
            width,
            height,
            format,
            p010: false,
            color,
            frame: VideoFrame {
                pts: timestamp,
                planes,
            },
        })
    }
}
