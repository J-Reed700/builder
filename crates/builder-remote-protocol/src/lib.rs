//! Bounded wire messages shared by the Builder host and remote gateway.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_REQUEST_BODY: usize = 128 * 1024;
pub const MAX_RESPONSE_BODY: usize = 8 * 1024 * 1024;
// A JSON response is carried as a JSON string, which can double quotes and
// backslashes on the wire while the decoded response remains capped at 8 MiB.
pub const MAX_WIRE_MESSAGE: usize = MAX_RESPONSE_BODY * 2 + 16 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Hello {
        protocol: u16,
        device_id: String,
        device_name: String,
        credential: Credential,
    },
    Response {
        request_id: String,
        status: u16,
        body: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Credential {
    Enroll { code: String },
    Device { token: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayMessage {
    Ready {
        protocol: u16,
        device_id: String,
        device_token: Option<String>,
    },
    Request {
        request_id: String,
        method: String,
        path: String,
        body: String,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_kind_is_explicit_on_the_wire() {
        let value = serde_json::to_value(HostMessage::Hello {
            protocol: PROTOCOL_VERSION,
            device_id: "device".into(),
            device_name: "laptop".into(),
            credential: Credential::Enroll {
                code: "secret".into(),
            },
        })
        .unwrap();
        assert_eq!(value["type"], "hello");
        assert_eq!(value["credential"]["kind"], "enroll");
    }
}
