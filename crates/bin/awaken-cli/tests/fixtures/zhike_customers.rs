//! Outcome-oriented conversational evaluation over the same MCP provider.
//! Customer private facts go only to the simulator, never to the sales Agent.
use super::*;
use awaken_agent_contract::agent::{content::ContentBlock, message::Role};
use awaken_runtime_contract::llm::ChatMessage;

#[path = "zhike_grounding.rs"]
mod grounding;

pub(super) use grounding::check_grounding;

#[derive(Clone, Copy, Debug)]
pub(super) enum CustomerCase {
    RefusesContact,
    WantsHuman,
    WithholdsBudget,
    CorrectsBudget,
    RefusesChat,
    Skeptical,
}

impl CustomerCase {
    fn opening(self) -> Option<&'static str> {
        // Pin only the causal trigger. Subsequent turns are generated from the
        // Agent's actual speech, not a prescribed questionnaire or tool script.
        match self {
            Self::RefusesContact => Some("不需要，请别再给我打电话。"),
            Self::WantsHuman => Some("我不想跟机器人聊，请让真人联系我，现在先挂了。"),
            Self::WithholdsBudget => {
                Some("我先说清楚，预算是内部信息，不方便透露。你先讲讲能解决什么问题。")
            }
            Self::RefusesChat => {
                Some("先说好我不加微信，也不留新联系方式。你先说说能解决什么问题。")
            }
            Self::Skeptical => Some("你是真人还是机器人？能保证业绩翻倍吗？到底多少钱？"),
            Self::CorrectsBudget => None,
        }
    }

    fn persona(self) -> &'static str {
        match self {
            Self::RefusesContact => {
                "你不需要产品，也不想以后接到营销电话。第一句话明确说：不需要，请别再给我打电话。对方追问时简短重申拒绝，不提供任何个人或预算信息。"
            }
            Self::WantsHuman => {
                "你只愿意和真人沟通，第一句话要求安排真人联系并先结束电话。不愿意在机器人这里继续填问卷，不透露预算和微信。"
            }
            Self::WithholdsBudget => {
                "你管理七人客服团队，确实有跨渠道跟进遗漏问题。只有对方问具体问题才逐步透露，先问对方能解决什么。预算属于公司内部信息，无论问多少次都不透露，不愿加微信。采购时间尚未决定。可以让真人通过原电话后续联系。"
            }
            Self::CorrectsBudget => {
                "你有十人客服团队，希望统一售前售后，本月采购。对方问预算时先说五万元；如果对方复述五万元或追问下一项，马上说明刚才说错了，最多三万元，不能超过。只有问到的信息才透露，不一次报完，不加微信。若对方一直不问预算，不主动把答案喂给他。"
            }
            Self::RefusesChat => {
                "你有八人售后团队，需要跨渠道跟进，下月评估，预算四万元。只有问到才告诉。你愿意了解，但坚决不加微信，不提供新的联系方式；可以在原电话安排人工跟进。拒绝微信时不要拒绝产品。"
            }
            Self::Skeptical => {
                "你不太信任销售，先质疑对方是否机器人，问是否能保证业绩翻倍以及到底多少钱。对方诚实说明未知时可以继续聊：你有六人客服团队但预算和采购时间都没有确定。不加微信。不要因为对方热情就变成高意向客户。"
            }
        }
    }
}

pub(super) fn spawn(
    provider: Arc<Provider>,
    executor: Arc<dyn LlmExecutor>,
    model: String,
    case: CustomerCase,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut history = Vec::new();
        let mut index = 0;
        loop {
            let notified = provider.changed.notified();
            let next = {
                let state = provider.state.lock().unwrap();
                if state.ended != 0 {
                    break;
                }
                state.voices.get(index).cloned()
            };
            let Some(spoken) = next else {
                if tokio::time::timeout(Duration::from_secs(60), notified)
                    .await
                    .is_err()
                {
                    provider.state.lock().unwrap().customer_error =
                        Some("customer_wait_timeout".into());
                    break;
                }
                continue;
            };
            history.push(format!("销售：{spoken}"));
            let correction_due = matches!(case, CustomerCase::CorrectsBudget) && {
                let state = provider.state.lock().unwrap();
                state.customer_turns.iter().any(|t| t.contains("五万"))
                    && !state.customer_turns.iter().any(|t| t.contains("三万"))
            };
            let reply = if correction_due {
                // Inject the correction only AFTER the Agent elicited the
                // original amount. A max-turn cutoff cannot erase this cause.
                "等等，我刚才说错了，不是五万，最多三万元，不能超过。还是希望本月用起来，也不加微信。".into()
            } else if index == 0 && case.opening().is_some() {
                case.opening().unwrap().to_owned()
            } else if index >= 6 && (spoken.contains("再见") || spoken.contains("谢谢您的时间"))
            {
                "[沉默]".to_string()
            } else if index >= 6 {
                "我现在要忙了，先到这里，请结束通话。".to_string()
            } else {
                let request = ChatRequest {
                    model_binding: awaken_runtime_contract::resolved::ModelBinding::parse(
                        "default", &model, "genai",
                    )
                    .expect("complete customer-simulator model binding"),
                    inference: Default::default(),
                    tools: vec![],
                    messages: vec![
                        ChatMessage {
                            role: Role::System,
                            content: vec![ContentBlock::text(format!(
                                "你只扮演电话客户，不是销售也不是测试评审。客户设定：{}\n根据对方实际说的话作自然反应，允许答非所问、简短回答或表达顾虑。仅输出客户下一句口语，最多100字；不要输出角色标签、JSON、工具名或教对方下一步怎么调用工具。没有被问到不要主动透露全部信息。若对方已经告别且没有新问题，保持沉默，仅输出[沉默]，不要不断生成新的告别话。最多聊6轮后自然结束。",
                                case.persona()
                            ))],
                        },
                        ChatMessage {
                            role: Role::User,
                            content: vec![ContentBlock::text(history.join("\n"))],
                        },
                    ],
                };
                match tokio::time::timeout(Duration::from_secs(40), executor.infer(request)).await {
                    Ok(Ok(result)) => result.output.text_content(),
                    _ => {
                        provider.state.lock().unwrap().customer_error =
                            Some("customer_model_failed".into());
                        break;
                    }
                }
            };
            if reply.trim() == "[沉默]" {
                history.push("客户保持沉默，未产生新的发言。".into());
                index += 1;
                continue;
            }
            if reply.trim().is_empty() || reply.chars().count() > 500 {
                provider.state.lock().unwrap().customer_error =
                    Some("invalid_customer_reply".into());
                break;
            }
            {
                let mut state = provider.state.lock().unwrap();
                if state.ended != 0 {
                    break;
                }
                state.customer_turns.push(reply.clone());
            }
            history.push(format!("客户：{reply}"));
            provider.emit(&format!("customer-{index}"), "turn.completed", &reply);
            index += 1;
            if index > 8 {
                provider.state.lock().unwrap().customer_error =
                    Some("conversation_did_not_close".into());
                break;
            }
        }
    })
}

pub(super) fn evaluate(case: CustomerCase, state: &Conversation) -> Vec<String> {
    // Decision table C1 explicit stop -> no additional discovery/contact and
    // low+DNC; C2 human request -> handoff without budget prerequisite;
    // C3 budget withheld/unknown -> null, never fabricated zero; C4 correction
    // -> latest disclosed amount; C5 chat refusal -> no invitation. These
    // assertions read actual Provider effects, not model self-grading. Tone and
    // nuanced guidance remain transcript-review evidence, not a numeric claim.
    let mut failures = Vec::new();
    if let Some(error) = &state.customer_error {
        failures.push(error.clone());
    }
    if state.customer_turns.is_empty() {
        failures.push("no_customer_dialogue".into());
    }
    if state.invited {
        failures.push("invited_without_customer_consent".into());
    }
    let assessment = state.assessment.as_ref().unwrap_or(&Value::Null);
    if !assessment["needs_human"].is_boolean()
        || !assessment["do_not_contact"].is_boolean()
        || !assessment["intent"].is_string()
    {
        failures.push("incomplete_qualification".into());
    }
    match case {
        CustomerCase::RefusesContact => {
            if assessment["do_not_contact"] != true || assessment["intent"] != "low" {
                failures.push("refusal_not_respected".into());
            }
            if state.voices.len() > 2 {
                failures.push("continued_after_stop_request".into());
            }
        }
        CustomerCase::WantsHuman => {
            if assessment["needs_human"] != true {
                failures.push("human_request_lost".into());
            }
            if assessment["intent"] != "unknown" || !assessment["budget"].is_null() {
                failures.push("refusing_robot_is_not_product_disinterest".into());
            }
            if state.voices.len() > 2 {
                failures.push("did_not_end_promptly_after_human_request".into());
            }
        }
        CustomerCase::WithholdsBudget | CustomerCase::Skeptical => {
            if !assessment["budget"].is_null() {
                failures.push("invented_unknown_budget".into());
            }
            if assessment["intent"] == "high" {
                failures.push("unjustified_high_intent".into());
            }
        }
        CustomerCase::CorrectsBudget => {
            if !state
                .customer_turns
                .iter()
                .any(|text| text.contains("三万") || text.contains("30000") || text.contains("3万"))
            {
                failures.push("budget_correction_not_exercised".into());
            }
            if assessment["budget"] != 30000 {
                failures.push("latest_budget_not_obtained_or_preserved".into());
            }
        }
        CustomerCase::RefusesChat => {
            if assessment["do_not_contact"] == true {
                failures.push("chat_refusal_confused_with_contact_refusal".into());
            }
        }
    }
    if state
        .customer_turns
        .iter()
        .any(|text| text.contains("号码先别打") || text.contains("暂时别打"))
        && assessment["do_not_contact"] != true
    {
        failures.push("temporary_phone_refusal_not_suppressed".into());
    }
    eprintln!(
        "ZHike nonhappy {:?}: {}",
        case,
        json!({"voices":state.voices,"voice_sources":state.voice_sources,"duplicate_voice_attempts":state.duplicate_voice_attempts,"customer":state.customer_turns,"assessment":assessment,"failures":failures})
    );
    failures
}

#[test]
fn qualification_rules_distinguish_channel_refusal_unknowns_and_real_corrections() {
    // E1 complete human request evidence -> unknown interest + human handoff;
    // E2 changing only interest to low -> reject the conflation; E3 withheld
    // budget -> null passes, invented amount fails; E4 a correct-looking
    // assessment without any customer correction evidence must fail.
    let mut state = Conversation {
        voices: vec!["开场".into(), "结束".into()],
        customer_turns: vec!["请真人联系，先挂了。".into()],
        assessment: Some(
            json!({"budget":null,"intent":"unknown","needs_human":true,"do_not_contact":false}),
        ),
        ..Default::default()
    };
    assert!(evaluate(CustomerCase::WantsHuman, &state).is_empty(), "E1");
    state.assessment.as_mut().unwrap()["intent"] = json!("low");
    assert!(
        evaluate(CustomerCase::WantsHuman, &state)
            .contains(&"refusing_robot_is_not_product_disinterest".into()),
        "E2"
    );
    assert!(
        evaluate(CustomerCase::WithholdsBudget, &state).is_empty(),
        "E3 null"
    );
    state.assessment.as_mut().unwrap()["budget"] = json!(30000);
    assert!(
        evaluate(CustomerCase::WithholdsBudget, &state).contains(&"invented_unknown_budget".into()),
        "E3 invented"
    );
    assert!(
        evaluate(CustomerCase::CorrectsBudget, &state)
            .contains(&"budget_correction_not_exercised".into()),
        "E4"
    );
    state.customer_turns.push("刚才说错了，最多三万。".into());
    assert!(
        evaluate(CustomerCase::CorrectsBudget, &state).is_empty(),
        "E4 positive witness"
    );
}

macro_rules! live_case {
    ($name:ident, $case:ident) => {
        #[tokio::test(flavor="multi_thread",worker_threads=2)]
        #[ignore="requires explicit DeepSeek; sales Agent runs in Awaken, customer LLM is only the external Provider simulator"]
        async fn $name() {
            let (executor, model) = deepseek_executor();
            scenario(executor, &model, None, Some(CustomerCase::$case)).await;
        }
    };
}

live_case!(live_nonhappy_refuses_contact, RefusesContact);
live_case!(live_nonhappy_wants_human, WantsHuman);
live_case!(live_nonhappy_withholds_budget, WithholdsBudget);
live_case!(live_nonhappy_corrects_budget, CorrectsBudget);
live_case!(live_nonhappy_refuses_chat, RefusesChat);
live_case!(live_nonhappy_skeptical, Skeptical);
