//! Resolves a failed server-managed handoff through its existing native output.

use super::RealtimeConversationManager;
use super::RealtimeOutbound;
use crate::context::ContextualUserFragment;
use crate::context::RealtimeTaskFailure;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;

impl RealtimeConversationManager {
    pub(crate) async fn fail_active_handoff(&self) -> Result<()> {
        let handoff = {
            let guard = self.state.lock().await;
            guard.as_ref().map(|state| state.handoff.clone())
        };
        let Some(handoff) = handoff else {
            return Ok(());
        };
        if handoff.client_managed_handoffs {
            return Ok(());
        }
        let handoff_id = {
            let mut stream = handoff.stream.lock().await;
            let Some(handoff_id) = stream.active_handoff.take() else {
                return Ok(());
            };
            // Pending flushes must not append a stale acknowledgment after failure.
            stream.items.clear();
            handoff_id
        };
        // TurnComplete must not repeat the last successful-looking progress message.
        *handoff.last_output.lock().await = None;
        handoff
            .output_tx
            .send(RealtimeOutbound::FailedHandoff {
                handoff_id,
                text: RealtimeTaskFailure.render(),
            })
            .await
            .map_err(|_| CodexErr::InvalidRequest("conversation is not running".to_string()))
    }
}
