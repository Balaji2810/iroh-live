//! Network quality management and congestion control
//!
//! This module implements Google Congestion Control (GCC) for adaptive bitrate
//! streaming over QUIC connections.

mod congestion_controller;
mod feedback_collector;

#[cfg(test)]
mod minimal_test;

pub use congestion_controller::{BitrateEstimate, CongestionController, CongestionControllerConfig};
pub use feedback_collector::{FeedbackCollector, PacketInfo};

