use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::{
    acp::encoder::AcpEncoder, ag_ui::encoder::AgUiEncoder, ai_sdk_v6::encoder::AiSdkEncoder,
};

#[test]
fn text_delta_has_output_in_all_protocols() {
    let ev = AgentEvent::TextDelta {
        delta: "delta".into(),
    };
    assert!(!AcpEncoder::new().transcode(&ev).is_empty());
    assert!(!AgUiEncoder::new().transcode(&ev).is_empty());
    assert!(!AiSdkEncoder::new().transcode(&ev).is_empty());
}
