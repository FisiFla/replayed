//! Places captured audio packets on the shared clock by their device timestamps.
//!
//! Downstream times audio by byte count alone (ffmpeg reads raw s16le), so a missing stretch of
//! samples plays everything after it early against the video. Loopback delivers no packets while
//! nothing plays and loses data when it is read late, so the stream is padded with silence (or
//! trimmed) until each packet's first sample sits where its timestamp says.

use crate::{AudioBuffer, AudioFormat};
use anyhow::Result;
use std::collections::VecDeque;
use std::time::Duration;

/// How far behind `now` the silence fill stays while no packets arrive. A packet is handed over up
/// to a device period after its first sample played, and padding over it first would trim real
/// audio; 100ms is ten periods of headroom and still nothing a clip cut notices.
pub(crate) const LIVE_LAG: Duration = Duration::from_millis(100);

/// The longest stretch of *late* audio the timeline treats as already covered.
///
/// A packet that claims to start before the running count was probably overtaken by silence that
/// ran ahead of it (see [`AudioTimeline::fill_to`]), and its overlapping head is trimmed. A
/// packet later than this is not overtaken by anything this recording placed: it carries a
/// timestamp nobody can use, and its audio is kept and appended instead of being trimmed away.
pub(crate) const MAX_FILL: Duration = Duration::from_secs(1);

/// The most silence one gap may materialise: the longest outage the timeline pads.
///
/// Downstream times audio by byte count alone (the encoder discards [`AudioBuffer::pts`]), so a
/// stretch the timeline does not account for plays everything after it *early* against the video,
/// for the rest of the recording. The video is on the wall clock (ffmpeg stamps each frame with
/// the moment the pipe delivered it), so a pump thread that stalled for twenty seconds produced
/// twenty seconds of frozen video, and the audio has to carry the same twenty seconds. A bound of
/// one second dropped those stretches instead.
///
/// The bound exists because a gap is a contiguous run of zeroes whose size is a timestamp's to
/// choose: 11.9 hours of it is 8TB, and the allocator aborts the process on a request it cannot
/// serve. A minute is 11.5MB at 48kHz stereo s16le, which is cheap and transient: the encoder's
/// audio queue drops what it cannot hold and replaces it with silence later, so the byte count is
/// preserved. Beyond a minute is not a pump stall: it is a clock the recording cannot follow
/// (a suspended session, a device position belonging to another clock), and the stretch is
/// dropped or the timestamp ignored, with a warning, instead of being filled.
pub(crate) const MAX_PAD: Duration = Duration::from_secs(60);

/// One packet as the source described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PacketInfo {
    pub frames: usize,
    /// When the packet's first sample played, on the shared clock. `None` when the device has no
    /// usable timestamp for it, so it simply follows the previous packet.
    pub time: Option<Duration>,
    /// The engine lost data before this packet (`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`).
    pub discontinuity: bool,
}

/// Where packets come from: the WASAPI capture client, or a scripted fake in tests.
pub(crate) trait PacketSource {
    /// Append the next packet's samples to `out` (a silent packet as zeros) and describe it, or
    /// return `None` when nothing is waiting.
    fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>>;
}

/// A 10ms block taken off the timeline.
#[derive(Debug)]
pub(crate) struct Block {
    pub buffer: AudioBuffer,
    /// Every sample is padding, not something the device delivered. Lets the converted-silence
    /// diagnostic ignore stretches where the engine sent nothing at all.
    pub filler: bool,
}

/// The game-audio timeline: frame `n` plays at `origin + n / sample_rate` on the shared clock.
pub(crate) struct AudioTimeline {
    format: AudioFormat,
    origin: Duration,
    /// Frames placed so far, emitted or still pending.
    placed: u64,
    /// Frames already handed out as blocks.
    emitted: u64,
    pending: Vec<u8>,
    /// Padded frame ranges not yet fully emitted, oldest first.
    fills: VecDeque<(u64, u64)>,
    scratch: Vec<u8>,
    /// Packets whose timestamp the recording could not account for, and the stretches dropped for
    /// the same reason. Counted so the anomaly can be reported once instead of per packet.
    unusable_timestamps: u64,
    dropped_stretches: u64,
}

impl AudioTimeline {
    pub fn new(format: AudioFormat, origin: Duration) -> Self {
        Self {
            format,
            origin,
            placed: 0,
            emitted: 0,
            pending: Vec::new(),
            fills: VecDeque::new(),
            scratch: Vec::new(),
            unusable_timestamps: 0,
            dropped_stretches: 0,
        }
    }

    fn bytes_per_frame(&self) -> usize {
        self.format.channels as usize * 2
    }

    fn block_frames(&self) -> u64 {
        u64::from(self.format.sample_rate / 100)
    }

    /// The most lateness treated as already covered, in frames.
    fn max_fill_frames(&self) -> u64 {
        u64::from(self.format.sample_rate).saturating_mul(MAX_FILL.as_secs())
    }

    /// The most silence one gap may materialise, in frames.
    fn max_pad_frames(&self) -> u64 {
        u64::from(self.format.sample_rate).saturating_mul(MAX_PAD.as_secs())
    }

    /// The frame index `time` falls on, rounded; anything before the origin is frame 0.
    fn frame_at(&self, time: Duration) -> u64 {
        let nanos = time.saturating_sub(self.origin).as_nanos();
        let frames = (nanos * u128::from(self.format.sample_rate) + 500_000_000) / 1_000_000_000;
        // Saturate rather than truncate: a time far outside the recording would otherwise wrap
        // into an arbitrary frame index, and the timeline would pad that many frames of silence.
        u64::try_from(frames).unwrap_or(u64::MAX)
    }

    fn time_of(&self, frame: u64) -> Duration {
        let nanos = u128::from(frame) * 1_000_000_000 / u128::from(self.format.sample_rate);
        self.origin.saturating_add(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }

    /// Pull and place up to `max` packets. Returns whether the source ran dry, the only state in
    /// which padding up to the live edge cannot overwrite audio still queued in the engine.
    pub fn drain(&mut self, source: &mut impl PacketSource, max: usize) -> Result<bool> {
        for _ in 0..max {
            let mut scratch = std::mem::take(&mut self.scratch);
            scratch.clear();
            let pulled = source.pull(&mut scratch);
            let info = match pulled {
                Ok(Some(info)) => info,
                other => {
                    self.scratch = scratch;
                    return other.map(|_| true);
                }
            };
            self.place(info, &scratch);
            self.scratch = scratch;
        }
        Ok(false)
    }

    /// Put one packet where its timestamp says. Within one block of the running count it is just
    /// appended, so timestamp jitter never chops a steady stream; a discontinuity is placed exactly
    /// because the engine has said samples are missing.
    ///
    /// A gap ahead of the running count is padded with silence up to [`MAX_PAD`]: the pump thread
    /// stalling for several seconds is a gap the recording can account for, and the video carries
    /// those seconds too.
    ///
    /// A timestamp the recording cannot account for - further away than that - is not
    /// obeyed: the packet follows the previous one instead. Such a timestamp is not a gap the
    /// device left silent, it is a reading from another clock, and padding it would ask for
    /// gigabytes of zeroes.
    pub fn place(&mut self, info: PacketInfo, samples: &[u8]) {
        let bytes_per_frame = self.bytes_per_frame();
        let frames = (samples.len() / bytes_per_frame).min(info.frames) as u64;
        let mut skip = 0;
        if let Some(time) = info.time {
            let at = self.frame_at(time);
            let tolerance = if info.discontinuity {
                0
            } else {
                self.block_frames()
            };
            if at > self.placed.saturating_add(tolerance) {
                let gap = at - self.placed;
                if gap <= self.max_pad_frames() {
                    self.pad(gap);
                } else {
                    self.note_unusable_timestamp(gap);
                }
            } else if at.saturating_add(tolerance) < self.placed {
                let late = self.placed - at;
                if late <= self.max_fill_frames() {
                    // The stream already covers this stretch (padding that ran ahead of a late packet).
                    skip = late.min(frames);
                } else {
                    // Too late to be jitter, and too late to be a stretch this recording can
                    // account for either. Dropping the packet would lose real audio over a
                    // timestamp nobody can use, so the packet follows the previous one.
                    self.note_unusable_timestamp(late);
                }
            }
        }
        let start = (skip as usize).saturating_mul(bytes_per_frame);
        let end = (frames as usize).saturating_mul(bytes_per_frame);
        self.pending.extend_from_slice(&samples[start..end]);
        self.placed = self.placed.saturating_add(frames - skip);
    }

    /// Pad with silence up to `LIVE_LAG` before `now`, so a stretch with no packets still advances
    /// the audio at the video's pace instead of arriving later as one burst.
    ///
    /// A live edge ahead by up to [`MAX_PAD`] is a stall the recording can account for and is
    /// padded, so the audio never loses wall-clock time the video kept. One further ahead is a
    /// clock the recording cannot follow - the session was suspended, or the clock was set - and
    /// the stretch is dropped rather than filled. The timeline carries on at the live edge, which is where the device's next packet
    /// lands.
    pub fn fill_to(&mut self, now: Duration) {
        let at = self.frame_at(now.saturating_sub(LIVE_LAG));
        if at > self.placed {
            let gap = at - self.placed;
            if gap <= self.max_pad_frames() {
                self.pad(gap);
            } else {
                self.drop_stretch(at);
            }
        }
    }

    /// Count a timestamp the recording cannot account for, and say so once.
    ///
    /// The audio is still captured, but the track is no longer aligned to the clock to the
    /// millisecond, and the reason is a device timestamp that does not belong to this recording.
    fn note_unusable_timestamp(&mut self, distance: u64) {
        self.unusable_timestamps += 1;
        if self.unusable_timestamps == 1 {
            tracing::warn!(
                distance_secs = distance as f64 / f64::from(self.format.sample_rate),
                "an audio packet carried a timestamp the recording cannot account for, so the \
                 timestamp is ignored: the audio carries on on the clock instead of the gap being \
                 padded with silence"
            );
        }
    }

    /// Move the timeline to `frames`, the live edge, after a stretch it cannot account for.
    ///
    /// The buffered audio is kept: it is the last stretch before the clock jumped and it is still
    /// worth having. What is dropped is the stretch itself, which is why the frame index moves
    /// without the bytes moving with it.
    fn drop_stretch(&mut self, frames: u64) {
        self.dropped_stretches += 1;
        if self.dropped_stretches == 1 {
            tracing::warn!(
                "the clock moved further ahead than the recording can account for, so the stretch \
                 is dropped instead of being padded with silence: the audio track continues from \
                 the live edge and is shorter than the video by the length of that stretch"
            );
        }
        self.fills.clear();
        self.placed = frames;
        self.emitted = frames;
    }

    fn pad(&mut self, frames: u64) {
        // The last line of defence: a gap is never materialised beyond the bound, whatever a
        // caller asks for, so no allocation can be sized by a timestamp alone.
        let frames = frames.min(self.max_pad_frames());
        let start = self.placed;
        let end = start.saturating_add(frames);
        match self.fills.back_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => self.fills.push_back((start, end)),
        }
        let bytes = usize::try_from(frames)
            .unwrap_or(usize::MAX)
            .saturating_mul(self.bytes_per_frame());
        let len = self.pending.len().saturating_add(bytes);
        self.pending.resize(len, 0);
        self.placed = end;
    }

    /// The next whole 10ms block, stamped from its position on the timeline.
    pub fn next_block(&mut self) -> Option<Block> {
        let block_frames = self.block_frames();
        let block_bytes = (block_frames as usize).saturating_mul(self.bytes_per_frame());
        if self.pending.len() < block_bytes {
            return None;
        }
        let data: Vec<u8> = self.pending.drain(..block_bytes).collect();
        let (start, end) = (self.emitted, self.emitted.saturating_add(block_frames));
        let filler = self
            .fills
            .iter()
            .any(|&(from, to)| from <= start && end <= to);
        while self.fills.front().is_some_and(|&(_, to)| to <= end) {
            self.fills.pop_front();
        }
        let pts = self.time_of(start);
        self.emitted = end;
        Some(Block {
            buffer: AudioBuffer {
                data,
                frames: block_frames as usize,
                pts,
                format: self.format,
            },
            filler,
        })
    }
}

/// The QPC (in ticks since boot) as the audio engine reports it: `10,000,000 * t / f`.
///
/// The engine computes this in 64-bit arithmetic, so this does too: the multiply wraps after
/// 51.24 hours of machine uptime (2^64 / 10^7 ticks), and an anchor computed exactly in 128 bits
/// would no longer be the same kind of number as the packet timestamps it is subtracted from.
/// Computing both the same way is what keeps them on one scale.
pub(crate) fn qpc_to_hns(counter: u64, frequency: u64) -> u64 {
    counter.wrapping_mul(10_000_000) / frequency
}

/// A device QPC position (100ns units) on the shared clock, given one instant read on both:
/// `anchor_hns` on the QPC and `anchor` on the shared clock. `None` before the shared clock began.
pub(crate) fn qpc_to_shared(hns: u64, anchor_hns: u64, anchor: Duration) -> Option<Duration> {
    if hns >= anchor_hns {
        Some(anchor.saturating_add(Duration::from_nanos((hns - anchor_hns).saturating_mul(100))))
    } else {
        anchor.checked_sub(Duration::from_nanos((anchor_hns - hns).saturating_mul(100)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    const BLOCK: usize = 480;
    const ORIGIN: Duration = Duration::from_secs(5);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A packet of `frames` frames, every sample `value` (0 for a silent packet).
    fn packet(
        at: Option<Duration>,
        frames: usize,
        value: u8,
        discontinuity: bool,
    ) -> (PacketInfo, Vec<u8>) {
        (
            PacketInfo {
                frames,
                time: at,
                discontinuity,
            },
            vec![value; frames * 4],
        )
    }

    /// Plays back a scripted list of packets, then reports nothing waiting.
    struct FakeSource(VecDeque<(PacketInfo, Vec<u8>)>);

    impl FakeSource {
        fn new(packets: Vec<(PacketInfo, Vec<u8>)>) -> Self {
            Self(packets.into())
        }
    }

    impl PacketSource for FakeSource {
        fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<PacketInfo>> {
            Ok(self.0.pop_front().map(|(info, data)| {
                out.extend_from_slice(&data);
                info
            }))
        }
    }

    /// Drain `source` completely and return every block it produced.
    fn run(packets: Vec<(PacketInfo, Vec<u8>)>) -> (Vec<Block>, AudioTimeline) {
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        let mut source = FakeSource::new(packets);
        while !timeline.drain(&mut source, 16).unwrap() {}
        let mut blocks = Vec::new();
        while let Some(block) = timeline.next_block() {
            blocks.push(block);
        }
        (blocks, timeline)
    }

    /// The first byte of every frame across `blocks`, in order: one value per frame on the timeline.
    fn frames(blocks: &[Block]) -> Vec<u8> {
        blocks
            .iter()
            .flat_map(|b| b.buffer.data.chunks_exact(4).map(|f| f[0]))
            .collect()
    }

    /// The gap the 4K recording on a real Windows 11 machine died on: a device timestamp
    /// 2^32 ten-millisecond blocks - 42,949,672.96 seconds, 497 days - ahead of the origin.
    /// Padding it asked the allocator for 8,246,337,208,320 bytes and the process was aborted.
    const ABORT_GAP: Duration = Duration::from_nanos((1u64 << 32) * 10_000_000);

    /// The most silence the timeline may hold, in bytes.
    fn max_fill_bytes() -> usize {
        FORMAT.sample_rate as usize * MAX_PAD.as_secs() as usize * 4
    }

    #[test]
    fn a_timestamp_the_recording_cannot_account_for_is_ignored() {
        // The device says the packet belongs 497 days into the recording. No stretch of a
        // recording can account for that, so the timestamp is not obeyed: the audio is kept and
        // the gap is not padded with 8TB of zeroes.
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        let (info, samples) = packet(Some(ORIGIN + ABORT_GAP), BLOCK, 7, false);
        timeline.place(info, &samples);

        assert!(
            timeline.pending.len() <= max_fill_bytes() + BLOCK * 4,
            "the gap was not materialised: {} bytes buffered",
            timeline.pending.len()
        );
        assert_eq!(
            timeline.placed, BLOCK as u64,
            "the packet is on the timeline"
        );
        let block = timeline.next_block().expect("a block");
        assert!(
            block.buffer.data.iter().all(|&s| s == 7),
            "the audio is kept, only the timestamp is ignored"
        );
    }

    #[test]
    fn the_silence_a_gap_may_hold_is_bounded() {
        // A gap of an hour is still a gap, not a stretch to fill with an hour of zeroes.
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        let (info, samples) = packet(Some(ORIGIN + Duration::from_secs(3600)), BLOCK, 7, false);
        timeline.place(info, &samples);

        assert!(
            timeline.pending.len() <= max_fill_bytes() + BLOCK * 4,
            "{} bytes buffered for a one hour gap",
            timeline.pending.len()
        );
    }

    #[test]
    fn a_gap_within_the_bound_is_still_filled_with_silence() {
        // A second is a gap a recording can account for, and it is padded as before.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + MAX_FILL), BLOCK, 2, false),
        ];
        let (blocks, timeline) = run(packets);
        let frames = frames(&blocks);
        let silence = MAX_FILL.as_secs() as usize * 100 * BLOCK;

        assert_eq!(frames.len(), silence + BLOCK);
        assert!(
            frames[BLOCK..silence].iter().all(|&s| s == 0),
            "the gap is silence"
        );
        assert!(frames[silence..].iter().all(|&s| s == 2));
        assert!(timeline.pending.is_empty());
    }

    #[test]
    fn a_stall_of_seconds_before_a_packet_is_padded_so_the_audio_is_not_early() {
        // The pump thread stalled for ten seconds. The video carried those ten seconds (it is on
        // the wall clock), and the audio is timed by byte count alone, so anything short of the
        // full ten seconds of silence plays every later sample ten seconds early.
        let stall = Duration::from_secs(10);
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + stall), BLOCK, 2, false),
        ];
        let (blocks, timeline) = run(packets);
        let frames = frames(&blocks);
        let silence = stall.as_secs() as usize * 48_000 - BLOCK;

        assert_eq!(
            frames.len(),
            BLOCK + silence + BLOCK,
            "the stall is on the timeline, not dropped"
        );
        assert!(frames[BLOCK..BLOCK + silence].iter().all(|&s| s == 0));
        assert!(frames[BLOCK + silence..].iter().all(|&s| s == 2));
        assert_eq!(timeline.unusable_timestamps, 0, "the timestamp was obeyed");
    }

    #[test]
    fn a_live_edge_seconds_ahead_is_padded_not_dropped() {
        // No packets for twenty seconds (the poll loop was not running): the timeline catches up
        // with the clock by padding, so the next packet lands where the video says it does.
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        let now = ORIGIN + Duration::from_secs(20);
        timeline.fill_to(now);

        assert_eq!(timeline.dropped_stretches, 0, "nothing was dropped");
        assert_eq!(
            timeline.placed,
            timeline.frame_at(now - LIVE_LAG),
            "the timeline is level with the live edge less the lag"
        );
        assert_eq!(
            timeline.emitted, 0,
            "none of it was skipped over: it is all still to be emitted"
        );
        assert_eq!(
            timeline.pending.len() as u64,
            timeline.placed * 4,
            "and it is silence, materialised"
        );
    }

    #[test]
    fn a_clock_that_jumped_ahead_does_not_materialise_the_gap() {
        // The clock moved 497 days forward in one step: a suspended session, or a clock that was
        // set. The stretch is dropped, and the timeline carries on at the live edge.
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        timeline.fill_to(ORIGIN + ABORT_GAP);

        assert!(
            timeline.pending.len() <= max_fill_bytes(),
            "{} bytes buffered for a clock that jumped",
            timeline.pending.len()
        );

        // The next packet lands at the live edge and is captured.
        let (info, samples) = packet(Some(ORIGIN + ABORT_GAP), BLOCK, 7, false);
        timeline.place(info, &samples);
        let mut blocks = Vec::new();
        while let Some(block) = timeline.next_block() {
            blocks.push(block);
        }
        assert!(
            frames(&blocks).contains(&7),
            "the audio after the jump is captured"
        );
    }

    #[test]
    fn a_timestamp_too_late_to_be_jitter_keeps_the_audio() {
        // The timeline is eleven seconds in, and the packet claims to belong before the
        // recording started: too late to be jitter, and too far back for any stretch of this
        // recording to account for. The audio is kept rather than dropped over a timestamp
        // nobody can use.
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        let (info, samples) = packet(Some(ORIGIN), 11 * 48_000, 1, false);
        timeline.place(info, &samples);
        let (info, samples) = packet(Some(Duration::ZERO), BLOCK, 2, false);
        timeline.place(info, &samples);

        let mut blocks = Vec::new();
        while let Some(block) = timeline.next_block() {
            blocks.push(block);
        }
        let frames = frames(&blocks);
        assert!(
            frames.contains(&2),
            "the late packet's audio is kept, not dropped"
        );
    }

    #[test]
    fn a_frame_index_beyond_the_recording_saturates_instead_of_wrapping() {
        let timeline = AudioTimeline::new(FORMAT, ORIGIN);
        assert_eq!(
            timeline.frame_at(Duration::MAX),
            u64::MAX,
            "a time beyond the recording saturates, it does not wrap into a frame index"
        );
    }

    #[test]
    fn the_qpc_anchor_is_the_value_the_audio_engine_computes() {
        // The audio engine puts the QPC of every packet in its buffer as 10,000,000 * t / f,
        // computed in 64-bit arithmetic, so the multiply wraps after 51.24 hours of machine
        // uptime. The anchor has to be that same number, or the two are not on one scale and
        // every packet timestamp comes out nonsense.
        let frequency = 10_000_000u64; // the 10MHz QPC of a typical machine
        for hours in [1, 60, 60 * 24 * 30] {
            let counter = hours * 3600 * frequency;
            let engine = counter.wrapping_mul(10_000_000) / frequency;
            assert_eq!(
                qpc_to_hns(counter, frequency),
                engine,
                "the anchor after {hours} hours of uptime is the engine's own number"
            );
        }
    }

    #[test]
    fn a_packet_timestamp_stays_on_the_shared_clock_after_the_engine_wraps() {
        let frequency = 10_000_000u64;
        let uptime = 60 * 3600 * frequency; // 60 hours of ticks: the engine's multiply has wrapped
        let anchor_hns = qpc_to_hns(uptime, frequency);
        // The packet's own timestamp, as the engine computes it.
        let packet_hns = (uptime + frequency).wrapping_mul(10_000_000) / frequency;
        let anchor = Duration::from_secs(10);

        assert_eq!(
            qpc_to_shared(packet_hns, anchor_hns, anchor),
            Some(anchor + Duration::from_secs(1)),
            "a packet one second after the anchor is one second later on the shared clock"
        );
    }

    #[test]
    fn a_steady_stream_passes_through_byte_for_byte() {
        let packets = (0..10)
            .map(|i| packet(Some(ORIGIN + ms(10 * i)), BLOCK, i as u8 + 1, false))
            .collect();
        let (blocks, timeline) = run(packets);

        assert_eq!(blocks.len(), 10);
        for (i, block) in blocks.iter().enumerate() {
            assert!(
                block.buffer.data.iter().all(|&b| b == i as u8 + 1),
                "block {i} is packet {i}"
            );
            assert_eq!(block.buffer.pts, ORIGIN + ms(10 * i as u64));
            assert!(!block.filler);
        }
        assert!(timeline.pending.is_empty());
    }

    #[test]
    fn jitter_within_a_block_does_not_move_the_stream() {
        // Timestamps 3ms early and late: still contiguous, nothing padded or trimmed.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + ms(13)), BLOCK, 2, false),
            packet(Some(ORIGIN + ms(17)), BLOCK, 3, false),
        ];
        let (blocks, _) = run(packets);
        assert_eq!(
            frames(&blocks),
            [vec![1; BLOCK], vec![2; BLOCK], vec![3; BLOCK]].concat()
        );
    }

    #[test]
    fn a_gap_between_packets_is_filled_with_silence() {
        // Nothing played for 40ms: the second packet must start at frame 2400, not 480.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + ms(50)), BLOCK, 2, false),
        ];
        let (blocks, _) = run(packets);
        let frames = frames(&blocks);

        assert_eq!(frames.len(), 6 * BLOCK);
        assert!(frames[..BLOCK].iter().all(|&s| s == 1));
        assert!(
            frames[BLOCK..5 * BLOCK].iter().all(|&s| s == 0),
            "40ms of silence"
        );
        assert!(frames[5 * BLOCK..].iter().all(|&s| s == 2));
        let filler: Vec<bool> = blocks.iter().map(|b| b.filler).collect();
        assert_eq!(filler, [false, true, true, true, true, false]);
        assert_eq!(
            blocks[5].buffer.pts,
            ORIGIN + ms(50),
            "the late packet keeps its own time"
        );
    }

    #[test]
    fn a_discontinuity_is_placed_exactly_even_under_the_jitter_tolerance() {
        // 5ms (240 frames) missing: ignored as jitter without the flag, filled exactly with it.
        let gap = |discontinuity| {
            let packets = vec![
                packet(Some(ORIGIN), BLOCK, 1, false),
                packet(Some(ORIGIN + ms(15)), BLOCK, 2, discontinuity),
            ];
            let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
            while !timeline
                .drain(&mut FakeSource::new(packets.clone()), 16)
                .unwrap()
            {}
            timeline.placed
        };
        assert_eq!(gap(false), 2 * BLOCK as u64);
        assert_eq!(gap(true), 2 * BLOCK as u64 + 240);
    }

    #[test]
    fn a_silent_packet_holds_its_place_on_the_timeline() {
        // The source writes a silent packet as zeros; it is real time, not a gap, so nothing is padded.
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(Some(ORIGIN + ms(10)), 2 * BLOCK, 0, false),
            packet(Some(ORIGIN + ms(30)), BLOCK, 3, false),
        ];
        let (blocks, _) = run(packets);
        assert_eq!(
            frames(&blocks),
            [vec![1; BLOCK], vec![0; 2 * BLOCK], vec![3; BLOCK]].concat()
        );
        assert!(
            blocks.iter().all(|b| !b.filler),
            "device silence is not filler"
        );
    }

    #[test]
    fn a_packet_overlapping_what_is_already_placed_is_trimmed() {
        // The second packet claims to start 20ms after the first, which already covers 40ms.
        let packets = vec![
            packet(Some(ORIGIN), 4 * BLOCK, 1, false),
            packet(Some(ORIGIN + ms(20)), 3 * BLOCK, 2, false),
        ];
        let (blocks, _) = run(packets);
        let frames = frames(&blocks);
        assert_eq!(frames.len(), 5 * BLOCK, "only the part past 40ms survives");
        assert!(frames[4 * BLOCK..].iter().all(|&s| s == 2));
    }

    #[test]
    fn a_packet_without_a_timestamp_follows_the_previous_one() {
        let packets = vec![
            packet(Some(ORIGIN), BLOCK, 1, false),
            packet(None, BLOCK, 2, false),
        ];
        let (blocks, _) = run(packets);
        assert_eq!(frames(&blocks), [vec![1; BLOCK], vec![2; BLOCK]].concat());
    }

    #[test]
    fn with_no_packets_silence_keeps_pace_with_the_clock_behind_the_live_edge() {
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        assert!(timeline.drain(&mut FakeSource::new(vec![]), 16).unwrap());
        timeline.fill_to(ORIGIN + Duration::from_secs(1));

        let mut blocks = Vec::new();
        while let Some(block) = timeline.next_block() {
            blocks.push(block);
        }
        assert_eq!(blocks.len(), 90, "one second less the 100ms lag");
        assert!(blocks
            .iter()
            .all(|b| b.filler && b.buffer.data.iter().all(|&s| s == 0)));

        // The engine resumes: its first packet lands at its own time, after the rest of the gap.
        let resumed = packet(Some(ORIGIN + Duration::from_secs(1)), BLOCK, 7, false);
        timeline
            .drain(&mut FakeSource::new(vec![resumed]), 16)
            .unwrap();
        assert_eq!(timeline.placed, 48_000 + BLOCK as u64);
        let mut tail = Vec::new();
        while let Some(block) = timeline.next_block() {
            tail.push(block);
        }
        assert_eq!(tail.len(), 11);
        assert!(tail[..10].iter().all(|b| b.filler));
        assert_eq!(tail[10].buffer.pts, ORIGIN + Duration::from_secs(1));
        assert!(!tail[10].filler && tail[10].buffer.data.iter().all(|&s| s == 7));
    }

    #[test]
    fn filling_never_runs_ahead_of_the_live_lag() {
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        timeline.fill_to(ORIGIN + LIVE_LAG);
        assert_eq!(timeline.placed, 0);
        timeline.fill_to(ORIGIN); // before the lag has passed: nothing, not an underflow
        assert_eq!(timeline.placed, 0);
    }

    #[test]
    fn drain_reports_a_source_that_still_has_packets() {
        let packets = (0..20)
            .map(|i| packet(Some(ORIGIN + ms(10 * i)), BLOCK, 1, false))
            .collect();
        let mut source = FakeSource::new(packets);
        let mut timeline = AudioTimeline::new(FORMAT, ORIGIN);
        assert!(
            !timeline.drain(&mut source, 16).unwrap(),
            "four packets are still waiting"
        );
        assert!(timeline.drain(&mut source, 16).unwrap());
        assert_eq!(timeline.placed, 20 * BLOCK as u64);
    }

    #[test]
    fn a_qpc_position_converts_onto_the_shared_clock() {
        let anchor = Duration::from_secs(10);
        assert_eq!(qpc_to_shared(1_000_000, 1_000_000, anchor), Some(anchor));
        assert_eq!(
            qpc_to_shared(1_000_010, 1_000_000, anchor),
            Some(anchor + Duration::from_micros(1))
        );
        assert_eq!(
            qpc_to_shared(900_000, 1_000_000, anchor),
            Some(anchor - ms(10))
        );
        assert_eq!(
            qpc_to_shared(0, 200_000_000, anchor),
            None,
            "before the shared clock began"
        );
    }
}
