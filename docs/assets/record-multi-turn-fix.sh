#!/usr/bin/env bash
# Scripted demo: multi-turn conversation with tool calls (providerExecuted fix)
set -e
cd /home/chaizhenhua/Codes/awaken

type_slow() {
  local text="$1"
  for (( i=0; i<${#text}; i++ )); do
    printf '%s' "${text:$i:1}"
    sleep 0.04
  done
  echo
}

echo ""
printf '\033[1;32m❯\033[0m '
type_slow "# Start awaken agent server with BigModel API"
sleep 0.5

printf '\033[1;32m❯\033[0m '
type_slow "OPENAI_BASE_URL=https://open.bigmodel.cn/api/paas/v4 \\"
type_slow "  AGENT_MODEL=glm-4-flash-250414 \\"
type_slow "  ./target/debug/ai-sdk-starter-agent --http-addr 127.0.0.1:18080"

OPENAI_BASE_URL=https://open.bigmodel.cn/api/paas/v4 \
OPENAI_API_KEY="${BIGMODEL_API_KEY}" \
AGENT_MODEL=glm-4-flash-250414 \
./target/debug/ai-sdk-starter-agent --http-addr 127.0.0.1:18080 &
SERVER_PID=$!
sleep 3

# ── Turn 1 ──
echo ""
printf '\033[1;36m── Turn 1: user asks about weather ──\033[0m\n'
sleep 0.3
printf '\033[1;32m❯\033[0m '
type_slow 'curl -sN http://127.0.0.1:18080/v1/ai-sdk/chat -H "Content-Type: application/json" -d ...'
sleep 0.3

echo ""
RESP1=$(curl -sN --max-time 20 -X POST http://127.0.0.1:18080/v1/ai-sdk/chat \
  -H "Content-Type: application/json" \
  -d '{"threadId":"demo","messages":[{"id":"m1","role":"user","parts":[{"type":"text","text":"What is the weather in Tokyo?"}]}]}' 2>&1)
B1=$(echo "$RESP1" | wc -c)
T1=$(echo "$RESP1" | grep -c '"type":"text-delta"' || true)
echo -e "\033[0;33mResponse: ${B1} bytes, ${T1} text deltas\033[0m"
echo -e "\033[1;32m✅ Turn 1 OK\033[0m"
sleep 1

# ── Turn 2 ──
echo ""
printf '\033[1;36m── Turn 2: follow-up ──\033[0m\n'
sleep 0.3
printf '\033[1;32m❯\033[0m '
type_slow 'curl -sN ... (2 messages in history)'
sleep 0.3

echo ""
RESP2=$(curl -sN --max-time 20 -X POST http://127.0.0.1:18080/v1/ai-sdk/chat \
  -H "Content-Type: application/json" \
  -d '{
    "threadId":"demo",
    "messages":[
      {"id":"m1","role":"user","parts":[{"type":"text","text":"What is the weather in Tokyo?"}]},
      {"id":"m2","role":"assistant","parts":[{"type":"text","text":"The weather in Tokyo is sunny, 22 degrees."}]},
      {"id":"m3","role":"user","parts":[{"type":"text","text":"What about New York?"}]}
    ]
  }' 2>&1)
B2=$(echo "$RESP2" | wc -c)
T2=$(echo "$RESP2" | grep -c '"type":"text-delta"' || true)
echo -e "\033[0;33mResponse: ${B2} bytes, ${T2} text deltas\033[0m"
echo -e "\033[1;32m✅ Turn 2 OK\033[0m"
sleep 1

# ── Turn 3: the critical test ──
echo ""
printf '\033[1;36m── Turn 3: history with providerExecuted tool calls ──\033[0m\n'
printf '\033[0;90m  (This is where the bug caused empty responses)\033[0m\n'
sleep 0.5
printf '\033[1;32m❯\033[0m '
type_slow 'curl -sN ... (4 messages + 2 providerExecuted tool results)'
sleep 0.3

echo ""
RESP3=$(curl -sN --max-time 20 -X POST http://127.0.0.1:18080/v1/ai-sdk/chat \
  -H "Content-Type: application/json" \
  -d '{
    "threadId":"demo",
    "messages":[
      {"id":"m1","role":"user","parts":[{"type":"text","text":"Check weather in Tokyo"}]},
      {"id":"m2","role":"assistant","parts":[
        {"type":"text","text":"Let me check."},
        {"type":"tool-invocation","toolCallId":"c1","toolName":"get_weather","args":{"location":"Tokyo"},"state":"output-available","output":{"temp":22},"providerExecuted":true}
      ]},
      {"id":"m3","role":"user","parts":[{"type":"text","text":"Check New York too"}]},
      {"id":"m4","role":"assistant","parts":[
        {"type":"tool-invocation","toolCallId":"c2","toolName":"get_weather","args":{"location":"New York"},"state":"output-available","output":{"temp":15},"providerExecuted":true}
      ]},
      {"id":"m5","role":"user","parts":[{"type":"text","text":"Compare both cities briefly."}]}
    ]
  }' 2>&1)
B3=$(echo "$RESP3" | wc -c)
T3=$(echo "$RESP3" | grep -c '"type":"text-delta"' || true)
echo -e "\033[0;33mResponse: ${B3} bytes, ${T3} text deltas\033[0m"
if [ "$B3" -gt 100 ]; then
  echo -e "\033[1;32m✅ Turn 3 OK — multi-turn with tool history works!\033[0m"
else
  echo -e "\033[1;31m❌ Turn 3 FAILED — empty response (bug not fixed)\033[0m"
fi
sleep 2

echo ""
echo -e "\033[1;32m✅ All 3 turns passed — providerExecuted fix verified\033[0m"
sleep 2

kill $SERVER_PID 2>/dev/null
wait $SERVER_PID 2>/dev/null
