//! What is inside a private message (messaging/content.py): plain UTF-8 text, or a structured
//! body b"\x01ARCB" ver 1 | json_len u16 | JSON {"text"?, "file"?: {name, type, size},
//! "profile"?: {...}} | file bytes. Unknown versions or bad JSON read as plain text.
//! V1.0-only fields ride in the same JSON (old arcades ignore keys they don't know).

use serde_json::{json, Value};

const MARK: &[u8] = b"\x01ARCB";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Body {
    pub text: String,
    pub file: Option<File>,
    /// The rest of the JSON (profile, V1.0 extras).
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Body {
    pub fn text(t: &str) -> Self {
        Body { text: t.into(), ..Default::default() }
    }

    pub fn parse(b: &[u8]) -> Body {
        let plain = || Body::text(&String::from_utf8_lossy(b));
        if !b.starts_with(MARK) || b.len() < 8 || b[5] != 1 {
            return plain();
        }
        let n = u16::from_be_bytes([b[6], b[7]]) as usize;
        let Some(j) = b.get(8..8 + n).and_then(|j| serde_json::from_slice::<Value>(j).ok()) else { return plain() };
        let Some(mut o) = j.as_object().cloned() else { return plain() };
        let text = o.remove("text").and_then(|t| t.as_str().map(String::from)).unwrap_or_default();
        let file = o.remove("file").and_then(|f| {
            let size = f["size"].as_u64()? as usize;
            let bytes = b.get(8 + n..8 + n + size)?.to_vec();
            Some(File { name: f["name"].as_str().unwrap_or("file").into(), mime: f["type"].as_str().unwrap_or("application/octet-stream").into(), bytes })
        });
        Body { text, file, extra: o }
    }

    /// Plain text when there is nothing but text (what the browser sends), else structured.
    pub fn encode(&self) -> Vec<u8> {
        if self.file.is_none() && self.extra.is_empty() {
            return self.text.as_bytes().to_vec();
        }
        let mut o = self.extra.clone();
        if !self.text.is_empty() {
            o.insert("text".into(), json!(self.text));
        }
        if let Some(f) = &self.file {
            o.insert("file".into(), json!({"name": f.name, "type": f.mime, "size": f.bytes.len()}));
        }
        let j = serde_json::to_vec(&Value::Object(o)).unwrap();
        let mut b = MARK.to_vec();
        b.push(1);
        b.extend_from_slice(&(j.len() as u16).to_be_bytes());
        b.extend_from_slice(&j);
        if let Some(f) = &self.file {
            b.extend_from_slice(&f.bytes);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_structured() {
        assert_eq!(Body::parse(b"hi"), Body::text("hi"));
        let mut b = Body::text("look");
        b.file = Some(File { name: "a.png".into(), mime: "image/png".into(), bytes: vec![1, 2, 3] });
        b.extra.insert("reply".into(), json!("x"));
        let e = b.encode();
        assert!(e.starts_with(MARK));
        assert_eq!(Body::parse(&e), b);
        // Damaged JSON reads as plain text.
        assert_eq!(Body::parse(b"\x01ARCB\x01\x00\x05{bad}").text.len(), 13);
    }
}
