//! The targets file named by `rbe_targets` (see docs/rbe-interface.md): the
//! targets that edges name in `rbe_target`, with their kind. Build event
//! streams report them; for now they label actions in verbose output.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    /// Free-form: `executable`, `static_library`, `utility`, ...
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TargetsFile {
    version: u32,
    #[serde(default)]
    targets: Vec<Target>,
}

#[derive(Debug, Clone, Default)]
pub struct Targets {
    by_name: BTreeMap<String, Target>,
}

impl Targets {
    pub fn parse(json: &str) -> Result<Self, String> {
        let f: TargetsFile = serde_json::from_str(json).map_err(|e| format!("invalid targets file: {e}"))?;
        if f.version != 1 {
            return Err(format!("unsupported targets file version {}", f.version));
        }
        Ok(Self { by_name: f.targets.into_iter().map(|t| (t.name.clone(), t)).collect() })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn get(&self, name: &str) -> Option<&Target> {
        self.by_name.get(name)
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

    #[test]
    fn parse() {
        let t = Targets::parse(
            r#"{"version": 1, "targets": [
              {"name": "app", "kind": "executable"},
              {"name": "util"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.get("app").unwrap().kind, "executable");
        assert_eq!(t.get("util").unwrap().kind, "");
        assert!(t.get("other").is_none());
    }

    #[test]
    fn bad_version() {
        assert!(Targets::parse(r#"{"version": 2, "targets": []}"#).is_err());
    }
}
