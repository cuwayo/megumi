//! The local half of `!shazam`: PCM in, a fingerprint out.
//!
//! This is the algorithm the open-source Shazam clients share — [SongRec] and
//! [shazamio] — reverse engineered from the service's own signatures. Every 128
//! samples a Hanning-windowed 2048-sample FFT is taken, each spectrum is smeared
//! across its neighbouring bins and folded into the passes before it, and the
//! local maxima that survive both are kept as peaks in four frequency bands.
//! `signature` then writes those peaks in the format the service matches.
//!
//! [SongRec]: https://github.com/marin-m/SongRec
//! [shazamio]: https://github.com/shazamio/ShazamIO

use std::collections::HashMap;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// The rate a signature is defined at: 16 kHz, one channel, signed 16-bit, which
/// is what `audio` decodes every clip down to.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Samples per FFT window, and the hop between windows.
pub const WINDOW: usize = 2048;
pub const HOP: usize = 128;
/// Bins in a real FFT of one window, including the Nyquist bin.
pub const BINS: usize = WINDOW / 2 + 1;
/// Passes kept in the rings. The peak search reaches 49 passes back, and 256 is
/// the next round power of two above it.
const PASSES: usize = 256;
/// How far back a peak candidate is compared against the raw spectra, and
/// against the smeared ones.
const SPECTRUM_PASS: usize = 46;
const SPREAD_PASS: usize = 49;
/// The magnitude a bin must reach to be a peak candidate at all, and the floor
/// spectra are clamped to before they are scaled.
const MIN_MAGNITUDE: f64 = 1.0 / 64.0;
const MAGNITUDE_FLOOR: f64 = 1e-10;
/// The scale stored peak magnitudes are in.
const MAGNITUDE_SCALE: f64 = 1477.3;
const MAGNITUDE_OFFSET: f64 = 6144.0;
/// How much audio, and how many peaks, a signature holds before it is handed
/// over. A longer signature is a stronger one, and twelve seconds is well inside
/// what the service accepts.
const MAX_TIME_SECONDS: f64 = 12.0;
const MAX_PEAKS: usize = 255;

/// The bins a peak candidate is compared against, in the same pass.
const FREQUENCY_OFFSETS: [i32; 8] = [-10, -7, -4, -3, 1, 2, 5, 8];

/// The other passes a peak candidate is compared against, as how many passes
/// back they were written: the two immediately before it, then every seventh
/// pass from 165 to 200 and from 214 to 249, which is roughly one and two
/// seconds behind it.
pub const TIME_OFFSETS: [usize; 14] = [53, 45, 91, 84, 77, 70, 63, 56, 42, 35, 28, 21, 14, 7];

/// The bands peaks are filed under, by the frequencies each covers. The values
/// are the band ids the signature format writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrequencyBand {
    Hz250To520 = 0,
    Hz520To1450 = 1,
    Hz1450To3500 = 2,
    Hz3500To5500 = 3,
}

/// One peak of a fingerprint: the pass it was heard in, its magnitude, and the
/// FFT bin its frequency falls in, scaled by 64 so the fractional part the
/// interpolation produces survives.
pub struct FrequencyPeak {
    pub fft_pass_number: i32,
    pub peak_magnitude: i32,
    pub corrected_peak_frequency_bin: i32,
}

/// A fingerprint: the peaks heard, band by band, and how many 16 kHz samples
/// went into it.
pub struct Signature {
    pub sample_rate_hz: i32,
    pub number_samples: i32,
    pub frequency_band_to_sound_peaks: HashMap<FrequencyBand, Vec<FrequencyPeak>>,
}

impl Signature {
    fn new() -> Self {
        Self {
            sample_rate_hz: SAMPLE_RATE_HZ as i32,
            number_samples: 0,
            frequency_band_to_sound_peaks: HashMap::new(),
        }
    }

    /// How many peaks the signature holds across its bands.
    pub fn peaks(&self) -> usize {
        self.frequency_band_to_sound_peaks
            .values()
            .map(Vec::len)
            .sum()
    }
}

/// Builds a fingerprint from queued samples.
pub struct SignatureGenerator {
    /// Samples fed in, and how many of them have been consumed.
    pending: Vec<i16>,
    consumed: usize,
    /// The last `WINDOW` samples, the spectra of the last `PASSES` windows, and
    /// those spectra smeared across neighbouring bins.
    samples: RingBuffer<f64>,
    spectra: RingBuffer<[f64; BINS]>,
    spread: RingBuffer<[f64; BINS]>,
    /// Passes written so far, which numbers the peaks.
    passes: usize,
    signature: Signature,
    hanning: [f64; WINDOW],
    fft: Arc<dyn Fft<f64>>,
}

impl SignatureGenerator {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            consumed: 0,
            samples: RingBuffer::new(WINDOW, 0.0),
            spectra: RingBuffer::new(PASSES, [0.0; BINS]),
            spread: RingBuffer::new(PASSES, [0.0; BINS]),
            passes: 0,
            signature: Signature::new(),
            hanning: hanning_window(),
            // Planning once and reusing the plan matters: twelve seconds of
            // audio runs this transform 1500 times.
            fft: FftPlanner::new().plan_fft_forward(WINDOW),
        }
    }

    /// Queues signed 16-bit, 16 kHz mono samples.
    pub fn feed_input(&mut self, samples: &[i16]) {
        self.pending.extend_from_slice(samples);
    }

    /// Consumes queued samples and returns the fingerprint so far, or `None`
    /// while fewer than one hop is queued.
    pub fn get_next_signature(&mut self) -> Option<Signature> {
        if self.pending.len() - self.consumed < HOP {
            return None;
        }

        while self.pending.len() - self.consumed >= HOP && !self.long_enough() {
            let batch = self.pending[self.consumed..self.consumed + HOP].to_vec();
            self.process(&batch);
            self.consumed += HOP;
        }

        let signature = std::mem::replace(&mut self.signature, Signature::new());
        self.samples = RingBuffer::new(WINDOW, 0.0);
        self.spectra = RingBuffer::new(PASSES, [0.0; BINS]);
        self.spread = RingBuffer::new(PASSES, [0.0; BINS]);
        self.passes = 0;
        self.pending.drain(..self.consumed);
        self.consumed = 0;
        Some(signature)
    }

    /// Whether the fingerprint covers enough audio and holds enough peaks to be
    /// worth asking about.
    fn long_enough(&self) -> bool {
        let seconds =
            self.signature.number_samples as f64 / f64::from(self.signature.sample_rate_hz);
        seconds >= MAX_TIME_SECONDS && self.signature.peaks() >= MAX_PEAKS
    }

    fn process(&mut self, samples: &[i16]) {
        self.signature.number_samples += samples.len() as i32;
        for batch in samples.chunks(HOP) {
            self.take_spectrum(batch);
            self.spread_into_passes();
            self.find_peaks();
        }
    }

    /// One windowed FFT of the last `WINDOW` samples, kept as magnitudes.
    fn take_spectrum(&mut self, batch: &[i16]) {
        for sample in batch {
            self.samples.push(f64::from(*sample));
        }
        self.passes += 1;

        // The window is read oldest first: `back(WINDOW)` is the sample about to
        // be overwritten, `back(1)` the one just written.
        let mut buffer: Vec<Complex<f64>> = (0..WINDOW)
            .map(|index| Complex::new(self.hanning[index] * self.samples.back(WINDOW - index), 0.0))
            .collect();
        self.fft.process(&mut buffer);

        let mut spectrum = [0.0; BINS];
        for (bin, value) in spectrum.iter_mut().enumerate() {
            let squared = buffer[bin].re * buffer[bin].re + buffer[bin].im * buffer[bin].im;
            *value = (squared / f64::from(1u32 << 17)).max(MAGNITUDE_FLOOR);
        }
        self.spectra.push(spectrum);
    }

    /// Smears each spectrum across its neighbouring bins, then folds the result
    /// into the passes just before it, so a peak that is loud in any nearby bin
    /// or pass still reads as one peak.
    fn spread_into_passes(&mut self) {
        let spread = spread_bins(self.spectra.back(1));
        let mut carried = spread;
        for ago in [1, 3, 6] {
            let window = self.spread.back_mut(ago);
            for (value, smeared) in window.iter_mut().zip(&carried) {
                *value = value.max(*smeared);
            }
            carried = *window;
        }
        self.spread.push(spread);
    }

    /// Files every bin that is a local maximum in both frequency and time as a
    /// peak of the fingerprint being built.
    fn find_peaks(&mut self) {
        if self.passes < SPECTRUM_PASS {
            return;
        }

        // Copied out of the rings, so the peaks can be pushed as they are found.
        let spectrum = *self.spectra.back(SPECTRUM_PASS);
        let spread = *self.spread.back(SPREAD_PASS);

        for bin in 10..BINS - 10 {
            let magnitude = spectrum[bin];
            if magnitude < MIN_MAGNITUDE || magnitude < spread[bin - 1] {
                continue;
            }

            let neighbour = FREQUENCY_OFFSETS
                .iter()
                .fold(0.0f64, |highest: f64, offset| {
                    highest.max(spread[(bin as i32 + offset) as usize])
                });
            if magnitude <= neighbour {
                continue;
            }

            let neighbour = TIME_OFFSETS.iter().fold(neighbour, |highest: f64, ago| {
                highest.max(self.spread.back(*ago)[bin - 1])
            });
            if magnitude <= neighbour {
                continue;
            }

            let before = scaled_magnitude(spectrum[bin - 1]);
            let at = scaled_magnitude(magnitude);
            let after = scaled_magnitude(spectrum[bin + 1]);

            // The peak sits between bins: the parabola through the three scaled
            // magnitudes opens downwards, and where it turns is the slope over
            // the curvature. The 64x scale is what the format stores.
            let curvature = at * 2.0 - before - after;
            if curvature <= 0.0 {
                continue;
            }
            let corrected_bin = bin as f64 * 64.0 + (after - before) * 32.0 / curvature;
            let frequency = corrected_bin * (f64::from(SAMPLE_RATE_HZ) / 2.0 / 1024.0 / 64.0);

            let Some(band) = band_of(frequency) else {
                continue;
            };

            self.signature
                .frequency_band_to_sound_peaks
                .entry(band)
                .or_default()
                .push(FrequencyPeak {
                    fft_pass_number: (self.passes - SPECTRUM_PASS) as i32,
                    peak_magnitude: at as i32,
                    corrected_peak_frequency_bin: corrected_bin as i32,
                });
        }
    }
}

impl Default for SignatureGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// The band a frequency is filed under. The bands leave gaps between them, and
/// the highest one is closed at its top, as in the implementations this follows.
pub fn band_of(frequency_hz: f64) -> Option<FrequencyBand> {
    if frequency_hz > 250.0 && frequency_hz < 520.0 {
        Some(FrequencyBand::Hz250To520)
    } else if frequency_hz > 520.0 && frequency_hz < 1450.0 {
        Some(FrequencyBand::Hz520To1450)
    } else if frequency_hz > 1450.0 && frequency_hz < 3500.0 {
        Some(FrequencyBand::Hz1450To3500)
    } else if frequency_hz > 3500.0 && frequency_hz <= 5500.0 {
        Some(FrequencyBand::Hz3500To5500)
    } else {
        None
    }
}

/// The logarithmic scale the format stores magnitudes in.
fn scaled_magnitude(magnitude: f64) -> f64 {
    magnitude.max(MIN_MAGNITUDE).ln() * MAGNITUDE_SCALE + MAGNITUDE_OFFSET
}

/// A spectrum with every bin raised to the largest of it and the two bins above
/// it. The three highest bins have no neighbours to borrow from and are kept as
/// they are.
pub fn spread_bins(spectrum: &[f64; BINS]) -> [f64; BINS] {
    let mut spread = [0.0; BINS];
    for (bin, value) in spread.iter_mut().enumerate().take(BINS - 3) {
        *value = spectrum[bin].max(spectrum[bin + 1]).max(spectrum[bin + 2]);
    }
    spread[BINS - 3..].copy_from_slice(&spectrum[BINS - 3..]);
    spread
}

/// The window applied before each FFT. The implementations this follows build it
/// as `numpy.hanning(2050)[1:-1]`, which drops the two zeroes at the ends of a
/// 2050-sample window: the values are index 1 upwards, so the denominator is
/// 2049 and the endpoints are not zero.
pub fn hanning_window() -> [f64; WINDOW] {
    std::array::from_fn(|index| {
        let i = (index + 1) as f64;
        0.5 * (1.0 - (2.0 * std::f64::consts::PI * i / 2049.0).cos())
    })
}

/// A fixed-size ring of slots written in order, tracking where the next write
/// goes. Slots are read by how many writes back they were, which is how the
/// algorithm refers to the audio and spectra it compares against.
struct RingBuffer<T> {
    slots: Vec<T>,
    position: usize,
}

impl<T: Clone> RingBuffer<T> {
    fn new(slots: usize, fill: T) -> Self {
        Self {
            slots: vec![fill; slots],
            position: 0,
        }
    }

    fn push(&mut self, value: T) {
        self.slots[self.position] = value;
        self.position = (self.position + 1) % self.slots.len();
    }

    /// The slot written `ago` writes ago, where 1 is the most recent write.
    fn back(&self, ago: usize) -> &T {
        let slots = self.slots.len();
        &self.slots[(self.position + slots - ago % slots) % slots]
    }

    fn back_mut(&mut self, ago: usize) -> &mut T {
        let slots = self.slots.len();
        &mut self.slots[(self.position + slots - ago % slots) % slots]
    }
}
