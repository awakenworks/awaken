#![allow(missing_docs)]

use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tirea_agentos::composition::AgentDefinition;
use tirea_agentos::runtime::AgentOs;
use tirea_contract::thread::Message;
use tirea_contract::{AgentEvent, RunOrigin, RunRequest};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn deepseek_real_multi_subagent_smoke() {
    if std::env::var("DEEPSEEK_API_KEY").is_err() {
        panic!(
            "DEEPSEEK_API_KEY not set. If it's in ~/.bashrc, run: source ~/.bashrc (or export it) before running ignored tests."
        );
    }

    // DeepSeek is occasionally flaky, retry a few times.
    let mut last_err = String::new();
    for attempt in 1..=3 {
        match run_multi_subagent().await {
            Ok(()) => return,
            Err(e) => {
                eprintln!("[attempt {attempt}/3] {e}");
                last_err = e;
            }
        }
    }

    panic!("all 3 attempts failed; last: {last_err}");
}

async fn run_multi_subagent() -> Result<(), String> {
    let os = AgentOs::builder()
        .with_agent(
            "writer",
            AgentDefinition::new("deepseek-chat")
                .with_system_prompt(
                    "You are writer agent. Reply with exactly one short Chinese sentence only.",
                )
                .with_excluded_tools(vec![
                    "agent_run".to_string(),
                    "task_status".to_string(),
                    "task_cancel".to_string(),
                    "task_output".to_string(),
                ]),
        )
        .with_agent(
            "reviewer",
            AgentDefinition::new("deepseek-chat")
                .with_system_prompt(
                    "You are reviewer agent. Reply with exactly one short Chinese review sentence only.",
                )
                .with_excluded_tools(vec![
                    "agent_run".to_string(),
                    "task_status".to_string(),
                    "task_cancel".to_string(),
                    "task_output".to_string(),
                ]),
        )
        .with_agent(
            "orchestrator",
            AgentDefinition::new("deepseek-chat")
                .with_max_rounds(6)
                .with_system_prompt(
                    "You are an orchestrator.\n\
You must complete this workflow:\n\
1) call tool agent_run for agent_id=writer with prompt '写一句关于 Rust 并发安全的中文短句' and background=false.\n\
2) call tool agent_run for agent_id=reviewer with prompt '请点评上一步短句（中文一句话）' and background=false.\n\
3) then answer in Chinese, summarizing both outputs in 2 short sentences.\n\
Do not skip tool calls.",
                ),
        )
        .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
        .build()
        .unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "orchestrator".to_string(),
            thread_id: Some("real-multi-subagent-smoke".to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user("按流程执行，最后给我结果。")],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        })
        .await
        .map_err(|e| format!("run start failed: {e}"))?;
    let stream = run.events;
    tokio::pin!(stream);

    let mut called_agents: HashSet<String> = HashSet::new();
    let mut successful_calls = 0usize;
    let mut text = String::new();
    let mut last_error = String::new();

    let deadline = Duration::from_secs(180);
    let timed_out = tokio::time::timeout(deadline, async {
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ToolCallDone { result, .. } => {
                    if result.tool_name == "agent_run" && result.is_success() {
                        successful_calls += 1;
                        if let Some(agent_id) = result.data.get("agent_id").and_then(|v| v.as_str())
                        {
                            called_agents.insert(agent_id.to_string());
                        }
                    }
                }
                AgentEvent::TextDelta { delta } => {
                    text.push_str(&delta);
                }
                AgentEvent::Error { message, .. } => {
                    last_error = message;
                    break;
                }
                AgentEvent::RunFinish { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .is_err();

    if timed_out {
        return Err("timed out after 180s".to_string());
    }

    if !called_agents.contains("writer") || !called_agents.contains("reviewer") {
        return Err(format!(
            "expected writer+reviewer subagent calls, got {:?} (success_calls={}, last_error={})",
            called_agents, successful_calls, last_error
        ));
    }

    if text.trim().is_empty() {
        return Err(format!(
            "no orchestrator assistant text produced (last_error={})",
            last_error
        ));
    }

    Ok(())
}
