//! ADR-0045 first vertical slice: InProcess and Unix channels round-trip, and a
//! plan carrying a credential reference serializes without any secret material.

use awaken_connection_plan::{
    ChannelFactory, ConnectionPlan, CredentialRef, DialAddr, DialPolicy, TokioChannelFactory,
    bind_unix, in_process_pair,
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
async fn credential_resolver_and_applied_auth() {
    use awaken_connection_plan::{AppliedAuth, CredentialResolver, NoAuth};
    // NoAuth (the loopback default) grants nothing.
    let applied = NoAuth
        .resolve(&CredentialRef("vault://x".into()))
        .await
        .expect("resolve");
    assert!(applied.is_empty());
    assert!(applied.headers().is_empty());
    // Bearer material presents an authorization header.
    let bearer = AppliedAuth::bearer("tok-123");
    assert!(!bearer.is_empty());
    assert_eq!(bearer.headers()[0].0, "authorization");
    assert!(bearer.headers()[0].1.contains("Bearer tok-123"));
    // none() is the empty material.
    assert!(AppliedAuth::none().is_empty());
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
async fn a_plan_serializes_a_credential_reference_but_no_material() {
    let plan = ConnectionPlan::unix_dial("/run/hand.sock")
        .with_credential(CredentialRef("vault://hand-bearer".to_string()));

    let json = serde_json::to_string(&plan).unwrap();
    // The reference is present…
    assert!(json.contains("vault://hand-bearer"));
    // …but no resolved secret material of any recognizable shape leaks.
    assert!(!json.to_lowercase().contains("bearer "));
    assert!(!json.to_lowercase().contains("authorization"));

    // Round-trips back to the same value object.
    let back: ConnectionPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(back, plan);
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
