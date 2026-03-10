#![allow(missing_docs)]

//! End-to-end tests verifying multi-agent design patterns documented in
//! `docs/book/src/explanation/multi-agent-design-patterns.md`.
//!
//! Covered patterns: Coordinator, Sequential Pipeline, Parallel Fan-Out/Gather,
//! Hierarchical Decomposition, Generator-Critic.
//!
//! Not covered (by design):
//! - Iterative Refinement — same mechanism as Generator-Critic (doc note)
//! - Human-in-the-Loop — uses built-in suspension model, not LLM delegation
//! - Swarm/Peer Handoff — not yet supported
//!
//! Run: `DEEPSEEK_API_KEY=... cargo test --package tirea-agentos --test real_multi_agent_patterns_deepseek -- --ignored --nocapture`

use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, ToolExecutionMode};
use tirea_agentos::runtime::AgentOs;
use tirea_contract::thread::Message;
use tirea_contract::{AgentEvent, RunOrigin, RunRequest};

fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn require_deepseek_key() {
    if std::env::var("DEEPSEEK_API_KEY").is_err() {
        panic!("DEEPSEEK_API_KEY not set. Export it before running ignored tests.");
    }
}

struct RunResult {
    called_agents: HashSet<String>,
    successful_calls: usize,
    text: String,
    last_error: String,
}

async fn execute_run(
    os: &AgentOs,
    agent_id: &str,
    thread_id: &str,
    prompt: &str,
) -> Result<RunResult, String> {
    let run = os
        .run_stream(RunRequest {
            agent_id: agent_id.to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user(prompt)],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        })
        .await
        .map_err(|e| format!("run start failed: {e}"))?;

    let stream = run.events;
    tokio::pin!(stream);

    let mut result = RunResult {
        called_agents: HashSet::new(),
        successful_calls: 0,
        text: String::new(),
        last_error: String::new(),
    };

    let deadline = Duration::from_secs(180);
    let timed_out = tokio::time::timeout(deadline, async {
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ToolCallDone { result: tr, .. } => {
                    if tr.tool_name == "agent_run" && tr.is_success() {
                        result.successful_calls += 1;
                        if let Some(agent_id) = tr.data.get("agent_id").and_then(|v| v.as_str()) {
                            result.called_agents.insert(agent_id.to_string());
                        }
                    }
                }
                AgentEvent::TextDelta { delta } => {
                    result.text.push_str(&delta);
                }
                AgentEvent::Error { message, .. } => {
                    result.last_error = message;
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

    Ok(result)
}

/// Retry wrapper for flaky DeepSeek API.
async fn with_retries<F, Fut>(name: &str, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut last_err = String::new();
    for attempt in 1..=3 {
        match f().await {
            Ok(()) => return,
            Err(e) => {
                eprintln!("[{name} attempt {attempt}/3] {e}");
                last_err = e;
            }
        }
    }
    panic!("[{name}] all 3 attempts failed; last: {last_err}");
}

// ---------------------------------------------------------------------------
// Pattern 1: Coordinator / Dispatcher
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_coordinator_routes_to_specialist() {
    require_deepseek_key();
    with_retries("coordinator", || async {
        let os = AgentOs::builder()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "billing",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a billing specialist. Reply in Chinese. Always mention '账单' in your answer.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "support",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a technical support specialist. Reply in Chinese. Always mention '技术' in your answer.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "orchestrator",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(6)
                    .with_system_prompt(
                        "You are a help desk coordinator. Route user requests:\n\
                        - billing/payment/invoice issues -> agent_run for agent_id=billing\n\
                        - technical/bug/error issues -> agent_run for agent_id=support\n\
                        Use agent_run with background=false. Then return the specialist's answer to the user.",
                    )
                    .with_allowed_agents(vec!["billing".to_string(), "support".to_string()]),
            ))
            .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
            .build()
            .unwrap();

        let r = execute_run(&os, "orchestrator", "coordinator-billing", "我的发票有问题，请帮我查一下").await?;

        if !r.called_agents.contains("billing") {
            return Err(format!(
                "expected routing to billing, got {:?} (text={})",
                r.called_agents,
                &truncate_str(&r.text, 200)
            ));
        }
        if r.text.trim().is_empty() {
            return Err("no response text".to_string());
        }
        eprintln!("[coordinator] OK: routed to {:?}, text={}", r.called_agents, &truncate_str(&r.text, 100));
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Pattern 2: Sequential Pipeline
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_sequential_pipeline() {
    require_deepseek_key();
    with_retries("sequential", || async {
        let os = AgentOs::builder()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "translator",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a translator. Translate the given Chinese text to English. Output only the English translation.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "summarizer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a summarizer. Summarize the given English text into one short sentence. Output only the summary.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "orchestrator",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(8)
                    .with_system_prompt(
                        "You are a pipeline orchestrator. Process the user's Chinese text through two stages in order:\n\
                        1. Call agent_run for agent_id=translator with the Chinese text, background=false. Read the English output.\n\
                        2. Call agent_run for agent_id=summarizer with the English text from step 1, background=false. Read the summary.\n\
                        Return the final English summary to the user. Do not skip any step.",
                    )
                    .with_allowed_agents(vec!["translator".to_string(), "summarizer".to_string()]),
            ))
            .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
            .build()
            .unwrap();

        let r = execute_run(
            &os,
            "orchestrator",
            "sequential-pipeline",
            "Rust 语言以其所有权系统闻名，它在编译期就能检测内存安全问题，无需垃圾回收器，同时提供零成本抽象。"
        ).await?;

        if !r.called_agents.contains("translator") || !r.called_agents.contains("summarizer") {
            return Err(format!(
                "expected translator+summarizer calls, got {:?}",
                r.called_agents
            ));
        }
        if r.successful_calls < 2 {
            return Err(format!("expected >=2 successful calls, got {}", r.successful_calls));
        }
        if r.text.trim().is_empty() {
            return Err("no response text".to_string());
        }
        eprintln!("[sequential] OK: called {:?}, text={}", r.called_agents, &truncate_str(&r.text, 200));
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Pattern 3: Parallel Fan-Out/Gather
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_parallel_fan_out_gather() {
    require_deepseek_key();
    with_retries("parallel", || async {
        let os = AgentOs::builder()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "security_reviewer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a security reviewer. Analyze the code for security issues. Reply with a short security report (2-3 sentences).")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "style_reviewer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a code style reviewer. Check the code for style and readability. Reply with a short style report (2-3 sentences).")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "perf_reviewer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a performance reviewer. Analyze the code for performance issues. Reply with a short performance report (2-3 sentences).")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "orchestrator",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(10)
                    .with_tool_execution_mode(ToolExecutionMode::ParallelBatchApproval)
                    .with_system_prompt(
                        "You are a code review orchestrator. Review the submitted code:\n\
                        1. Call ALL THREE reviewers IN PARALLEL by issuing three agent_run tool calls in the SAME response:\n\
                           - agent_run for agent_id=security_reviewer\n\
                           - agent_run for agent_id=style_reviewer\n\
                           - agent_run for agent_id=perf_reviewer\n\
                           All three with background=false, passing the code as the prompt.\n\
                        IMPORTANT: You MUST call all three agent_run tools at the same time in one response, NOT one after another.\n\
                        2. After receiving all three results, write a unified review combining all findings.\n\
                        Do not skip any reviewer.",
                    )
                    .with_allowed_agents(vec![
                        "security_reviewer".to_string(),
                        "style_reviewer".to_string(),
                        "perf_reviewer".to_string(),
                    ]),
            ))
            .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
            .build()
            .unwrap();

        let code_snippet = r#"Review this code:
```python
import subprocess
def run_cmd(user_input):
    result = subprocess.run(user_input, shell=True, capture_output=True)
    return result.stdout.decode()
```"#;

        let r = execute_run(&os, "orchestrator", "parallel-fanout", code_snippet).await?;

        let expected = ["security_reviewer", "style_reviewer", "perf_reviewer"];
        let missing: Vec<_> = expected.iter().filter(|a| !r.called_agents.contains(**a)).collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing reviewer calls: {:?} (got {:?})",
                missing, r.called_agents
            ));
        }
        if r.text.trim().is_empty() {
            return Err("no unified review text".to_string());
        }
        eprintln!(
            "[parallel] OK: called {:?} ({} calls), text={}",
            r.called_agents,
            r.successful_calls,
            &truncate_str(&r.text, 200)
        );
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Pattern 4: Hierarchical Decomposition
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_hierarchical_decomposition() {
    require_deepseek_key();
    with_retries("hierarchical", || async {
        let os = AgentOs::builder()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "fact_finder",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a fact finder. When asked about a topic, provide 3 key facts. Be concise.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "summarizer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a summarizer. Condense the given facts into one paragraph.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "researcher",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(8)
                    .with_system_prompt(
                        "You are a research assistant. To research a topic:\n\
                        1. Call agent_run for agent_id=fact_finder with the topic, background=false.\n\
                        2. Call agent_run for agent_id=summarizer with the facts, background=false.\n\
                        Return the summarized research.",
                    )
                    .with_allowed_agents(vec!["fact_finder".to_string(), "summarizer".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "report_writer",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(8)
                    .with_system_prompt(
                        "You are a report writer. To write a report:\n\
                        1. Call agent_run for agent_id=researcher with the topic, background=false.\n\
                        2. Use the research to write a short report (3-5 sentences).\n\
                        Return the final report.",
                    )
                    .with_allowed_agents(vec!["researcher".to_string()]),
            ))
            .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
            .build()
            .unwrap();

        let r = execute_run(&os, "report_writer", "hierarchical", "Write a short report on Rust's ownership system").await?;

        if !r.called_agents.contains("researcher") {
            return Err(format!(
                "report_writer should have called researcher, got {:?}",
                r.called_agents
            ));
        }
        if r.successful_calls < 1 {
            return Err(format!("expected >=1 successful call, got {}", r.successful_calls));
        }
        if r.text.trim().is_empty() {
            return Err("no report text".to_string());
        }
        eprintln!(
            "[hierarchical] OK: top-level called {:?}, text={}",
            r.called_agents,
            &truncate_str(&r.text, 200)
        );
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Pattern 5: Generator-Critic
//
// Generator is the main agent; Critic is a child agent.
// Generator writes, calls critic via agent_run, reads feedback,
// revises if needed — all within its own loop with full history.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_generator_critic() {
    require_deepseek_key();
    with_retries("gen-critic", || async {
        let os = AgentOs::builder()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "critic",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt(
                        "You are a haiku critic. Check if the text is a valid haiku (3 lines, roughly 5-7-5 syllables).\n\
                        If valid, output exactly: PASS\n\
                        If invalid, output exactly: FAIL followed by a brief reason.\n\
                        Output only PASS or FAIL with reason, nothing else.",
                    )
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            ))
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "generator",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(20)
                    .with_system_prompt(
                        "You are a haiku writer. Your workflow:\n\
                        1. Write a haiku (3 lines, 5-7-5 syllables) about the user's topic.\n\
                        2. Call agent_run for agent_id=critic with your haiku, background=false.\n\
                        3. If critic says FAIL, revise your haiku based on the feedback and call critic again.\n\
                        4. Repeat until critic says PASS or you have tried 3 times.\n\
                        5. Once PASS, output the final haiku to the user.\n\
                        Always call the critic before finishing. Do not skip the review step.",
                    )
                    .with_allowed_agents(vec!["critic".to_string()]),
            ))
            .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
            .build()
            .unwrap();

        let r = execute_run(&os, "generator", "gen-critic", "Write a haiku about rustaceans").await?;

        if !r.called_agents.contains("critic") {
            return Err(format!(
                "expected critic call, got {:?}",
                r.called_agents
            ));
        }
        if r.successful_calls < 1 {
            return Err(format!("expected >=1 critic call, got {}", r.successful_calls));
        }
        if r.text.trim().is_empty() {
            return Err("no haiku text".to_string());
        }
        eprintln!(
            "[gen-critic] OK: {} critic calls, text={}",
            r.successful_calls,
            &truncate_str(&r.text, 200)
        );
        Ok(())
    })
    .await;
}
