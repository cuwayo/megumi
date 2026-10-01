#[derive(Debug, Default)]
pub struct AudioOptions {
    pub start_offset_seconds: f64,
}

#[derive(Debug)]
struct WavHeader {
    pub num_channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub data_offset: usize,
    pub data_size: u32,
}

pub async fn load_wav_as_pcm(path: &str, opts: Option<&AudioOptions>) -> Result<Vec<i16>, String> {
    let data = tokio::fs::read(path)
        .await
        .map_err(|error| error.to_string())?;
    wav_bytes_as_pcm(&data, opts)
}

pub fn wav_bytes_as_pcm(data: &[u8], opts: Option<&AudioOptions>) -> Result<Vec<i16>, String> {
    let hdr = parse_wav_header(data)?;
    if hdr.bits_per_sample != 16 {
        return Err(format!(
            "Only 16-bit WAV is supported (got {}-bit)",
            hdr.bits_per_sample
        ));
    }

    let pcm_data_length = hdr.data_size as usize;
    let pcm_data_offset = hdr.data_offset;

    if data.len() < pcm_data_offset + pcm_data_length {
        return Err("WAV data size exceeds file length".into());
    }

    let num_samples = pcm_data_length / 2;
    let mut interleaved = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let idx = pcm_data_offset + i * 2;
        interleaved.push(i16::from_le_bytes([data[idx], data[idx + 1]]));
    }

    let nch = hdr.num_channels as usize;
    let mut mono = Vec::with_capacity(num_samples / nch);
    for i in 0..(num_samples / nch) {
        let mut sum: i64 = 0;
        for ch in 0..nch {
            sum += interleaved[i * nch + ch] as i64;
        }
        mono.push((sum / nch as i64) as i16);
    }

    if let Some(opts) = opts
        && opts.start_offset_seconds > 0.0
    {
        let skip = (opts.start_offset_seconds * hdr.sample_rate as f64) as usize;
        if skip >= mono.len() {
            return Err("Start offset exceeds audio duration".into());
        }
        mono = mono[skip..].to_vec();
    }

    if hdr.sample_rate != 16000 {
        mono = resample_linear(&mono, hdr.sample_rate as usize, 16000);
    }

    Ok(mono)
}

fn parse_wav_header(data: &[u8]) -> Result<WavHeader, String> {
    if data.len() < 44 {
        return Err("File too short to be a valid WAV".into());
    }

    let riff = std::str::from_utf8(&data[0..4]).map_err(|_| "Not a RIFF file")?;
    if riff != "RIFF" {
        return Err("Not a RIFF file".into());
    }

    let wave = std::str::from_utf8(&data[8..12]).map_err(|_| "Not a WAVE file")?;
    if wave != "WAVE" {
        return Err("Not a WAVE file".into());
    }

    let mut offset = 12;
    let mut hdr = WavHeader {
        num_channels: 0,
        sample_rate: 0,
        bits_per_sample: 0,
        data_offset: 0,
        data_size: 0,
    };

    while offset + 8 <= data.len() {
        let chunk_id = std::str::from_utf8(&data[offset..offset + 4]).unwrap_or("");
        let chunk_size = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap());
        offset += 8;

        if chunk_id == "fmt " {
            if chunk_size < 16 {
                return Err("fmt chunk too small".into());
            }
            let audio_format = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
            if audio_format != 1 {
                return Err(format!(
                    "Unsupported WAV format {} (only PCM=1 is supported)",
                    audio_format
                ));
            }

            hdr.num_channels = u16::from_le_bytes(data[offset + 2..offset + 4].try_into().unwrap());
            hdr.sample_rate = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap());
            hdr.bits_per_sample =
                u16::from_le_bytes(data[offset + 14..offset + 16].try_into().unwrap());
        } else if chunk_id == "data" {
            hdr.data_offset = offset;
            hdr.data_size = chunk_size;
            return Ok(hdr);
        }

        offset += chunk_size as usize;
        if chunk_size % 2 != 0 {
            offset += 1;
        }
    }

    Err("No 'data' chunk found in WAV file".into())
}

fn resample_linear(src: &[i16], src_rate: usize, dst_rate: usize) -> Vec<i16> {
    if src_rate == dst_rate {
        return src.to_vec();
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    let dst_len = (src.len() as f64 / ratio).round() as usize;
    let mut dst = Vec::with_capacity(dst_len);

    for i in 0..dst_len {
        let src_idx = i as f64 * ratio;
        let lo = src_idx as usize;
        let mut hi = lo + 1;
        if hi >= src.len() {
            hi = src.len() - 1;
        }

        let frac = src_idx - lo as f64;
        let v = src[lo] as f64 * (1.0 - frac) + src[hi] as f64 * frac;
        dst.push(v.round() as i16);
    }

    dst
}
