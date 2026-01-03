# Google Congestion Control (GCC) Implementation

**Implemented:** 2026-01-03  
**Crate:** [`goog_cc`](https://crates.io/crates/goog_cc) v0.1.4

## Overview

This implementation adds industry-standard Google Congestion Control to `iroh-live`, enabling adaptive bitrate streaming that responds to network conditions in real-time.

## Architecture

### Module Structure

```
iroh-live/src/network/
├── mod.rs                      # Module exports
├── congestion_controller.rs    # Core GCC logic
└── feedback_collector.rs       # Packet tracking
```

### Key Components

#### 1. CongestionController

The main GCC implementation that:
- Monitors network delay and loss
- Calculates target bitrate
- Provides encoder recommendations
- Adapts to changing network conditions

```rust
pub struct CongestionController {
    gcc: goog_cc::Gcc,
    config: CongestionControllerConfig,
    current_estimate: BitrateEstimate,
    // ... internal state
}
```

#### 2. FeedbackCollector

Tracks sent packets and processes receiver feedback:
- Assigns sequence numbers to packets
- Matches acknowledgments with sent packets
- Detects lost packets based on timeouts
- Cleans up stale tracking data

```rust
pub struct FeedbackCollector {
    pending_packets: HashMap<u64, PacketMetadata>,
    max_age: Duration,
    // ... tracking state
}
```

## Configuration

### Default Settings

```rust
CongestionControllerConfig {
    initial_bitrate_bps: 1_000_000,    // 1 Mbps
    min_bitrate_bps: 128_000,          // 128 kbps
    max_bitrate_bps: 10_000_000,       // 10 Mbps
    probe_interval: Duration::from_secs(30),
}
```

### Customization

Adjust based on your use case:

**Low-latency gaming/remote desktop:**
```rust
CongestionControllerConfig {
    initial_bitrate_bps: 5_000_000,    // Start higher
    min_bitrate_bps: 500_000,          // Higher minimum for quality
    max_bitrate_bps: 50_000_000,       // Allow high bitrates on fast networks
    probe_interval: Duration::from_secs(10),  // Probe more frequently
}
```

**Conservative mobile/unstable networks:**
```rust
CongestionControllerConfig {
    initial_bitrate_bps: 500_000,      // Start conservative
    min_bitrate_bps: 64_000,           // Very low minimum
    max_bitrate_bps: 3_000_000,        // Cap at 3 Mbps
    probe_interval: Duration::from_secs(60),  // Probe less often
}
```

## Integration Guide

### Step 1: Initialize in Publisher

In `publish.rs`, add congestion controller to the video/audio encoder state:

```rust
use crate::network::{CongestionController, CongestionControllerConfig, FeedbackCollector};

// In PublishBroadcast or encoder state
pub struct EncoderState {
    // ... existing fields ...
    congestion_controller: Arc<Mutex<CongestionController>>,
    feedback_collector: Arc<Mutex<FeedbackCollector>>,
}

impl EncoderState {
    pub fn new() -> Self {
        let config = CongestionControllerConfig {
            initial_bitrate_bps: 2_000_000,  // 2 Mbps initial
            min_bitrate_bps: 256_000,
            max_bitrate_bps: 10_000_000,
            probe_interval: Duration::from_secs(30),
        };
        
        Self {
            // ... existing fields ...
            congestion_controller: Arc::new(Mutex::new(
                CongestionController::new(config)
            )),
            feedback_collector: Arc::new(Mutex::new(
                FeedbackCollector::new()
            )),
        }
    }
}
```

### Step 2: Track Sent Packets

When sending video/audio packets:

```rust
// In the packet sending loop
let packet_size = encoded_packet.data.len();

// Track with congestion controller
let mut cc = congestion_controller.lock().unwrap();
let mut feedback = feedback_collector.lock().unwrap();

let metadata = cc.on_packet_sent(packet_size);
feedback.track_sent(metadata);

// Send packet over network
writer.write_object(&encoded_packet).await?;
```

### Step 3: Collect Connection Statistics

Periodically update GCC with QUIC connection stats:

```rust
// In a background monitoring task or periodically in the main loop
use std::time::Duration;

async fn update_congestion_control(
    session: &MoqSession,
    cc: Arc<Mutex<CongestionController>>,
) {
    let stats = session.conn().stats();
    let rtt = stats.rtt;
    
    // Simple update based on RTT only
    let mut controller = cc.lock().unwrap();
    controller.update_network_state(rtt, 0.0);  // 0.0 loss for now
    
    let estimate = controller.current_estimate();
    tracing::info!(
        "network stats: rtt={:?}, target_bitrate={}bps",
        estimate.rtt,
        estimate.target_bps
    );
}

// Call this every 1-5 seconds
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        update_congestion_control(&session, cc.clone()).await;
    }
});
```

### Step 4: Adjust Encoder Bitrate

Use GCC recommendations to adjust encoder settings:

```rust
// Periodically (e.g., every 1-2 seconds)
let mut cc = congestion_controller.lock().unwrap();
let video_bitrate = cc.recommended_video_bitrate();
let audio_bitrate = cc.recommended_audio_bitrate();

// Apply to video encoder
if let Some(video_encoder) = video_encoder.as_mut() {
    // Note: Actual API depends on your encoder wrapper
    video_encoder.set_target_bitrate(video_bitrate)?;
    tracing::debug!("updated video bitrate to {}bps", video_bitrate);
}

// Apply to audio encoder (Opus)
if let Some(audio_encoder) = audio_encoder.as_mut() {
    audio_encoder.set_bitrate(audio_bitrate as i32)?;
    tracing::debug!("updated audio bitrate to {}bps", audio_bitrate);
}
```

### Step 5: Handle Packet Loss

Detect and report packet loss to GCC:

```rust
// Periodically check for lost packets
let mut feedback = feedback_collector.lock().unwrap();
let timeout = Duration::from_secs(3);  // Consider lost after 3 RTTs
let lost_packets = feedback.get_lost_packets(timeout);

if !lost_packets.is_empty() {
    let mut cc = congestion_controller.lock().unwrap();
    cc.on_packet_loss(lost_packets.len());
}
```

## Advanced: Feedback from Receivers

For optimal GCC performance, receivers should send packet reception reports back to the publisher. This requires extending the protocol:

### Feedback Message Format

```rust
#[derive(Serialize, Deserialize)]
pub struct PacketFeedback {
    /// Sequence numbers of received packets
    pub received_sequences: Vec<u64>,
    /// Timestamps when packets were received (relative to first)
    pub receive_times_ms: Vec<u64>,
    /// Receiver's current time
    pub feedback_time: u64,
}
```

### Receiver Side

```rust
// Track received packets
let mut received_packets = Vec::new();
let start_time = Instant::now();

// When receiving a packet
let receive_time = Instant::now();
let time_offset_ms = receive_time.duration_since(start_time).as_millis() as u64;

received_packets.push((sequence_number, time_offset_ms));

// Periodically send feedback
if received_packets.len() >= 10 || last_feedback.elapsed() > Duration::from_millis(200) {
    let feedback = PacketFeedback {
        received_sequences: received_packets.iter().map(|(seq, _)| *seq).collect(),
        receive_times_ms: received_packets.iter().map(|(_, time)| *time).collect(),
        feedback_time: Instant::now().duration_since(start_time).as_millis() as u64,
    };
    
    // Send feedback to publisher
    send_feedback_to_publisher(&feedback).await?;
    received_packets.clear();
}
```

### Publisher Side (Processing Feedback)

```rust
// When receiving feedback from subscriber
fn process_feedback(
    feedback: PacketFeedback,
    cc: &mut CongestionController,
    stats: &ConnectionStats,
) -> Result<()> {
    // Convert to ReceivedPacketInfo
    let received: Vec<ReceivedPacketInfo> = feedback
        .received_sequences
        .iter()
        .zip(feedback.receive_times_ms.iter())
        .map(|(seq, time_ms)| ReceivedPacketInfo {
            sequence: *seq,
            send_time: /* lookup from tracking */,
            receive_time: /* convert time_ms to Instant */,
        })
        .collect();
    
    // Update GCC with actual feedback
    cc.on_feedback(&received, stats.rtt)?;
    
    Ok(())
}
```

## Monitoring and Observability

### Key Metrics to Track

```rust
// Export these metrics to your monitoring system
pub struct CongestionMetrics {
    pub target_bitrate_bps: u64,
    pub bandwidth_estimate_bps: u64,
    pub rtt_ms: u64,
    pub loss_rate_percent: f32,
    pub video_bitrate_bps: u64,
    pub audio_bitrate_bps: u64,
}

impl CongestionController {
    pub fn get_metrics(&self) -> CongestionMetrics {
        let est = self.current_estimate();
        CongestionMetrics {
            target_bitrate_bps: est.target_bps,
            bandwidth_estimate_bps: est.bandwidth_bps,
            rtt_ms: est.rtt.as_millis() as u64,
            loss_rate_percent: est.loss_rate * 100.0,
            video_bitrate_bps: self.recommended_video_bitrate(),
            audio_bitrate_bps: self.recommended_audio_bitrate(),
        }
    }
}
```

### Logging

The implementation includes structured logging:

```
INFO  congestion controller initialized: initial=1000000bps, min=128000bps, max=10000000bps
DEBUG congestion controller update: target=1200000bps, bandwidth=1200000bps, rtt=45ms, usage=Under
WARN  packet loss detected: 5 packets lost, total_loss_rate=0.52%
```

## Testing

### Unit Tests

The implementation includes unit tests:

```bash
cd iroh-live
cargo test network::
```

### Integration Testing

Test with varying network conditions:

```bash
# Use tc (Linux) or similar tools to simulate network conditions

# Add 100ms latency
sudo tc qdisc add dev eth0 root netem delay 100ms

# Add 2% packet loss
sudo tc qdisc change dev eth0 root netem loss 2%

# Add bandwidth limit
sudo tc qdisc change dev eth0 root tbf rate 1mbit burst 32kbit latency 400ms
```

Observe GCC adapting bitrates accordingly.

## Performance Considerations

### CPU Usage
- GCC computations are lightweight (< 1% CPU on modern hardware)
- Feedback processing is O(n) where n = packets per feedback interval
- Cleanup runs every 1 second, removing stale entries

### Memory Usage
- `FeedbackCollector` stores ~200 bytes per pending packet
- Automatically cleans up packets older than 5 seconds
- Typical memory usage: < 100KB for normal streaming

### Latency Impact
- Minimal: GCC is designed for real-time media
- Bitrate changes apply on next encoder frame
- No additional buffering required

## Troubleshooting

### Bitrate Too Conservative

If GCC is too conservative:
- Increase `initial_bitrate_bps`
- Increase `max_bitrate_bps`
- Reduce `probe_interval` for more aggressive probing

### Bitrate Too Aggressive

If experiencing quality issues or buffering:
- Decrease `initial_bitrate_bps`
- Increase `min_bitrate_bps` if minimum quality is unacceptable
- Ensure loss detection is working properly

### Bitrate Not Adapting

Check that:
- Connection stats are being fed to GCC regularly
- RTT values are reasonable (not 0 or extremely high)
- Feedback collector is tracking packets correctly
- Lost packet detection is running

## References

- [GCC Algorithm](https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02)
- [WebRTC Congestion Control](https://www.w3.org/TR/webrtc-stats/#dom-rtcstatstype-transport)
- [`goog_cc` crate documentation](https://docs.rs/goog_cc/0.1.4/goog_cc/)

## Future Enhancements

1. **Bandwidth probing** - Active probing for available bandwidth
2. **Multi-stream coordination** - Coordinate bitrates across multiple tracks
3. **FEC integration** - Combine with Forward Error Correction for better loss recovery
4. **Machine learning** - Use ML models for prediction and optimization
5. **Receiver-driven adaptation** - Allow receivers to request specific bitrates

