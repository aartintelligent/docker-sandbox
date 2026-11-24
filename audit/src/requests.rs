//! The requests a capture holds, scenario by scenario.
//!
//! What a replayed suite sent to the daemon is the first thing to read when a
//! CLI feature is measured: [`print`] lists each request of a capture under
//! its scenario, with its body, the opaque `kit_artifacts` of creation
//! requests abridged. Reads (`GET`) are left out unless asked for.

use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Prints the requests of `capture` (`<suite>.jsonl`), those of the scenarios
/// in `only` when it is not empty, reads included with `reads`.
///
/// # Errors
///
/// Fails when the capture cannot be read.
pub(crate) fn print(capture: &Path, only: &[String], reads: bool) -> Result<()> {
    let file = std::fs::File::open(capture).with_context(|| capture.display().to_string())?;
    let mut scenario = String::new();
    for line in std::io::BufReader::new(file).lines() {
        let Ok(entry) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let Some(text) = describe(&entry, reads) else {
            continue;
        };
        let tag = entry["tag"].as_str().unwrap_or_default();
        if !only.is_empty() && !only.iter().any(|name| name == tag) {
            continue;
        }
        if tag != scenario {
            println!("== {tag}");
            tag.clone_into(&mut scenario);
        }
        println!("{text}");
    }
    Ok(())
}

/// `METHOD path body` for a request entry of a capture, or `None` for any
/// other entry, and for reads unless `reads`.
fn describe(entry: &Value, reads: bool) -> Option<String> {
    if entry["kind"] != "request" {
        return None;
    }
    let method = entry["method"].as_str()?;
    if method == "GET" && !reads {
        return None;
    }
    let path = entry["path"].as_str().unwrap_or_default();
    let body = match &entry["body"] {
        Value::Null => String::new(),
        Value::Object(fields) => {
            let mut fields = fields.clone();
            if let Some(artifacts) = fields.get("kit_artifacts").and_then(Value::as_array) {
                let abridged = format!("<{} kit artifacts>", artifacts.len());
                fields.insert("kit_artifacts".to_owned(), Value::String(abridged));
            }
            format!(" {}", Value::Object(fields))
        }
        other => format!(" {other}"),
    };
    Some(format!("{method} {path}{body}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_are_described_with_their_artifacts_abridged() {
        let create = json!({ "kind": "request", "tag": "create-a", "method": "POST", "path": "/sandbox",
                             "body": { "name": "audit-a", "kit_artifacts": [{}, {}] } });
        assert_eq!(
            describe(&create, false).as_deref(),
            Some(r#"POST /sandbox {"kit_artifacts":"<2 kit artifacts>","name":"audit-a"}"#)
        );
        let read = json!({ "kind": "request", "method": "GET", "path": "/sandbox", "body": null });
        assert_eq!(describe(&read, false), None);
        assert_eq!(describe(&read, true).as_deref(), Some("GET /sandbox"));
        assert_eq!(describe(&json!({ "kind": "response" }), true), None);
    }
}
