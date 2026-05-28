use awaken_server::protocols::{
    acp::encoder::AcpEncoder, ag_ui::encoder::AgUiEncoder, ai_sdk_v6::encoder::AiSdkEncoder,
};
use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::transport::Transcoder;

#[test]
fn encoders_link_and_transcode() {
    let ev = AgentEvent::TextDelta { delta: "x".into() };
    assert!(!AcpEncoder::new().transcode(&ev).is_empty());
    assert!(!AgUiEncoder::new().transcode(&ev).is_empty());
    assert!(!AiSdkEncoder::new().transcode(&ev).is_empty());
}
