#!/usr/bin/env bash
# Scripted demo for asciinema recording.
#
# Prerequisites:
#   - cargo build --workspace --release  (build BEFORE recording)
#   - LLM_API_KEY set
#   - asciinema + svg-term-cli installed
#
# Record:
#   LLM_BASE_URL=... LLM_API_KEY=... LLM_MODEL=... \
#     asciinema rec docs/assets/demo.cast -c "bash docs/assets/record-demo.sh"
#
# Convert:
#   svg-term --in docs/assets/demo.cast --out docs/assets/demo.svg --window --width 80 --height 22

set -e
cd "$(dirname "$0")/../.."

type_slow() {
  local text="$1"
  for (( i=0; i<${#text}; i++ )); do
    printf '%s' "${text:$i:1}"
    sleep 0.04
  done
  echo
}

# Filter: keep only the meaningful lines, strip internal IDs
filter_output() {
  grep -E "^(===|Asking|🚀|🔧|✅|🏁|---)" | \
    sed 's/ (id=[^ )]*)//' | \
    sed 's/✅ Tool result: [^ ]* → /✅ Tool result: /' | \
    sed 's/ ---$//' | \
    sed 's/--- Response:/\n💬 Answer:/'
}

clear
echo ""
printf '  \033[1;34mAwaken\033[0m — Production AI agent runtime for Rust\n'
printf '  Type-safe state · Multi-protocol serving · Plugin extensibility\n'
echo ""
sleep 1

printf '\033[1;32m❯\033[0m '
type_slow "# 1. Define a tool + agent in Rust, build to a single binary"
sleep 0.3

printf '\033[1;32m❯\033[0m '
type_slow "cat crates/awaken/examples/tool_call_live.rs | head -6"
sleep 0.2
echo -e "\033[0;37m//! Live tool call test with a real LLM provider via GenaiExecutor."
echo "//!"
echo "//! Run with BigModel GLM:"
echo "//!   LLM_BASE_URL=... LLM_API_KEY=<key> LLM_MODEL=GLM-4.7-Flash"
echo "//!"
echo -e "//! Also supports: OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.\033[0m"
echo ""
sleep 0.8

printf '\033[1;32m❯\033[0m '
type_slow "# 2. Run — the agent calls the calculator tool, then answers"
sleep 0.3

printf '\033[1;32m❯\033[0m '
type_slow "./target/release/examples/tool_call_live"
sleep 0.2
echo ""

# Run the pre-built binary, filter output to focus on key events
./target/release/examples/tool_call_live 2>&1 | filter_output

echo ""
sleep 1

printf '\033[1;32m❯\033[0m '
type_slow "# 3. Same binary also serves HTTP — React, CopilotKit, A2A, MCP"
sleep 0.3
echo ""
echo -e "  \033[0;36mPOST /v1/ai-sdk/chat\033[0m   → React useChat()"
echo -e "  \033[0;36mPOST /v1/ag-ui/run\033[0m     → CopilotKit"
echo -e "  \033[0;36mPOST /v1/a2a/message:send\033[0m → Agent-to-Agent"
echo -e "  \033[0;36mPOST /v1/mcp\033[0m           → Model Context Protocol"
echo ""

sleep 2
printf '\033[1;32m✅ One Rust binary. Tools + agents + state + plugins. Serve everywhere.\033[0m\n'
sleep 3
