//! The arguments of a base kit: their values resolved and checked, and
//! substituted for their placeholders.
//!
//! A kit declares its arguments under `args`, each with an optional
//! `default`, `required`, `enum` of accepted values, `pattern`, and `env`, the
//! variable that carries the value into the sandbox. The `sbx` CLI checks the
//! values it is given, substitutes them for `${{ kit.args.<name> }}`
//! everywhere in the descriptor but in the raw descriptor it forwards, and
//! exports them as the variables the kit names; the daemon does none of this,
//! so the library does it in its place. Build arguments (`buildArg`) are
//! already part of a published image and take no value.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// A placeholder of a kit argument, `${{ kit.args.<name> }}`, as the CLI
/// reads it.
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{\{\s*kit\.args\.([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}")
        .expect("the placeholder pattern is valid")
});

/// The arguments of a kit resolved with the values given.
#[derive(Debug, Default, PartialEq)]
pub(super) struct ResolvedArgs {
    /// Value of every argument the kit declares, but build arguments.
    pub(super) values: BTreeMap<String, String>,
    /// Declarations of those arguments, without the variable they set, as
    /// the `args` of the artifact.
    pub(super) declared: Map<String, Value>,
    /// Variables the arguments set, by name.
    pub(super) environment: BTreeMap<String, String>,
}

/// What the declaration of an argument says, as this crate reads it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Declaration {
    /// Value the argument takes when none is given.
    #[serde(default)]
    default: Option<String>,
    /// Whether a value must be given.
    #[serde(default)]
    required: bool,
    /// The values the argument accepts.
    #[serde(default, rename = "enum")]
    values: Option<Vec<String>>,
    /// A regular expression the whole value matches.
    #[serde(default)]
    pattern: Option<String>,
    /// Variable the value is set in.
    #[serde(default)]
    env: Option<String>,
    /// Build argument of the image: already part of a published image, it
    /// takes no value.
    #[serde(default)]
    build_arg: Option<Value>,
}

/// Resolves the arguments `descriptor` declares with the values `provided`:
/// the value given, else the default, else an empty value. Build arguments
/// are left out.
///
/// # Errors
///
/// Returns [`Error::InvalidKit`] for `kit` on a value given for an argument
/// the kit does not declare or for a build argument, a required argument
/// without a value, a value outside the declared values, a value its pattern
/// does not match as a whole, and a pattern that is not a regular
/// expression.
pub(super) fn resolve(
    kit: &str,
    descriptor: &Value,
    provided: &BTreeMap<String, String>,
) -> Result<ResolvedArgs> {
    let invalid = |message: String| Error::InvalidKit {
        kit: kit.to_owned(),
        message,
    };
    let declared: Map<String, Value> = descriptor
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut declarations = BTreeMap::new();
    for (name, declaration) in &declared {
        let read: Declaration = serde_json::from_value(declaration.clone()).map_err(|error| {
            invalid(format!(
                "the declaration of the argument {name} is not one this crate reads: {error}"
            ))
        })?;
        declarations.insert(name.as_str(), (read, declaration));
    }
    let takes_value = |name: &str| {
        declarations
            .get(name)
            .is_some_and(|(read, _)| read.build_arg.is_none())
    };
    if let Some(name) = provided.keys().find(|name| !takes_value(name)) {
        return Err(invalid(format!("the kit declares no argument {name}")));
    }
    let mut resolved = ResolvedArgs::default();
    for (name, (read, declaration)) in declarations {
        if read.build_arg.is_some() {
            continue;
        }
        let value = match (provided.get(name), read.default) {
            (Some(value), _) => value.clone(),
            (None, Some(default)) => default,
            (None, None) if read.required => {
                return Err(invalid(format!("the argument {name} is required")));
            }
            (None, None) => String::new(),
        };
        if let Some(values) = &read.values
            && !values.contains(&value)
        {
            return Err(invalid(format!(
                "{value:?} is not one of the values of the argument {name}"
            )));
        }
        if let Some(pattern) = &read.pattern {
            let whole = Regex::new(&format!("^(?:{pattern})$")).map_err(|error| {
                invalid(format!(
                    "the pattern of the argument {name} is invalid: {error}"
                ))
            })?;
            if !whole.is_match(&value) {
                return Err(invalid(format!(
                    "{value:?} does not match the pattern of the argument {name}"
                )));
            }
        }
        // The artifact declares the argument as the kit does, but for the
        // variable it sets, given apart.
        let mut kept = declaration.clone();
        if let Some(fields) = kept.as_object_mut() {
            fields.remove("env");
        }
        if let Some(variable) = read.env {
            resolved.environment.insert(variable, value.clone());
        }
        resolved.declared.insert(name.to_owned(), kept);
        resolved.values.insert(name.to_owned(), value);
    }
    Ok(resolved)
}

/// `value` with every `${{ kit.args.<name> }}` replaced by the value of the
/// argument.
///
/// # Errors
///
/// Returns [`Error::InvalidKit`] for `kit` on a placeholder of an argument
/// without a value: one the kit does not declare, or a build argument.
pub(super) fn substitute(
    kit: &str,
    value: &Value,
    values: &BTreeMap<String, String>,
) -> Result<Value> {
    let mut unknown = None;
    let substituted = map_strings(value, &mut |text: &str| {
        PLACEHOLDER
            .replace_all(text, |captures: &regex::Captures<'_>| {
                let name = &captures[1];
                values.get(name).cloned().unwrap_or_else(|| {
                    unknown = Some(name.to_owned());
                    captures[0].to_owned()
                })
            })
            .into_owned()
    });
    match unknown {
        Some(name) => Err(Error::InvalidKit {
            kit: kit.to_owned(),
            message: format!("the kit uses the argument {name}, which takes no value"),
        }),
        None => Ok(substituted),
    }
}

/// `value` with `change` applied to every string it holds, keys aside.
fn map_strings(value: &Value, change: &mut impl FnMut(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(change(text)),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_strings(v, change)).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, v)| (key.clone(), map_strings(v, change)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The arguments `pairs` as a map.
    fn given(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// A descriptor declaring an argument of each kind.
    fn descriptor() -> Value {
        json!({ "args": {
            "model": { "default": "", "env": "AGENT_MODEL", "pattern": "[A-Za-z0-9._-]*" },
            "effort": { "default": "low", "enum": ["low", "high"] },
            "token": { "required": true, "env": "AGENT_TOKEN" },
            "version": { "buildArg": "VERSION", "required": true }
        } })
    }

    /// The message of the refusal of `provided`.
    fn refusal(descriptor: &Value, provided: &[(&str, &str)]) -> String {
        let error = resolve("kit", descriptor, &given(provided)).unwrap_err();
        assert!(matches!(error, Error::InvalidKit { .. }), "{error}");
        error.to_string()
    }

    #[test]
    fn values_are_given_or_defaulted_and_exported() {
        let resolved = resolve("kit", &descriptor(), &given(&[("token", "t1")])).unwrap();
        assert_eq!(
            resolved.values,
            given(&[("effort", "low"), ("model", ""), ("token", "t1")])
        );
        assert_eq!(
            resolved.environment,
            given(&[("AGENT_MODEL", ""), ("AGENT_TOKEN", "t1")])
        );
        assert!(resolved.declared["model"].get("env").is_none());
        assert!(
            !resolved.declared.contains_key("version"),
            "build arguments"
        );

        let resolved =
            resolve("kit", &json!({ "args": { "free": {} } }), &BTreeMap::new()).unwrap();
        assert_eq!(resolved.values["free"], "");
        assert!(
            resolve("kit", &json!({}), &BTreeMap::new())
                .unwrap()
                .values
                .is_empty()
        );
    }

    #[test]
    fn values_the_kit_refuses_are_errors() {
        let kit = descriptor();
        assert!(refusal(&kit, &[]).contains("token is required"));
        assert!(
            refusal(&kit, &[("token", "t"), ("effort", "max")])
                .contains("values of the argument effort")
        );
        assert!(
            refusal(&kit, &[("token", "t"), ("model", "it's")])
                .contains("pattern of the argument model")
        );
        // The pattern holds for the whole value, not a part of it.
        let anchored = json!({ "args": { "n": { "pattern": "[0-9]+" } } });
        assert!(refusal(&anchored, &[("n", "12a")]).contains("does not match"));
        assert!(refusal(&kit, &[("token", "t"), ("version", "1")]).contains("no argument version"));
        assert!(refusal(&kit, &[("token", "t"), ("other", "x")]).contains("no argument other"));
        let broken = json!({ "args": { "n": { "pattern": "(" } } });
        assert!(refusal(&broken, &[("n", "1")]).contains("pattern of the argument n is invalid"));
        let unread = json!({ "args": { "n": { "required": "yes" } } });
        assert!(
            refusal(&unread, &[])
                .contains("declaration of the argument n is not one this crate reads")
        );
    }

    #[test]
    fn placeholders_take_the_values_of_their_arguments() {
        let values = given(&[("model", "small-1"), ("effort", "low")]);
        let value = json!({ "a": ["${{ kit.args.model }}/${{kit.args.effort}}", 3], "${{ kit.args.model }}": "${{ other }}" });
        assert_eq!(
            substitute("kit", &value, &values).unwrap(),
            json!({ "a": ["small-1/low", 3], "${{ kit.args.model }}": "${{ other }}" })
        );
        let error = substitute("kit", &json!("${{ kit.args.version }}"), &values).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("argument version, which takes no value"),
            "{error}"
        );
    }
}
