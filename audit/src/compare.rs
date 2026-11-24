//! `compare`: what changed in the daemon API between two snapshots.
//!
//! The report lists, in Markdown:
//!
//! - operations of the contract added and removed;
//! - for every operation present in both: response statuses added and removed,
//!   and request and response fields added and removed (as dotted paths, `[]`
//!   for array items);
//! - routes of the router inventory added and removed, or whose methods changed;
//! - event kinds (`type`/`action`) added and removed.
//!
//! This is the starting point of an upgrade: every line is something the
//! crates may have to handle.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::snapshot::{Manifest, Snapshot};

/// Maximum depth followed in a schema.
const MAX_DEPTH: usize = 12;

/// Compares the snapshots `old` and `new` and returns the Markdown report.
///
/// # Errors
///
/// Fails if a contract cannot be read.
pub(crate) fn run(old: &Path, new: &Path) -> Result<String> {
    let (old, new) = (Snapshot::new(old), Snapshot::new(new));
    let mut md = String::new();
    let version = |s: &Snapshot| {
        read(&s.manifest())
            .ok()
            .and_then(|v| serde_json::from_value::<Manifest>(v).ok())
            .map_or_else(
                || s.dir.display().to_string(),
                |m| format!("{} (API {})", m.sbx_version, m.api_version),
            )
    };
    writeln!(
        md,
        "# Daemon API changes: {} → {}\n",
        version(&old),
        version(&new)
    )?;

    let (old_api, new_api) = (
        read(&old.api().join("openapi.json"))?,
        read(&new.api().join("openapi.json"))?,
    );
    let (old_ops, new_ops) = (operations(&old_api), operations(&new_api));
    section(
        &mut md,
        "Operations",
        &added(&old_ops, &new_ops),
        &removed(&old_ops, &new_ops),
    )?;

    let mut changed = String::new();
    for (key, old_op) in &old_ops {
        let Some(new_op) = new_ops.get(key) else {
            continue;
        };
        let mut lines = Vec::new();
        let (old_statuses, new_statuses) = (statuses(old_op), statuses(new_op));
        for status in new_statuses.difference(&old_statuses) {
            lines.push(format!("+ response {status}"));
        }
        for status in old_statuses.difference(&new_statuses) {
            lines.push(format!("- response {status}"));
        }
        let parts = [
            ("request", request_schema(old_op), request_schema(new_op)),
            ("response", success_schema(old_op), success_schema(new_op)),
        ];
        for (part, old_schema, new_schema) in parts {
            let old_fields = fields(old_schema, &old_api);
            let new_fields = fields(new_schema, &new_api);
            for field in new_fields.difference(&old_fields) {
                lines.push(format!("+ {part} `{field}`"));
            }
            for field in old_fields.difference(&new_fields) {
                lines.push(format!("- {part} `{field}`"));
            }
        }
        if !lines.is_empty() {
            writeln!(changed, "### `{key}`\n")?;
            for line in lines {
                writeln!(changed, "    {line}")?;
            }
            changed.push('\n');
        }
    }
    md.push_str("## Changed operations\n\n");
    md.push_str(if changed.is_empty() {
        "None.\n\n"
    } else {
        &changed
    });

    let (old_routes, new_routes) = (routes(&old.routes()), routes(&new.routes()));
    let mut route_changes: Vec<String> = Vec::new();
    for (shape, methods) in &new_routes {
        match old_routes.get(shape) {
            None => route_changes.push(format!("+ `{shape}` {methods}")),
            Some(before) if before != methods => {
                route_changes.push(format!("~ `{shape}` {before} → {methods}"));
            }
            Some(_) => {}
        }
    }
    for (shape, methods) in &old_routes {
        if !new_routes.contains_key(shape) {
            route_changes.push(format!("- `{shape}` {methods}"));
        }
    }
    list(&mut md, "Router inventory", &route_changes)?;

    let (old_events, new_events) = (event_kinds(&old.events()), event_kinds(&new.events()));
    section(
        &mut md,
        "Event kinds",
        &new_events
            .difference(&old_events)
            .cloned()
            .collect::<Vec<_>>(),
        &old_events
            .difference(&new_events)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    Ok(md)
}

/// Writes a section listing added and removed items.
fn section(md: &mut String, title: &str, added: &[String], removed: &[String]) -> Result<()> {
    let lines: Vec<String> = added
        .iter()
        .map(|a| format!("+ `{a}`"))
        .chain(removed.iter().map(|r| format!("- `{r}`")))
        .collect();
    list(md, title, &lines)
}

/// Writes a section made of `lines`, or "None.".
fn list(md: &mut String, title: &str, lines: &[String]) -> Result<()> {
    writeln!(md, "## {title}\n")?;
    if lines.is_empty() {
        md.push_str("None.\n\n");
    } else {
        for line in lines {
            writeln!(md, "    {line}")?;
        }
        md.push('\n');
    }
    Ok(())
}

/// Keys of `new` missing from `old`.
fn added<V>(old: &BTreeMap<String, V>, new: &BTreeMap<String, V>) -> Vec<String> {
    new.keys()
        .filter(|k| !old.contains_key(*k))
        .cloned()
        .collect()
}

/// Keys of `old` missing from `new`.
fn removed<V>(old: &BTreeMap<String, V>, new: &BTreeMap<String, V>) -> Vec<String> {
    added(new, old)
}

/// Reads a JSON file.
fn read(path: &Path) -> Result<Value> {
    serde_json::from_slice(&std::fs::read(path).with_context(|| path.display().to_string())?)
        .with_context(|| path.display().to_string())
}

/// Operations of an OpenAPI document, keyed by `METHOD /path`.
fn operations(api: &Value) -> BTreeMap<String, Value> {
    let mut ops = BTreeMap::new();
    for (path, item) in api["paths"].as_object().into_iter().flatten() {
        for (method, op) in item.as_object().into_iter().flatten() {
            if op.is_object() && op.get("responses").is_some() {
                ops.insert(format!("{} {path}", method.to_uppercase()), op.clone());
            }
        }
    }
    ops
}

/// Response statuses of an operation.
fn statuses(op: &Value) -> BTreeSet<String> {
    op["responses"]
        .as_object()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default()
}

/// Schemas of the request body, every content type.
fn request_schema(op: &Value) -> Vec<&Value> {
    content_schemas(&op["requestBody"])
}

/// Schemas of the successful (2xx) responses, every content type.
fn success_schema(op: &Value) -> Vec<&Value> {
    op["responses"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(status, _)| status.starts_with('2'))
        .flat_map(|(_, response)| content_schemas(response))
        .collect()
}

/// The schemas under `content` of a request body or response.
fn content_schemas(holder: &Value) -> Vec<&Value> {
    holder["content"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(_, media)| &media["schema"])
        .collect()
}

/// Every field path of `schemas`, `$ref` resolved against `api`.
fn fields(schemas: Vec<&Value>, api: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for schema in schemas {
        walk(schema, api, "", 0, &mut out);
    }
    out
}

/// Collects the field paths of `schema` under `prefix`.
fn walk(schema: &Value, api: &Value, prefix: &str, depth: usize, out: &mut BTreeSet<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(reference) = schema["$ref"].as_str() {
        let target = reference
            .strip_prefix("#/")
            .map_or(&Value::Null, |pointer| {
                api.pointer(&format!("/{pointer}")).unwrap_or(&Value::Null)
            });
        walk(target, api, prefix, depth + 1, out);
        return;
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for alternative in schema[key].as_array().into_iter().flatten() {
            walk(alternative, api, prefix, depth + 1, out);
        }
    }
    for (name, property) in schema["properties"].as_object().into_iter().flatten() {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        out.insert(path.clone());
        walk(property, api, &path, depth + 1, out);
    }
    for key in ["items", "x-ndjson-item"] {
        if schema.get(key).is_some() {
            walk(&schema[key], api, &format!("{prefix}[]"), depth + 1, out);
        }
    }
    if schema["additionalProperties"].is_object() {
        walk(
            &schema["additionalProperties"],
            api,
            &format!("{prefix}.*"),
            depth + 1,
            out,
        );
    }
}

/// Router inventory: route shape (parameters as `:*`) → sorted methods.
fn routes(path: &Path) -> BTreeMap<String, String> {
    let Ok(inventory) = read(path) else {
        return BTreeMap::new();
    };
    let mut shapes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (route, allow) in inventory["routes"].as_object().into_iter().flatten() {
        let shape: Vec<&str> = route
            .split('/')
            .map(|s| if s.starts_with(':') { ":*" } else { s })
            .collect();
        let methods = shapes.entry(shape.join("/")).or_default();
        for method in allow.as_str().unwrap_or_default().split(',') {
            let method = method.trim();
            if !method.is_empty() && method != "OPTIONS" {
                methods.insert(method.to_owned());
            }
        }
    }
    shapes
        .into_iter()
        .map(|(shape, methods)| (shape, methods.into_iter().collect::<Vec<_>>().join(", ")))
        .collect()
}

/// Event kinds of a recorded stream, as `type/action`.
fn event_kinds(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|event| {
            format!(
                "{}/{}",
                event["type"].as_str().unwrap_or("?"),
                event["action"].as_str().unwrap_or("")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_follow_references_arrays_and_alternatives() {
        let api = json!({"components": {"schemas": {"Port": {"properties": {"host_port": {}}}}}});
        let schema = json!({
            "properties": {
                "ports": {"items": {"$ref": "#/components/schemas/Port"}},
                "value": {"anyOf": [{"type": "string"}, {"properties": {"raw": {}}}]}
            }
        });
        let found = fields(vec![&schema], &api);
        assert_eq!(
            found,
            BTreeSet::from(["ports", "ports[].host_port", "value", "value.raw"].map(str::to_owned))
        );
    }

    #[test]
    fn operations_are_keyed_by_method_and_path() {
        let api =
            json!({"paths": {"/sandbox": {"get": {"responses": {"200": {}}}, "parameters": []}}});
        assert_eq!(
            operations(&api).keys().collect::<Vec<_>>(),
            ["GET /sandbox"]
        );
    }
}
