//! Approval prompts: requests the sandboxes raise for a human decision, such
//! as access to a host the policy does not allow.
//!
//! `GET /user-prompts` is a stream that starts with `{"action": "synced"}` and
//! then delivers prompts as they are raised; each is answered with the
//! identifier of one of its options.
//!
//! A connection denied by a rule is refused by the proxy (HTTP 403) without a
//! prompt. Prompts are presumably raised when no rule decides, which the
//! default `default-allow-all` rule never lets happen; the shape of a prompt
//! has therefore not been observed yet, and [`PromptEvent`] keeps every field
//! in [`PromptEvent::extra`].

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Extra;
use crate::client::Client;
use crate::error::Result;
use crate::transport::{NdjsonStream, Request, parse_json_line, segment};

/// Stream of approval prompts, returned by [`Prompts::subscribe`].
pub type PromptStream = NdjsonStream<PromptEvent>;

/// One line of the prompt stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptEvent {
    /// What happened, e.g. `synced` once the pending prompts are delivered.
    #[serde(default)]
    pub action: Option<String>,
    /// Identifier of the prompt, to pass to [`Prompts::respond`].
    #[serde(default)]
    pub id: Option<String>,
    /// Every other field.
    #[serde(flatten)]
    pub extra: Extra,
}

impl PromptEvent {
    /// Whether this line marks the end of the pending prompts.
    #[must_use]
    pub fn is_synced(&self) -> bool {
        self.action.as_deref() == Some("synced")
    }

    /// Parses one NDJSON line of the stream.
    fn parse_line(line: &str) -> Result<Option<Self>> {
        parse_json_line("prompt stream", line)
    }
}

/// Approval prompt operations. Obtained with
/// [`Client::prompts`](crate::Client::prompts).
#[derive(Debug, Clone)]
pub struct Prompts {
    /// Client the requests go through.
    client: Client,
}

impl Prompts {
    /// Prompt operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Subscribes to the approval prompts (`GET /user-prompts`). The stream
    /// never ends on its own; dropping it unsubscribes.
    ///
    /// # Errors
    ///
    /// Fails if the subscription cannot be opened.
    pub async fn subscribe(&self) -> Result<PromptStream> {
        let request = Request::get("prompt stream", "/user-prompts");
        self.client
            .transport()
            .ndjson(request, PromptEvent::parse_line)
            .await
    }

    /// Answers the prompt `id` with the option `option_id`
    /// (`POST /user-prompts/{id}/response`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) for an unknown
    /// prompt.
    pub async fn respond(&self, id: &str, option_id: &str) -> Result<()> {
        let path = format!("/user-prompts/{}/response", segment(id));
        let request =
            Request::post("prompt response", path).json(&json!({ "option_id": option_id }))?;
        self.client.transport().unit(request).await
    }
}
