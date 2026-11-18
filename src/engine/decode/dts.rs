use anyhow::{Context, Result, bail, ensure};
use oxideav_core::{Decoder, Frame, Packet};
use oxideav_dts::{DtsFrameHeader, parse_frame_header, parse_frame_header_14bit};

use super::{DialogueLayout, Samples};

pub(super) fn decode(decoder: &mut dyn Decoder, packet: &Packet) -> Result<Samples> {
    let header = parse_frame_header(&packet.data)
        .or_else(|_| parse_frame_header_14bit(&packet.data))
        .context("No DTS Core frame")?;
    let len = header.frame_size_container_bytes(header.sync_word_encoding) as usize;
    let data = packet.data.get(..len).context("Truncated DTS Core frame")?;
    let mut core = Packet::new(packet.stream_index, packet.time_base, data.to_vec());
    core.pts = packet.pts;
    decoder.send_packet(&core)?;
    let Frame::Audio(frame) = decoder.receive_frame()? else {
        bail!("Expected DTS Core PCM");
    };
    let count = frame.samples as usize;
    let channels = frame.data.len();
    ensure!(
        (1..=9).contains(&channels) && frame.data.iter().all(|p| p.len() == count * 4),
        "Invalid DTS PCM planes"
    );
    let rate = header.sample_rate_hz().context("Invalid DTS sample rate")?;
    let order = channel_order(&header, channels);
    let mut output = Vec::with_capacity(count * channels);
    for i in 0..count {
        for &ch in &order {
            let sample = frame.data[ch].as_chunks::<4>().0[i];
            // The Core factory writes signed 32-bit full-scale PCM, independent
            // of the source resolution declared in the bitstream.
            output.push(i32::from_le_bytes(sample) as f32 * (1.0 / 2_147_483_648.0));
        }
    }
    Ok(Samples {
        rate,
        channels,
        layout: DialogueLayout {
            front: None,
            center: (matches!(header.amode, 5 | 7 | 9) && channels >= 3).then_some(2),
        },
        data: output,
    })
}

fn channel_order(header: &DtsFrameHeader, channels: usize) -> Vec<usize> {
    let mode = header.amode_arrangement();
    let Some((left, right)) = mode
        .front_lr_channels()
        .filter(|&(l, r)| l < channels && r < channels)
    else {
        return (0..channels).collect();
    };
    let surround = mode.surround_lr_channels();
    let lfe = mode
        .channel_count()
        .filter(|&n| usize::from(n) + 1 == channels)
        .map(usize::from);
    let mut order = vec![left, right];
    for ch in 0..channels {
        if ch != left
            && ch != right
            && lfe != Some(ch)
            && surround.is_none_or(|(sl, sr)| ch != sl && ch != sr)
        {
            order.push(ch);
        }
    }
    order.extend(lfe);
    if let Some((sl, sr)) = surround {
        order.extend([sl, sr].into_iter().filter(|&ch| ch < channels));
    }
    order
}
