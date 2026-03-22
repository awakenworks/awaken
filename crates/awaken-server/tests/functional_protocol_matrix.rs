use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::{
    acp::encoder::AcpEncoder, ag_ui::encoder::AgUiEncoder, ai_sdk_v6::encoder::AiSdkEncoder,
};

#[test]
fn protocol_matrix_basic_flow() {
    let seq = vec![
        AgentEvent::RunStart {
            thread_id: "t".into(),
            run_id: "r".into(),
            parent_run_id: None,
        },
        AgentEvent::TextDelta {
            delta: "hello".into(),
        },
        AgentEvent::RunFinish {
            thread_id: "t".into(),
            run_id: "r".into(),
            result: None,
            termination: awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd,
        },
    ];

    let mut acp = AcpEncoder::new();
    let mut ag = AgUiEncoder::new();
    let mut ai = AiSdkEncoder::new();

    let acp_count: usize = seq.iter().map(|e| acp.transcode(e).len()).sum();
    let ag_count: usize = seq.iter().map(|e| ag.transcode(e).len()).sum();
    let ai_count: usize = seq.iter().map(|e| ai.transcode(e).len()).sum();

    assert!(acp_count > 0 && ag_count > 0 && ai_count > 0);
}
