#![no_main]

use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(commit) = serde_json::from_slice::<ThreadCommit>(data) else {
        return;
    };
    commit
        .validate()
        .expect("decoded ThreadCommit remains valid");
    let encoded = serde_json::to_vec(&commit).expect("accepted ThreadCommit serializes");
    let decoded: ThreadCommit =
        serde_json::from_slice(&encoded).expect("canonical ThreadCommit round-trips");
    assert_eq!(decoded, commit);
});
