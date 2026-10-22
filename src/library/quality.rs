//! Reads codec, bit depth and sample rate of local files.

use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::FileType;
use lofty::prelude::*;
use lofty::probe::Probe;

use crate::model::AudioQuality;

pub fn read(path: &Path) -> Option<AudioQuality> {
    let file = Probe::open(path)
        .ok()?
        .options(ParseOptions::new().read_tags(false).read_cover_art(false))
        .read()
        .ok()?;
    let props = file.properties();
    let bits = props.bit_depth();
    let (codec, lossless) = match file.file_type() {
        FileType::Flac => ("FLAC", true),
        FileType::Wav => ("WAV", true),
        FileType::Aiff => ("AIFF", true),
        FileType::Ape => ("APE", true),
        FileType::WavPack => ("WavPack", true),
        // lofty only reports a bit depth for ALAC inside MP4 containers.
        FileType::Mp4 if bits.is_some() => ("ALAC", true),
        FileType::Mp4 | FileType::Aac => ("AAC", false),
        FileType::Mpeg => ("MP3", false),
        FileType::Opus => ("Opus", false),
        FileType::Vorbis => ("Ogg Vorbis", false),
        FileType::Mpc => ("Musepack", false),
        FileType::Speex => ("Speex", false),
        FileType::Custom(name) => (name, false),
        _ => ("Audio", false),
    };
    Some(AudioQuality {
        codec: codec.to_string(),
        lossless,
        bits: if lossless { bits } else { None },
        sample_rate: props.sample_rate(),
        bitrate_kbps: props.audio_bitrate(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wav_quality() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("t.wav");
        // 24-bit mono 96 kHz, 0.1 s of silence.
        let (rate, bits, n) = (96_000u32, 24u16, 9_600u32);
        let data_len = n * 3;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 3).to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        b.resize(b.len() + data_len as usize, 0);
        std::fs::write(&wav, b).unwrap();
        let q = read(&wav).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(q.codec, "WAV");
        assert!(q.lossless && q.hi_res());
        assert_eq!(q.bits, Some(24));
        assert_eq!(q.sample_rate, Some(96_000));
    }
}
