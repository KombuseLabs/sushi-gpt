//! A single bounded TypeSafe Choice request; never starts an agent or logs task/key data.
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

/// Inputs for one Choice evaluation. The caller owns the data disclosure policy.
pub struct JevRequest<'a> {
    pub endpoint: &'a str,
    pub model: &'a str,
    pub state: Value,
    pub instructions: &'a str,
    pub criteria: BTreeMap<&'a str, &'a str>,
    pub timeout: Duration,
    pub min_confidence: f64,
}

/// Validated classification; confidence measures the class distribution, not task success.
#[derive(Debug, PartialEq)]
pub struct JevDecision {
    pub choice: String,
    pub confidence: f64,
}

/// Safe diagnostic reasons: no response bodies, URLs, task text, or credentials.
#[derive(Debug, PartialEq, Eq)]
pub enum JevFallback {
    MissingKey,
    Transport,
    Timeout,
    Http(u16),
    OversizedResponse,
    InvalidResponse,
    Uncertain,
}

#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, Answer>,
}
#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "type")]
    kind: String,
    choice: String,
    confidence: f64,
    probabilities: BTreeMap<String, f64>,
}

/// Calls the HTTP API directly once, respecting proxy policy and rejecting redirects.
/// Missing credentials and every untrusted response failure are recoverable routing fallbacks.
pub async fn classify_jev(
    factory: &HttpClientFactory,
    request: JevRequest<'_>,
    api_key: Option<&str>,
    on_send: impl FnOnce(),
) -> Result<JevDecision, JevFallback> {
    let api_key = api_key
        .filter(|key| !key.trim().is_empty())
        .ok_or(JevFallback::MissingKey)?;
    tokio::time::timeout(request.timeout, async {
        let client = HttpClientBuilder::new()
            .without_redirects()
            .without_request_logging()
            .connect_timeout(request.timeout)
            .build_respecting_outbound_proxy_policy(
                factory,
                request.endpoint,
                ClientRouteClass::Other,
            )
            .map_err(|_| JevFallback::Transport)?;
        let pending = client
            .post(request.endpoint)
            .bearer_auth(api_key)
            .timeout(request.timeout)
            .json(&json!({
                "model": request.model,
                "state": request.state,
                "questions": {
                    "route": {
                        "type": "choice",
                        "instructions": request.instructions,
                        "criteria": request.criteria
                    }
                }
            }));
        on_send();
        let mut response = pending.send().await.map_err(|error| {
            if error.is_timeout() {
                JevFallback::Timeout
            } else {
                JevFallback::Transport
            }
        })?;
        if !response.status().is_success() {
            return Err(JevFallback::Http(response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| JevFallback::Transport)? {
            if body.len() + chunk.len() > 32768 {
                return Err(JevFallback::OversizedResponse);
            }
            body.extend_from_slice(&chunk);
        }
        let mut response: Response =
            serde_json::from_slice(&body).map_err(|_| JevFallback::InvalidResponse)?;
        let answer = response
            .answers
            .remove("route")
            .ok_or(JevFallback::InvalidResponse)?;
        if answer.kind != "choice"
            || !answer.confidence.is_finite()
            || !(0.0..=1.0).contains(&answer.confidence)
            || answer.probabilities.len() != request.criteria.len()
            || request
                .criteria
                .keys()
                .any(|key| !answer.probabilities.contains_key(*key))
            || answer
                .probabilities
                .values()
                .any(|p| !p.is_finite() || !(0.0..=1.0).contains(p))
            || (answer.probabilities.values().sum::<f64>() - 1.0).abs() > 0.001
        {
            return Err(JevFallback::InvalidResponse);
        }
        let probability = answer
            .probabilities
            .get(&answer.choice)
            .ok_or(JevFallback::InvalidResponse)?;
        if answer.probabilities.values().any(|p| p > probability) {
            return Err(JevFallback::InvalidResponse);
        }
        if answer.choice == "abstain"
            || answer.confidence < request.min_confidence
            || answer
                .probabilities
                .iter()
                .any(|(name, p)| name != &answer.choice && p == probability)
        {
            return Err(JevFallback::Uncertain);
        }
        Ok(JevDecision {
            choice: answer.choice,
            confidence: answer.confidence,
        })
    })
    .await
    .map_err(|_| JevFallback::Timeout)?
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
