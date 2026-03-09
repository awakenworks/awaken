#![allow(missing_docs)]

//! End-to-end tests verifying each multi-agent design pattern
//! is achievable through LLM natural language orchestration with DeepSeek.
//!
//! All patterns use the same primitives: `agent_run`, `agent_output`, `agent_stop`
//! plus system prompt engineering. No dedicated workflow agent types.
//!
//! Run: `DEEPSEEK_API_KEY=... cargo test --package tirea-agentos --test real_multi_agent_patterns_deepseek -- --ignored --nocapture`

use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tirea_agentos::orchestrator::ToolExecutionMode;
use tirea_agentos::orchestrator::AgentDefinition;
use tirea_agentos::orchestrator::AgentOs;
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
        panic!(
            "DEEPSEEK_API_KEY not set. Export it before running ignored tests."
        );
    }
}

struct RunResult {
    called_agents: HashSet<String>,
    successful_calls: usize,
    text: String,
    last_error: String,
}

async fn execute_run(os: &AgentOs, agent_id: &str, thread_id: &str, prompt: &str) -> Result<RunResult, String> {
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
            .with_agent(
                "billing",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a billing specialist. Reply in Chinese. Always mention '账单' in your answer.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
                "support",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a technical support specialist. Reply in Chinese. Always mention '技术' in your answer.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
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
            )
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
            .with_agent(
                "translator",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a translator. Translate the given Chinese text to English. Output only the English translation.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
                "summarizer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a summarizer. Summarize the given English text into one short sentence. Output only the summary.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
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
            )
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
            .with_agent(
                "security_reviewer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a security reviewer. Analyze the code for security issues. Reply with a short security report (2-3 sentences).")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
                "style_reviewer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a code style reviewer. Check the code for style and readability. Reply with a short style report (2-3 sentences).")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
                "perf_reviewer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a performance reviewer. Analyze the code for performance issues. Reply with a short performance report (2-3 sentences).")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
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
            )
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
            .with_agent(
                "fact_finder",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a fact finder. When asked about a topic, provide 3 key facts. Be concise.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
                "summarizer",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt("You are a summarizer. Condense the given facts into one paragraph.")
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
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
            )
            .with_agent(
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
            )
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
            .with_agent(
                "critic",
                AgentDefinition::new("deepseek-chat")
                    .with_system_prompt(
                        "You are a haiku critic. Check if the text is a valid haiku (3 lines, roughly 5-7-5 syllables).\n\
                        If valid, output exactly: PASS\n\
                        If invalid, output exactly: FAIL followed by a brief reason.\n\
                        Output only PASS or FAIL with reason, nothing else.",
                    )
                    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
            )
            .with_agent(
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
            )
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

// ---------------------------------------------------------------------------
// Pattern 6: Swarm / Peer Handoff
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_swarm_peer_handoff() {
    require_deepseek_key();
    with_retries("swarm", || async {
        let os = AgentOs::builder()
            .with_agent(
                "math_agent",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(6)
                    .with_system_prompt(
                        "You are a math specialist. Answer math questions directly.\n\
                        If the user asks about code/programming, hand off by calling agent_run for agent_id=code_agent with the question, background=false, and return their answer.\n\
                        If the user asks about writing/text, hand off by calling agent_run for agent_id=writing_agent with the question, background=false, and return their answer.",
                    )
                    .with_allowed_agents(vec!["code_agent".to_string(), "writing_agent".to_string()]),
            )
            .with_agent(
                "code_agent",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(6)
                    .with_system_prompt(
                        "You are a coding specialist. Answer programming questions directly.\n\
                        If the user asks about math/calculations, hand off by calling agent_run for agent_id=math_agent with the question, background=false, and return their answer.\n\
                        If the user asks about writing/text, hand off by calling agent_run for agent_id=writing_agent with the question, background=false, and return their answer.",
                    )
                    .with_allowed_agents(vec!["math_agent".to_string(), "writing_agent".to_string()]),
            )
            .with_agent(
                "writing_agent",
                AgentDefinition::new("deepseek-chat")
                    .with_max_rounds(6)
                    .with_system_prompt(
                        "You are a writing specialist. Answer writing and text questions directly.\n\
                        If the user asks about math/calculations, hand off by calling agent_run for agent_id=math_agent with the question, background=false, and return their answer.\n\
                        If the user asks about code/programming, hand off by calling agent_run for agent_id=code_agent with the question, background=false, and return their answer.",
                    )
                    .with_allowed_agents(vec!["math_agent".to_string(), "code_agent".to_string()]),
            )
            .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
            .build()
            .unwrap();

        // Enter via math_agent but ask a coding question -> should hand off to code_agent
        let r = execute_run(
            &os,
            "math_agent",
            "swarm-handoff",
            "How do I implement a linked list in Rust?",
        ).await?;

        if !r.called_agents.contains("code_agent") {
            return Err(format!(
                "math_agent should have handed off to code_agent, got {:?}",
                r.called_agents
            ));
        }
        if r.text.trim().is_empty() {
            return Err("no response text".to_string());
        }
        eprintln!(
            "[swarm] OK: math_agent handed off to {:?}, text={}",
            r.called_agents,
            &truncate_str(&r.text, 200)
        );
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Pattern 5b: Generator-Critic with verbose trace
//
// Generator is the main agent with full history. Critic is a child agent.
// Generator writes a limerick, calls critic, revises if needed.
// This trace logs every agent_run call to show the generate→critique→revise loop.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pattern_generator_critic_verbose_trace() {
    require_deepseek_key();

    let os = AgentOs::builder()
        .with_agent(
            "critic",
            AgentDefinition::new("deepseek-chat")
                .with_system_prompt(
                    "You are a strict limerick critic. Check if the text is a valid limerick:\n\
                    - Must have exactly 5 lines\n\
                    - Lines 1, 2, 5 must rhyme (A rhyme)\n\
                    - Lines 3, 4 must rhyme (B rhyme)\n\
                    - Lines 1, 2, 5 should be longer than lines 3, 4\n\
                    Be strict. If ANY rule is violated, output: FAIL: <specific issue>\n\
                    Only if ALL rules pass, output: PASS\n\
                    Output ONLY PASS or FAIL with reason.",
                )
                .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()]),
        )
        .with_agent(
            "generator",
            AgentDefinition::new("deepseek-chat")
                .with_max_rounds(20)
                .with_system_prompt(
                    "You are a limerick writer with a built-in quality review process.\n\
                    Follow these steps EXACTLY:\n\
                    1. Write a limerick (5 lines, AABBA rhyme) about the user's topic.\n\
                    2. Call agent_run for agent_id=critic with your limerick, background=false.\n\
                    3. If critic says FAIL, revise your limerick based on the feedback.\n\
                    4. Call agent_run for agent_id=critic again with the revised version.\n\
                    5. Repeat until critic says PASS or you have tried 4 times.\n\
                    6. Output the final limerick to the user.\n\
                    You MUST call the critic before finishing. Never skip the review.",
                )
                .with_allowed_agents(vec!["critic".to_string()]),
        )
        .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
        .build()
        .unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "generator".to_string(),
            thread_id: Some("gen-critic-verbose".to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user("Write a limerick about memory safety in Rust")],
            initial_decisions: vec![],
        })
        .await
        .unwrap();

    let stream = run.events;
    tokio::pin!(stream);

    let mut call_sequence: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut call_count = 0usize;

    let deadline = Duration::from_secs(180);
    let _ = tokio::time::timeout(deadline, async {
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::ToolCallReady { name, arguments, .. } => {
                    if name == "agent_run" {
                        let agent_id = arguments
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let prompt_preview = arguments
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .map(|s| truncate_str(s, 80).to_string())
                            .unwrap_or_default();
                        call_count += 1;
                        let entry = format!("#{call_count} agent_run({agent_id}) prompt={prompt_preview}");
                        eprintln!("[gen-critic-trace] {entry}");
                        call_sequence.push(entry);
                    }
                }
                AgentEvent::ToolCallDone { result: tr, .. } => {
                    if tr.tool_name == "agent_run" {
                        let status = if tr.is_success() { "OK" } else { "ERR" };
                        let output_preview = tr.data
                            .get("output")
                            .and_then(|v| v.as_str())
                            .map(|s| truncate_str(s, 120).to_string())
                            .unwrap_or_else(|| format!("{}", tr.data));
                        eprintln!("[gen-critic-trace]   -> {status}: {output_preview}");
                    }
                }
                AgentEvent::TextDelta { delta } => {
                    text.push_str(&delta);
                }
                AgentEvent::RunFinish { .. } => break,
                AgentEvent::Error { message, .. } => {
                    eprintln!("[gen-critic-trace] ERROR: {message}");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;

    eprintln!("\n[gen-critic-trace] === Call Sequence ===");
    for entry in &call_sequence {
        eprintln!("  {entry}");
    }
    eprintln!("[gen-critic-trace] Total agent_run calls: {call_count}");
    eprintln!("[gen-critic-trace] Final output: {}", truncate_str(&text, 300));

    assert!(call_count >= 1, "expected at least 1 critic call, got {call_count}");
    assert!(!text.trim().is_empty(), "expected non-empty final text");
}
