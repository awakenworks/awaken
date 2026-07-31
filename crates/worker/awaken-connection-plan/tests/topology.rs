//! ADR-0045 topology contract: InProcess, Unix, and TCP channels round-trip while
//! unsupported topology/authentication fields fail closed.

#[cfg(unix)]
use awaken_connection_plan::bind_unix;
use awaken_connection_plan::{
    ChannelFactory, ConnectionPlan, DialAddr, DialPolicy, TokioChannelFactory, in_process_pair,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn in_process_pair_round_trips() {
    let (mut brain, mut hand) = in_process_pair();
    brain.write_all(b"ping").await.unwrap();
    brain.flush().await.unwrap();

    let mut buf = [0u8; 4];
    hand.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    hand.write_all(b"pong").await.unwrap();
    hand.flush().await.unwrap();
    let mut back = [0u8; 4];
    brain.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"pong");
}

#[tokio::test]
#[cfg(unix)]
async fn unix_direct_dial_round_trips() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("awaken-relay-test-{}.sock", std::process::id()));
    let path = path.to_string_lossy().to_string();

    let listen_plan = ConnectionPlan::unix_listen(&path);
    let dial_plan = ConnectionPlan::unix_dial(&path);

    let listener = bind_unix(&listen_plan).expect("bind unix");
    let accept = tokio::spawn(async move {
        let mut hand = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4];
        hand.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        hand.write_all(b"pong").await.unwrap();
        hand.flush().await.unwrap();
    });

    let mut brain = TokioChannelFactory
        .connect(&dial_plan)
        .await
        .expect("dial unix");
    brain.write_all(b"ping").await.unwrap();
    brain.flush().await.unwrap();
    let mut back = [0u8; 4];
    brain.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"pong");

    accept.await.unwrap();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn tcp_direct_dial_round_trips() {
    use awaken_connection_plan::{bind_tcp, connect_with_retry};
    // Bind on an ephemeral port, then dial it back (the Direct-over-network case).
    let listener = bind_tcp(&ConnectionPlan::tcp_listen("127.0.0.1:0"))
        .await
        .expect("bind tcp");
    let addr = listener.local_addr().expect("addr");
    let accept = tokio::spawn(async move {
        let mut hand = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4];
        hand.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        hand.write_all(b"pong").await.unwrap();
        hand.flush().await.unwrap();
    });

    let plan = ConnectionPlan::tcp_dial(addr.to_string());
    let mut brain = connect_with_retry(
        &TokioChannelFactory,
        &plan,
        20,
        std::time::Duration::from_millis(25),
    )
    .await
    .expect("dial tcp");
    brain.write_all(b"ping").await.unwrap();
    brain.flush().await.unwrap();
    let mut back = [0u8; 4];
    brain.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"pong");
    accept.await.unwrap();
}

#[tokio::test]
async fn factory_and_bind_error_paths() {
    use awaken_connection_plan::{bind_tcp, bind_unix, connect_with_retry};
    // InProcess must be established via in_process_pair(), not connect().
    assert!(
        TokioChannelFactory
            .connect(&ConnectionPlan::in_process())
            .await
            .is_err()
    );
    // Transport/binder mismatches fail closed.
    assert!(bind_tcp(&ConnectionPlan::unix_dial("/x")).await.is_err());
    assert!(bind_unix(&ConnectionPlan::tcp_dial("127.0.0.1:1")).is_err());
    // connect_with_retry to an address nothing listens on errors after its budget
    // (and does not hang, per the per-attempt timeout).
    let r = connect_with_retry(
        &TokioChannelFactory,
        &ConnectionPlan::tcp_dial("127.0.0.1:1"),
        2,
        std::time::Duration::from_millis(10),
    )
    .await;
    assert!(r.is_err());
    // The Unix dial arm fails closed the same way when no socket is listening.
    let u = connect_with_retry(
        &TokioChannelFactory,
        &ConnectionPlan::unix_dial("/no/such/awaken-relay.sock"),
        2,
        std::time::Duration::from_millis(1),
    )
    .await;
    assert!(u.is_err(), "dialing a dead unix socket is an Io error");
}

#[tokio::test]
async fn connect_rejects_a_listen_plan() {
    let plan = ConnectionPlan::unix_listen("/tmp/never.sock");
    match TokioChannelFactory.connect(&plan).await {
        Err(err) => assert!(err.to_string().contains("dial-policy mismatch")),
        Ok(_) => panic!("a Listen plan must not connect"),
    }
}

#[tokio::test]
async fn topology_axes_compose_the_four_named_cases() {
    // InProcess degenerate.
    assert_eq!(ConnectionPlan::in_process().transport, DialAddr::InProcess);
    // Direct = Unix/Tcp + Dial.
    assert_eq!(ConnectionPlan::unix_dial("/x").dial, DialPolicy::Dial);
    // Reverse = Unix/Tcp + Listen.
    assert_eq!(ConnectionPlan::unix_listen("/x").dial, DialPolicy::Listen);
}

#[test]
fn constructors_set_only_transport_and_dial() {
    // Cause/effect inventory: each supported constructor fixes exactly one
    // transport/direction pair; no speculative authentication state exists.
    // Decision rules: in-process => InProcess+Dial, *_dial => Dial,
    // *_listen => Listen, and TCP/Unix preserve their exact address.
    let ip = ConnectionPlan::in_process();
    assert_eq!(ip.transport, DialAddr::InProcess);
    assert_eq!(ip.dial, DialPolicy::Dial);

    assert_eq!(
        ConnectionPlan::unix_dial("/s").transport,
        DialAddr::Unix("/s".to_string())
    );
    assert_eq!(ConnectionPlan::unix_listen("/s").dial, DialPolicy::Listen);

    // The network Direct/Reverse arms — neither transport nor dial was asserted
    // for the Tcp constructors before.
    let td = ConnectionPlan::tcp_dial("10.0.0.1:9000");
    assert_eq!(td.transport, DialAddr::Tcp("10.0.0.1:9000".to_string()));
    assert_eq!(td.dial, DialPolicy::Dial);

    let tl = ConnectionPlan::tcp_listen("0.0.0.0:0");
    assert_eq!(tl.transport, DialAddr::Tcp("0.0.0.0:0".to_string()));
    assert_eq!(tl.dial, DialPolicy::Listen);
}

#[test]
fn plan_wire_shape_is_closed_over_topology_axes() {
    // Cause/effect decision table:
    // R1 known transport+direction -> decode and round-trip;
    // R2 legacy credential field -> reject instead of silently accepting an
    // authentication promise no factory realizes; R3 snake_case tags remain stable.
    let ip = serde_json::to_string(&ConnectionPlan::in_process()).unwrap();
    assert!(ip.contains("\"in_process\""), "{ip}");
    // The Listen policy tag is snake_case too.
    let listen = serde_json::to_string(&ConnectionPlan::unix_listen("/s")).unwrap();
    assert!(listen.contains("\"listen\""), "{listen}");

    let back: ConnectionPlan =
        serde_json::from_str(r#"{"transport":"in_process","dial":"dial"}"#).unwrap();
    assert_eq!(back, ConnectionPlan::in_process());
    assert!(
        serde_json::from_str::<ConnectionPlan>(
            r#"{"transport":"in_process","dial":"dial","credential":"legacy"}"#
        )
        .is_err()
    );
}

#[tokio::test]
async fn connect_rejects_unsupported_transports() {
    use awaken_connection_plan::ConnectError;
    // Http and Nats have no producer in this slice — connect must fail closed with
    // Unsupported rather than attempt a dial. Only the InProcess arm was covered.
    let http = ConnectionPlan {
        transport: DialAddr::Http("https://peer".to_string()),
        dial: DialPolicy::Dial,
    };
    match TokioChannelFactory.connect(&http).await {
        Err(ConnectError::Unsupported(_)) => {}
        // `Box<dyn AgentChannel>` is not Debug, so keep the Ok arm formatting-free.
        Err(other) => panic!("Http transport must be Unsupported, got {other:?}"),
        Ok(_) => panic!("Http transport must not connect"),
    }

    let nats = ConnectionPlan {
        transport: DialAddr::Nats {
            url: "nats://broker".to_string(),
            inbox: "in".to_string(),
            outbox: "out".to_string(),
        },
        dial: DialPolicy::Dial,
    };
    assert!(matches!(
        TokioChannelFactory.connect(&nats).await,
        Err(ConnectError::Unsupported(_))
    ));
}

#[tokio::test]
async fn connect_with_retry_tries_at_least_once_with_zero_attempts() {
    use awaken_connection_plan::{ConnectError, connect_with_retry};
    // `attempts.max(1)`: a zero budget still makes one real attempt, so a dead
    // address surfaces a genuine connect error, not the "no attempts" placeholder.
    let r = connect_with_retry(
        &TokioChannelFactory,
        &ConnectionPlan::tcp_dial("127.0.0.1:1"),
        0,
        std::time::Duration::from_millis(1),
    )
    .await;
    match r {
        Err(ConnectError::Io(msg)) => assert!(
            !msg.contains("no attempts"),
            "exactly one attempt should have run, got: {msg}"
        ),
        Err(other) => panic!("expected an Io connect error, got {other:?}"),
        Ok(_) => panic!("dialing a dead address must not succeed"),
    }
}

#[tokio::test]
#[cfg(unix)]
async fn bind_unix_replaces_a_stale_socket_file() {
    // A dropped UnixListener leaves its socket file on disk; bind_unix removes the
    // stale file best-effort before rebinding, so a restart at the same path works.
    let path = std::env::temp_dir().join(format!(
        "awaken-stale-{}-{}.sock",
        std::process::id(),
        std::line!()
    ));
    let path = path.to_string_lossy().to_string();
    let plan = ConnectionPlan::unix_listen(&path);

    let first = bind_unix(&plan).expect("first bind creates the socket file");
    drop(first); // socket file lingers on disk after drop

    // Without the stale-file removal this second bind would fail EADDRINUSE.
    let _second = bind_unix(&plan).expect("stale socket file is removed before rebind");
    let _ = std::fs::remove_file(&path);
}
