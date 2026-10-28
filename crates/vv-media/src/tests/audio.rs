use super::*;

/// Stereo PCM in MKV has an "unknown" channel layout: it used to fail
/// with "Input changed".
#[test]
fn decode_audio_track_handles_unspecified_channel_layout() {
    let dir = std::env::temp_dir().join(format!("vv-media-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pcm_unknown_layout.mkv");

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
        ],
        &path,
    );

    let audio = decode_audio_track(&path, 0)
        .unwrap()
        .expect("audio expected");
    assert_eq!(audio.channels, 2);
    assert_eq!(audio.samples.len(), 48_000 * 2);
}

#[test]
fn decode_audio_track_reads_correct_length_and_is_not_silent() {
    let dir = std::env::temp_dir().join(format!("vv-media-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sample.mp4");

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-c:a",
            "aac",
        ],
        &path,
    );

    let audio = decode_audio_track(&path, 0)
        .unwrap()
        .expect("audio expected");
    assert_eq!(audio.sample_rate, 48000);
    assert_eq!(audio.channels, 1);

    let total_frames = audio.samples.len() / audio.channels as usize;
    // ~1s at 48kHz: the AAC encoder adds some priming/padding.
    assert!(
        (45_000..=52_000).contains(&total_frames),
        "total_frames={total_frames}"
    );

    // A 440Hz sine is not silent: the peak must be well above 0.
    let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
    assert!(peak > 0.1, "peak={peak}, expected a non-silent signal");
}

#[test]
fn streaming_decode_resamples_continuously_in_many_chunks() {
    let dir = std::env::temp_dir().join(format!("vv-media-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streaming_44k.wav");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=2",
        ],
        &path,
    );

    let mut chunks = 0;
    let mut samples = Vec::new();
    let formats =
        decode_audio_streams_streaming(&path, &[0], Some(48_000), |_, channels, chunk| {
            assert_eq!(channels, 1);
            chunks += 1;
            samples.extend_from_slice(chunk);
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(formats, vec![Some((48_000, 1))]);
    assert!(chunks > 1, "chunks={chunks}");
    assert!(
        (samples.len() as i64 - 96_000).abs() < 100,
        "len={}",
        samples.len()
    );
    // A continuous sine: no jumps between samples near the chunk boundaries.
    let max_step = samples
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    assert!(max_step < 0.1, "max_step={max_step}");
}

#[test]
fn streaming_decode_stops_when_the_callback_breaks() {
    let dir = std::env::temp_dir().join(format!("vv-media-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streaming_break.wav");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2",
        ],
        &path,
    );

    let mut chunks = 0;
    decode_audio_streams_streaming(&path, &[0], None, |_, _, _| {
        chunks += 1;
        ControlFlow::Break(())
    })
    .unwrap();
    assert_eq!(chunks, 1);
}

#[test]
fn streaming_decode_delivers_every_requested_stream_in_one_pass() {
    let dir = std::env::temp_dir().join(format!("vv-media-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streaming_three.mkv");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=44100:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=3",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:a",
            "aac",
        ],
        &path,
    );

    let mut order = Vec::new();
    let mut lengths = [0usize; 3];
    let formats =
        decode_audio_streams_streaming(&path, &[2, 0, 5, 1], Some(48_000), |slot, _, chunk| {
            order.push(slot);
            let i = [0, 1, usize::MAX, 2][slot];
            lengths[i] += chunk.len();
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(
        formats,
        vec![
            Some((48_000, 1)),
            Some((48_000, 1)),
            None,
            Some((48_000, 1))
        ]
    );
    for len in lengths {
        assert!((len as i64 - 144_000).abs() < 3_000, "{lengths:?}");
    }
    // Interleaved: the first chunk of each stream arrives well before the end.
    let first_of = |slot| order.iter().position(|&s| s == slot).unwrap();
    assert!(
        [0, 1, 3].iter().all(|&s| first_of(s) < order.len() / 4),
        "{order:?}"
    );
}

/// A file with *two* audio streams (real case: stereo mix + separate
/// 5.1) must be able to decode one or the other based on
/// `stream_index`, not always "the best one" according to ffmpeg — here
/// they are told apart by sample_rate (44100 vs 48000) to check it
/// without spectral analysis.
#[test]
fn decode_audio_track_selects_the_requested_stream_index_not_just_the_best() {
    let dir = std::env::temp_dir().join(format!("vv-media-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("two_streams.mp4");

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=1",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-c:a",
            "aac",
        ],
        &path,
    );

    let first = decode_audio_track(&path, 0)
        .unwrap()
        .expect("stream 0 expected");
    assert_eq!(first.sample_rate, 44100);

    let second = decode_audio_track(&path, 1)
        .unwrap()
        .expect("stream 1 expected");
    assert_eq!(second.sample_rate, 48000);

    assert!(
        decode_audio_track(&path, 2).unwrap().is_none(),
        "no audio stream at index 2"
    );
}

/// 20 s of seeded noise: unlike a tone, a shift of one sample shows.
const NOISE: &str = "anoisesrc=duration=20:color=white:amplitude=0.5:seed=7:sample_rate=48000";

/// A sweep, never periodic either: lossy codecs fill noise with noise of
/// their own (AAC's PNS), different after a seek.
const CHIRP: &str = "aevalsrc=0.5*sin(2*PI*(100+300*t)*t):duration=20:sample_rate=48000";

fn test_file(
    name: &str,
    source: &str,
    output_args: &[&str],
    with_video: bool,
) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("vv-media-seek-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut args = vec!["-f", "lavfi", "-i", source];
    if with_video {
        args.extend([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=20:size=320x240:rate=30",
            "-map",
            "1:v",
            "-map",
            "0:a",
            "-c:v",
            "libx264",
            "-g",
            "120",
        ]);
    }
    args.extend(["-ac", "2"]);
    args.extend(output_args);
    crate::test_support::ffmpeg(&args, &path);
    path
}

/// The frames `from..from + len` of stream 0 at 48 kHz stereo, by seeking.
fn seeked(path: &std::path::Path, from: u64, len: u64) -> Vec<f32> {
    let mut out = vec![f32::NAN; len as usize * 2];
    decode_audio_streams_from(path, &[0], 48_000, from, |_, channels, at, chunk| {
        assert_eq!(channels, 2);
        for (i, frame) in chunk.as_chunks::<2>().0.iter().enumerate() {
            let pos = at + i as u64;
            if (from..from + len).contains(&pos) {
                let o = (pos - from) as usize * 2;
                out[o..o + 2].copy_from_slice(frame);
            }
        }
        if at >= from + len {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .unwrap();
    out
}

/// Stream 0 decoded from the start, each sample where `at` puts it: NaN
/// where nothing plays (a gap in the timestamps).
fn placed_full_decode(path: &std::path::Path) -> Vec<f32> {
    let mut full = Vec::new();
    decode_audio_streams_from(path, &[0], 48_000, 0, |_, _, at, chunk| {
        let start = at as usize * 2;
        if full.len() < start + chunk.len() {
            full.resize(start + chunk.len(), f32::NAN);
        }
        full[start..start + chunk.len()].copy_from_slice(chunk);
        ControlFlow::Continue(())
    })
    .unwrap();
    full
}

/// Inside the preroll, past a seek, and up to the end of the file.
const SEEK_OFFSETS: [u64; 4] = [300, 48_000 * 7 + 123, 48_000 * 13 + 7, 48_000 * 19 + 5];

fn assert_seek_matches_full_decode(path: &std::path::Path, tolerance: f32) {
    assert_seeks_match_full_decode(path, tolerance, &SEEK_OFFSETS);
}

fn assert_seeks_match_full_decode(path: &std::path::Path, tolerance: f32, offsets: &[u64]) {
    let full = placed_full_decode(path);
    let mut failures = Vec::new();
    for &from in offsets {
        let len = 48_000.min(full.len() as u64 / 2 - from);
        let got = seeked(path, from, len);
        let want = &full[from as usize * 2..(from + len) as usize * 2];
        let worst = got
            .iter()
            .zip(want)
            .fold(0.0f32, |m, (g, w)| match (g.is_nan(), w.is_nan()) {
                (true, true) => m,
                (false, false) => m.max((g - w).abs()),
                _ => f32::INFINITY,
            });
        if worst > tolerance {
            let at = got
                .iter()
                .zip(want)
                .position(|(g, w)| (g - w).abs() == worst)
                .map_or(from, |index| from + index as u64 / 2);
            failures.push(format!(
                "from frame {from} the seeked decode differs by {worst} at frame {at}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}: {}",
        path.display(),
        failures.join("; ")
    );
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_pcm() {
    let path = test_file("pcm.wav", NOISE, &["-c:a", "pcm_s16le"], false);
    assert_seek_matches_full_decode(&path, 0.0);
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_pcm_next_to_h264() {
    let path = test_file("obs.mkv", NOISE, &["-c:a", "pcm_s16le"], true);
    assert_seek_matches_full_decode(&path, 0.0);
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_flac() {
    let path = test_file("noise.flac", NOISE, &["-c:a", "flac"], false);
    assert_seek_matches_full_decode(&path, 0.0);
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_aac() {
    let path = test_file("aac.mp4", CHIRP, &["-c:a", "aac"], true);
    assert_seek_matches_full_decode(&path, 1e-4);
}

#[test]
fn a_seeked_decode_matches_full_decode_with_slow_aac_timestamp_drift() {
    let source = test_file("drift-source.mp4", CHIRP, &["-c:a", "aac"], false);
    let path = source.with_file_name("drifted.mp4");
    crate::test_support::ffmpeg(
        &[
            "-itsscale",
            "1.001",
            "-i",
            source.to_str().unwrap(),
            "-c",
            "copy",
        ],
        &path,
    );
    assert_seeks_match_full_decode(&path, 1e-4, &[48_000 * 7 + 123]);
}

#[test]
fn clean_native_rate_pcm_preserves_the_original_decoded_samples() {
    let path = test_file("clock-clean.wav", NOISE, &["-c:a", "pcm_f32le"], false);
    let raw = path.with_extension("f32le");
    crate::test_support::ffmpeg(&["-i", path.to_str().unwrap(), "-f", "f32le"], &raw);
    let expected: Vec<f32> = std::fs::read(raw)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    let decoded = decode_audio_track(&path, 0).unwrap().unwrap();
    assert!(
        decoded
            .samples
            .iter()
            .chain(expected.iter())
            .all(|sample| sample.is_finite())
    );
    assert_eq!(decoded.samples.len(), expected.len());
    let worst = decoded
        .samples
        .iter()
        .zip(expected)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst <= f32::EPSILON, "clean PCM changed by {worst}");
}

fn assert_drifted_aac_alignment(rate: u32, scale: f64) {
    let source = test_file(
        &format!("clock-{rate}-{scale}-source.mp4"),
        &chirp_at(rate),
        &["-c:a", "aac"],
        false,
    );
    let path = source.with_file_name(format!("clock-{rate}-{scale}.mp4"));
    crate::test_support::ffmpeg(
        &[
            "-itsscale",
            &scale.to_string(),
            "-i",
            source.to_str().unwrap(),
            "-c",
            "copy",
        ],
        &path,
    );
    assert_seeks_match_full_decode(
        &path,
        1e-4,
        &[48_000 * 7 + 123, 48_000 * 13 + 7, 48_000 * 18 + 5],
    );
}

#[test]
fn drifted_aac_48k_slow_clock_matches_full_decode() {
    assert_drifted_aac_alignment(48_000, 1.001);
}

#[test]
fn drifted_aac_48k_fast_clock_matches_full_decode() {
    assert_drifted_aac_alignment(48_000, 0.999);
}

#[test]
fn drifted_aac_44k_slow_clock_matches_full_decode() {
    assert_drifted_aac_alignment(44_100, 1.001);
}

#[test]
fn drifted_aac_44k_fast_clock_matches_full_decode() {
    assert_drifted_aac_alignment(44_100, 0.999);
}

#[test]
fn drifted_aac_48k_small_clock_error_matches_full_decode() {
    assert_drifted_aac_alignment(48_000, 1.0001);
}

#[test]
fn drifted_aac_44k_small_clock_error_matches_full_decode() {
    assert_drifted_aac_alignment(44_100, 1.0001);
}

#[test]
fn drifted_aac_48k_small_fast_clock_matches_full_decode() {
    assert_drifted_aac_alignment(48_000, 0.9999);
}

#[test]
fn drifted_aac_44k_small_fast_clock_matches_full_decode() {
    assert_drifted_aac_alignment(44_100, 0.9999);
}

#[test]
fn drifted_aac_48k_ten_ppm_clock_matches_full_decode() {
    assert_drifted_aac_alignment(48_000, 1.00001);
}

#[test]
fn drifted_aac_44k_one_ppm_clock_matches_full_decode() {
    assert_drifted_aac_alignment(44_100, 1.000001);
}

fn assert_random_timestamp_jitter_preserves_audio(rate: u32) {
    let source = test_file(
        &format!("random-clock-source-{rate}.mp4"),
        &chirp_at(rate),
        &["-c:a", "aac"],
        false,
    );
    let path = source.with_file_name(format!("random-clock-{rate}.mp4"));
    let noise = "if(gte(N,2),floor(3*mod(abs(sin(N*12.9898)*43758.5453),1))-1,0)";
    let timestamps = format!("setts=pts='PTS+{noise}':dts='DTS+{noise}'");
    crate::test_support::ffmpeg(
        &[
            "-i",
            source.to_str().unwrap(),
            "-c",
            "copy",
            "-bsf:a",
            &timestamps,
        ],
        &path,
    );
    let clean = placed_full_decode(&source);
    let noisy = placed_full_decode(&path);
    assert!(
        clean
            .iter()
            .chain(noisy.iter())
            .all(|sample| sample.is_finite())
    );
    assert_eq!(clean.len(), noisy.len());
    let worst = clean
        .iter()
        .zip(noisy)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst <= f32::EPSILON,
        "random {rate}-Hz timestamp jitter changed samples by {worst}"
    );
    assert_seek_matches_full_decode(&path, 1e-4);
}

#[test]
fn random_timestamp_jitter_preserves_48k_audio() {
    assert_random_timestamp_jitter_preserves_audio(48_000);
}

#[test]
fn random_timestamp_jitter_preserves_44k_audio() {
    assert_random_timestamp_jitter_preserves_audio(44_100);
}

fn assert_coarse_timestamp_drift(codec: &str, name: &str, rate: u32) {
    let source = test_file(
        &format!("coarse-{name}-source.mkv"),
        &chirp_at(rate),
        &["-c:a", codec],
        false,
    );
    let path = source.with_file_name(format!("coarse-{name}-drift.mkv"));
    crate::test_support::ffmpeg(
        &[
            "-itsscale",
            "1.001",
            "-i",
            source.to_str().unwrap(),
            "-c",
            "copy",
        ],
        &path,
    );
    assert_seeks_match_full_decode(
        &path,
        1e-4,
        &[48_000 * 7 + 123, 48_000 * 13 + 7, 48_000 * 18 + 5],
    );
}

#[test]
fn coarse_aac_clock_drift_matches_full_decode() {
    assert_coarse_timestamp_drift("aac", "aac", 48_000);
}

#[test]
fn coarse_pcm_clock_drift_matches_full_decode() {
    assert_coarse_timestamp_drift("pcm_s16le", "pcm", 48_000);
}

#[test]
fn coarse_resampled_aac_clock_drift_matches_full_decode() {
    assert_coarse_timestamp_drift("aac", "aac44", 44_100);
}

#[test]
fn coarse_resampled_pcm_clock_drift_matches_full_decode() {
    assert_coarse_timestamp_drift("pcm_s16le", "pcm44", 44_100);
}

fn assert_drifted_pulse_timing(name: &str, codec: &str) {
    let source = test_file(
        &format!("pulse-source-{name}"),
        "aevalsrc='if(eq(mod(n,48000),0),0.8,0)':duration=20:sample_rate=48000",
        &["-c:a", codec],
        false,
    );
    let path = source.with_file_name(format!("pulse-drift-{name}"));
    crate::test_support::ffmpeg(
        &[
            "-itsscale",
            "1.001",
            "-i",
            source.to_str().unwrap(),
            "-c",
            "copy",
        ],
        &path,
    );
    let decoded = placed_full_decode(&path);
    assert!(
        decoded.iter().all(|sample| sample.is_finite()),
        "drift inserted a gap"
    );
    for second in 1..19 {
        let expected = (second as f64 * 48_000.0 * 1.001).round() as usize;
        let radius = 2400;
        let peak = (expected - radius..expected + radius)
            .max_by(|&a, &b| decoded[a * 2].abs().total_cmp(&decoded[b * 2].abs()))
            .unwrap();
        assert!(
            decoded[peak * 2].abs() > 0.1,
            "missing pulse at second {second}"
        );
        assert!(
            peak.abs_diff(expected) <= 240,
            "pulse {second} at {peak}, expected {expected} within 5 ms"
        );
    }
}

#[test]
fn fine_aac_drift_follows_the_recorded_clock_without_gaps() {
    assert_drifted_pulse_timing("aac.mp4", "aac");
}

#[test]
fn coarse_pcm_drift_follows_the_recorded_clock_without_gaps() {
    assert_drifted_pulse_timing("pcm.mkv", "pcm_s16le");
}

fn assert_clean_coarse_pcm_matches_reference(rate: u32) {
    let path = test_file(
        &format!("clean-coarse-{rate}.mkv"),
        &chirp_at(rate),
        &["-c:a", "pcm_s16le"],
        false,
    );
    let raw = path.with_extension("f32le");
    crate::test_support::ffmpeg(&["-i", path.to_str().unwrap(), "-f", "f32le"], &raw);
    let expected: Vec<f32> = std::fs::read(raw)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    let decoded = decode_audio_track(&path, 0).unwrap().unwrap();
    assert_eq!(decoded.samples.len(), expected.len());
    assert!(
        decoded
            .samples
            .iter()
            .zip(expected)
            .all(|(a, b)| a.is_finite() && *a == b),
        "clean coarse-clock PCM must not be compensated"
    );
}

#[test]
fn clean_coarse_48k_pcm_preserves_reference_samples() {
    assert_clean_coarse_pcm_matches_reference(48_000);
}

#[test]
fn clean_coarse_44k_pcm_preserves_reference_samples() {
    assert_clean_coarse_pcm_matches_reference(44_100);
}

#[test]
fn coarse_clock_preroll_delivers_the_first_chunk_near_the_request() {
    let path = test_file(
        "near-coarse.mkv",
        &chirp_at(44_100),
        &["-c:a", "pcm_s16le"],
        false,
    );
    let from = 48_000 * 13;
    let mut first = None;
    decode_audio_streams_from(&path, &[0], 48_000, from, |_, _, at, _| {
        first = Some(at);
        ControlFlow::Break(())
    })
    .unwrap();
    let first = first.unwrap();
    assert!(
        first <= from && from - first <= 48_000,
        "first chunk at {first}"
    );
}

#[test]
fn indexed_aac_keeps_a_real_timestamp_gap() {
    let path = test_file(
        "indexed-gap.mp4",
        CHIRP,
        &["-af", "asetpts='if(gte(T,5),PTS+SR,PTS)'", "-c:a", "aac"],
        false,
    );
    let full = placed_full_decode(&path);
    let missing = full
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|frame| frame[0].is_nan())
        .count();
    assert!(
        (47_000..49_000).contains(&missing),
        "{missing} missing frames"
    );
    assert_seeks_match_full_decode(
        &path,
        1e-4,
        &[48_000 * 3, 48_000 * 6 + 600, 48_000 * 13 + 7],
    );
}

#[test]
fn aac_clock_drift_starting_mid_file_matches_full_decode() {
    let source = test_file("late-clock-source.mp4", CHIRP, &["-c:a", "aac"], false);
    let path = source.with_file_name("late-clock.mp4");
    let timestamps = "setts=pts='PTS+max(PTS-384000,0)/1000':dts='DTS+max(DTS-384000,0)/1000'";
    crate::test_support::ffmpeg(
        &[
            "-i",
            source.to_str().unwrap(),
            "-c",
            "copy",
            "-bsf:a",
            timestamps,
        ],
        &path,
    );
    assert_seeks_match_full_decode(
        &path,
        1e-4,
        &[48_000 * 7 + 123, 48_000 * 13 + 7, 48_000 * 18 + 5],
    );
}

#[test]
fn clean_mpeg_ts_aac_preserves_reference_decoded_samples() {
    let path = test_file(
        "clock-clean-ts.ts",
        &chirp_at(44_100),
        &["-c:a", "aac"],
        false,
    );
    let raw = path.with_extension("f32le");
    crate::test_support::ffmpeg(&["-i", path.to_str().unwrap(), "-f", "f32le"], &raw);
    let expected: Vec<f32> = std::fs::read(raw)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    let decoded = decode_audio_track(&path, 0).unwrap().unwrap();
    assert!(
        decoded
            .samples
            .iter()
            .chain(expected.iter())
            .all(|sample| sample.is_finite())
    );
    assert_eq!(decoded.samples.len(), expected.len());
    let worst = decoded
        .samples
        .iter()
        .zip(expected)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst <= f32::EPSILON,
        "clean MPEG-TS audio changed by {worst}"
    );
}

#[test]
fn fine_timestamp_jitter_does_not_change_full_decoded_audio() {
    for rate in [48_000, 44_100] {
        let source = test_file(
            &format!("clock-jitter-source-{rate}.mp4"),
            &chirp_at(rate),
            &["-c:a", "aac"],
            false,
        );
        let path = source.with_file_name(format!("clock-jitter-{rate}.mp4"));
        let jitter = "setts=pts='PTS+if(gte(N,2),if(mod(N,2),1,-1),0)':dts='DTS+if(gte(N,2),if(mod(N,2),1,-1),0)'";
        crate::test_support::ffmpeg(
            &[
                "-i",
                source.to_str().unwrap(),
                "-c",
                "copy",
                "-bsf:a",
                jitter,
            ],
            &path,
        );
        let clean = placed_full_decode(&source);
        let noisy = placed_full_decode(&path);
        assert!(
            clean
                .iter()
                .chain(noisy.iter())
                .all(|sample| sample.is_finite())
        );
        assert_eq!(clean.len(), noisy.len());
        let worst = clean
            .iter()
            .zip(noisy)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst <= f32::EPSILON,
            "jitter changed {rate}-Hz audio by {worst}"
        );
    }
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_aac_in_mkv() {
    let path = test_file("aac.mkv", CHIRP, &["-c:a", "aac"], true);
    assert_seek_matches_full_decode(&path, 1e-4);
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_opus() {
    let path = test_file("opus.mkv", CHIRP, &["-c:a", "libopus"], true);
    assert_seek_matches_full_decode(&path, 1e-4);
}

#[test]
fn a_seeked_decode_matches_the_full_decode_on_mp3() {
    let path = test_file("chirp.mp3", CHIRP, &["-c:a", "libmp3lame"], false);
    assert_seek_matches_full_decode(&path, 1e-4);
}

fn chirp_at(rate: u32) -> String {
    format!("aevalsrc=0.5*sin(2*PI*(100+300*t)*t):duration=20:sample_rate={rate}")
}

/// Seeked onto an input frame that falls on an output frame, the resampler
/// is in phase with one fed from the start.
#[test]
fn a_seeked_resampled_decode_matches_the_full_decode() {
    for (name, rate, codec, tolerance) in [
        ("chirp44.wav", 44_100, "pcm_s16le", 0.0),
        ("chirp22.flac", 22_050, "flac", 0.0),
        ("chirp32.wav", 32_000, "pcm_s16le", 0.0),
        ("chirp96.wav", 96_000, "pcm_s16le", 0.0),
        // No channel layout in the file.
        ("chirp44.mkv", 44_100, "pcm_s16le", 0.0),
        ("chirp44.mp3", 44_100, "libmp3lame", 1e-4),
    ] {
        let path = test_file(name, &chirp_at(rate), &["-c:a", codec], false);
        assert_seek_matches_full_decode(&path, tolerance);
    }
    let path = test_file("chirp44.mp4", &chirp_at(44_100), &["-c:a", "aac"], true);
    assert_seeks_match_full_decode(&path, 1e-4, &SEEK_OFFSETS);
}

#[test]
fn a_resampled_file_is_read_from_near_the_frame_asked_not_from_its_start() {
    let path = test_file(
        "near44.wav",
        &chirp_at(44_100),
        &["-c:a", "pcm_s16le"],
        false,
    );
    let from = 48_000 * 13;
    let mut first = None;
    decode_audio_streams_from(&path, &[0], 48_000, from, |_, _, at, _| {
        first = Some(at);
        ControlFlow::Break(())
    })
    .unwrap();
    let first = first.unwrap();
    assert!(from - first <= 48_000, "first chunk at {first}");
}

/// 1 s missing from the timestamps at 5 s (a dropout, a cut capture): a
/// seek past it and a decode from the start both leave it silent.
#[test]
fn a_gap_in_the_timestamps_is_kept_by_a_decode_from_the_start_and_by_a_seek() {
    for (name, rate) in [("gap.mkv", 48_000), ("gap44.mkv", 44_100)] {
        let path = test_file(
            name,
            &chirp_at(rate),
            &[
                "-af",
                "asetpts='if(gte(T,5),PTS+SR,PTS)'",
                "-c:a",
                "pcm_s16le",
            ],
            false,
        );
        let full = placed_full_decode(&path);
        let silent = full[..full.len() / 2 * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|frame| frame[0].is_nan())
            .count();
        assert!(
            (47_000..49_000).contains(&silent),
            "{name}: {silent} frames missing"
        );
        assert_seeks_match_full_decode(
            &path,
            0.0,
            &[
                48_000 * 3,
                48_000 * 4 + 24_000,
                48_000 * 6 + 600,
                48_000 * 13 + 7,
            ],
        );
    }
}
