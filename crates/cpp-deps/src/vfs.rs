//! Clang's virtual file system overlays (`-ivfsoverlay`, clang-cl's
//! `/vfsoverlay:`): virtual paths mapped to real files or directories, such
//! as the case-insensitive views of Windows SDK headers. Only the flow
//! (JSON-like) form of the YAML that Clang and generators write is read;
//! a block-style overlay is an error, so callers fall back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::scan::normalize;

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    Bool(bool),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
}

impl Value {
    fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    fn bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Str(s) => match s.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'#' => {
                    while self.pos < self.b.len() && self.b[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        let line = self.b[..self.pos.min(self.b.len())].iter().filter(|&&c| c == b'\n').count() + 1;
        Err(format!("line {line}: {what}"))
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip();
        if self.b.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn scalar(&mut self) -> Result<String, String> {
        self.skip();
        match self.b.get(self.pos) {
            Some(b'\'') => {
                let mut s = Vec::new();
                self.pos += 1;
                loop {
                    match self.b.get(self.pos) {
                        None => return self.err("unterminated string"),
                        Some(b'\'') if self.b.get(self.pos + 1) == Some(&b'\'') => {
                            s.push(b'\'');
                            self.pos += 2;
                        }
                        Some(b'\'') => {
                            self.pos += 1;
                            break;
                        }
                        Some(&c) => {
                            s.push(c);
                            self.pos += 1;
                        }
                    }
                }
                Ok(String::from_utf8_lossy(&s).into_owned())
            }
            Some(b'"') => {
                let mut s = Vec::new();
                self.pos += 1;
                loop {
                    match self.b.get(self.pos) {
                        None => return self.err("unterminated string"),
                        Some(b'\\') => {
                            let c = *self.b.get(self.pos + 1).unwrap_or(&b'\\');
                            s.push(match c {
                                b'n' => b'\n',
                                b't' => b'\t',
                                c => c,
                            });
                            self.pos += 2;
                        }
                        Some(b'"') => {
                            self.pos += 1;
                            break;
                        }
                        Some(&c) => {
                            s.push(c);
                            self.pos += 1;
                        }
                    }
                }
                Ok(String::from_utf8_lossy(&s).into_owned())
            }
            Some(_) => {
                let start = self.pos;
                while self.pos < self.b.len() && !matches!(self.b[self.pos], b',' | b':' | b']' | b'}' | b'\n' | b'#') {
                    self.pos += 1;
                }
                Ok(String::from_utf8_lossy(&self.b[start..self.pos]).trim().to_string())
            }
            None => self.err("missing value"),
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip();
        if self.eat(b'{') {
            let mut m = Vec::new();
            loop {
                if self.eat(b'}') {
                    return Ok(Value::Map(m));
                }
                let k = self.scalar()?;
                if !self.eat(b':') {
                    return self.err("missing ':'");
                }
                let v = self.value()?;
                m.push((k, v));
                if !self.eat(b',') {
                    if self.eat(b'}') {
                        return Ok(Value::Map(m));
                    }
                    return self.err("missing ',' or '}'");
                }
            }
        }
        if self.eat(b'[') {
            let mut l = Vec::new();
            loop {
                if self.eat(b']') {
                    return Ok(Value::List(l));
                }
                l.push(self.value()?);
                if !self.eat(b',') {
                    if self.eat(b']') {
                        return Ok(Value::List(l));
                    }
                    return self.err("missing ',' or ']'");
                }
            }
        }
        let s = self.scalar()?;
        Ok(match s.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Str(s),
        })
    }
}

fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser { b: text.as_bytes(), pos: 0 };
    p.skip();
    if p.b.get(p.pos) != Some(&b'{') {
        return Err("only flow-style (JSON-like) overlays are supported".into());
    }
    let v = p.value()?;
    p.skip();
    if p.pos < p.b.len() {
        return p.err("unexpected text after the overlay");
    }
    Ok(v)
}

/// How an overlay combines with the real file system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redirect {
    /// The overlay first, then the real file system (the default).
    Fallthrough,
    /// The real file system first, then the overlay.
    Fallback,
    /// The overlay only.
    RedirectOnly,
}

#[derive(Debug)]
pub struct Overlay {
    pub path: PathBuf,
    /// Virtual file → real file (keys from `key`).
    files: HashMap<String, PathBuf>,
    /// Virtual directory → real directory (`directory-remap`).
    dirs: Vec<(String, PathBuf)>,
    pub case_insensitive: bool,
    pub redirect: Redirect,
}

impl Overlay {
    fn key(&self, p: &Path) -> String {
        let s = normalize(p).to_string_lossy().replace('\\', "/");
        if self.case_insensitive { s.to_lowercase() } else { s }
    }

    /// Reads an overlay; relative paths are against `cwd`, or the
    /// overlay's directory as it says (`root-relative`, `overlay-relative`).
    pub fn load(path: &Path, cwd: &Path) -> Result<Overlay, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Overlay::parse(&text, &normalize(&cwd.join(path)), cwd)
    }

    pub fn parse(text: &str, path: &Path, cwd: &Path) -> Result<Overlay, String> {
        let at = |e: String| format!("{}: {e}", path.display());
        let v = parse(text).map_err(at)?;
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let root_base = if v.get("root-relative").and_then(Value::str) == Some("overlay-dir") { dir.clone() } else { cwd.to_path_buf() };
        let external_base = if v.get("overlay-relative").and_then(Value::bool) == Some(true) { dir.clone() } else { cwd.to_path_buf() };
        let redirect = match (v.get("redirecting-with").and_then(Value::str), v.get("fallthrough").and_then(Value::bool)) {
            (Some("fallback"), _) => Redirect::Fallback,
            (Some("redirect-only"), _) | (None, Some(false)) => Redirect::RedirectOnly,
            _ => Redirect::Fallthrough,
        };
        let mut o = Overlay {
            path: path.to_path_buf(),
            files: HashMap::new(),
            dirs: Vec::new(),
            case_insensitive: v.get("case-sensitive").and_then(Value::bool) == Some(false),
            redirect,
        };
        let Some(Value::List(roots)) = v.get("roots") else { return Err(at("no roots".into())) };
        for r in roots {
            o.add(r, &root_base, &external_base).map_err(at)?;
        }
        Ok(o)
    }

    fn add(&mut self, e: &Value, parent: &Path, external_base: &Path) -> Result<(), String> {
        let name = e.get("name").and_then(Value::str).ok_or("an entry without a name")?;
        let virtual_path = normalize(&parent.join(name));
        let external = || e.get("external-contents").and_then(Value::str).map(|x| normalize(&external_base.join(x)));
        match e.get("type").and_then(Value::str) {
            Some("file") => {
                let real = external().ok_or("a file without external-contents")?;
                let k = self.key(&virtual_path);
                self.files.insert(k, real);
            }
            Some("directory-remap") => {
                let real = external().ok_or("a directory-remap without external-contents")?;
                let k = self.key(&virtual_path);
                self.dirs.push((k, real));
            }
            Some("directory") => {
                if let Some(Value::List(contents)) = e.get("contents") {
                    for c in contents {
                        self.add(c, &virtual_path, external_base)?;
                    }
                }
            }
            other => return Err(format!("unknown entry type {other:?}")),
        }
        Ok(())
    }

    /// The real path of a virtual one, if the overlay maps it.
    pub fn map(&self, p: &Path) -> Option<PathBuf> {
        let k = self.key(p);
        if let Some(real) = self.files.get(&k) {
            return Some(real.clone());
        }
        for (d, real) in &self.dirs {
            if let Some(rest) = k.strip_prefix(d.as_str()).and_then(|r| r.strip_prefix('/')) {
                // The rest keeps the virtual spelling, which the real file
                // system may not match on a case-insensitive overlay.
                let original = normalize(p).to_string_lossy().replace('\\', "/");
                let rest_original = original.get(original.len().saturating_sub(rest.len())..).unwrap_or(rest);
                return Some(real.join(rest_original));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_yaml() {
        let v = parse("# comment\n{ 'a': 'it''s', \"b\": [1, true, { c: d }, ], e: false, }\n").unwrap();
        assert_eq!(v.get("a"), Some(&Value::Str("it's".into())));
        assert_eq!(v.get("b"), Some(&Value::List(vec![Value::Str("1".into()), Value::Bool(true), Value::Map(vec![("c".into(), Value::Str("d".into()))])])));
        assert_eq!(v.get("e"), Some(&Value::Bool(false)));
        assert!(parse("version: 0\nroots: []\n").is_err());
        assert!(parse("{ 'a': 'b'").is_err());
    }

    #[test]
    fn overlays() {
        let text = "# generated\n{\n  'version': 0,\n  'case-sensitive': 'false',\n  'root-relative': 'overlay-dir',\n  'overlay-relative': true,\n  'roots': [\n    { 'name': '../sdk/include', 'type': 'directory', 'contents': [\n      { 'name': 'Windows.h', 'type': 'file', 'external-contents': '../sdk/real/windows.h' },\n      { 'name': 'sub', 'type': 'directory', 'contents': [ { 'name': 'a.h', 'type': 'file', 'external-contents': '../sdk/real/sub/a.h' } ] }\n    ] },\n    { 'name': '/remap', 'type': 'directory-remap', 'external-contents': '/elsewhere' }\n  ]\n}\n";
        let o = Overlay::parse(text, Path::new("/t/overlays/o.yaml"), Path::new("/cwd")).unwrap();
        assert!(o.case_insensitive);
        assert_eq!(o.redirect, Redirect::Fallthrough);
        assert_eq!(o.map(Path::new("/t/sdk/include/windows.H")), Some(PathBuf::from("/t/sdk/real/windows.h")));
        assert_eq!(o.map(Path::new("/t/sdk/include/SUB/a.h")), Some(PathBuf::from("/t/sdk/real/sub/a.h")));
        assert_eq!(o.map(Path::new("/t/sdk/include/other.h")), None);
        assert_eq!(o.map(Path::new("/remap/x/Y.h")), Some(PathBuf::from("/elsewhere/x/Y.h")));
        let only = Overlay::parse("{ 'version': 0, 'fallthrough': false, 'roots': [] }", Path::new("/o.yaml"), Path::new("/")).unwrap();
        assert_eq!(only.redirect, Redirect::RedirectOnly);
    }
}
