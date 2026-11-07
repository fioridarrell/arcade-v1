//! Collection membership from an inscription's JSON (inscriptions.py collection_of /
//! collection_details), with Python's string semantics.

use serde_json::Value;

fn int_not_bool(v: &Value) -> Option<i64> {
    if v.is_boolean() {
        return None;
    }
    v.as_i64().filter(|_| v.is_i64() || v.is_u64())
}

/// `^(.*\S)\s*#\s*(\d+)\s*$` on `name` (`.` does not cross newlines; greedy: the last `#N` wins).
fn edition_name(name: &str) -> Option<(String, i64)> {
    let chars: Vec<char> = name.chars().collect();
    // `$` may also match before a final "\n".
    let end = if chars.last() == Some(&'\n') { chars.len() - 1 } else { chars.len() };
    for hash in (0..end).rev() {
        if chars[hash] != '#' {
            continue;
        }
        // before '#': (.*\S)\s*  — no newline in .*\S, and \s* may be any whitespace
        let mut j = hash;
        while j > 0 && chars[j - 1].is_whitespace() {
            j -= 1;
        }
        if j == 0 || chars[..j].contains(&'\n') {
            continue;
        }
        // after '#': \s*(\d+)\s*$
        let mut i = hash + 1;
        while i < end && chars[i].is_whitespace() {
            i += 1;
        }
        let ds = i;
        while i < end && chars[i].to_digit(10).is_some() {
            i += 1;
        }
        if i == ds {
            continue;
        }
        let digits: String = chars[ds..i].iter().collect();
        while i < end && chars[i].is_whitespace() {
            i += 1;
        }
        if i != end {
            continue;
        }
        let prefix: String = chars[..j].iter().collect();
        return Some((prefix, digits.parse().ok()?));
    }
    None
}

/// (collection, edition, name) or None.
pub fn collection_of(json_text: &str) -> Option<(String, Option<i64>, String)> {
    if json_text.is_empty() {
        return None;
    }
    let data: Value = serde_json::from_str(json_text).ok()?;
    let obj = data.as_object()?;
    let name = obj.get("name").and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut edition = obj.get("edition").and_then(int_not_bool);
    let collection = match obj.get("collection") {
        Some(Value::Object(m)) => m.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut collection = collection.trim().to_string();
    if collection.is_empty() {
        let (prefix, n) = edition_name(&name)?;
        collection = prefix.trim().to_string();
        if edition.is_none() {
            edition = Some(n);
        }
    }
    if collection.is_empty() || collection.chars().count() > 200 {
        return None;
    }
    Some((collection, edition, name.chars().take(200).collect()))
}

/// The declared supply: first of collection.{supply,total,count}, then top-level, that is a
/// non-bool int in 1..=10,000,000; 0 if none.
pub fn declared_supply(json_text: &str) -> u64 {
    let Ok(data) = serde_json::from_str::<Value>(json_text) else { return 0 };
    let Some(top) = data.as_object() else { return 0 };
    let mut sources = Vec::new();
    if let Some(Value::Object(inner)) = top.get("collection") {
        sources.push(inner);
    }
    sources.push(top);
    for s in sources {
        for alias in ["supply", "total", "count"] {
            if let Some(n) = s.get(alias).and_then(int_not_bool) {
                if (1..=10_000_000).contains(&n) {
                    return n as u64;
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership() {
        assert_eq!(collection_of(r#"{"name":"Frog #12"}"#), Some(("Frog".into(), Some(12), "Frog #12".into())));
        assert_eq!(collection_of(r#"{"name":"A #1 #2"}"#), Some(("A #1".into(), Some(2), "A #1 #2".into())));
        assert_eq!(collection_of(r#"{"name":"x","collection":{"name":" Pepes "},"edition":3}"#), Some(("Pepes".into(), Some(3), "x".into())));
        assert_eq!(collection_of(r#"{"name":"no number"}"#), None);
        assert_eq!(collection_of(r#"{"name":"Frog #12","edition":true}"#).unwrap().1, Some(12), "bool edition ignored");
        assert_eq!(declared_supply(r#"{"collection":{"total":0,"count":50},"supply":7}"#), 50);
        assert_eq!(declared_supply(r#"{"supply":100.0,"total":"9","count":20000000}"#), 0);
    }
}
