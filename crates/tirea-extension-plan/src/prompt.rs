/// Build the plan mode system prompt.
pub fn plan_mode_system_prompt() -> String {
    "Plan mode is active. The user indicated that they do not want you to execute yet.\n\
     \n\
     You MUST NOT:\n\
     - Make any edits to files\n\
     - Run any non-read-only tools (no Bash, Edit, Write, NotebookEdit)\n\
     - Make commits, push code, or modify system state\n\
     \n\
     You SHOULD:\n\
     - Use Read, Glob, Grep to explore the codebase\n\
     - Analyze the problem thoroughly before proposing a plan\n\
     - When your plan is ready, call ExitPlanMode with your plan content\n\
     \n\
     This overrides any other instructions you have received."
        .to_string()
}

/// Build a context hint for an approved plan that is no longer in plan mode.
pub fn approved_plan_hint(summary: &str) -> String {
    format!(
        "You have an approved plan: {summary}\n\
         The plan content was provided when plan mode was exited. \
         Refer to it in the conversation history if needed."
    )
}
