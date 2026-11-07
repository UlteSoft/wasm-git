//! Presentation/automation transport; deliberately separate from repository and
//! network encodings. A machine response is one versioned record, even on error.
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Native,
    Text,
    Json,
    BinaryV1,
}

impl Format {
    pub fn default_output() -> Self {
        if std::io::stdout().is_terminal() {
            Self::Text
        } else {
            Self::Native
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "native" => Ok(Self::Native),
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "binary-v1" => Ok(Self::BinaryV1),
            _ => Err(
                "global --format expects text, json, binary-v1 or native (before the command)"
                    .into(),
            ),
        }
    }

    pub fn machine(self) -> bool {
        matches!(self, Self::Json | Self::BinaryV1)
    }

    pub fn emit(self, command: &str, output: &str, code: i32, error: Option<&str>, newline: bool) {
        if self == Self::Native {
            if let Some(error) = error {
                crate::cli_error(error);
            } else if !output.is_empty() {
                crate::cli_stdout(output, newline);
            }
            return;
        }
        let data = serde_json::from_str::<Value>(output)
            .ok()
            .filter(|v| v.is_object() || v.is_array());
        if self == Self::Text {
            if let Some(error) = error {
                crate::cli_error(error);
            } else if let Some(data) = data {
                let mut lines = Vec::new();
                human(&data, "", 0, &mut lines);
                if lines.len() > 120 {
                    lines.truncate(120);
                    lines.push("… summary limited to 120 lines; use --format json before the command for the complete report.".into());
                }
                crate::cli_stdout(&lines.join("\n"), true);
            } else {
                crate::cli_stdout(&crate::report::safe_text(output), newline);
            }
            return;
        }
        let result = if let Some(value) = data {
            json!({"kind":"data", "value":value})
        } else if output.is_empty() {
            json!({"kind":"empty"})
        } else {
            json!({"kind":"text", "text":output})
        };
        let record = json!({"protocol":"wasm-git.command", "version":1,
            "command":command, "ok":code == 0, "exit_code":code, "result":result,
            "error":error.map(|message| json!({"code":"command_failed", "message":message}))});
        let bytes = if self == Self::Json {
            let mut bytes = serde_json::to_vec(&record).expect("serializable command response");
            bytes.push(b'\n');
            bytes
        } else {
            binary(&record)
        };
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                crate::cli_error(&format!("write command response: {error}"));
                std::process::exit(1);
            }
        }
    }
}

fn human(value: &Value, label: &str, depth: usize, lines: &mut Vec<String>) {
    if lines.len() > 120 {
        return;
    }
    let prefix = "  ".repeat(depth.min(4));
    match value {
        Value::Object(fields) => {
            if !label.is_empty() {
                lines.push(format!("{prefix}{label}"));
            }
            if depth > 3 {
                lines.push(format!(
                    "{prefix}  {} fields (machine report contains full detail)",
                    fields.len()
                ));
                return;
            }
            for (key, item) in fields {
                human(
                    item,
                    &key.replace('_', " "),
                    depth + usize::from(!label.is_empty()),
                    lines,
                );
            }
        }
        Value::Array(items) => {
            lines.push(format!("{prefix}{label}: {} entries", items.len()));
            if depth > 3 {
                return;
            }
            for (index, item) in items.iter().take(20).enumerate() {
                human(item, &format!("{}", index + 1), depth + 1, lines);
            }
            if items.len() > 20 {
                lines.push(format!("{prefix}  … {} more entries", items.len() - 20));
            }
        }
        other => {
            let text = match other {
                Value::String(s) => s.clone(),
                _ => other.to_string(),
            };
            let text = crate::report::safe_text(&text);
            let short: String = text.chars().take(240).collect();
            lines.push(format!(
                "{prefix}{label}: {short}{}",
                if text.chars().count() > 240 {
                    "…"
                } else {
                    ""
                }
            ));
        }
    }
}

// WGTCLI\0\x01 + u64 BE payload length + tagged value. This is a real binary
// value tree, not UTF-8 JSON in a binary wrapper. The version freezes the tags.
fn binary(value: &Value) -> Vec<u8> {
    fn size(out: &mut Vec<u8>, n: usize) {
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    fn string(out: &mut Vec<u8>, s: &str) {
        size(out, s.len());
        out.extend_from_slice(s.as_bytes());
    }
    fn encode(out: &mut Vec<u8>, value: &Value) {
        match value {
            Value::Null => out.push(0),
            Value::Bool(false) => out.push(1),
            Value::Bool(true) => out.push(2),
            Value::Number(n) => {
                if let Some(n) = n.as_u64() {
                    out.push(4);
                    out.extend_from_slice(&n.to_be_bytes());
                } else if let Some(n) = n.as_i64() {
                    out.push(3);
                    out.extend_from_slice(&n.to_be_bytes());
                } else {
                    out.push(5);
                    out.extend_from_slice(&n.as_f64().unwrap().to_be_bytes());
                }
            }
            Value::String(s) => {
                out.push(6);
                string(out, s);
            }
            Value::Array(items) => {
                out.push(7);
                size(out, items.len());
                for item in items {
                    encode(out, item);
                }
            }
            Value::Object(fields) => {
                out.push(8);
                size(out, fields.len());
                for (key, value) in fields {
                    string(out, key);
                    encode(out, value);
                }
            }
        }
    }
    let mut out = b"WGTCLI\0\x01".to_vec();
    out.extend_from_slice(&[0; 8]);
    encode(&mut out, value);
    let length = (out.len() - 16) as u64;
    out[8..16].copy_from_slice(&length.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_v1_golden_bytes_are_host_independent() {
        let value = json!([
            null,
            false,
            true,
            -0x0102030405060708_i64,
            0x8192a3b4c5d6e7f8_u64,
            -1.125_f64,
            "中文"
        ]);
        let mut payload = vec![7, 0, 0, 0, 0, 0, 0, 0, 7, 0, 1, 2];
        payload.extend([3, 0xfe, 0xfd, 0xfc, 0xfb, 0xfa, 0xf9, 0xf8, 0xf8]);
        payload.extend([4, 0x81, 0x92, 0xa3, 0xb4, 0xc5, 0xd6, 0xe7, 0xf8]);
        payload.extend([5, 0xbf, 0xf2, 0, 0, 0, 0, 0, 0]);
        payload.extend([
            6, 0, 0, 0, 0, 0, 0, 0, 6, 0xe4, 0xb8, 0xad, 0xe6, 0x96, 0x87,
        ]);
        let frame = binary(&value);
        assert_eq!(&frame[..8], b"WGTCLI\0\x01");
        assert_eq!(&frame[8..16], &[0, 0, 0, 0, 0, 0, 0, 54]);
        assert_eq!(&frame[16..], payload);
    }

    #[test]
    fn binary_maps_have_ordered_keys_and_unsigned_lengths() {
        assert_eq!(
            &binary(&json!({"b":true,"a":null}))[16..],
            &[
                8, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, b'a', 0, 0, 0, 0, 0, 0, 0, 0, 1,
                b'b', 2
            ]
        );
    }
}
