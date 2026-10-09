//! The targets file named by `rbe_targets` (see docs/rbe-interface.md): the
//! targets that edges name in `rbe_target`, with their kind and build event
//! label, and the build configurations edges belong to (`rbe_config`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    /// Free-form: `executable`, `static_library`, `utility`, ...
    #[serde(default)]
    pub kind: String,
    /// The label build events use, such as `//src/foo:foo`.
    #[serde(default)]
    pub label: String,
}

/// A build configuration (a CMake build type with its toolchain).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct Configuration {
    pub name: String,
    /// Short description, such as `x86_64-linux-clang-Release`.
    #[serde(default)]
    pub mnemonic: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub cpu: String,
    /// Settings shown with the configuration (build type, compiler, ...).
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
}

impl Configuration {
    /// A stable id: the digest of everything that describes it, so that
    /// configurations differing in any setting never share an id.
    pub fn id(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        rbe_client::digest_of(&json).hash
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TargetsFile {
    version: u32,
    #[serde(default)]
    targets: Vec<Target>,
    #[serde(default)]
    configurations: Vec<Configuration>,
}

#[derive(Debug, Clone, Default)]
pub struct Targets {
    by_name: BTreeMap<String, Target>,
    configurations: Vec<Configuration>,
}

impl Targets {
    pub fn parse(json: &str) -> Result<Self, String> {
        let f: TargetsFile = serde_json::from_str(json).map_err(|e| format!("invalid targets file: {e}"))?;
        if f.version != 1 {
            return Err(format!("unsupported targets file version {}", f.version));
        }
        Ok(Self { by_name: f.targets.into_iter().map(|t| (t.name.clone(), t)).collect(), configurations: f.configurations })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn get(&self, name: &str) -> Option<&Target> {
        self.by_name.get(name)
    }

    /// The label of target `name`: the file's, else `//:<name>`.
    pub fn label(&self, name: &str) -> String {
        match self.get(name) {
            Some(t) if !t.label.is_empty() => t.label.clone(),
            _ => format!("//:{name}"),
        }
    }

    /// The configuration an edge's `rbe_config` names; without one, the
    /// first (single-configuration builds name none).
    pub fn configuration(&self, name: Option<&str>) -> Option<&Configuration> {
        match name {
            Some(n) => self.configurations.iter().find(|c| c.name == n),
            None => self.configurations.first(),
        }
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"version": 1,
      "configurations": [
        {"name": "Debug", "mnemonic": "x86_64-linux-clang-Debug", "variables": {"CMAKE_BUILD_TYPE": "Debug"}},
        {"name": "Release", "mnemonic": "x86_64-linux-clang-Release", "variables": {"CMAKE_BUILD_TYPE": "Release"}}
      ],
      "targets": [
        {"name": "app", "kind": "executable", "label": "//src:app"},
        {"name": "util"}
      ]}"#;

    #[test]
    fn parse() {
        let t = Targets::parse(JSON).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.get("app").unwrap().kind, "executable");
        assert_eq!(t.label("app"), "//src:app");
        assert_eq!(t.label("util"), "//:util");
        assert_eq!(t.label("other"), "//:other");
        assert_eq!(t.get("util").unwrap().kind, "");
    }

    #[test]
    fn configurations() {
        let t = Targets::parse(JSON).unwrap();
        assert_eq!(t.configuration(None).unwrap().name, "Debug");
        assert_eq!(t.configuration(Some("Release")).unwrap().mnemonic, "x86_64-linux-clang-Release");
        assert!(t.configuration(Some("Other")).is_none());
        let (d, r) = (t.configuration(Some("Debug")).unwrap().id(), t.configuration(Some("Release")).unwrap().id());
        assert_eq!(d.len(), 64);
        assert_ne!(d, r);
        assert_eq!(d, t.configuration(None).unwrap().id());
        assert!(Targets::parse(r#"{"version": 1}"#).unwrap().configuration(None).is_none());
    }

    #[test]
    fn bad_version() {
        assert!(Targets::parse(r#"{"version": 2, "targets": []}"#).is_err());
    }
}
