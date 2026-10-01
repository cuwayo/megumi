use crate::shazam::models::{DecodedMessage, FrequencyBand, FrequencyPeak};
use rustfft::{FftPlanner, num_complex::Complex};

pub struct RingBuffer {
    pub data: Vec<f64>,
    pub position: usize,
    pub buffer_size: usize,
    pub num_written: usize,
}

impl RingBuffer {
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0.0; size],
            position: 0,
            buffer_size: size,
            num_written: 0,
        }
    }

    pub fn append(&mut self, v: f64) {
        self.data[self.position] = v;
        self.position = (self.position + 1) % self.buffer_size;
        self.num_written += 1;
    }
}

pub struct FftRingBuffer {
    pub data: Vec<Vec<f64>>,
    pub position: usize,
    pub buffer_size: usize,
    pub num_written: usize,
}

impl FftRingBuffer {
    pub fn new(size: usize, slice_len: usize) -> Self {
        Self {
            data: vec![vec![0.0; slice_len]; size],
            position: 0,
            buffer_size: size,
            num_written: 0,
        }
    }

    pub fn append(&mut self, v: &[f64]) {
        self.data[self.position].copy_from_slice(v);
        self.position = (self.position + 1) % self.buffer_size;
        self.num_written += 1;
    }
}

pub struct SignatureGenerator {
    input_pending_processing: Vec<i16>,
    samples_processed: usize,
    ring_buffer_of_samples: RingBuffer,
    fft_outputs: FftRingBuffer,
    spread_fft_output: FftRingBuffer,
    pub max_time_seconds: f64,
    pub max_peaks: usize,
    next_signature: DecodedMessage,
    hanning_window: Vec<f64>,
}

impl SignatureGenerator {
    pub fn new() -> Self {
        let mut hanning_window = vec![0.0; 2048];
        let n = 2050;
        for i in 1..(n - 1) {
            hanning_window[i - 1] =
                0.5 * (1.0 - f64::cos(2.0 * std::f64::consts::PI * i as f64 / (n as f64 - 1.0)));
        }

        Self {
            input_pending_processing: Vec::new(),
            samples_processed: 0,
            ring_buffer_of_samples: RingBuffer::new(2048),
            fft_outputs: FftRingBuffer::new(256, 1025),
            spread_fft_output: FftRingBuffer::new(256, 1025),
            max_time_seconds: 12.0,
            max_peaks: 255,
            next_signature: DecodedMessage::new(),
            hanning_window,
        }
    }

    fn reset_buffers(&mut self) {
        self.ring_buffer_of_samples = RingBuffer::new(2048);
        self.fft_outputs = FftRingBuffer::new(256, 1025);
        self.spread_fft_output = FftRingBuffer::new(256, 1025);
    }

    fn reset_signature(&mut self) {
        self.next_signature = DecodedMessage::new();
    }

    pub fn feed_input(&mut self, samples: &[i16]) {
        self.input_pending_processing.extend_from_slice(samples);
    }

    pub fn get_next_signature(&mut self) -> Option<DecodedMessage> {
        let mut remaining = self.input_pending_processing.len() - self.samples_processed;
        if remaining < 128 {
            return None;
        }

        loop {
            remaining = self.input_pending_processing.len() - self.samples_processed;
            if remaining < 128 {
                break;
            }

            let duration_secs = self.next_signature.number_samples as f64
                / self.next_signature.sample_rate_hz as f64;
            let mut total_peaks = 0;
            for peaks in self.next_signature.frequency_band_to_sound_peaks.values() {
                total_peaks += peaks.len();
            }

            if duration_secs >= self.max_time_seconds && total_peaks >= self.max_peaks {
                break;
            }

            let batch = self.input_pending_processing
                [self.samples_processed..self.samples_processed + 128]
                .to_vec();
            self.process_input(&batch);
            self.samples_processed += 128;
        }

        let result = self.next_signature.clone();
        self.reset_buffers();
        self.reset_signature();

        self.input_pending_processing
            .drain(0..self.samples_processed);
        self.samples_processed = 0;

        Some(result)
    }

    fn process_input(&mut self, samples: &[i16]) {
        self.next_signature.number_samples += samples.len() as i32;
        let mut offset = 0;
        while offset < samples.len() {
            let mut end = offset + 128;
            if end > samples.len() {
                end = samples.len();
            }

            let batch = &samples[offset..end];
            self.do_fft(batch);
            self.do_peak_spreading_and_recognition();
            offset += 128;
        }
    }

    fn do_fft(&mut self, batch: &[i16]) {
        for s in batch {
            self.ring_buffer_of_samples.append(*s as f64);
        }

        let mut excerpt = [0.0; 2048];
        let pos = self.ring_buffer_of_samples.position;
        for (i, sample) in excerpt.iter_mut().enumerate() {
            *sample = self.ring_buffer_of_samples.data[(pos + i) % 2048];
        }

        let mut windowed = [0.0; 2048];
        for i in 0..2048 {
            windowed[i] = self.hanning_window[i] * excerpt[i];
        }

        let freqs = rfft(&windowed);
        let mut fft_result = [0.0; 1025];
        for i in 0..1025 {
            let r = freqs[i].re;
            let im = freqs[i].im;
            let mut v = (r * r + im * im) / (1 << 17) as f64;
            if v < 1e-10 {
                v = 1e-10;
            }
            fft_result[i] = v;
        }

        self.fft_outputs.append(&fft_result);
    }

    fn do_peak_spreading_and_recognition(&mut self) {
        self.do_peak_spreading();
        if self.spread_fft_output.num_written >= 46 {
            self.do_peak_recognition();
        }
    }

    fn do_peak_spreading(&mut self) {
        let fo = &self.fft_outputs;
        let so = &mut self.spread_fft_output;

        let last_fft_idx = (fo.position + fo.buffer_size - 1) % fo.buffer_size;
        let origin_last_fft = &fo.data[last_fft_idx];

        let mut spread = [0.0; 1025];
        for i in 0..1022 {
            let mut v = origin_last_fft[i];
            if origin_last_fft[i + 1] > v {
                v = origin_last_fft[i + 1];
            }
            if origin_last_fft[i + 2] > v {
                v = origin_last_fft[i + 2];
            }
            spread[i] = v;
        }
        spread[1022] = origin_last_fft[1022];
        spread[1023] = origin_last_fft[1023];
        spread[1024] = origin_last_fft[1024];

        let i1 = (so.position + so.buffer_size - 1) % so.buffer_size;
        let i2 = (so.position + so.buffer_size - 3) % so.buffer_size;
        let i3 = (so.position + so.buffer_size - 6) % so.buffer_size;

        for (k, spread_value) in spread.iter().copied().enumerate() {
            let mut v1 = so.data[i1][k];
            if spread_value > v1 {
                v1 = spread_value;
            }
            so.data[i1][k] = v1;

            let mut v2 = so.data[i2][k];
            if v1 > v2 {
                v2 = v1;
            }
            so.data[i2][k] = v2;

            let mut v3 = so.data[i3][k];
            if v2 > v3 {
                v3 = v2;
            }
            so.data[i3][k] = v3;
        }

        so.append(&spread);
    }

    fn do_peak_recognition(&mut self) {
        let fo = &self.fft_outputs;
        let so = &self.spread_fft_output;

        let fft_minus_46 = &fo.data[(fo.position + fo.buffer_size - 46) % fo.buffer_size];
        let fft_minus_49 = &so.data[(so.position + so.buffer_size - 49) % so.buffer_size];

        for bin_pos in 10..1015 {
            if fft_minus_46[bin_pos] < 1.0 / 64.0 {
                continue;
            }
            if fft_minus_46[bin_pos] < fft_minus_49[bin_pos - 1] {
                continue;
            }

            let mut max_neighbour = 0.0;
            let freqs_offsets: [i32; 7] = [-10, -7, -4, -3, 1, 4, 7];
            for off in freqs_offsets {
                let nb = fft_minus_49[(bin_pos as i32 + off) as usize];
                if nb > max_neighbour {
                    max_neighbour = nb;
                }
            }
            if fft_minus_46[bin_pos] <= max_neighbour {
                continue;
            }

            let mut max_neighbour_time = max_neighbour;
            let time_offsets = build_time_offsets();
            for off in time_offsets {
                let idx = (so.position as i32 + off + (so.buffer_size as i32 * 16)) as usize
                    % so.buffer_size;
                let nb = so.data[idx][bin_pos - 1];
                if nb > max_neighbour_time {
                    max_neighbour_time = nb;
                }
            }
            if fft_minus_46[bin_pos] <= max_neighbour_time {
                continue;
            }

            let fft_pass_number = so.num_written - 46;

            let mag = f64::ln(f64::max(1.0 / 64.0, fft_minus_46[bin_pos])) * 1477.3 + 6144.0;
            let mag_before =
                f64::ln(f64::max(1.0 / 64.0, fft_minus_46[bin_pos - 1])) * 1477.3 + 6144.0;
            let mag_after =
                f64::ln(f64::max(1.0 / 64.0, fft_minus_46[bin_pos + 1])) * 1477.3 + 6144.0;

            let peak_var1 = mag * 2.0 - mag_before - mag_after;
            if peak_var1 <= 0.0 {
                continue;
            }
            let peak_var2 = (mag_after - mag_before) * 32.0 / peak_var1;

            let corrected_bin = bin_pos as f64 * 64.0 + peak_var2;
            let freq_hz = corrected_bin * (16000.0 / 2.0 / 1024.0 / 64.0);

            let band = if freq_hz > 250.0 && freq_hz < 520.0 {
                FrequencyBand::FreqBandHz250520
            } else if freq_hz > 520.0 && freq_hz < 1450.0 {
                FrequencyBand::FreqBandHz5201450
            } else if freq_hz > 1450.0 && freq_hz < 3500.0 {
                FrequencyBand::FreqBandHz14503500
            } else if freq_hz > 3500.0 && freq_hz <= 5500.0 {
                FrequencyBand::FreqBandHz35005500
            } else {
                continue;
            };

            self.next_signature
                .frequency_band_to_sound_peaks
                .entry(band)
                .or_default()
                .push(FrequencyPeak {
                    fft_pass_number: fft_pass_number as i32,
                    peak_magnitude: mag as i32,
                    corrected_peak_frequency_bin: corrected_bin as i32,
                    sample_rate_hz: 16000,
                });
        }
    }
}

impl Default for SignatureGenerator {
    fn default() -> Self {
        Self::new()
    }
}

fn build_time_offsets() -> Vec<i32> {
    let mut offsets = vec![-53, -45];
    let mut v = 165;
    while v < 201 {
        offsets.push(v);
        v += 7;
    }
    v = 214;
    while v < 250 {
        offsets.push(v);
        v += 7;
    }
    offsets
}

fn rfft(input: &[f64]) -> Vec<Complex<f64>> {
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(input.len());
    let mut buffer: Vec<Complex<f64>> = input.iter().map(|&x| Complex::new(x, 0.0)).collect();
    fft.process(&mut buffer);
    buffer.into_iter().take(input.len() / 2 + 1).collect()
}
