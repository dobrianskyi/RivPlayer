use std::{fs::File, io::Read, path::Path, sync::Arc};

use super::Cue;
use anyhow::{Context, Result, ensure};
use libbitsub_core::{PgsParser, SubtitleComposition, SubtitleFrame, VobSubParser};

pub type BitmapFrame = SubtitleFrame;

enum Parser {
    Pgs(PgsParser),
    VobSub(VobSubParser),
}

pub struct BitmapTrack {
    parser: Parser,
    cached: Option<(usize, Option<Arc<BitmapFrame>>)>,
    cues: Vec<Cue>,
}

impl BitmapTrack {
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let mut parser = if ext.eq_ignore_ascii_case("sup") {
            let mut parser = PgsParser::new();
            ensure!(
                parser.parse(&read(path, 32 << 20)?) > 0,
                "No PGS cues in {}",
                path.display()
            );
            Parser::Pgs(parser)
        } else if ext.eq_ignore_ascii_case("idx") {
            let idx =
                String::from_utf8(read(path, 1 << 20)?).context("Invalid VobSub index text")?;
            let sub = path.with_extension("sub");
            let sub = if sub.is_file() {
                sub
            } else {
                path.with_extension("SUB")
            };
            let mut parser = VobSubParser::new();
            parser.load_from_data(&idx, read(&sub, 32 << 20)?);
            ensure!(parser.count() > 0, "No VobSub cues in {}", path.display());
            Parser::VobSub(parser)
        } else {
            return Ok(None);
        };
        let mut cues = match &mut parser {
            Parser::Pgs(p) => (0..p.count())
                .filter(|&i| p.get_cue_composition_count(i) > 0)
                .map(|i| (p.get_cue_start_time(i), p.get_cue_end_time(i)))
                .collect::<Vec<_>>(),
            Parser::VobSub(p) => (0..p.count())
                .map(|i| (p.get_cue_start_time(i), p.get_cue_end_time(i)))
                .collect(),
        }
        .into_iter()
        .filter(|(start, end)| *start >= 0.0 && end > start)
        .map(|(start, end)| Cue {
            start: start / 1000.0,
            end: end / 1000.0,
            text: "Bitmap subtitle".into(),
        })
        .collect::<Vec<_>>();
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));
        Ok(Some(Self {
            parser,
            cached: None,
            cues,
        }))
    }

    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }

    pub fn frame_at(&mut self, time: f64) -> Option<Arc<BitmapFrame>> {
        let time = time * 1000.0;
        let index = match &mut self.parser {
            Parser::Pgs(parser) => usize::try_from(parser.find_index_at_timestamp(time))
                .ok()
                .filter(|&i| time < parser.get_cue_end_time(i)),
            Parser::VobSub(parser) => usize::try_from(parser.find_index_at_timestamp(time))
                .ok()
                .filter(|&i| time < parser.get_cue_end_time(i)),
        }?;
        if self
            .cached
            .as_ref()
            .is_none_or(|(cached, _)| *cached != index)
        {
            let frame = match &mut self.parser {
                Parser::Pgs(parser) => parser.render_at_index(index),
                Parser::VobSub(parser) => {
                    parser.render_at_index(index).map(|image| SubtitleFrame {
                        width: image.screen_width(),
                        height: image.screen_height(),
                        compositions: vec![SubtitleComposition {
                            x: image.x(),
                            y: image.y(),
                            width: image.width(),
                            height: image.height(),
                            rgba: image.get_rgba().to_vec(),
                        }],
                    })
                }
            };
            self.cached = Some((
                index,
                frame
                    .filter(|frame| !frame.compositions.is_empty())
                    .map(Arc::new),
            ));
        }
        self.cached.as_ref()?.1.clone()
    }
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("Opening subtitle {}", path.display()))?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Subtitle file is too large: {}",
        path.display()
    );
    Ok(bytes)
}
