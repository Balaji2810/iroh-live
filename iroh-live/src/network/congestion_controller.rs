//! Google Congestion Control (GCC) implementation
//!
//! Provides bandwidth estimation and bitrate adaptation based on
//! delay-based and loss-based signals from the network.

use std::time::{Duration, Instant};
use anyhow::Result;
use goog_cc::{
    GoogCcNetworkController, GoogCcConfig,
    experiments::FieldTrials,
    network_control::{NetworkControllerInterface, NetworkControllerConfig},
    transport::{PacketResult, TransportPacketsFeedback, TargetRateConstraints, NetworkStateEstimate, StreamsConfig},
    units::{DataRate, DataSize, Timestamp, TimeDelta},
};
use tracing::{debug, info, trace, warn};

/// Configuration for congestion controller
#[derive(Debug, Clone)]
pub struct CongestionControllerConfig {
    /// Initial target bitrate (bits per second)
    pub initial_bitrate_bps: u64,
    /// Minimum allowed bitrate (bits per second)
    pub min_bitrate_bps: u64,
    /// Maximum allowed bitrate (bits per second)
    pub max_bitrate_bps: u64,
    /// How often to probe for higher bandwidth
    pub probe_interval: Duration,
}

impl Default for CongestionControllerConfig {
    fn default() -> Self {
        Self {
            initial_bitrate_bps: 1_000_000,      // 1 Mbps
            min_bitrate_bps: 128_000,            // 128 kbps
            max_bitrate_bps: 10_000_000,         // 10 Mbps
            probe_interval: Duration::from_secs(30),
        }
    }
}

/// Current bitrate estimate from congestion control
#[derive(Debug, Clone, Copy)]
pub struct BitrateEstimate {
    /// Target bitrate in bits per second
    pub target_bps: u64,
    /// Estimated available bandwidth
    pub bandwidth_bps: u64,
    /// Current RTT estimate
    pub rtt: Duration,
    /// Packet loss rate (0.0 to 1.0)
    pub loss_rate: f32,
}

/// Google Congestion Control state manager
pub struct CongestionController {
    /// GCC algorithm instance
    gcc: GoogCcNetworkController,
    /// Configuration
    config: CongestionControllerConfig,
    /// Current bitrate estimate
    current_estimate: BitrateEstimate,
    /// Last probe time
    last_probe: Instant,
    /// Packet sequence number
    next_sequence: u64,
    /// Start time for timestamp calculations
    start_time: Instant,
    /// Total packets sent
    total_sent: u64,
    /// Total packets lost
    total_lost: u64,
}

impl CongestionController {
    /// Create a new congestion controller
    pub fn new(config: CongestionControllerConfig) -> Self {
        let now = Instant::now();
        
        // Initialize GCC with configuration
        let gcc_config = GoogCcConfig {
            feedback_only: false,
        };
        
        // Create initial network controller config with rate constraints
        let network_config = NetworkControllerConfig {
            constraints: TargetRateConstraints {
                at_time: Timestamp::from_millis(0),
                min_data_rate: Some(DataRate::from_bits_per_sec(config.min_bitrate_bps as i64)),
                max_data_rate: Some(DataRate::from_bits_per_sec(config.max_bitrate_bps as i64)),
                starting_rate: Some(DataRate::from_bits_per_sec(config.initial_bitrate_bps as i64)),
            },
            stream_based_config: StreamsConfig::default(),
            field_trials: FieldTrials::default(),
        };
        
        let gcc = GoogCcNetworkController::new(network_config, gcc_config);

        let current_estimate = BitrateEstimate {
            target_bps: config.initial_bitrate_bps,
            bandwidth_bps: config.initial_bitrate_bps,
            rtt: Duration::from_millis(50), // Initial estimate
            loss_rate: 0.0,
        };

        info!(
            "congestion controller initialized: initial={}bps, min={}bps, max={}bps",
            config.initial_bitrate_bps,
            config.min_bitrate_bps,
            config.max_bitrate_bps
        );

        Self {
            gcc,
            config,
            current_estimate,
            last_probe: now,
            next_sequence: 0,
            start_time: now,
            total_sent: 0,
            total_lost: 0,
        }
    }

    /// Register that a packet is being sent
    ///
    /// Returns packet metadata that should be tracked for feedback
    pub fn on_packet_sent(&mut self, size_bytes: usize) -> PacketMetadata {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.total_sent += 1;
        
        let now = Instant::now();
        
        trace!(
            "packet sent: seq={}, size={} bytes, time={:?}",
            sequence,
            size_bytes,
            now.duration_since(self.start_time)
        );

        PacketMetadata {
            sequence,
            send_time: now,
            size_bytes,
        }
    }

    /// Process feedback from the receiver
    ///
    /// # Arguments
    /// * `received_packets` - List of packets successfully received with timing info
    /// * `rtt` - Current round-trip time
    pub fn on_feedback(
        &mut self,
        received_packets: &[ReceivedPacketInfo],
        rtt: Duration,
    ) -> Result<()> {
        let now = Instant::now();

        if received_packets.is_empty() {
            return Ok(());
        }

        // Convert received packets to GCC format
        let packet_results: Vec<PacketResult> = received_packets
            .iter()
            .map(|pkt| PacketResult {
                receive_time: self.instant_to_timestamp(pkt.receive_time),
                sent_packet: goog_cc::transport::SentPacket {
                    send_time: self.instant_to_timestamp(pkt.send_time),
                    size: DataSize::from_bytes(1400), // Approximate packet size
                    sequence_number: pkt.sequence as i64,
                    ..Default::default()
                },
                ecn: goog_cc::transport::EcnMarking::NotEct,
            })
            .collect();

        // Create feedback message
        let feedback = TransportPacketsFeedback {
            feedback_time: self.instant_to_timestamp(now),
            packet_feedbacks: packet_results,
            ..Default::default()
        };

        // Process the feedback through GCC
        let update = self.gcc.on_transport_packets_feedback(feedback);

        // Extract target bitrate from update if available
        if let Some(target_rate) = update.target_rate {
            self.current_estimate.target_bps = target_rate.target_rate.bps() as u64;
            self.current_estimate.bandwidth_bps = target_rate.target_rate.bps() as u64;
        }
        
        self.current_estimate.rtt = rtt;

        debug!(
            "congestion controller update: target={}bps, rtt={:?}",
            self.current_estimate.target_bps,
            self.current_estimate.rtt
        );

        Ok(())
    }

    /// Process packet loss information
    pub fn on_packet_loss(&mut self, lost_count: usize) {
        if lost_count == 0 {
            return;
        }

        self.total_lost += lost_count as u64;

        // Calculate loss rate
        self.current_estimate.loss_rate = 
            (self.total_lost as f32) / (self.total_sent as f32).max(1.0);

        warn!(
            "packet loss detected: {} packets lost, total_loss_rate={:.2}%",
            lost_count,
            self.current_estimate.loss_rate * 100.0
        );

        // GCC will handle loss through the feedback mechanism
        // Loss information is part of the TransportPacketsFeedback
    }

    /// Get current bitrate estimate
    pub fn current_estimate(&self) -> BitrateEstimate {
        self.current_estimate
    }

    /// Check if it's time to probe for higher bandwidth
    pub fn should_probe(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_probe) > self.config.probe_interval {
            self.last_probe = now;
            true
        } else {
            false
        }
    }

    /// Get recommended bitrate for video encoding
    pub fn recommended_video_bitrate(&self) -> u64 {
        // Reserve some headroom for audio and overhead
        // Use 80% of target for video
        (self.current_estimate.target_bps as f64 * 0.80) as u64
    }

    /// Get recommended bitrate for audio encoding
    pub fn recommended_audio_bitrate(&self) -> u64 {
        // Clamp audio between 32kbps and 128kbps based on available bandwidth
        let target = (self.current_estimate.target_bps as f64 * 0.10) as u64;
        target.clamp(32_000, 128_000)
    }

    /// Update GCC with current network state
    pub fn update_network_state(&mut self, rtt: Duration, _loss_rate: f32) {
        self.current_estimate.rtt = rtt;
        
        // Send RTT update to GCC through network state estimate
        let now = Instant::now();
        let update = NetworkStateEstimate {
            update_time: self.instant_to_timestamp(now),
            pre_link_buffer_delay: TimeDelta::from_millis(rtt.as_millis() as i64),
            ..Default::default()
        };

        // Process network state estimate
        let result = self.gcc.on_network_state_estimate(update);
        
        // Update estimates if we got new target rate
        if let Some(target_rate) = result.target_rate {
            self.current_estimate.target_bps = target_rate.target_rate.bps() as u64;
            self.current_estimate.bandwidth_bps = target_rate.target_rate.bps() as u64;
        }
    }

    /// Convert Instant to GCC timestamp
    fn instant_to_timestamp(&self, instant: Instant) -> Timestamp {
        let duration = instant.duration_since(self.start_time);
        Timestamp::from_millis(duration.as_millis() as i64)
    }
}

/// Metadata about a sent packet that needs to be tracked
#[derive(Debug, Clone)]
pub struct PacketMetadata {
    pub sequence: u64,
    pub send_time: Instant,
    pub size_bytes: usize,
}

/// Information about a received packet (from receiver feedback)
#[derive(Debug, Clone)]
pub struct ReceivedPacketInfo {
    pub sequence: u64,
    pub send_time: Instant,
    pub receive_time: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_congestion_controller_init() {
        let config = CongestionControllerConfig::default();
        let controller = CongestionController::new(config.clone());
        
        let estimate = controller.current_estimate();
        assert_eq!(estimate.target_bps, config.initial_bitrate_bps);
    }

    #[test]
    fn test_packet_tracking() {
        let config = CongestionControllerConfig::default();
        let mut controller = CongestionController::new(config);
        
        let metadata = controller.on_packet_sent(1000);
        assert_eq!(metadata.sequence, 0);
        assert_eq!(metadata.size_bytes, 1000);
        
        let metadata2 = controller.on_packet_sent(500);
        assert_eq!(metadata2.sequence, 1);
    }

    #[test]
    fn test_bitrate_recommendations() {
        let config = CongestionControllerConfig {
            initial_bitrate_bps: 1_000_000,
            ..Default::default()
        };
        let controller = CongestionController::new(config);
        
        // Video should be ~80% of target
        let video_bps = controller.recommended_video_bitrate();
        assert!(video_bps > 700_000 && video_bps < 850_000);
        
        // Audio should be clamped appropriately
        let audio_bps = controller.recommended_audio_bitrate();
        assert!(audio_bps >= 32_000 && audio_bps <= 128_000);
    }
}

