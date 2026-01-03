// Adaptive audio jitter buffer using industry-standard approaches
//
// This module provides an adaptive jitter buffer with packet loss concealment
// for smooth audio playback over networks with varying jitter and packet loss.

use std::{collections::VecDeque, time::Instant};

use tracing::{info, trace, warn};

/// Adaptive audio jitter buffer with dynamic buffer sizing
///
/// This implementation provides:
/// - Adaptive buffer management based on observed jitter
/// - Packet loss concealment through silence insertion
/// - Dynamic buffer size adjustment
/// - EMA-based jitter estimation
pub struct AdaptiveJitterBuffer {
    /// Buffered audio frames
    buffer: VecDeque<AudioFrame>,
    /// Leftover samples from previous frame (for partial reads)
    leftover_samples: Vec<f32>,
    /// Current target buffer size (in number of frames)
    target_buffer_size: usize,
    /// Minimum buffer size before playback starts/resumes
    min_buffer_size: usize,
    /// Maximum buffer size before dropping frames
    max_buffer_size: usize,
    /// Whether playback has started
    started_playback: bool,
    /// Statistics
    stats: BufferStats,
    /// Jitter estimation (EMA of inter-arrival variance)
    jitter_ema: Option<f32>,
    /// Expected frame duration in ms (for jitter calculation)
    expected_interval_ms: f32,
    /// Last packet arrival time
    last_arrival: Option<Instant>,
    /// Number of ticks with stable conditions
    stable_ticks: u32,
}

#[derive(Debug, Clone)]
struct AudioFrame {
    /// Audio samples (interleaved)
    samples: Vec<f32>,
    /// Timestamp when received
    #[allow(dead_code)]
    received_at: Instant,
}

#[derive(Debug, Default)]
pub struct BufferStats {
    pub packets_received: u64,
    pub underruns: u32,
    pub overruns: u32,
    pub frames_dropped: u32,
}

impl AdaptiveJitterBuffer {
    /// Create a new adaptive jitter buffer
    ///
    /// # Arguments
    /// * `initial_target_ms` - Initial target buffer depth in milliseconds (e.g., 60ms)
    /// * `frame_duration_ms` - Duration of each audio frame in milliseconds (e.g., 20ms for Opus)
    pub fn new(initial_target_ms: u32, frame_duration_ms: u32) -> Self {
        let target_frames = (initial_target_ms / frame_duration_ms).max(5) as usize;
        let min_frames = (target_frames / 2).max(3); // At least 3 frames before playback
        let max_frames = target_frames * 3; // Triple target as maximum

        info!(
            "adaptive jitter buffer: target={}ms ({}frames), min={} frames, max={} frames",
            initial_target_ms,
            target_frames,
            min_frames,
            max_frames
        );

        Self {
            buffer: VecDeque::with_capacity(max_frames),
            leftover_samples: Vec::new(),
            target_buffer_size: target_frames,
            min_buffer_size: min_frames,
            max_buffer_size: max_frames,
            started_playback: false,
            stats: Default::default(),
            jitter_ema: None,
            expected_interval_ms: frame_duration_ms as f32,
            last_arrival: None,
            stable_ticks: 0,
        }
    }

    /// Insert audio samples into the buffer
    pub fn insert(&mut self, samples: Vec<f32>) {
        let now = Instant::now();
        
        // Track jitter (deviation from expected interval)
        if let Some(last) = self.last_arrival {
            let interval_ms = now.duration_since(last).as_secs_f32() * 1000.0;
            // Jitter = deviation from expected interval
            let jitter = (interval_ms - self.expected_interval_ms).abs();
            
            // Update jitter EMA
            match self.jitter_ema {
                Some(prev_ema) => {
                    // Use a smoothing factor of 0.1 for slow adaptation
                    self.jitter_ema = Some(0.1 * jitter + 0.9 * prev_ema);
                }
                None => {
                    self.jitter_ema = Some(jitter);
                }
            }
        }
        self.last_arrival = Some(now);

        // Check for buffer overflow
        if self.buffer.len() >= self.max_buffer_size {
            self.stats.overruns += 1;
            // Drop oldest frame to make room
            self.buffer.pop_front();
            self.stats.frames_dropped += 1;
            warn!(
                "jitter buffer overflow: dropping frame (total dropped: {})",
                self.stats.frames_dropped
            );
        }

        self.buffer.push_back(AudioFrame {
            samples,
            received_at: now,
        });
        
        self.stats.packets_received += 1;

        trace!(
            "jitter buffer: inserted frame, depth={}/{}",
            self.buffer.len(),
            self.target_buffer_size
        );
    }

    /// Get audio samples for playback
    ///
    /// Returns exactly `frame_size` samples, handling partial frame consumption.
    /// Returns `None` only if buffer is not yet ready (initial buffering).
    pub fn get_audio(&mut self, frame_size: usize) -> Option<Vec<f32>> {
        // Adapt buffer size based on observed jitter
        self.adapt_buffer_size();

        // Wait for initial buffering before starting playback
        if !self.started_playback {
            if self.buffer.len() >= self.min_buffer_size {
                info!(
                    "jitter buffer: starting playback with {} frames buffered (min={})",
                    self.buffer.len(),
                    self.min_buffer_size
                );
                self.started_playback = true;
            } else {
                trace!(
                    "jitter buffer: waiting for initial buffer ({}/{})",
                    self.buffer.len(),
                    self.min_buffer_size
                );
                return None;
            }
        }

        // Build output from leftover samples + new frames as needed
        let mut output = Vec::with_capacity(frame_size);
        
        // First, use any leftover samples from the previous call
        if !self.leftover_samples.is_empty() {
            let take = self.leftover_samples.len().min(frame_size);
            output.extend(self.leftover_samples.drain(..take));
        }
        
        // Pull frames from buffer until we have enough samples
        while output.len() < frame_size {
            if let Some(frame) = self.buffer.pop_front() {
                let needed = frame_size - output.len();
                if frame.samples.len() <= needed {
                    // Use entire frame
                    output.extend(&frame.samples);
                } else {
                    // Use part of frame, save rest for later
                    output.extend(&frame.samples[..needed]);
                    self.leftover_samples.extend(&frame.samples[needed..]);
                }
                trace!(
                    "jitter buffer: consuming frame, remaining={}",
                    self.buffer.len()
                );
            } else {
                // Underrun - no more frames available
                self.stats.underruns += 1;
                
                // Only log every 10th underrun to avoid spam
                if self.stats.underruns % 10 == 1 {
                    warn!(
                        "jitter buffer underrun #{} - filling with silence ({} samples short)",
                        self.stats.underruns,
                        frame_size - output.len()
                    );
                }
                
                // Increase target buffer size to prevent future underruns
                self.target_buffer_size = (self.target_buffer_size + 2).min(self.max_buffer_size);
                self.stable_ticks = 0;

                // Fill remaining with silence
                output.resize(frame_size, 0.0f32);
                
                // Reset playback to rebuild buffer if we're completely empty
                if self.buffer.is_empty() && self.leftover_samples.is_empty() {
                    self.started_playback = false;
                }
                break;
            }
        }
        
        Some(output)
    }

    /// Adapt buffer size based on network conditions
    fn adapt_buffer_size(&mut self) {
        let current_jitter = self.jitter_ema.unwrap_or(0.0);
        let buffer_depth = self.buffer.len();

        // FAST reaction to problems: increase buffer immediately
        if current_jitter > 25.0 || buffer_depth < self.min_buffer_size {
            self.target_buffer_size = (self.target_buffer_size + 2).min(self.max_buffer_size);
            self.stable_ticks = 0;
            trace!(
                "jitter buffer: increasing target to {} (jitter={:.1}ms, depth={})",
                self.target_buffer_size,
                current_jitter,
                buffer_depth
            );
        }
        // SLOW reaction to good conditions: reduce only after sustained stability
        else if current_jitter < 10.0 && buffer_depth > self.target_buffer_size + 3 {
            self.stable_ticks += 1;
            
            // Only reduce after 3 seconds of stability (assuming 10ms ticks = 300 ticks)
            if self.stable_ticks > 300 && self.target_buffer_size > self.min_buffer_size {
                self.target_buffer_size = self.target_buffer_size.saturating_sub(1);
                self.stable_ticks = 0;
                trace!(
                    "jitter buffer: reducing target to {} (sustained low jitter)",
                    self.target_buffer_size
                );
            }
        } else {
            // Neutral zone - no adjustment
            self.stable_ticks = 0;
        }
    }

    /// Get current buffer statistics
    pub fn stats(&self) -> &BufferStats {
        &self.stats
    }

    /// Get current buffer depth
    pub fn buffer_depth(&self) -> usize {
        self.buffer.len()
    }

    /// Get target buffer size
    pub fn target_size(&self) -> usize {
        self.target_buffer_size
    }

    /// Get current jitter estimate in milliseconds
    pub fn jitter_ms(&self) -> f32 {
        self.jitter_ema.unwrap_or(0.0)
    }
}

impl BufferStats {
    pub fn log_summary(&self) {
        info!(
            "jitter buffer stats: packets={}, underruns={}, overruns={}, dropped={}",
            self.packets_received,
            self.underruns,
            self.overruns,
            self.frames_dropped
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_buffering() {
        let mut buffer = AdaptiveJitterBuffer::new(60, 20); // 60ms target, 20ms frames
        
        // Buffer should initially be empty
        assert_eq!(buffer.buffer_depth(), 0);
        
        // Insert frames
        for _ in 0..5 {
            buffer.insert(vec![0.0f32; 960]); // 20ms of 48kHz mono audio
        }
        
        assert_eq!(buffer.buffer_depth(), 5);
    }

    #[test]
    fn test_playback_starts_after_min_buffer() {
        let mut buffer = AdaptiveJitterBuffer::new(60, 20);
        
        // Should not play before minimum buffer
        assert!(buffer.get_audio(960).is_none());
        
        // Fill to minimum
        for _ in 0..3 {
            buffer.insert(vec![0.0f32; 960]);
        }
        
        // Should now play
        assert!(buffer.get_audio(960).is_some());
    }

    #[test]
    fn test_underrun_recovery() {
        let mut buffer = AdaptiveJitterBuffer::new(60, 20);
        
        // Fill and start playback
        for _ in 0..5 {
            buffer.insert(vec![0.0f32; 960]);
        }
        buffer.get_audio(960);
        
        // Drain buffer to cause underrun
        for _ in 0..10 {
            buffer.get_audio(960);
        }
        
        // Should have registered underruns
        assert!(buffer.stats().underruns > 0);
        
        // Should return silence during underrun
        let audio = buffer.get_audio(960).unwrap();
        assert_eq!(audio.len(), 960);
        assert!(audio.iter().all(|&s| s == 0.0));
    }
}

