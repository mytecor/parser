use crate::{Error, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceQuestion {
    #[serde(rename = "type")]
    pub question_type: String,
    pub instructions: String,
    pub criteria: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub state: String,
    pub questions: BTreeMap<String, ChoiceQuestion>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceAnswer {
    pub choice: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, ChoiceAnswer>,
}

/// One call per parse, no retries. Implementations must not log document context or credentials.
#[async_trait]
pub trait JevClient: Send + Sync {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse>;
}

/// Official TypeSafe System One HTTP API. Credentials are deliberately not Debug.
pub struct HttpJevClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
}
impl HttpJevClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Error::Healing("could not create HTTP client".into()))?;
        Ok(Self {
            client,
            api_key: api_key.into(),
            model: "jev-latest".into(),
        })
    }
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}
#[async_trait]
impl JevClient for HttpJevClient {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        let body = serde_json::json!({ "model": self.model, "state": request.state, "questions": request.questions });
        let mut response = self
            .client
            .post("https://api.typesafe.ai/v1/systemone")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| Error::Healing("Jev transport error".into()))?;
        if !response.status().is_success() {
            return Err(Error::Healing(format!(
                "Jev HTTP {}",
                response.status().as_u16()
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Healing("Jev response read error".into()))?
        {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(Error::Limit("Jev response bytes"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Healing("invalid Jev response".into()))
    }
}
