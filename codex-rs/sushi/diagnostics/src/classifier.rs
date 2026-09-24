use super::Reason;
use super::Record;
use super::identifier;
use super::writer;
use codex_protocol::ThreadId;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Skipped,
    RequestStarted,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Transition {
    attempt_id: Uuid,
    parent_thread_id: ThreadId,
    parent_turn_id: Option<String>,
    phase: Phase,
    reason_code: Reason,
    request_started: bool,
    recommended_model: Option<String>,
    http_status_code: Option<u16>,
}

pub struct ClassifierAttempt {
    emitter: Option<writer::Emitter>,
    transition: Transition,
    finished: bool,
    succeeded: bool,
}

impl ClassifierAttempt {
    pub fn new(thread_id: ThreadId, turn_id: &str) -> Self {
        Self {
            emitter: writer::emitter().cloned(),
            transition: Transition {
                attempt_id: Uuid::new_v4(),
                parent_thread_id: thread_id,
                parent_turn_id: identifier(turn_id),
                phase: Phase::Skipped,
                reason_code: Reason::Native,
                request_started: false,
                recommended_model: None,
                http_status_code: None,
            },
            finished: false,
            succeeded: false,
        }
    }

    pub fn request_started(&mut self) {
        self.transition.request_started = true;
        self.emit(Phase::RequestStarted, Reason::RequestStarted);
    }

    pub fn recommended(&mut self, model: &str) {
        self.succeeded = true;
        self.transition.recommended_model = identifier(model);
        self.finish(Reason::JevSelected);
    }

    pub fn http_status(&mut self, status: u16) {
        self.transition.http_status_code = Some(status);
    }

    pub fn finish(&mut self, reason: Reason) {
        if self.finished {
            return;
        }
        self.finished = true;
        let phase = if self.succeeded {
            Phase::Succeeded
        } else if self.transition.request_started {
            Phase::Failed
        } else {
            Phase::Skipped
        };
        self.emit(phase, reason);
    }

    fn emit(&mut self, phase: Phase, reason: Reason) {
        self.transition.phase = phase;
        self.transition.reason_code = reason;
        if let Some(emitter) = &self.emitter {
            emitter.emit(Record::ClassifierTransition(self.transition.clone()));
        }
    }
}

impl Drop for ClassifierAttempt {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(Phase::Cancelled, Reason::Cancelled);
        }
    }
}

#[cfg(test)]
#[path = "classifier_tests.rs"]
mod tests;
