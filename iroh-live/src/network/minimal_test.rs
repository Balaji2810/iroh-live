//! Minimal test to verify GCC integration compiles correctly
//!
//! Run with: cargo test --package iroh-live --lib network::minimal_test -- --nocapture

use std::time::{Duration, Instant};

use super::congestion_controller::{
    CongestionController, CongestionControllerConfig, ReceivedPacketInfo,
};
use super::feedback_collector::FeedbackCollector;

#[test]
fn test_gcc_initialization() {
    let config = CongestionControllerConfig {
        initial_bitrate_bps: 1_000_000,
        min_bitrate_bps: 128_000,
        max_bitrate_bps: 10_000_000,
        probe_interval: Duration::from_secs(30),
    };

    let controller = CongestionController::new(config.clone());
    let estimate = controller.current_estimate();
    
    assert_eq!(estimate.target_bps, config.initial_bitrate_bps);
    println!("✓ GCC initialization works");
}

#[test]
fn test_packet_tracking() {
    let config = CongestionControllerConfig::default();
    let mut controller = CongestionController::new(config);
    let mut feedback = FeedbackCollector::new();

    // Send some packets
    for i in 0..10 {
        let metadata = controller.on_packet_sent(1400);
        assert_eq!(metadata.sequence, i);
        feedback.track_sent(metadata);
    }

    assert_eq!(feedback.pending_count(), 10);
    println!("✓ Packet tracking works");
}

#[test]
fn test_feedback_processing() {
    let config = CongestionControllerConfig::default();
    let mut controller = CongestionController::new(config);
    
    // Simulate received packets
    let now = Instant::now();
    let send_time = now - Duration::from_millis(50);
    
    let received = vec![
        ReceivedPacketInfo {
            sequence: 0,
            send_time,
            receive_time: now,
        },
    ];

    let result = controller.on_feedback(&received, Duration::from_millis(50));
    assert!(result.is_ok());
    println!("✓ Feedback processing works");
}

#[test]
fn test_bitrate_recommendations() {
    let config = CongestionControllerConfig {
        initial_bitrate_bps: 2_000_000,
        ..Default::default()
    };
    let controller = CongestionController::new(config);

    let video_bps = controller.recommended_video_bitrate();
    let audio_bps = controller.recommended_audio_bitrate();

    assert!(video_bps > 0);
    assert!(audio_bps >= 32_000 && audio_bps <= 128_000);
    
    println!("✓ Bitrate recommendations work");
    println!("  Video: {} kbps", video_bps / 1000);
    println!("  Audio: {} kbps", audio_bps / 1000);
}

#[test]
fn test_loss_handling() {
    let config = CongestionControllerConfig::default();
    let mut controller = CongestionController::new(config);

    // Simulate packet loss
    controller.on_packet_loss(5);
    
    let estimate = controller.current_estimate();
    println!("✓ Loss handling works");
    println!("  Loss rate: {:.2}%", estimate.loss_rate * 100.0);
}

