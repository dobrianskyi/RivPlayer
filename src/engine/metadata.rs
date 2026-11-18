use oxideav_core::CodecParameters;
use oxideav_h264::avcc::AvcDecoderConfigurationRecord;

pub(super) fn codec_configuration(params: &CodecParameters) -> (Option<String>, Option<u8>) {
    let extra = &params.extradata;
    let known = |name: &str, depth| (Some(name.to_owned()), depth);
    match params.codec_id.as_str() {
        "h264" => {
            let Ok(config) = AvcDecoderConfigurationRecord::parse(extra)
                .or_else(|_| AvcDecoderConfigurationRecord::from_annex_b(extra))
            else {
                return (None, None);
            };
            let profile = match config.profile_idc {
                66 if config.profile_compatibility & 0x40 != 0 => "Constrained Baseline",
                66 => "Baseline",
                77 => "Main",
                88 => "Extended",
                100 => "High",
                110 => "High 10",
                122 => "High 4:2:2",
                244 => "High 4:4:4 Predictive",
                id => return (Some(format!("Profile {id}")), None),
            };
            let depth = config
                .parsed_sps()
                .ok()
                .and_then(|sps| u8::try_from(sps.bit_depth_luma_minus8 + 8).ok());
            known(profile, depth)
        }
        "h265" | "hevc" => {
            let Ok(config) = oxideav_h265::hvcc::parse_hvcc(extra) else {
                return (None, None);
            };
            let depth = Some(config.bit_depth_luma_minus8 + 8);
            match config.general_profile_idc {
                0 => (None, depth),
                1 => known("Main", depth),
                2 => known("Main 10", depth),
                3 => known("Main Still Picture", depth),
                id => (Some(format!("Profile {id}")), depth),
            }
        }
        "av1" if extra.len() >= 4 && extra[0] == 0x81 => {
            // AV1CodecConfigurationRecord: seq_profile, high_bitdepth, twelve_bit.
            let profile = extra[1] >> 5;
            let depth = match (extra[2] & 0x40 != 0, extra[2] & 0x20 != 0) {
                (true, true) => Some(12),
                (true, false) => Some(10),
                (false, false) => Some(8),
                (false, true) => None,
            };
            match profile {
                0 => known("Main", depth),
                1 => known("High", depth),
                2 => known("Professional", depth),
                _ => (None, depth),
            }
        }
        "vp9" if extra.len() >= 12 && extra[..4] == [1, 0, 0, 0] && extra[4] <= 3 => {
            // VPCodecConfigurationRecord version 1, following its FullBox header.
            let depth = extra[6] >> 4;
            (
                Some(format!("Profile {}", extra[4])),
                matches!(depth, 8 | 10 | 12).then_some(depth),
            )
        }
        _ => (None, None),
    }
}
