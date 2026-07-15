//! Groq streaming inference via curl — OpenAI-compatible SSE, no HTTP/TLS deps.
//!
//! Key resolution: $GROQ_API_KEY, then ~/.config/pv/groq_api_key.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};

pub const DEFAULT_MODEL: &str = "llama-3.1-8b-instant";
const URL: &str = "https://api.groq.com/openai/v1/chat/completions";

#[derive(Debug)]
pub enum GroqEvent {
    Token(String),
    Done,
    Error(String),
}

pub fn api_key() -> Option<String> {
    std::env::var("GROQ_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string(crate::procfs::xdg("XDG_CONFIG_HOME", ".config").join("pv/groq_api_key"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|k| !k.is_empty())
        })
}

pub fn have_curl() -> bool {
    Command::new("curl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn a streaming chat completion. Events arrive on the returned channel.
pub fn stream(model: &str, system: &str, user: &str, key: &str) -> Receiver<GroqEvent> {
    let (tx, rx) = channel();
    let (model, system, user, key) = (
        model.to_string(),
        system.to_string(),
        user.to_string(),
        key.to_string(),
    );
    std::thread::spawn(move || {
        let payload = build_payload(&model, &system, &user);
        let spawned = Command::new("curl")
            .args([
                "-sN",
                "-X", "POST", URL,
                "-H", &format!("Authorization: Bearer {key}"),
                "-H", "Content-Type: application/json",
                "--data-binary", "@-",
                "--max-time", "30",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(GroqEvent::Error(format!("curl spawn: {e}")));
                return;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes());
        }
        let mut got_token = false;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Some(data) = line.strip_prefix("data:") else {
                    // non-SSE body (HTTP error path): surface the message
                    if line.contains("\"error\"") {
                        if let Some(msg) = extract_field(&line, "message") {
                            let _ = tx.send(GroqEvent::Error(msg));
                            return;
                        }
                    }
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    let _ = tx.send(GroqEvent::Done);
                    let _ = child.wait();
                    return;
                }
                if let Some(tok) = extract_field(data, "content") {
                    if !tok.is_empty() {
                        got_token = true;
                        if tx.send(GroqEvent::Token(tok)).is_err() {
                            let _ = child.kill();
                            return;
                        }
                    }
                }
            }
        }
        let status = child.wait().ok();
        if got_token {
            let _ = tx.send(GroqEvent::Done);
        } else {
            let _ = tx.send(GroqEvent::Error(format!(
                "stream ended without tokens ({})",
                status.map(|s| s.to_string()).unwrap_or_else(|| "killed".into())
            )));
        }
    });
    rx
}

fn build_payload(model: &str, system: &str, user: &str) -> String {
    format!(
        "{{\"model\":\"{model}\",\"stream\":true,\"max_tokens\":220,\"temperature\":0.2,\
\"messages\":[{{\"role\":\"system\",\"content\":\"{}\"}},{{\"role\":\"user\",\"content\":\"{}\"}}]}}",
        escape_json(system),
        escape_json(user)
    )
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Extract the string value of `"field"` from a flat-ish JSON chunk.
/// Handles standard JSON string escapes. Returns None when absent.
pub fn extract_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let pos = json.find(&needle)?;
    let after = &json[pos + needle.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code).unwrap_or('?'));
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_delta_content() {
        let chunk = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Chrome holds"}}]}"#;
        assert_eq!(extract_field(chunk, "content"), Some("Chrome holds".into()));
    }

    #[test]
    fn extracts_escaped_content() {
        let chunk = r#"{"choices":[{"delta":{"content":"line one\nline \"two\"\u00b0C"}}]}"#;
        assert_eq!(extract_field(chunk, "content"), Some("line one\nline \"two\"°C".into()));
    }

    #[test]
    fn role_chunk_has_no_content() {
        let chunk = r#"{"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#;
        assert_eq!(extract_field(chunk, "content"), None);
    }

    #[test]
    fn extracts_error_message() {
        let body = r#"{"error":{"message":"Invalid API Key","type":"authentication_error"}}"#;
        assert_eq!(extract_field(body, "message"), Some("Invalid API Key".into()));
    }

    #[test]
    fn payload_escapes() {
        let p = build_payload("m", "sys \"quoted\"", "user\nline");
        assert!(p.contains("sys \\\"quoted\\\""));
        assert!(p.contains("user\\nline"));
    }
}
