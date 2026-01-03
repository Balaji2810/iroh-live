//! Example demonstrating Google Congestion Control usage
//!
//! This example shows how to integrate GCC into a streaming application.
//!
//! Run with: cargo run --example gcc_demo

use std::time::{Duration, Instant};
use iroh_live::network::{CongestionController, CongestionControllerConfig, FeedbackCollector};

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("=== Google Congestion Control Demo ===\n");

    // 1. Create congestion controller with custom config
    let config = CongestionControllerConfig {
        initial_bitrate_bps: 2_000_000,    // 2 Mbps
        min_bitrate_bps: 256_000,          // 256 kbps
        max_bitrate_bps: 10_000_000,       // 10 Mbps
        probe_interval: Duration::from_secs(30),
    };

    let mut controller = CongestionController::new(config);
    let mut feedback_collector = FeedbackCollector::new();

    println!("Initial estimate:");
    print_estimate(&controller);
    println!();

    // 2. Simulate sending packets
    println!("Simulating packet sends...");
    for i in 0..100 {
        let packet_size = if i % 10 == 0 { 5000 } else { 1500 }; // Keyframes larger
        let metadata = controller.on_packet_sent(packet_size);
        feedback_collector.track_sent(metadata);
    }
    println!("Sent 100 packets\n");

    // 3. Simulate good network conditions (low delay)
    println!("Scenario 1: Good network (low delay)");
    simulate_feedback(
        &mut controller,
        &mut feedback_collector,
        Duration::from_millis(30),  // Low RTT
        0.0,  // No loss
    );
    print_estimate(&controller);
    print_recommendations(&controller);
    println!();

    // 4. Simulate network congestion (high delay)
    println!("Scenario 2: Network congestion (high delay)");
    simulate_feedback(
        &mut controller,
        &mut feedback_collector,
        Duration::from_millis(250),  // High RTT
        0.0,  // No loss yet
    );
    print_estimate(&controller);
    print_recommendations(&controller);
    println!();

    // 5. Simulate packet loss
    println!("Scenario 3: Packet loss detected");
    controller.on_packet_loss(5);  // 5 packets lost
    print_estimate(&controller);
    print_recommendations(&controller);
    println!();

    // 6. Simulate recovery
    println!("Scenario 4: Network recovery");
    simulate_feedback(
        &mut controller,
        &mut feedback_collector,
        Duration::from_millis(50),  // Normal RTT
        0.0,  // No more loss
    );
    print_estimate(&controller);
    print_recommendations(&controller);
    println!();

    // 7. Check for lost packets
    let lost = feedback_collector.get_lost_packets(Duration::from_secs(5));
    println!("Lost packets detected: {}", lost.len());
    
    println!("\n=== Demo Complete ===");
}

fn simulate_feedback(
    controller: &mut CongestionController,
    _feedback_collector: &mut FeedbackCollector,
    rtt: Duration,
    loss_rate: f32,
) {
    // In a real application, you would collect actual packet reception times
    // For this demo, we just update based on RTT and loss rate
    controller.update_network_state(rtt, loss_rate);
}

fn print_estimate(controller: &CongestionController) {
    let estimate = controller.current_estimate();
    println!("  Target bitrate:    {} Mbps", estimate.target_bps / 1_000_000);
    println!("  Bandwidth estimate: {} Mbps", estimate.bandwidth_bps / 1_000_000);
    println!("  RTT:               {:?}", estimate.rtt);
    println!("  Loss rate:         {:.2}%", estimate.loss_rate * 100.0);
}

fn print_recommendations(controller: &CongestionController) {
    let video_bps = controller.recommended_video_bitrate();
    let audio_bps = controller.recommended_audio_bitrate();
    
    println!("  Recommended video: {} kbps", video_bps / 1000);
    println!("  Recommended audio: {} kbps", audio_bps / 1000);
}

