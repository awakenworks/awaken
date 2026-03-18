#!/usr/bin/env bash
# Scripted demo for asciinema recording
set -e
cd /home/chaizhenhua/Codes/uncarve

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
type_slow "# Start the Tirea agent server"
sleep 0.5

printf '\033[1;32m❯\033[0m '
type_slow "./target/release/ai-sdk-starter-agent --http-addr 127.0.0.1:18080"

# Start server in background, capture output
./target/release/ai-sdk-starter-agent --http-addr 127.0.0.1:18080 &
SERVER_PID=$!
sleep 2

echo ""
printf '\033[1;32m❯\033[0m '
type_slow "# Send a request via AI SDK v6 protocol"
sleep 0.3

printf '\033[1;32m❯\033[0m '
type_slow 'curl -N -X POST http://127.0.0.1:18080/v1/ai-sdk/agents/default/runs \'
type_slow '  -H "Content-Type: application/json" \'
type_slow '  -d '\''{"id":"demo","messages":[{"role":"user","content":"What is the weather in Beijing? Use the weather tool."}]}'\'''
sleep 0.3

echo ""
echo -e "\033[1;36m--- streaming response ---\033[0m"

# Actually send the request and show streaming
curl -s -N -X POST http://127.0.0.1:18080/v1/ai-sdk/agents/default/runs \
  -H "Content-Type: application/json" \
  -d '{"id":"demo-rec","messages":[{"role":"user","content":"What is the weather in Beijing? Use the weather tool."}]}' \
  2>&1

echo ""
echo ""
echo -e "\033[1;32m✅ Done — tool call + LLM streaming from a single Rust binary\033[0m"
sleep 2

kill $SERVER_PID 2>/dev/null
wait $SERVER_PID 2>/dev/null
