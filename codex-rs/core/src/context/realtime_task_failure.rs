//! Bounded task outcome for voice; never includes provider errors or user content.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

pub(crate) struct RealtimeTaskFailure;

impl ContextualUserFragment for RealtimeTaskFailure {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("realtime.task_failure".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<realtime_task_failure>", "</realtime_task_failure>")
    }

    fn body(&self) -> String {
        "The delegated task failed. No successful completion is confirmed. Tell the user briefly, in the language of the conversation, that the task could not be completed. Do not merely acknowledge or ask them to wait. Do not invent a cause, claim success, or retry the task automatically.".to_string()
    }
}
