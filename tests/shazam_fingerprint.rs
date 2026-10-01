use megumi_whatsapp::commands::shazam::fingerprint::{
    BINS, FrequencyBand, HOP, SAMPLE_RATE_HZ, SignatureGenerator, TIME_OFFSETS, WINDOW, band_of,
    hanning_window, spread_bins,
};

/// A sine sweeping from `from_hz` to `to_hz` over `samples`, which is the
/// kind of signal the peak search is built for: a steady tone is a local
/// maximum in neither time nor frequency, so it leaves no peaks.
fn sweep(from_hz: f64, to_hz: f64, samples: usize) -> Vec<i16> {
    (0..samples)
        .map(|index| {
            let progress = index as f64 / samples as f64;
            let frequency = from_hz + (to_hz - from_hz) * progress;
            let seconds = index as f64 / f64::from(SAMPLE_RATE_HZ);
            let sample = (2.0 * std::f64::consts::PI * frequency * seconds).sin() * 0.5 * 32767.0;
            sample as i16
        })
        .collect()
}

#[test]
fn a_sweep_leaves_peaks_in_the_bands_it_covers() {
    let samples = sweep(400.0, 2000.0, SAMPLE_RATE_HZ as usize);
    let mut generator = SignatureGenerator::new();
    generator.feed_input(&samples);
    let signature = generator
        .get_next_signature()
        .expect("a second of audio is a signature");

    assert_eq!(signature.number_samples, samples.len() as i32);
    assert_eq!(signature.sample_rate_hz, SAMPLE_RATE_HZ as i32);
    assert!(signature.peaks() > 0, "the sweep should leave peaks");
    for band in signature.frequency_band_to_sound_peaks.keys() {
        assert!(
            *band != FrequencyBand::Hz3500To5500,
            "a sweep below 2 kHz must not reach the top band"
        );
    }
}

#[test]
fn a_clip_shorter_than_one_hop_has_no_signature() {
    let mut generator = SignatureGenerator::new();
    generator.feed_input(&[0; HOP - 1]);
    assert!(generator.get_next_signature().is_none());
}

#[test]
fn the_time_offsets_are_the_spans_the_original_compares() {
    let mut offsets = vec![53, 45];
    offsets.extend((165..201).step_by(7).map(|offset| 256 - offset));
    offsets.extend((214..250).step_by(7).map(|offset| 256 - offset));
    assert_eq!(TIME_OFFSETS.to_vec(), offsets);
}

#[test]
fn smearing_raises_a_bin_to_its_highest_neighbour() {
    let mut spectrum = [0.0; BINS];
    spectrum[5] = 4.0;
    let spread = spread_bins(&spectrum);

    assert_eq!(&spread[3..6], &[4.0, 4.0, 4.0]);
    assert_eq!(spread[2], 0.0);
    assert_eq!(spread[6], 0.0);
}

#[test]
fn the_hanning_window_peaks_in_the_middle_and_is_not_zero_edged() {
    let window = hanning_window();
    let (peak, value) = window
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.total_cmp(right.1))
        .unwrap();

    assert_eq!(peak, WINDOW / 2);
    // The midpoint sample sits half a step off the window's true peak,
    // because WINDOW is even while the denominator is 2049: the closest
    // value is `0.5 * (1 + cos(pi / 2049))`, short of 1.0 by ~5.9e-7.
    // Pinning it also catches an off-by-one denominator, which a loose
    // tolerance would not (that shifts the peak by only ~5.7e-10).
    let expected_peak = 0.5 * (1.0 + (std::f64::consts::PI / 2049.0).cos());
    assert!(
        (value - expected_peak).abs() < 1e-12,
        "{value} vs {expected_peak}"
    );
    assert!(
        window[0] > 0.0,
        "the leading zero of the source window is dropped"
    );
    assert!((window[0] - window[WINDOW - 1]).abs() < 1e-12);
}

#[test]
fn bands_leave_gaps_and_close_the_top_one() {
    assert_eq!(band_of(400.0), Some(FrequencyBand::Hz250To520));
    assert_eq!(band_of(1000.0), Some(FrequencyBand::Hz520To1450));
    assert_eq!(band_of(2000.0), Some(FrequencyBand::Hz1450To3500));
    assert_eq!(band_of(5500.0), Some(FrequencyBand::Hz3500To5500));

    assert_eq!(band_of(250.0), None);
    assert_eq!(band_of(520.0), None);
    assert_eq!(band_of(5500.1), None);
}
