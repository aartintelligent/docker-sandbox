//! The mixins of the program a spec composes: read from their directory, or
//! given whole.
//!
//! A mixin of a directory is written `{"dir"}` and read when the spec is
//! checked or launched, so that a spec stays plain data; a mixin given whole
//! is written in its own JSON form, `{"reference", "descriptor"}`.

use std::path::PathBuf;

use docker_sandbox_kit::Mixin;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::SandboxSpec;
use crate::error::Result;

/// A mixin of a spec: read from its directory at launch, or given whole.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MixinSpec {
    /// The directory of its `spec.yaml`.
    Dir(PathBuf),
    /// Its reference and its descriptor.
    Inline(Box<Mixin>),
}

/// The JSON form of a mixin read from its directory.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirForm {
    /// The directory.
    dir: PathBuf,
}

impl SandboxSpec {
    /// Composes `mixin`.
    #[must_use]
    pub fn with_mixin(mut self, mixin: Mixin) -> Self {
        self.mixins.push(MixinSpec::Inline(Box::new(mixin)));
        self
    }

    /// Composes the mixin of the directory `dir`, read when the spec is
    /// checked or launched.
    #[must_use]
    pub fn with_mixin_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.mixins.push(MixinSpec::Dir(dir.into()));
        self
    }
}

impl MixinSpec {
    /// The mixin, read from its directory or given whole.
    pub(crate) fn read(&self) -> Result<Mixin> {
        match self {
            Self::Dir(dir) => Ok(Mixin::read(dir)?),
            Self::Inline(mixin) => Ok(mixin.as_ref().clone()),
        }
    }
}

impl Serialize for MixinSpec {
    /// Writes `{"dir"}`, or the JSON form of the mixin.
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Dir(dir) => DirForm { dir: dir.clone() }.serialize(serializer),
            Self::Inline(mixin) => mixin.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for MixinSpec {
    /// Reads `{"dir"}`, or the JSON form of a mixin.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).and_then(|value| {
            if value.get("dir").is_some() {
                serde_json::from_value::<DirForm>(value).map(|form| Self::Dir(form.dir))
            } else {
                serde_json::from_value::<Mixin>(value).map(|mixin| Self::Inline(Box::new(mixin)))
            }
            .map_err(serde::de::Error::custom)
        })
    }
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::KitDescriptor;

    use super::*;

    #[test]
    fn mixins_are_read_from_their_directory_or_given_whole() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = KitDescriptor::mixin("Local");
        std::fs::write(dir.path().join("spec.yaml"), descriptor.to_spec()).unwrap();
        let read = MixinSpec::Dir(dir.path().to_owned()).read().unwrap();
        assert_eq!(read.descriptor(), &descriptor);
        let given = Mixin::new("/kits/given", descriptor).unwrap();
        assert_eq!(
            MixinSpec::Inline(Box::new(given.clone())).read().unwrap(),
            given
        );
        let missing = MixinSpec::Dir("/nonexistent/docker-sandbox-blueprint/mixin".into());
        let error = missing.read().unwrap_err().to_string();
        assert!(
            error.contains("/nonexistent/docker-sandbox-blueprint/mixin"),
            "{error}"
        );
    }
}
