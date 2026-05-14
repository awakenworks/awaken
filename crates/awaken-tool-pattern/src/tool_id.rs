/// Match a tool-id pattern against a literal tool id (catalog filtering for
/// `AgentSpec.allowed_tools` / `excluded_tools`).
///
/// Tool ids are opaque strings, so this matcher is intentionally simpler than
/// `wildcard_match`: `/`, `:`, and `_` are ordinary characters and there is
/// no path / glob / regex / negation semantics.
///
/// Grammar:
///
/// - The full pattern must match the full tool id (anchored).
/// - `*` matches any sequence of characters, including `/`, `:`, `_`.
/// - `\` escapes the next character (`\*` is a literal `*`; `\\` a literal `\`).
/// - Every other character is a literal.
#[must_use]
pub fn tool_id_match(pattern: &str, tool_id: &str) -> bool {
    let p = pattern.as_bytes();
    let v = tool_id.as_bytes();
    let mut pi = 0usize;
    let mut vi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < v.len() {
        if pi < p.len() {
            let c = p[pi];
            if c == b'\\' && pi + 1 < p.len() {
                if p[pi + 1] == v[vi] {
                    pi += 2;
                    vi += 1;
                    continue;
                }
            } else if c == b'*' {
                star_pi = Some(pi);
                star_vi = vi;
                pi += 1;
                continue;
            } else if c == v[vi] {
                pi += 1;
                vi += 1;
                continue;
            }
        }
        // Mismatch — backtrack to the last `*` and consume one more value byte.
        if let Some(sp) = star_pi {
            pi = sp + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    // Consume any trailing `*`s left in the pattern.
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}
