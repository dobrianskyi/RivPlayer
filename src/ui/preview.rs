use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, bounded};
use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};
use oxideav_core::{ColorRange, PixelFormat};
use rvideoplayer::{
    engine::{self, Picture},
    source::LocalFile,
};

const MAX_CACHED: usize = 48;

#[derive(Clone, PartialEq, Eq)]
struct Key {
    file: PathBuf,
    track: Option<u32>,
    tenth: u64,
}

struct Request {
    key: Key,
    cancelled: Arc<AtomicBool>,
}

pub struct Preview {
    tx: Sender<Request>,
    pending: Receiver<Request>,
    result: Receiver<(Key, Option<ColorImage>)>,
    cache: VecDeque<(Key, Option<TextureHandle>)>,
    requested: Option<(Key, Arc<AtomicBool>)>,
    hover: Option<(Key, Instant)>,
}

impl Preview {
    pub fn new(ctx: Context) -> Self {
        let (tx, pending) = bounded::<Request>(1);
        let worker = pending.clone();
        let (ready, result) = bounded(2);
        thread::spawn(move || {
            // The decoder can create worker threads. Lower this job's
            // scheduler priority so they leave playback frames time to run.
            // SAFETY: who=0 changes only the calling Linux thread.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 8) };
            while let Ok(Request { key, cancelled }) = worker.recv() {
                if cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let frame = engine::preview(
                    &LocalFile(key.file.clone()),
                    key.tenth as f64 / 10.0,
                    key.track,
                    &cancelled,
                )
                .ok()
                .flatten()
                .and_then(|frame| thumbnail(&frame.picture));
                if !cancelled.load(Ordering::Relaxed) {
                    let _ = ready.try_send((key, frame));
                    ctx.request_repaint();
                }
            }
        });
        Self {
            tx,
            pending,
            result,
            cache: VecDeque::new(),
            requested: None,
            hover: None,
        }
    }

    pub fn cancel(&mut self) {
        self.hover = None;
        self.cancel_request();
    }

    fn cancel_request(&mut self) {
        if let Some((_, cancelled)) = self.requested.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        let _ = self.pending.try_recv();
    }

    pub fn invalidate(&mut self, path: &Path) {
        self.cancel();
        self.cache.retain(|(key, _)| key.file != path);
        while self.result.try_recv().is_ok() {}
    }

    pub fn get(
        &mut self,
        ctx: &Context,
        file: &Path,
        track: Option<u32>,
        time: f64,
    ) -> Option<TextureHandle> {
        while let Ok((key, frame)) = self.result.try_recv() {
            if self
                .requested
                .as_ref()
                .is_some_and(|(pending, _)| *pending == key)
            {
                let texture = frame
                    .map(|image| ctx.load_texture("seek-frame", image, TextureOptions::LINEAR));
                self.cache.push_front((key, texture));
                self.cache.truncate(MAX_CACHED);
            }
        }
        let key = Key {
            file: file.to_owned(),
            track,
            tenth: (time * 10.0).floor() as u64,
        };
        if let Some(position) = self.cache.iter().position(|(cached, _)| *cached == key) {
            if self
                .requested
                .as_ref()
                .is_some_and(|(pending, _)| *pending != key)
            {
                self.cancel_request();
            }
            let cached = self.cache.remove(position).unwrap();
            let texture = cached.1.clone();
            self.cache.push_front(cached);
            return texture;
        }
        if self
            .requested
            .as_ref()
            .is_none_or(|(pending, _)| *pending != key)
        {
            if self
                .hover
                .as_ref()
                .is_none_or(|(hovered, _)| *hovered != key)
            {
                self.cancel_request();
                self.hover = Some((key, Instant::now()));
                ctx.request_repaint_after(Duration::from_millis(90));
                return None;
            }
            let elapsed = self.hover.as_ref().unwrap().1.elapsed();
            let delay = Duration::from_millis(90);
            if elapsed < delay {
                ctx.request_repaint_after(delay - elapsed);
                return None;
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            if self
                .tx
                .try_send(Request {
                    key: key.clone(),
                    cancelled: cancelled.clone(),
                })
                .is_ok()
            {
                self.requested = Some((key, cancelled));
            }
        }
        None
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn thumbnail(picture: &Picture) -> Option<ColorImage> {
    let format = picture.format;
    let (width, height) = (picture.width, picture.height);
    if width == 0 || height == 0 {
        return None;
    }
    let scale = (176.0 / f64::from(width))
        .min(100.0 / f64::from(height))
        .min(1.0);
    let w = (f64::from(width) * scale).round().max(1.0) as usize;
    let h = (f64::from(height) * scale).round().max(1.0) as usize;
    let planes = picture.frame.image_planes();
    let sample = |plane: usize, x: u32, y: u32, channel: usize| -> Option<f32> {
        let data = planes.get(plane)?;
        let (pw, ph) = format.plane_dimensions(plane, width, height)?;
        let cx = (u64::from(x) * u64::from(pw) / u64::from(width)) as usize;
        let cy = (u64::from(y) * u64::from(ph) / u64::from(height)) as usize;
        let packed = matches!(
            format,
            PixelFormat::Rgb24
                | PixelFormat::Rgba
                | PixelFormat::Bgr24
                | PixelFormat::Bgra
                | PixelFormat::Nv12
                | PixelFormat::Nv21
        );
        let bpp = format.plane_row_bytes(plane, width)? / pw as usize;
        let at = cy
            .checked_mul(data.stride)?
            .checked_add(cx * bpp + channel)?;
        if !packed && bpp == 2 {
            let depth = picture
                .frame
                .plane_significant_bits(plane)
                .unwrap_or_else(|| {
                    use PixelFormat::*;
                    match format {
                        Gray10Le | Yuv420P10Le | Yuv422P10Le | Yuv444P10Le | Yuv440P10Le => 10,
                        Gray12Le | Yuv420P12Le | Yuv422P12Le | Yuv444P12Le | Yuv440P12Le => 12,
                        _ => 16,
                    }
                })
                .clamp(8, 16);
            let value = u16::from_le_bytes([*data.data.get(at)?, *data.data.get(at + 1)?]);
            return Some(f32::from(value) * 255.0 / ((1_u32 << depth) - 1) as f32);
        }
        Some(f32::from(*data.data.get(at)?))
    };
    let mut rgb = Vec::with_capacity(w * h * 3);
    let full = picture.color.range == ColorRange::Full
        || format.implied_color_range() == Some(ColorRange::Full);
    let hd = picture.color.matrix.0 == 1 || (picture.color.matrix.0 == 2 && height >= 720);
    for row in 0..h {
        for col in 0..w {
            let x = (((col as f64 + 0.5) * f64::from(width)) / w as f64) as u32;
            let y = (((row as f64 + 0.5) * f64::from(height)) / h as f64) as u32;
            use PixelFormat::*;
            let get = |plane, channel| sample(plane, x, y, channel);
            let color = match format {
                Rgb24 | Rgba => [get(0, 0)?, get(0, 1)?, get(0, 2)?],
                Bgr24 | Bgra => [get(0, 2)?, get(0, 1)?, get(0, 0)?],
                _ => {
                    let luma = get(0, 0)?;
                    let (u, v) = match format {
                        Nv12 => (get(1, 0)?, get(1, 1)?),
                        Nv21 => (get(1, 1)?, get(1, 0)?),
                        _ if planes.len() >= 3 => (get(1, 0)?, get(2, 0)?),
                        _ => (128.0, 128.0),
                    };
                    let (l, u, v) = if full {
                        (luma, u - 128.0, v - 128.0)
                    } else {
                        (
                            (luma - 16.0) * 255.0 / 219.0,
                            (u - 128.0) * 255.0 / 224.0,
                            (v - 128.0) * 255.0 / 224.0,
                        )
                    };
                    if hd {
                        [l + 1.575 * v, l - 0.187 * u - 0.468 * v, l + 1.856 * u]
                    } else {
                        [l + 1.402 * v, l - 0.344 * u - 0.714 * v, l + 1.772 * u]
                    }
                }
            };
            rgb.extend(color.map(|v| v.clamp(0.0, 255.0) as u8));
        }
    }
    Some(ColorImage::from_rgb([w, h], &rgb))
}

pub fn encode(image: &ColorImage) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + image.pixels.len() * 3);
    bytes.extend([image.size[0] as u8, image.size[1] as u8]);
    for pixel in &image.pixels {
        bytes.extend([pixel.r(), pixel.g(), pixel.b()]);
    }
    bytes
}

pub fn decode(data: &[u8]) -> Option<ColorImage> {
    let (&w, data) = data.split_first()?;
    let (&h, data) = data.split_first()?;
    let size = [usize::from(w), usize::from(h)];
    (w > 0 && h > 0 && data.len() == size[0] * size[1] * 3)
        .then(|| ColorImage::from_rgb(size, data))
}

pub fn has_detail(image: &ColorImage) -> bool {
    let Some(reference) = image.pixels.get(image.pixels.len() / 3) else {
        return false;
    };
    image.pixels.iter().any(|pixel| {
        pixel.r().abs_diff(reference.r()) > 12
            || pixel.g().abs_diff(reference.g()) > 12
            || pixel.b().abs_diff(reference.b()) > 12
    })
}
