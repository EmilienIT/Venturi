//! Decoding whole audio tracks into interleaved f32. The track sits
//! entirely in RAM; the streaming version delivers it in chunks so the mixer
//! can play its start before the decode is over.

use ffmpeg::ChannelLayout;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type;
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg_next as ffmpeg;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::path::Path;

pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32, length = total_frames * channels.
    pub samples: Vec<f32>,
}

/// Decodes the N-th audio stream (order of `probe::audio_streams`).
/// `Ok(None)` if it does not exist.
pub fn decode_audio_track(
    path: &Path,
    stream_index: usize,
) -> Result<Option<AudioBuffer>, crate::MediaError> {
    let mut samples = Vec::new();
    let formats = decode_audio_streams_streaming(path, &[stream_index], None, |_, _, chunk| {
        samples.extend_from_slice(chunk);
        ControlFlow::Continue(())
    })?;
    Ok(formats[0].map(|(sample_rate, channels)| AudioBuffer {
        sample_rate,
        channels,
        samples,
    }))
}

/// Decoded from before the target of a seek and dropped: a codec needs a few
/// frames after a seek to output the same samples as a decode from the start.
const SEEK_PREROLL_SECS: f64 = 0.5;

// Millisecond timestamps need a longer clock window than codec warm-up.
const COARSE_CLOCK_WINDOW_SECS: f64 = 2.5;
const COARSE_SEEK_PREROLL_SECS: f64 = 4.0;
const CLOCK_OBSERVATION_SECS: f64 = 0.02;
const CLOCK_CONTROL_SAMPLES: usize = 32;
const INDEXED_CLOCK_RADIUS: usize = 64;

/// Read from the start to find where each stream's frames are.
const MAX_ORIGIN_PACKETS: usize = 2000;

/// Frames decoded from the start of each stream before a seek.
const ORIGIN_FRAMES: usize = 8;

/// Adjacent timestamps beyond the decoded frame length by this amount mark
/// a real gap or overlap. Above the rounding of millisecond timestamps.
const DISCONTINUITY_SECS: f64 = 0.005;

/// Where a decode from the start puts a frame with timestamp `ts`.
#[derive(Clone, Copy)]
struct Anchor {
    ts: i64,
    /// In input samples.
    position: u64,
    /// Samples of every frame but the first, if they are all the same: with
    /// timestamps coarser than a sample (milliseconds in Matroska) a
    /// position is snapped to them.
    frame_samples: Option<u64>,
    /// The first frame came out shorter: the decoder drops the codec delay
    /// (Opus pre-skip) from its first frame, after a seek too, without
    /// moving its timestamp.
    trims_first: bool,
}

struct IndexedClock {
    timestamps: Vec<i64>,
    frame_samples: u64,
    tick: f64,
    jump_samples: f64,
    drifting: bool,
}

impl IndexedClock {
    fn read(
        stream: &ffmpeg::format::stream::Stream,
        decoder: &ffmpeg::decoder::Audio,
    ) -> Option<Self> {
        if decoder.id() != ffmpeg::codec::Id::AAC || decoder.frame_size() == 0 {
            return None;
        }
        let tick = f64::from(stream.time_base()) * decoder.rate() as f64;
        if !tick.is_finite() || tick <= 0.0 || tick > 1.0 + 1e-9 {
            return None;
        }
        // SAFETY: this API only reads the index despite taking a mutable stream pointer.
        let timestamps = unsafe {
            let count = ffmpeg::ffi::avformat_index_get_entries_count(stream.as_ptr());
            if count < 8 || i64::from(count) != stream.frames() {
                return None;
            }
            let mut timestamps = Vec::with_capacity(count as usize);
            for index in 0..count {
                let entry =
                    ffmpeg::ffi::avformat_index_get_entry(stream.as_ptr().cast_mut(), index);
                if entry.is_null() || (*entry).timestamp == ffmpeg::ffi::AV_NOPTS_VALUE {
                    return None;
                }
                timestamps.push((*entry).timestamp);
            }
            timestamps
        };
        if timestamps.windows(2).any(|pair| pair[1] <= pair[0]) {
            return None;
        }
        let frame_samples = u64::from(decoder.frame_size());
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for (index, &timestamp) in timestamps.iter().enumerate() {
            let residual = timestamp.checked_sub(timestamps[0])? as f64 * tick
                - index as f64 * frame_samples as f64;
            low = low.min(residual);
            high = high.max(residual);
        }
        Some(Self {
            timestamps,
            frame_samples,
            tick,
            jump_samples: DISCONTINUITY_SECS * decoder.rate() as f64,
            drifting: high - low > 2.0 * tick + 1e-7,
        })
    }

    fn offset(&self, origin: i64, timestamp: i64) -> Option<f64> {
        let origin_index = self.timestamps.binary_search(&origin).ok()?;
        let at = self.timestamps.binary_search(&timestamp).ok()?;
        if !self.drifting {
            return Some((at as f64 - origin_index as f64) * self.frame_samples as f64);
        }
        let jump = |index: usize| {
            (((self.timestamps[index + 1] - self.timestamps[index]) as f64 * self.tick)
                - self.frame_samples as f64)
                .abs()
                > self.jump_samples
        };
        let mut start = at.saturating_sub(INDEXED_CLOCK_RADIUS);
        let mut end = (at + INDEXED_CLOCK_RADIUS + 1).min(self.timestamps.len());
        for index in (start..at).rev() {
            if jump(index) {
                start = index + 1;
                break;
            }
        }
        for index in at..end - 1 {
            if jump(index) {
                end = index + 1;
                break;
            }
        }
        let n = (end - start) as f64;
        let mean_x = (start as f64 + end as f64 - 1.0) / 2.0 - at as f64;
        let mean_y = self.timestamps[start..end]
            .iter()
            .map(|&ts| (ts - timestamp) as f64 * self.tick)
            .sum::<f64>()
            / n;
        let mut covariance = 0.0;
        let mut variance = 0.0;
        for index in start..end {
            let x = index as f64 - at as f64 - mean_x;
            let y = (self.timestamps[index] - timestamp) as f64 * self.tick - mean_y;
            covariance += x * y;
            variance += x * x;
        }
        let correction = if variance > 0.0 {
            mean_y - covariance / variance * mean_x
        } else {
            0.0
        };
        Some((timestamp - origin) as f64 * self.tick + correction)
    }
}

struct StreamDecoder {
    decoder: ffmpeg::decoder::Audio,
    resampler: Resampler,
    layout: ChannelLayout,
    in_rate: u32,
    out_rate: u32,
    channels: u16,
    /// Seconds per timestamp tick.
    time_base: f64,
    /// Where the timestamps put the frames: set by a seek, or learnt from
    /// `firsts` in a decode from the start.
    anchor: Option<Anchor>,
    firsts: Vec<(Option<i64>, u64)>,
    /// After a seek, until a whole frame is placed by its timestamp.
    placing: bool,
    /// Input frame of the next sample fed to the resampler.
    fed: u64,
    /// Output frame of the next sample.
    next: u64,
    previous_timestamp: Option<(Option<i64>, u64)>,
    clock_points: VecDeque<(f64, f64)>,
    compensating: bool,
    indexed_clock: Option<IndexedClock>,
}

/// Where a decoded frame goes.
enum Place {
    /// Right after the previous one.
    Counted,
    /// At this input frame, by its timestamp.
    At(u64),
    /// Nowhere: a frame after a seek without a timestamp.
    Skip,
}

impl StreamDecoder {
    fn open(
        stream: &ffmpeg::format::stream::Stream,
        out_rate: Option<u32>,
    ) -> Result<Self, crate::MediaError> {
        let time_base = f64::from(stream.time_base());
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        let layout = decoder_channel_layout(&decoder);
        let in_rate = decoder.rate();
        let out_rate = out_rate.unwrap_or(in_rate);
        let resampler = synchronized_resampler(decoder.format(), layout, in_rate, out_rate)?;
        let indexed_clock = IndexedClock::read(stream, &decoder);
        Ok(Self {
            channels: decoder.channels(),
            decoder,
            resampler,
            layout,
            in_rate,
            out_rate,
            time_base,
            anchor: None,
            firsts: Vec::new(),
            placing: false,
            fed: 0,
            next: 0,
            previous_timestamp: None,
            clock_points: VecDeque::new(),
            compensating: false,
            indexed_clock,
        })
    }

    fn fresh_resampler(&self) -> Result<Resampler, crate::MediaError> {
        Ok(synchronized_resampler(
            self.decoder.format(),
            self.layout,
            self.in_rate,
            self.out_rate,
        )?)
    }

    /// Input frames from `anchor` to timestamp `ts`, snapped to whole
    /// frames when the timestamps are coarser than a sample.
    fn input_at(&self, anchor: Anchor, ts: i64) -> f64 {
        let offset = self.clock_at(anchor, ts);
        let coarse = self.time_base * self.in_rate.max(1) as f64 > 1.0 + 1e-9;
        match anchor.frame_samples {
            Some(frame) if coarse => (offset / frame as f64).round() * frame as f64,
            _ => offset,
        }
    }

    fn clock_at(&self, anchor: Anchor, ts: i64) -> f64 {
        if let Some(offset) = self
            .indexed_clock
            .as_ref()
            .and_then(|clock| clock.offset(anchor.ts, ts))
        {
            return offset;
        }
        let in_rate = self.in_rate.max(1) as f64;
        (ts - anchor.ts) as f64 * self.time_base * in_rate
    }

    fn place(&mut self, decoded: &ffmpeg::frame::Audio) -> Place {
        let ts = decoded.timestamp().or(decoded.pts());
        let samples = decoded.samples() as u64;
        let previous = self.previous_timestamp.replace((ts, samples));
        if !self.placing {
            if self.anchor.is_none() {
                self.firsts.push((ts, samples));
                if self.firsts.len() == ORIGIN_FRAMES {
                    self.anchor = anchor(&self.firsts);
                }
            }
            let (Some(anchor), Some(ts)) = (self.anchor, ts) else {
                return Place::Counted;
            };
            let input = anchor.position as f64 + self.input_at(anchor, ts);
            let tolerance = DISCONTINUITY_SECS * self.in_rate as f64;
            let jump = previous.is_some_and(|(previous_ts, previous_samples)| {
                previous_ts.is_some_and(|previous_ts| {
                    (((ts - previous_ts) as f64 * self.time_base * self.in_rate as f64)
                        - previous_samples as f64)
                        .abs()
                        > tolerance
                })
            });
            return if jump {
                Place::At(input.round().max(0.0) as u64)
            } else {
                Place::Counted
            };
        }
        let (Some(anchor), Some(ts)) = (self.anchor, ts) else {
            return Place::Skip;
        };
        let mut offset = self.input_at(anchor, ts);
        match anchor.frame_samples {
            Some(frame) => {
                if anchor.trims_first && samples < frame {
                    offset += (frame - samples) as f64;
                }
                self.placing = samples < frame;
            }
            None => self.placing = false,
        }
        Place::At((anchor.position as f64 + offset.round()).max(0.0) as u64)
    }

    /// Resamples and delivers all the frames ready in the decoder.
    fn drain(
        &mut self,
        slot: usize,
        samples: &mut Vec<f32>,
        on_chunk: &mut impl FnMut(usize, u16, u64, &[f32]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.samples() == 0 {
                continue;
            }
            let placement = self.place(&decoded);
            let mut input = match placement {
                Place::Skip => continue,
                Place::Counted => self.fed,
                Place::At(input) => input,
            };
            let mut clock = match placement {
                Place::At(_) => input as f64,
                _ => self
                    .anchor
                    .zip(decoded.timestamp().or(decoded.pts()))
                    .map(|(anchor, ts)| anchor.position as f64 + self.clock_at(anchor, ts))
                    .unwrap_or(input as f64),
            };
            if input != self.fed {
                {
                    // A resampler is in phase with one fed from the start
                    // only from an input frame that falls on an output frame.
                    if self.flush(slot, samples, on_chunk)?.is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                    self.resampler = self.fresh_resampler()?;
                    self.clock_points.clear();
                    self.compensating = false;
                    let grid = u64::from(self.in_rate / gcd(self.in_rate, self.out_rate));
                    let skip = (grid - input % grid) % grid;
                    if skip >= decoded.samples() as u64 {
                        continue;
                    }
                    decoded = drop_front(&decoded, skip as usize, self.layout);
                    input += skip;
                    clock += skip as f64;
                }
                self.fed = input;
                self.next = input * u64::from(self.out_rate) / u64::from(self.in_rate.max(1));
            }
            let model = self.clock_model(clock);
            // Bound phase error from swresample's quantized compensation rate.
            let chunk_size = if model.is_some() {
                CLOCK_CONTROL_SAMPLES
            } else {
                decoded.samples()
            };
            for offset in (0..decoded.samples()).step_by(chunk_size) {
                let count = chunk_size.min(decoded.samples() - offset);
                let mut chunk = audio_slice(&decoded, offset, count, self.layout);
                let capacity = count * self.out_rate as usize / self.in_rate.max(1) as usize + 64;
                let mut resampled = ffmpeg::frame::Audio::new(
                    Sample::F32(SampleType::Packed),
                    capacity,
                    self.layout,
                );
                let clock = clock + offset as f64 * model.map_or(1.0, |(_, slope)| slope);
                // SAFETY: the resampler is initialized and exclusively borrowed here.
                let at = unsafe {
                    ffmpeg::ffi::swr_next_pts(
                        self.resampler.as_mut_ptr(),
                        (clock.max(0.0) * self.out_rate as f64).round() as i64,
                    )
                }
                .max(0) as u64
                    / u64::from(self.in_rate.max(1));
                if let Some((target, slope)) = model {
                    let target =
                        target + offset as f64 * slope * self.out_rate as f64 / self.in_rate as f64;
                    self.compensate(target, slope, count)?;
                }
                run_resampler(&mut self.resampler, &mut chunk, &mut resampled)?;
                self.fed += count as u64;
                self.next = at + resampled.samples() as u64;
                if emit(slot, self.channels, at, &resampled, samples, on_chunk).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn clock_model(&mut self, clock: f64) -> Option<(f64, f64)> {
        if self
            .indexed_clock
            .as_ref()
            .is_some_and(|clock| !clock.drifting)
        {
            return None;
        }
        let coarse = self.time_base * self.in_rate as f64 > 1.0 + 1e-9;
        let spacing = self.in_rate as f64 * CLOCK_OBSERVATION_SECS;
        if !coarse
            || self
                .clock_points
                .back()
                .is_none_or(|point| (point.1 / spacing).floor() != (clock / spacing).floor())
        {
            self.clock_points.push_back((self.fed as f64, clock));
        }
        let span = if coarse {
            self.in_rate as f64 * COARSE_CLOCK_WINDOW_SECS
        } else {
            0.0
        };
        while self.clock_points.len() > 8
            && (!coarse || self.fed as f64 - self.clock_points[1].0 >= span)
        {
            self.clock_points.pop_front();
        }
        if self.clock_points.len() < 8 || self.fed as f64 - self.clock_points[0].0 < span {
            return None;
        }
        let recent = self.clock_points.make_contiguous();
        let (origin_x, origin_y) = recent[0];
        let n = recent.len() as f64;
        let mean_x = recent.iter().map(|p| p.0 - origin_x).sum::<f64>() / n;
        let mean_y = recent.iter().map(|p| p.1 - origin_y).sum::<f64>() / n;
        let variance = recent
            .iter()
            .map(|p| (p.0 - origin_x - mean_x).powi(2))
            .sum::<f64>();
        let covariance = recent
            .iter()
            .map(|p| (p.0 - origin_x - mean_x) * (p.1 - origin_y - mean_y))
            .sum::<f64>();
        if variance == 0.0 {
            return None;
        }
        let slope = covariance / variance;
        let span = self.fed as f64 - origin_x;
        let jitter = (self.time_base * self.in_rate as f64 * 2.0 / span.max(1.0)).max(0.00025);
        self.compensating |= (slope - 1.0).abs() > jitter
            || self
                .indexed_clock
                .as_ref()
                .is_some_and(|clock| clock.drifting);
        if !self.compensating {
            return None;
        }
        let target = (origin_y + mean_y + slope * (self.fed as f64 - origin_x - mean_x))
            * self.out_rate as f64
            / self.in_rate as f64;
        Some((target, slope))
    }

    fn compensate(&mut self, target: f64, slope: f64, samples: usize) -> Result<(), ffmpeg::Error> {
        let output_samples = samples as f64 * self.out_rate as f64 / self.in_rate as f64;
        // SAFETY: all calls use this decoder's initialized, exclusively owned context.
        unsafe {
            let context = self.resampler.as_mut_ptr();
            let clock_scale = i64::from(self.in_rate) * i64::from(self.out_rate);
            let delay =
                ffmpeg::ffi::swr_get_delay(context, clock_scale) as f64 / self.in_rate as f64;
            let next = ffmpeg::ffi::swr_next_pts(context, i64::MIN) as f64 / self.in_rate as f64;
            let error = target - delay - next;
            let correction = (output_samples * (slope - 1.0) + error * 0.625)
                .clamp(-output_samples * 0.02, output_samples * 0.02);
            let precision = (i32::MAX as f64 / output_samples.max(1.0))
                .floor()
                .min(1_048_576.0);
            let delta = (correction * precision).round() as i32;
            let distance = (output_samples * precision)
                .round()
                .clamp(1.0, i32::MAX as f64) as i32;
            let result = ffmpeg::ffi::swr_set_compensation(context, delta, distance);
            if result < 0 {
                return Err(ffmpeg::Error::from(result));
            }
        }
        Ok(())
    }

    fn flush(
        &mut self,
        slot: usize,
        samples: &mut Vec<f32>,
        on_chunk: &mut impl FnMut(usize, u16, u64, &[f32]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, crate::MediaError> {
        while self.resampler.delay().is_some() {
            let mut resampled =
                ffmpeg::frame::Audio::new(Sample::F32(SampleType::Packed), 4096, self.layout);
            self.resampler.flush(&mut resampled)?;
            if resampled.samples() == 0 {
                break;
            }
            let at = self.next;
            self.next = at + resampled.samples() as u64;
            if emit(slot, self.channels, at, &resampled, samples, on_chunk).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

fn synchronized_resampler(
    format: Sample,
    layout: ChannelLayout,
    in_rate: u32,
    out_rate: u32,
) -> Result<Resampler, ffmpeg::Error> {
    let mut options = ffmpeg::Dictionary::new();
    options.set("flags", "res");
    // Preserve native-rate samples until clock compensation is needed.
    if in_rate == out_rate {
        options.set("cutoff", "1");
    }
    options.set("async", "1000");
    options.set("min_comp", "1000000");
    options.set("min_hard_comp", "0.1");
    options.set("comp_duration", "0.05");
    Resampler::get_with(
        format,
        layout,
        in_rate,
        Sample::F32(SampleType::Packed),
        layout,
        out_rate,
        options,
    )
}

fn emit(
    slot: usize,
    channels: u16,
    at: u64,
    frame: &ffmpeg::frame::Audio,
    samples: &mut Vec<f32>,
    on_chunk: &mut impl FnMut(usize, u16, u64, &[f32]) -> ControlFlow<()>,
) -> ControlFlow<()> {
    samples.clear();
    append_f32(frame, samples);
    if samples.is_empty() {
        ControlFlow::Continue(())
    } else {
        on_chunk(slot, channels, at, samples)
    }
}

/// Several streams in a single read, in chunks: `on_chunk(i, channels,
/// samples)` with `i` an index into `stream_indices`; `Break` interrupts.
/// `out_rate` resamples with one swresample context per stream, without
/// discontinuities between chunks. Returns `(rate, channels)` per stream,
/// `None` if it does not exist.
pub fn decode_audio_streams_streaming(
    path: &Path,
    stream_indices: &[usize],
    out_rate: Option<u32>,
    mut on_chunk: impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
) -> Result<Vec<Option<(u32, u16)>>, crate::MediaError> {
    decode_streams(
        path,
        stream_indices,
        out_rate,
        None,
        |slot, channels, _, chunk| on_chunk(slot, channels, chunk),
    )
}

/// Like `decode_audio_streams_streaming`, from about frame `from` of
/// `out_rate` on: it seeks there instead of reading the file from its start.
/// `on_chunk(i, channels, at, samples)` with `at` the frame of the first
/// sample where a decode from the start puts it; chunks may start before
/// `from`, and after a jump in the stream's timestamps `at` jumps too.
/// Coarse timestamps require a longer bounded read before the first callback.
pub fn decode_audio_streams_from(
    path: &Path,
    stream_indices: &[usize],
    out_rate: u32,
    from: u64,
    on_chunk: impl FnMut(usize, u16, u64, &[f32]) -> ControlFlow<()>,
) -> Result<Vec<Option<(u32, u16)>>, crate::MediaError> {
    let seek_to = from as f64 / out_rate.max(1) as f64 - SEEK_PREROLL_SECS;
    let seek_to = (seek_to > 0.0).then_some(seek_to);
    decode_streams(path, stream_indices, Some(out_rate), seek_to, on_chunk)
}

fn decode_streams(
    path: &Path,
    stream_indices: &[usize],
    out_rate: Option<u32>,
    seek_to: Option<f64>,
    mut on_chunk: impl FnMut(usize, u16, u64, &[f32]) -> ControlFlow<()>,
) -> Result<Vec<Option<(u32, u16)>>, crate::MediaError> {
    crate::probe::ensure_init();

    let mut ictx = ffmpeg::format::input(&path)?;
    let audio_streams: Vec<_> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == Type::Audio)
        .collect();
    // (index in the container, decoder) per slot.
    let mut decoders: Vec<Option<(usize, StreamDecoder)>> = stream_indices
        .iter()
        .map(|&i| {
            audio_streams
                .get(i)
                .map(|stream| Ok((stream.index(), StreamDecoder::open(stream, out_rate)?)))
                .transpose()
        })
        .collect::<Result<_, crate::MediaError>>()?;
    drop(audio_streams);
    let formats = decoders
        .iter()
        .map(|d| d.as_ref().map(|(_, d)| (d.out_rate, d.channels)))
        .collect();
    if decoders.iter().all(Option::is_none) {
        return Ok(formats);
    }
    if let Some(secs) = seek_to {
        seek(&mut ictx, &mut decoders, out_rate, secs)?;
    }

    let coarse = decoders
        .iter()
        .filter_map(Option::as_ref)
        .any(|(_, stream)| stream.time_base * stream.in_rate as f64 > 1.0 + 1e-9);
    let emit_from = if coarse {
        seek_to
            .zip(out_rate)
            .map_or(0, |(secs, rate)| (secs * rate as f64) as u64)
    } else {
        0
    };
    let mut deliver = |slot, channels: u16, at: u64, chunk: &[f32]| {
        let channels_count = channels.max(1) as usize;
        let frames = chunk.len() / channels_count;
        let skip = emit_from.saturating_sub(at).min(frames as u64) as usize;
        if skip == frames {
            ControlFlow::Continue(())
        } else {
            on_chunk(
                slot,
                channels,
                at + skip as u64,
                &chunk[skip * channels_count..],
            )
        }
    };

    let mut samples = Vec::new();
    loop {
        // `Packet::read` does not unref the previous packet: reusing one
        // would leak every packet read.
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => {
                let Some((slot, (_, stream))) = decoders
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(slot, d)| Some((slot, d.as_mut()?)))
                    .find(|(_, (i, _))| *i == packet.stream())
                else {
                    continue;
                };
                stream.decoder.send_packet(&packet)?;
                if stream.drain(slot, &mut samples, &mut deliver)?.is_break() {
                    return Ok(formats);
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    for (slot, entry) in decoders.iter_mut().enumerate() {
        let Some((_, stream)) = entry else {
            continue;
        };
        stream.decoder.send_eof()?;
        if stream.drain(slot, &mut samples, &mut deliver)?.is_break()
            || stream.flush(slot, &mut samples, &mut deliver)?.is_break()
        {
            break;
        }
    }
    Ok(formats)
}

/// Seeks to `secs` after the first frame, with fresh decoders that place
/// their frames by timestamp, anchored on the first frames from the start.
fn seek(
    ictx: &mut ffmpeg::format::context::Input,
    decoders: &mut [Option<(usize, StreamDecoder)>],
    out_rate: Option<u32>,
    secs: f64,
) -> Result<(), crate::MediaError> {
    let coarse = decoders
        .iter()
        .filter_map(Option::as_ref)
        .any(|(_, stream)| stream.time_base * stream.in_rate as f64 > 1.0 + 1e-9);
    let secs = if coarse {
        (secs - (COARSE_SEEK_PREROLL_SECS - SEEK_PREROLL_SECS)).max(0.0)
    } else {
        secs
    };
    // (timestamp, samples) of the first frames of every stream.
    let mut firsts: Vec<Vec<(Option<i64>, u64)>> = decoders.iter().map(|_| Vec::new()).collect();
    let mut decoded = ffmpeg::frame::Audio::empty();
    for _ in 0..MAX_ORIGIN_PACKETS {
        let mut packet = ffmpeg::Packet::empty();
        if packet.read(ictx).is_err() {
            break;
        }
        let found = decoders
            .iter_mut()
            .zip(&mut firsts)
            .find(|(d, _)| d.as_ref().is_some_and(|(i, _)| *i == packet.stream()));
        let Some((Some((_, stream)), frames)) = found else {
            continue;
        };
        if frames.len() < ORIGIN_FRAMES && stream.decoder.send_packet(&packet).is_ok() {
            while stream.decoder.receive_frame(&mut decoded).is_ok() {
                frames.push((
                    decoded.timestamp().or(decoded.pts()),
                    decoded.samples() as u64,
                ));
            }
        }
        if decoders
            .iter()
            .zip(&firsts)
            .all(|(d, frames)| d.is_none() || frames.len() >= ORIGIN_FRAMES)
        {
            break;
        }
    }
    let mut first_secs = f64::INFINITY;
    for (entry, frames) in decoders.iter_mut().zip(&firsts) {
        let Some((index, old)) = entry else {
            continue;
        };
        let stream = ictx.stream(*index).ok_or(ffmpeg::Error::StreamNotFound)?;
        let mut fresh = StreamDecoder::open(&stream, out_rate)?;
        fresh.anchor = anchor(frames).or_else(|| {
            eprintln!(
                "audio seek: no anchor frames in stream {index}, \
                 placing from its start time (may be off by up to a frame)"
            );
            Some(Anchor {
                ts: stream.start_time().max(0),
                position: 0,
                frame_samples: None,
                trims_first: false,
            })
        });
        fresh.placing = true;
        if let Some(anchor) = fresh.anchor {
            let start = anchor.ts as f64 * fresh.time_base
                - anchor.position as f64 / fresh.in_rate.max(1) as f64;
            first_secs = first_secs.min(start);
        }
        *old = fresh;
    }
    let target = ((first_secs.max(0.0) + secs) * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
    // Some demuxers refuse a target past the end: from the start, the
    // frames are still placed by timestamp.
    if ictx.seek(target, ..target).is_err() {
        ictx.seek(0, ..0)?;
    }
    Ok(())
}

/// From the first frames of a stream: the second one sits where the first
/// one's samples end.
fn anchor(frames: &[(Option<i64>, u64)]) -> Option<Anchor> {
    match frames {
        [(Some(ts), _)] => Some(Anchor {
            ts: *ts,
            position: 0,
            frame_samples: None,
            trims_first: false,
        }),
        [(_, first), (Some(ts), _), rest @ ..] => {
            // The last one read may be the end of the stream, shorter.
            let others = &frames[1..frames.len() - usize::from(!rest.is_empty())];
            let samples = others[0].1;
            let fixed = others.len() >= 2 && others.iter().all(|&(_, n)| n == samples);
            Some(Anchor {
                ts: *ts,
                position: *first,
                frame_samples: fixed.then_some(samples),
                trims_first: fixed && *first < samples,
            })
        }
        _ => None,
    }
}

/// Decoder channel layout, with a default one in place of an unspecified
/// one (e.g. PCM in MKV): otherwise swresample rejects every frame with
/// "Input changed".
pub(crate) fn decoder_channel_layout(decoder: &ffmpeg::decoder::Audio) -> ChannelLayout {
    let layout = decoder.channel_layout();
    if layout.is_empty() {
        ChannelLayout::default(i32::from(decoder.channels()))
    } else {
        layout
    }
}

/// `Resampler::run` after aligning the frame layout to the resampler's
/// (see `decoder_channel_layout`).
pub(crate) fn run_resampler(
    resampler: &mut Resampler,
    decoded: &mut ffmpeg::frame::Audio,
    resampled: &mut ffmpeg::frame::Audio,
) -> Result<(), crate::MediaError> {
    if decoded.channel_layout().is_empty() {
        decoded.set_channel_layout(resampler.input().channel_layout);
    }
    resampler.run(decoded, resampled)?;
    Ok(())
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

/// `frame` without its first `skip` samples. `layout`: see
/// `decoder_channel_layout`.
fn drop_front(
    frame: &ffmpeg::frame::Audio,
    skip: usize,
    layout: ChannelLayout,
) -> ffmpeg::frame::Audio {
    audio_slice(frame, skip, frame.samples() - skip, layout)
}

fn audio_slice(
    frame: &ffmpeg::frame::Audio,
    skip: usize,
    samples: usize,
    layout: ChannelLayout,
) -> ffmpeg::frame::Audio {
    let mut out = ffmpeg::frame::Audio::new(frame.format(), samples, layout);
    out.set_rate(frame.rate());
    let sample_bytes = frame.format().bytes();
    let (planes, plane_samples) = if frame.is_planar() {
        (frame.planes(), 1)
    } else {
        (1, frame.channels() as usize)
    };
    for plane in 0..planes {
        let from = skip * plane_samples * sample_bytes;
        let len = samples * plane_samples * sample_bytes;
        out.data_mut(plane)[..len].copy_from_slice(&frame.data(plane)[from..from + len]);
    }
    out
}

fn append_f32(frame: &ffmpeg::frame::Audio, out: &mut Vec<f32>) {
    let byte_len = frame.samples() * frame.channels() as usize * 4;
    out.extend(
        frame.data(0)[..byte_len]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_ne_bytes(*b)),
    );
}

#[cfg(test)]
#[path = "tests/audio.rs"]
mod tests;
