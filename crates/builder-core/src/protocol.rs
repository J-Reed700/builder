use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    /// The model's visible reasoning (`reasoning_content`), kept for the
    /// transcript only. Providers strip it before sending history back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn tool(id: &str, content: String) -> Self {
        Self {
            tool_call_id: Some(id.into()),
            ..Self::text(Role::Tool, content)
        }
    }

    /// Serialized size of the representation that belongs in a future model
    /// prompt. Reasoning is transcript-only and providers deliberately omit it.
    pub fn prompt_bytes(&self) -> Result<usize, serde_json::Error> {
        #[derive(Serialize)]
        struct PromptMessage<'a> {
            role: Role,
            content: Option<&'a str>,
            #[serde(skip_serializing_if = "<[ToolCall]>::is_empty")]
            tool_calls: &'a [ToolCall],
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_call_id: Option<&'a str>,
        }

        serde_json::to_vec(&PromptMessage {
            role: self.role,
            content: self.content.as_deref(),
            tool_calls: &self.tool_calls,
            tool_call_id: self.tool_call_id.as_deref(),
        })
        .map(|bytes| bytes.len())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: Function,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Function {
    pub name: String,
    pub arguments: String,
}
