use crate::commands::shazam::fingerprint::{FrequencyBand, Signature};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use byteorder::{LittleEndian, WriteBytesExt};
use crc32fast::Hasher;
use std::io::Cursor;
use std::io::Write;

const MAGIC1: u32 = 0xCAFE2580;
const MAGIC2: u32 = 0x94119C00;
const DATA_URI_PREFIX: &str = "data:audio/vnd.shazam.sig;base64,";
const SAMPLE_RATE_ID_16000: u32 = 3;

pub fn encode_to_binary(msg: &Signature) -> Result<Vec<u8>, std::io::Error> {
    let mut contents_buf = Vec::new();

    let bands = [
        FrequencyBand::Hz250To520,
        FrequencyBand::Hz520To1450,
        FrequencyBand::Hz1450To3500,
        FrequencyBand::Hz3500To5500,
    ];

    for band in bands.iter() {
        if let Some(peaks) = msg.frequency_band_to_sound_peaks.get(band) {
            if peaks.is_empty() {
                continue;
            }

            let mut peaks_buf = Vec::new();
            let mut fft_pass_number = 0;

            for peak in peaks {
                if peak.fft_pass_number < fft_pass_number {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Peaks must be sorted by FFT pass number",
                    ));
                }

                if peak.fft_pass_number - fft_pass_number >= 255 {
                    peaks_buf.write_u8(0xFF)?;
                    peaks_buf.write_u32::<LittleEndian>(peak.fft_pass_number as u32)?;
                    fft_pass_number = peak.fft_pass_number;
                }

                peaks_buf.write_u8((peak.fft_pass_number - fft_pass_number) as u8)?;
                peaks_buf.write_u16::<LittleEndian>(peak.peak_magnitude as u16)?;
                peaks_buf.write_u16::<LittleEndian>(peak.corrected_peak_frequency_bin as u16)?;

                fft_pass_number = peak.fft_pass_number;
            }

            let mut padding = (-(peaks_buf.len() as isize)) % 4;
            if padding < 0 {
                padding += 4;
            }

            contents_buf.write_u32::<LittleEndian>(0x60030040 + *band as i32 as u32)?;
            contents_buf.write_u32::<LittleEndian>(peaks_buf.len() as u32)?;
            contents_buf.write_all(&peaks_buf)?;
            for _ in 0..padding {
                contents_buf.write_u8(0x00)?;
            }
        }
    }

    let mut header = vec![0u8; 48];
    let mut header_cursor = Cursor::new(&mut header);

    header_cursor.write_u32::<LittleEndian>(MAGIC1)?;
    header_cursor.write_u32::<LittleEndian>(0)?; // CRC placeholder
    let size_minus_header = (contents_buf.len() + 8) as u32;
    header_cursor.write_u32::<LittleEndian>(size_minus_header)?;
    header_cursor.write_u32::<LittleEndian>(MAGIC2)?;
    header_cursor.write_all(&[0u8; 12])?; // void1
    let shifted_sample_rate = SAMPLE_RATE_ID_16000 << 27;
    header_cursor.write_u32::<LittleEndian>(shifted_sample_rate)?;
    header_cursor.write_all(&[0u8; 8])?; // void2
    let num_samples_plus_divided =
        msg.number_samples as u32 + (msg.sample_rate_hz as f64 * 0.24) as u32;
    header_cursor.write_u32::<LittleEndian>(num_samples_plus_divided)?;
    let fixed_value = (15 << 19) + 0x40000;
    header_cursor.write_u32::<LittleEndian>(fixed_value)?;

    let mut chunk_hdr = Vec::new();
    chunk_hdr.write_u32::<LittleEndian>(0x40000000)?;
    chunk_hdr.write_u32::<LittleEndian>((contents_buf.len() + 8) as u32)?;

    // Compute CRC
    let mut hasher = Hasher::new();
    hasher.update(&header[8..48]);
    hasher.update(&chunk_hdr);
    hasher.update(&contents_buf);
    let checksum = hasher.finalize();

    (&mut header[4..8]).write_u32::<LittleEndian>(checksum)?;

    let mut out_buf = Vec::new();
    out_buf.write_all(&header)?;
    out_buf.write_all(&chunk_hdr)?;
    out_buf.write_all(&contents_buf)?;

    Ok(out_buf)
}

pub fn encode_to_uri(msg: &Signature) -> Result<String, std::io::Error> {
    let binary = encode_to_binary(msg)?;
    let base64_str = STANDARD.encode(&binary);
    Ok(format!("{}{}", DATA_URI_PREFIX, base64_str))
}
