//! Feedback collection for congestion control
//!
//! Collects network statistics and packet timing information
//! to feed into the congestion controller.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, trace};

use super::congestion_controller::{PacketMetadata, ReceivedPacketInfo};

/// Collects packet information for feedback
pub struct FeedbackCollector {
    /// Tracking sent packets awaiting feedback
    pending_packets: HashMap<u64, PacketMetadata>,
    /// Maximum age before discarding packet info
    max_age: Duration,
    /// Last cleanup time
    last_cleanup: Instant,
}

impl FeedbackCollector {
    pub fn new() -> Self {
        Self {
            pending_packets: HashMap::new(),
            max_age: Duration::from_secs(5),
            last_cleanup: Instant::now(),
        }
    }

    /// Track a sent packet
    pub fn track_sent(&mut self, metadata: PacketMetadata) {
        trace!("tracking sent packet: seq={}", metadata.sequence);
        self.pending_packets.insert(metadata.sequence, metadata);
        self.maybe_cleanup();
    }

    /// Process received packet acknowledgment
    pub fn on_packet_ack(&mut self, sequence: u64, receive_time: Instant) -> Option<ReceivedPacketInfo> {
        if let Some(sent_metadata) = self.pending_packets.remove(&sequence) {
            let one_way_delay = receive_time.saturating_duration_since(sent_metadata.send_time);
            trace!(
                "packet acked: seq={}, delay={:?}",
                sequence,
                one_way_delay
            );
            
            Some(ReceivedPacketInfo {
                sequence,
                send_time: sent_metadata.send_time,
                receive_time,
            })
        } else {
            debug!("received ack for unknown packet: seq={}", sequence);
            None
        }
    }

    /// Get list of packets that appear to be lost (no ack after timeout)
    pub fn get_lost_packets(&mut self, timeout: Duration) -> Vec<u64> {
        let now = Instant::now();
        let mut lost = Vec::new();
        
        self.pending_packets.retain(|&seq, metadata| {
            if now.duration_since(metadata.send_time) > timeout {
                lost.push(seq);
                false // Remove from pending
            } else {
                true // Keep
            }
        });
        
        if !lost.is_empty() {
            debug!("detected {} lost packets", lost.len());
        }
        
        lost
    }

    /// Clean up old packet entries
    fn maybe_cleanup(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_cleanup) < Duration::from_secs(1) {
            return;
        }
        
        self.last_cleanup = now;
        let before_count = self.pending_packets.len();
        
        self.pending_packets.retain(|_, metadata| {
            now.duration_since(metadata.send_time) < self.max_age
        });
        
        let removed = before_count - self.pending_packets.len();
        if removed > 0 {
            debug!("cleaned up {} stale packet entries", removed);
        }
    }

    /// Get number of pending packets
    pub fn pending_count(&self) -> usize {
        self.pending_packets.len()
    }
}

impl Default for FeedbackCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Information about a packet being sent
#[derive(Debug, Clone)]
pub struct PacketInfo {
    pub sequence: u64,
    pub size_bytes: usize,
    pub send_time: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feedback_collector() {
        let mut collector = FeedbackCollector::new();
        
        let metadata = PacketMetadata {
            sequence: 0,
            send_time: Instant::now(),
            size_bytes: 1000,
        };
        
        collector.track_sent(metadata.clone());
        assert_eq!(collector.pending_count(), 1);
        
        let receive_time = Instant::now();
        let ack = collector.on_packet_ack(0, receive_time);
        assert!(ack.is_some());
        assert_eq!(collector.pending_count(), 0);
    }

    #[test]
    fn test_lost_packet_detection() {
        let mut collector = FeedbackCollector::new();
        
        let old_metadata = PacketMetadata {
            sequence: 0,
            send_time: Instant::now() - Duration::from_secs(10),
            size_bytes: 1000,
        };
        
        collector.track_sent(old_metadata);
        
        let lost = collector.get_lost_packets(Duration::from_secs(5));
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0], 0);
    }
}

