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

/// An image attached to a user message for a vision-capable model. The
/// bytes are base64-encoded so the transcript stays plain JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Image {
    /// MIME type of the payload, e.g. `image/png`.
    pub mime: String,
    /// Base64-encoded image bytes.
    pub data: String,
}

/// Prompt budget charged for one image, in serialized bytes (about 2k tokens
/// at the 2-bytes-per-token estimate). Vision models bill an image by its
/// tiles, not by the size of its base64 payload, so counting the payload
/// would make one screenshot look like a million tokens.
pub const IMAGE_PROMPT_BYTES: usize = 4096;

impl Image {
    /// The `data:` URL carried on the wire.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
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
    /// Images attached to this message, sent to the model as multimodal
    /// content. Only vision-capable profiles receive them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
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
            images: Vec::new(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    /// Attach images to a message, e.g. a pasted screenshot for a
    /// vision-capable model.
    pub fn with_images(mut self, images: Vec<Image>) -> Self {
        self.images = images;
        self
    }
    pub fn tool(id: &str, content: String) -> Self {
        Self {
            tool_call_id: Some(id.into()),
            ..Self::text(Role::Tool, content)
        }
    }

    /// The content exactly as sent to the model: plain text when no images
    /// are attached, otherwise a multimodal array with the text and each
    /// image as a data URL.
    pub fn wire_content(&self) -> serde_json::Value {
        if self.images.is_empty() {
            return serde_json::Value::String(self.content.clone().unwrap_or_default());
        }
        let mut parts = Vec::with_capacity(self.images.len() + 1);
        if let Some(text) = self.content.as_deref().filter(|text| !text.is_empty()) {
            parts.push(serde_json::json!({ "type": "text", "text": text }));
        }
        for image in &self.images {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": image.data_url() },
            }));
        }
        serde_json::Value::Array(parts)
    }

    /// The text content with a note in place of each image, for a model or
    /// summarizer that cannot (or need not) see the images themselves.
    pub fn text_with_image_notes(&self) -> Option<String> {
        if self.images.is_empty() {
            return self.content.clone();
        }
        let note = match self.images.len() {
            1 => "[1 image attached; not shown here]".to_owned(),
            n => format!("[{n} images attached; not shown here]"),
        };
        Some(match self.content.as_deref().filter(|t| !t.is_empty()) {
            Some(text) => format!("{text}\n{note}"),
            None => note,
        })
    }

    /// Serialized size of the representation that belongs in a future model
    /// prompt. Reasoning is transcript-only and providers deliberately omit it.
    /// Each image is charged [`IMAGE_PROMPT_BYTES`] rather than its payload.
    /// Measured with a counting writer: this runs over the whole history on
    /// every turn, so it must not allocate a copy of each message.
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

        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0 += buf.len();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut count = Count(0);
        serde_json::to_writer(
            &mut count,
            &PromptMessage {
                role: self.role,
                content: self.content.as_deref(),
                tool_calls: &self.tool_calls,
                tool_call_id: self.tool_call_id.as_deref(),
            },
        )?;
        Ok(count.0 + self.images.len() * IMAGE_PROMPT_BYTES)
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
