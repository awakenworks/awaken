use awaken_connection_plan::MultiplexedChannel;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn pair() -> (MultiplexedChannel, MultiplexedChannel) {
    let (client, server) = tokio::io::duplex(4096);
    (
        MultiplexedChannel::client(client),
        MultiplexedChannel::server(server),
    )
}

#[tokio::test]
async fn concurrent_streams_keep_bytes_and_half_closes_independent() {
    // Graph: C1 two clients share one physical channel; C2 identical application
    // request IDs but different payloads; C3 one request half-closes; C4 another
    // client remains live; C5 later reconnect. Effects: E1 independent bytes;
    // E2 EOF drains only its own response; E3 the other and later clients work.
    // Rules M1 C1+C2=>E1; M2 C3+C4=>E2/E3; M3 C5=>E3. No payload parsing,
    // request-ID rewriting, replay or application state exists in this layer.
    let (client, mut server) = pair();
    let mut one = client.open().await.unwrap();
    let mut two = client.open().await.unwrap();
    one.write_all(b"id=1:A").await.unwrap();
    two.write_all(b"id=1:B").await.unwrap();
    let mut first = server.accept().await.unwrap().unwrap();
    let mut second = server.accept().await.unwrap().unwrap();
    let mut a = [0; 6];
    let mut b = [0; 6];
    first.read_exact(&mut a).await.unwrap();
    second.read_exact(&mut b).await.unwrap();
    assert_ne!(a, b, "M1 no stream mixing");
    assert!([a, b].contains(b"id=1:A"));
    assert!([a, b].contains(b"id=1:B"));
    // Inbound stream ordering is deliberately not part of the contract.
    let (mut server_one, mut server_two) = if a == *b"id=1:A" {
        (first, second)
    } else {
        (second, first)
    };
    one.shutdown().await.unwrap();
    assert_eq!(
        server_one.read(&mut [0]).await.unwrap(),
        0,
        "M2 request EOF"
    );
    server_one.write_all(b"reply-A").await.unwrap();
    server_one.shutdown().await.unwrap();
    let mut reply = Vec::new();
    one.read_to_end(&mut reply).await.unwrap();
    assert_eq!(reply, b"reply-A");
    server_two.write_all(b"B").await.unwrap();
    let mut byte = [0];
    two.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, *b"B", "M2 other client survives");
    drop(server_one);
    drop(one);
    let mut later = client.open().await.unwrap();
    later.write_all(b"C").await.unwrap();
    let mut accepted = server.accept().await.unwrap().unwrap();
    accepted.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, *b"C", "M3 reconnect creates another stream");
}

#[tokio::test]
async fn stream_limit_and_cancelled_open_do_not_replace_the_physical_owner() {
    // C1 cancelled open before driver admission; C2 max simultaneous streams;
    // C3 one more open. M4 C1=>no leaked slot; M5 C2+C3=>bounded error, with
    // existing streams retaining their identity and remaining usable.
    use std::future::Future as _;
    let (client, mut server) = pair();
    let mut cancelled = Box::pin(client.open());
    std::future::poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(cancelled);
    let mut streams = Vec::new();
    let mut peers = Vec::new();
    for id in 0..MultiplexedChannel::MAX_STREAMS {
        let mut stream = client.open().await.unwrap();
        stream.write_all(&[id as u8]).await.unwrap();
        let mut peer = server.accept().await.unwrap().unwrap();
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(usize::from(byte[0]), id, "M4/M5");
        streams.push(stream);
        peers.push(peer);
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(2), client.open())
            .await
            .expect("M5 bounded admission")
            .is_err()
    );
    peers[0].write_all(b"alive").await.unwrap();
    let mut reply = [0; 5];
    streams[0].read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, *b"alive", "M5 retained stream");
}

#[tokio::test]
async fn connection_fault_and_owner_drop_revoke_all_streams() {
    // M6 physical EOF / invalid framing -> accept terminates and open fails;
    // M7 owner drop with multiple live streams -> every reader observes closure.
    // No malformed peer may allocate unbounded payloads or leave a busy driver.
    for malformed in [false, true] {
        let (mut physical, server) = tokio::io::duplex(64);
        let mut server = MultiplexedChannel::server(server);
        if malformed {
            physical.write_all(&[255; 12]).await.unwrap();
        }
        drop(physical);
        let result = tokio::time::timeout(Duration::from_secs(2), server.accept())
            .await
            .expect("M6 connection terminates");
        assert!(result.is_none() || result.unwrap().is_err());
        assert!(server.open().await.is_err());
    }
    let (client, mut server) = pair();
    let mut streams = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..2 {
        let mut stream = client.open().await.unwrap();
        stream.write_all(b"x").await.unwrap();
        let mut peer = server.accept().await.unwrap().unwrap();
        peer.read_exact(&mut [0]).await.unwrap();
        streams.push(stream);
        peers.push(peer);
    }
    drop(server);
    for stream in &mut streams {
        let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut [0]))
            .await
            .expect("M7 owner drop closes every stream");
        assert!(result.is_err() || result.unwrap() == 0);
    }
}

#[tokio::test]
async fn slow_reader_backpressure_is_local_and_released_slots_are_reusable() {
    // C1 an unread stream exceeds its receive window; C2 another client calls;
    // C3 the slow stream is cancelled; C4 repeated full-capacity open/drop waves.
    // M8 C1+C2 -> the slow writer waits while the other makes progress;
    // M9 C3+C4 -> cancelled/closed slots are reusable, no physical-owner reset.
    // Exercise more than the wire's bounded closing tail to expose leaked slots.
    tokio::time::timeout(Duration::from_secs(10), async {
        let (client, mut server) = pair();
        let mut slow = client.open().await.unwrap();
        slow.write_all(b"s").await.unwrap();
        let unread = server.accept().await.unwrap().unwrap();
        let mut blocked =
            tokio::spawn(async move { slow.write_all(&vec![0; 32 * 1024 * 1024]).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut blocked)
                .await
                .is_err(),
            "M8 unread stream applies backpressure"
        );
        let mut other = client.open().await.unwrap();
        other.write_all(b"live").await.unwrap();
        let mut peer = server.accept().await.unwrap().unwrap();
        let mut bytes = [0; 4];
        peer.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, *b"live", "M8 other client progresses");
        blocked.abort();
        assert!(blocked.await.unwrap_err().is_cancelled());
        drop((unread, other, peer));
        for _ in 0..4 {
            let mut wave = Vec::new();
            for id in 0..MultiplexedChannel::MAX_STREAMS {
                let mut stream = client.open().await.unwrap();
                stream.write_all(&[id as u8]).await.unwrap();
                let mut peer = server.accept().await.unwrap().unwrap();
                let mut byte = [0];
                peer.read_exact(&mut byte).await.unwrap();
                assert_eq!(byte[0], id as u8, "M9 admitted stream identity");
                wave.push((stream, peer));
            }
            drop(wave);
        }
    })
    .await
    .expect("M8/M9 progress and reclamation are bounded");
}
