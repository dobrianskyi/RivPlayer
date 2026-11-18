mod fragmented;
mod mpeg1ps;

use std::{
    fs::File,
    io::{BufReader, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use oxideav_core::{CodecInfo, CodecTag, Demuxer, ReadSeek, RuntimeContext};

/// A provider supplies media without depending on the UI or decoders.
/// Segmented providers can expose cached byte ranges or merge their own
/// parallel audio and video streams through a demuxer.
pub trait MediaSource: Send + Sync {
    fn title(&self) -> String;
    fn extension(&self) -> Option<&str>;
    fn reader(&self) -> Result<Box<dyn ReadSeek>>;
    fn open_demuxer(&self, ctx: &RuntimeContext) -> Result<Box<dyn Demuxer>> {
        open(ctx, self).map(|(demux, _)| demux)
    }

    fn open_described(&self, ctx: &RuntimeContext) -> Result<(Box<dyn Demuxer>, String)> {
        let demux = self.open_demuxer(ctx)?;
        let format = demux.format_name().to_owned();
        Ok((demux, format))
    }

    fn sidecar(&self) -> Option<PathBuf> {
        None
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

pub struct LocalFile(pub PathBuf);

impl MediaSource for LocalFile {
    fn open_described(&self, ctx: &RuntimeContext) -> Result<(Box<dyn Demuxer>, String)> {
        open(ctx, self)
    }

    fn title(&self) -> String {
        self.0
            .file_name()
            .unwrap_or(self.0.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    fn extension(&self) -> Option<&str> {
        self.0.extension()?.to_str()
    }

    fn reader(&self) -> Result<Box<dyn ReadSeek>> {
        let file = File::open(&self.0).with_context(|| format!("Opening {}", self.0.display()))?;
        Ok(Box::new(BufReader::new(file)))
    }

    fn sidecar(&self) -> Option<PathBuf> {
        ["srt", "vtt", "ass", "ssa", "sup", "idx"]
            .into_iter()
            .map(|ext| self.0.with_extension(ext))
            .find(|p| p.is_file())
    }

    fn byte_len(&self) -> Option<u64> {
        let metadata = self.0.metadata().ok()?;
        metadata.is_file().then_some(metadata.len())
    }
}

pub fn runtime() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_ac3::register(&mut ctx);
    oxideav_avi::register(&mut ctx);
    oxideav_flv::register(&mut ctx);
    oxideav_mpegts::register(&mut ctx);
    oxideav_ogg::register(&mut ctx);
    oxideav_mpeg12video::register(&mut ctx);
    oxideav_theora::register(&mut ctx);
    oxideav_mpeg4video::register(&mut ctx);
    ctx.containers.register_extension("3gp", "mp4");
    ctx.containers.register_extension("m4v", "mp4");
    oxideav_mkv::register_containers(&mut ctx.containers);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::register_containers(&mut ctx.containers);
    oxideav_flac::register_containers(&mut ctx.containers);
    oxideav_mp3::register(&mut ctx);
    oxideav_h264::register_codecs(&mut ctx.codecs);
    oxideav_h265::register(&mut ctx);
    oxideav_vp8::register(&mut ctx);
    oxideav_vp9::register(&mut ctx);
    for (id, tags) in [
        ("av1", &[*b"av01"][..]),
        ("aac", &[*b"mp4a"][..]),
        ("opus", &[*b"Opus"][..]),
        ("flac", &[*b"fLaC"][..]),
    ] {
        ctx.codecs
            .register(CodecInfo::new(id.into()).tags(tags.iter().map(CodecTag::fourcc)));
    }
    ctx
}

fn open(
    ctx: &RuntimeContext,
    source: &(impl MediaSource + ?Sized),
) -> Result<(Box<dyn Demuxer>, String)> {
    let mut reader = source.reader()?;
    let mut header = [0; 5];
    let mpeg1 = reader.read_exact(&mut header).is_ok()
        && header[..4] == [0, 0, 1, 0xba]
        && header[4] & 0xf0 == 0x20;
    reader.seek(SeekFrom::Start(0))?;
    if mpeg1 {
        return Ok((Box::new(mpeg1ps::open(reader)?), "mpeg1ps".into()));
    }
    let format = ctx
        .containers
        .probe_input(reader.as_mut(), source.extension())?;
    if format == "mov"
        && let Ok(mov) = ctx
            .containers
            .open_demuxer("mov", source.reader()?, &ctx.codecs)
    {
        return Ok((mov, "mov".into()));
    }
    if matches!(format.as_str(), "matroska" | "webm") {
        let mkv = oxideav_mkv::demux::open_typed(reader, &ctx.codecs)?;
        let label = mkv.ebml_header().doc_type.clone();
        return Ok((Box::new(mkv), label));
    }
    if format == "mp4" || format == "mov" {
        let mp4 = oxideav_mp4::demux::open_typed(reader, &ctx.codecs)?;
        let label = match mp4.major_brand().as_ref() {
            Some(b"qt  ") => "mov",
            Some(b"M4V " | b"M4VH" | b"M4VP") => "m4v",
            Some(brand) if brand.starts_with(b"3gp") || brand.starts_with(b"3g2") => "3gp",
            _ => "mp4",
        }
        .to_owned();
        if mp4.is_fragmented()
            && let Some(fragmented) = fragmented::Fragmented::open(&mp4, source.reader()?)?
        {
            return Ok((Box::new(fragmented), label));
        }
        return Ok((Box::new(mp4), label));
    }
    Ok((
        ctx.containers.open_demuxer(&format, reader, &ctx.codecs)?,
        format,
    ))
}

pub fn is_caption(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["srt", "vtt", "ass", "ssa", "sup", "idx"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e))
        })
}
