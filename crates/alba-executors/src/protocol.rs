//! Plugin wire protocol.
//!
//! Alba can spawn external executor plugins: separate binaries that Alba spawns
//! and talks to over stdin/stdout, one JSON object per line.
//!
//! The host speaks [`HostMessage`] and the plugin answers with [`PluginMessage`].
//! Stderr is free-form and relayed into the beam's output.

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Open {
        protocol: u32,
        beam: String,
        dir: String,
        options: serde_json::Value,
    },
    Execute {
        command: String,
        env: Vec<(String, String)>,
        cwd: String,
    },
    Cancel,
    Close,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    Ready,
    Output { stream: WireStream, text: String },
    Exit { code: i32 },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireStream {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_encoding_matches_the_published_specification() {
        let open = HostMessage::Open {
            protocol: PROTOCOL_VERSION,
            beam: "deploy".into(),
            dir: "/abs/path".into(),
            options: serde_json::json!({"image": "x"}),
        };
        assert_eq!(
            serde_json::to_string(&open).unwrap(),
            r#"{"type":"open","protocol":1,"beam":"deploy","dir":"/abs/path","options":{"image":"x"}}"#,
        );
        let execute = HostMessage::Execute {
            command: "echo hi".into(),
            env: vec![("K".into(), "V".into())],
            cwd: "/abs/path".into(),
        };
        assert_eq!(
            serde_json::to_string(&execute).unwrap(),
            r#"{"type":"execute","command":"echo hi","env":[["K","V"]],"cwd":"/abs/path"}"#,
        );
        assert_eq!(
            serde_json::to_string(&HostMessage::Cancel).unwrap(),
            r#"{"type":"cancel"}"#
        );
        assert_eq!(
            serde_json::to_string(&HostMessage::Close).unwrap(),
            r#"{"type":"close"}"#
        );
    }

    #[test]
    fn plugin_messages_decode_from_their_wire_form() {
        assert_eq!(
            serde_json::from_str::<PluginMessage>(r#"{"type":"ready"}"#).unwrap(),
            PluginMessage::Ready,
        );
        assert_eq!(
            serde_json::from_str::<PluginMessage>(
                r#"{"type":"output","stream":"stderr","text":"warm"}"#,
            )
            .unwrap(),
            PluginMessage::Output {
                stream: WireStream::Stderr,
                text: "warm".into()
            },
        );
        assert_eq!(
            serde_json::from_str::<PluginMessage>(r#"{"type":"exit","code":3}"#).unwrap(),
            PluginMessage::Exit { code: 3 },
        );
    }

    #[test]
    fn an_unknown_message_type_fails_to_decode() {
        assert!(serde_json::from_str::<PluginMessage>(r#"{"type":"surprise"}"#).is_err());
    }
}
