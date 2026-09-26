use serde_json::json;

/// Classifier state: the task name and role, plus a bounded `task_message` excerpt when the
/// caller supplies an authorized plaintext message and `max_bytes` is nonzero. The excerpt
/// drops control characters other than newline and is cut at the last UTF-8 character
/// boundary within `max_bytes`, so it never exceeds the limit or splits a sequence.
pub(super) fn state(
    task_name: &str,
    role: &str,
    plaintext_message: Option<&str>,
    max_bytes: u32,
) -> serde_json::Value {
    let mut state = json!({"task_name": task_name, "agent_type": role});
    let Some(message) = plaintext_message.filter(|_| max_bytes > 0) else {
        return state;
    };
    let max_bytes = max_bytes as usize;
    let mut excerpt = String::with_capacity(message.len().min(max_bytes));
    for character in message.chars().filter(|c| *c == '\n' || !c.is_control()) {
        if excerpt.len() + character.len_utf8() > max_bytes {
            break;
        }
        excerpt.push(character);
    }
    state["task_message"] = json!(excerpt);
    state
}
