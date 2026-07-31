//! End-to-end smoke test for the single-owner core: a UDP datagram injected
//! as a raw IP frame must surface on the `UdpSocket`, and a reply sent
//! through `SendHalf` must come back out as an egress frame. Runs two stack
//! generations sequentially to validate the teardown/`core_done` restart
//! contract (lwIP globals are process-wide, so generations must serialize —
//! which is also why this is ONE test function, not several).

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use lwip::NetStack;
use tokio::time::timeout;

const DEVICE: &str = "172.19.0.1:50505";
const REMOTE: &str = "28.0.14.9:8000";

/// Minimal IPv4+UDP frame. Checksums are zeroed: CHECKSUM_CHECK_IP/UDP are 0
/// in lwipopts.h, and a zero UDP checksum means "none" per RFC 768 anyway.
fn udp_frame(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let (src_ip, dst_ip) = match (src.ip(), dst.ip()) {
        (std::net::IpAddr::V4(s), std::net::IpAddr::V4(d)) => (s.octets(), d.octets()),
        _ => panic!("v4 only"),
    };
    let udp_len = 8 + payload.len();
    let total_len = 20 + udp_len;
    let mut f = Vec::with_capacity(total_len);
    // IPv4 header
    f.push(0x45); // version 4, IHL 5
    f.push(0); // TOS
    f.extend_from_slice(&(total_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]); // identification
    f.extend_from_slice(&[0, 0]); // flags/fragment offset
    f.push(64); // TTL
    f.push(17); // protocol: UDP
    f.extend_from_slice(&[0, 0]); // header checksum (unchecked)
    f.extend_from_slice(&src_ip);
    f.extend_from_slice(&dst_ip);
    // UDP header
    f.extend_from_slice(&src.port().to_be_bytes());
    f.extend_from_slice(&dst.port().to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]); // checksum: none
    f.extend_from_slice(payload);
    f
}

/// Parse an egress frame; return (src, dst, payload) when it is IPv4/UDP.
fn parse_udp_frame(frame: &[u8]) -> Option<(SocketAddr, SocketAddr, Vec<u8>)> {
    if frame.len() < 28 || frame[0] >> 4 != 4 || frame[9] != 17 {
        return None;
    }
    let ihl = ((frame[0] & 0x0f) as usize) * 4;
    let src_ip = std::net::Ipv4Addr::new(frame[12], frame[13], frame[14], frame[15]);
    let dst_ip = std::net::Ipv4Addr::new(frame[16], frame[17], frame[18], frame[19]);
    let udp = &frame[ihl..];
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    Some((
        SocketAddr::new(src_ip.into(), src_port),
        SocketAddr::new(dst_ip.into(), dst_port),
        udp[8..].to_vec(),
    ))
}

async fn run_generation(tag: &str) {
    let device: SocketAddr = DEVICE.parse().unwrap();
    let remote: SocketAddr = REMOTE.parse().unwrap();

    let (mut stack, listener, udp) =
        NetStack::with_buffer_size(64, 16).unwrap_or_else(|e| panic!("{tag}: stack build: {e}"));
    let done = stack.core_done();
    let (send_half, mut recv_half) = udp.split();

    // Device → stack: inject the query.
    stack
        .send(udp_frame(device, remote, b"ping"))
        .await
        .unwrap_or_else(|e| panic!("{tag}: ingress send: {e}"));

    // Stack → UdpSocket: the datagram must surface with both addresses.
    let (payload, src, dst) = timeout(Duration::from_secs(5), recv_half.recv_from())
        .await
        .unwrap_or_else(|_| panic!("{tag}: timed out waiting for inbound datagram"))
        .unwrap_or_else(|e| panic!("{tag}: recv_from: {e}"));
    assert_eq!(payload, b"ping", "{tag}: payload");
    assert_eq!(src, device, "{tag}: src addr");
    assert_eq!(dst, remote, "{tag}: dst addr");

    // SendHalf → stack: reply flows back out as an egress frame.
    send_half
        .send_to(b"pong", &remote, &device)
        .unwrap_or_else(|e| panic!("{tag}: send_to: {e}"));

    let reply = timeout(Duration::from_secs(5), async {
        // Skip any unrelated frames lwIP emits on its own (IGMP etc.).
        loop {
            let frame = match stack.next().await {
                Some(Ok(f)) => f,
                other => panic!("{tag}: egress stream ended: {other:?}"),
            };
            if let Some((src, dst, payload)) = parse_udp_frame(&frame) {
                return (src, dst, payload);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{tag}: timed out waiting for egress frame"));
    assert_eq!(reply.0, remote, "{tag}: egress src");
    assert_eq!(reply.1, device, "{tag}: egress dst");
    assert_eq!(reply.2, b"pong", "{tag}: egress payload");

    // Teardown: dropping the handles must complete the core task.
    drop(stack);
    drop(listener);
    drop(send_half);
    drop(recv_half);
    let mut done = done;
    timeout(Duration::from_secs(5), done.wait_for(|d| *d))
        .await
        .unwrap_or_else(|_| panic!("{tag}: core task did not finish teardown"))
        .unwrap_or_else(|e| panic!("{tag}: done watch: {e}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_loopback_across_two_generations() {
    run_generation("gen1").await;
    // Second generation exercises the restart path: lwip_init already ran,
    // netif hooks get re-pointed, fresh pcbs, fresh core task.
    run_generation("gen2").await;
}
