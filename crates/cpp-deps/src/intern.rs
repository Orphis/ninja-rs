//! Interned strings: identifiers and token spellings, shared by every file
//! and thread. They live as long as the process (a daemon's set of headers
//! is bounded).

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sym(u32);

impl std::fmt::Debug for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl Sym {
    pub fn as_str(self) -> &'static str {
        interner().names.read().unwrap()[self.0 as usize]
    }
}

struct Interner {
    map: RwLock<HashMap<&'static str, Sym>>,
    names: RwLock<Vec<&'static str>>,
}

/// Symbols the preprocessor knows, in `KNOWN`'s order.
macro_rules! known {
    ($($name:ident = $s:literal),* $(,)?) => {
        const KNOWN: &[&str] = &[$($s),*];
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[repr(u32)]
        enum Index { $($name),* }
        $(pub const $name: Sym = Sym(Index::$name as u32);)*
    };
}

known! {
    DEFINED = "defined",
    HAS_INCLUDE = "__has_include",
    HAS_INCLUDE_NEXT = "__has_include_next",
    VA_ARGS = "__VA_ARGS__",
    VA_OPT = "__VA_OPT__",
    LINE = "__LINE__",
    FILE = "__FILE__",
    COUNTER = "__COUNTER__",
    INCLUDE_LEVEL = "__INCLUDE_LEVEL__",
    BASE_FILE = "__BASE_FILE__",
    FILE_NAME = "__FILE_NAME__",
    TRUE = "true",
    FALSE = "false",
    ONCE = "once",
    PUSH_MACRO = "push_macro",
    POP_MACRO = "pop_macro",
    PRAGMA = "pragma",
    PRAGMA_OPERATOR = "_Pragma",
    HAS_EMBED = "__has_embed",
    DATE = "__DATE__",
    TIME = "__TIME__",
    TIMESTAMP = "__TIMESTAMP__",
    ZERO = "0",
    ONE = "1",
    EMPTY = "",
}

fn interner() -> &'static Interner {
    static I: OnceLock<Interner> = OnceLock::new();
    I.get_or_init(|| {
        let mut map = HashMap::new();
        let mut names = Vec::new();
        for (i, s) in KNOWN.iter().enumerate() {
            map.insert(*s, Sym(i as u32));
            names.push(*s);
        }
        Interner { map: RwLock::new(map), names: RwLock::new(names) }
    })
}

pub fn intern(s: &str) -> Sym {
    let i = interner();
    if let Some(&sym) = i.map.read().unwrap().get(s) {
        return sym;
    }
    let mut map = i.map.write().unwrap();
    if let Some(&sym) = map.get(s) {
        return sym;
    }
    let mut names = i.names.write().unwrap();
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    let sym = Sym(names.len() as u32);
    names.push(leaked);
    map.insert(leaked, sym);
    sym
}

pub fn intern_bytes(b: &[u8]) -> Sym {
    match std::str::from_utf8(b) {
        Ok(s) => intern(s),
        Err(_) => intern(&String::from_utf8_lossy(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_symbols() {
        assert_eq!(intern("defined"), DEFINED);
        assert_eq!(intern("__VA_OPT__"), VA_OPT);
        assert_eq!(EMPTY.as_str(), "");
        let a = intern("some_identifier");
        assert_eq!(intern("some_identifier"), a);
        assert_eq!(a.as_str(), "some_identifier");
    }
}
