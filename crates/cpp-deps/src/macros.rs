//! Macro definitions and expansion.
//!
//! Expansion follows the standard algorithm with hide sets (Prosser's): a
//! token remembers the macros it came from and is never expanded by them
//! again. Function-like arguments are fully expanded before substitution,
//! except next to `#` (stringized) and `##` (pasted), and `##` pastes after
//! substitution, with placemarkers for empty arguments. GNU's `, ##
//! __VA_ARGS__` drops the comma when there are no variadic arguments; C++20's
//! `__VA_OPT__(...)` keeps its contents only when there are.
//!
//! Expansion only runs on `#if`/`#elif` expressions and `#include`
//! operands. In `#if` mode, `defined`, `__has_include` and the compiler's
//! queries (`__has_builtin` and the like) are evaluated as they appear,
//! before their operands could be expanded (a query naming a macro the
//! source defined is unknown: compilers may expand it).

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use crate::intern::{self, Sym};
use crate::lex::{self, Kind, P, Token};

#[derive(Debug, PartialEq, Eq)]
pub struct Macro {
    pub name: Sym,
    /// None for an object-like macro.
    pub params: Option<Box<[Sym]>>,
    /// The last parameter takes the variadic arguments (`...` is named
    /// `__VA_ARGS__`, GNU's `name...` keeps its name).
    pub variadic: bool,
    pub body: Box<[Token]>,
    /// What the body does through pragmas where it's expanded: in text,
    /// which the scan doesn't expand.
    pub pragmas: Pragmas,
    /// The operands of the body's `_Pragma`s that macros make
    /// (`_Pragma(STR(clang diagnostic ignored x))`): what they do depends
    /// on the macros defined where it is (see `Macro::pragmas_in`).
    pub pragma_operands: Box<[Box<[Token]>]>,
}

/// What tokens do through pragmas that change macros or inclusion
/// (`once`, `push_macro`, `pop_macro`, `include_alias`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Pragmas {
    #[default]
    None,
    /// Pushes or pops these macros (libc++'s `_LIBCPP_PUSH_MACROS`): no
    /// effect unless they're defined or undefined meanwhile.
    Macros(Box<[Sym]>),
    /// Something else, or an operand the scan can't read.
    Other,
}

impl Pragmas {
    /// What both do.
    fn and(self, other: Pragmas) -> Pragmas {
        match (self, other) {
            (Pragmas::Other, _) | (_, Pragmas::Other) => Pragmas::Other,
            (Pragmas::None, p) | (p, Pragmas::None) => p,
            (Pragmas::Macros(a), Pragmas::Macros(b)) => Pragmas::Macros(a.iter().chain(b.iter()).copied().collect()),
        }
    }
}

/// The string literal's contents (`"a\"b"` is `a"b`).
pub(crate) fn destringize(lit: &str) -> String {
    let body = lit.trim_start_matches(|c| c != '"').trim_matches('"');
    body.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// The pragmas that change macros or inclusion.
const PRAGMAS: [Sym; 4] = [intern::ONCE, intern::PUSH_MACRO, intern::POP_MACRO, intern::INCLUDE_ALIAS];

/// What one pragma's tokens (after `#pragma`) do to macros or inclusion:
/// the macros pushed or popped are added to `names`, anything else of
/// those sets `other`. Other pragmas (`clang`, `GCC`, `message`,
/// `warning`, `diagnostic`, `region`...) change neither.
fn pragma_words(p: &[Token], names: &mut Vec<Sym>, other: &mut bool) {
    match p {
        [w, l, s, r, ..] if (w.sym == intern::PUSH_MACRO || w.sym == intern::POP_MACRO) && l.is(P::LParen) && s.kind == Kind::Str && r.is(P::RParen) => {
            names.push(intern::intern(&destringize(s.sym.as_str())));
        }
        [w, ..] if PRAGMAS.contains(&w.sym) => *other = true,
        _ => {}
    }
}

/// The index of the `)` closing the `(` at `open`, or the end.
fn closing(toks: &[Token], open: usize) -> usize {
    let mut depth = 0;
    (open..toks.len())
        .find(|&j| {
            depth += toks[j].is(P::LParen) as i32 - toks[j].is(P::RParen) as i32;
            depth == 0
        })
        .unwrap_or(toks.len())
}

/// What tokens do through pragmas: `__pragma(...)`, the words of a pragma
/// given to a macro (`DO_PRAGMA(once)`, `PRAGMA("once")`), `_Pragma("...")`
/// (which text runs where it's written: only `literal` ones count). A
/// `_Pragma` operand of one of `params` is the macro's argument's, seen
/// where the macro is used; other operands that aren't a string literal
/// are returned, to be expanded with the macros defined (see
/// `operand_pragmas`).
pub(crate) fn pragmas(toks: &[Token], literal: bool, params: &[Sym]) -> (Pragmas, Vec<Box<[Token]>>) {
    let (mut names, mut other, mut operands) = (Vec::new(), false, Vec::new());
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        let call = toks.get(i + 1).is_some_and(|p| p.is(P::LParen));
        if call && t.sym == intern::PRAGMA_OPERATOR {
            let end = closing(toks, i + 1);
            // Closed by what follows where it's used.
            other |= end == toks.len();
            match &toks[i + 2..end] {
                [s] if s.kind == Kind::Str => {
                    if literal {
                        pragma_words(&lex::tokens(destringize(s.sym.as_str()).as_bytes()), &mut names, &mut other);
                    }
                }
                [] => {}
                [p] if p.ident().is_some_and(|p| params.contains(&p)) => {}
                operand => operands.push(operand.into()),
            }
            i = end + 1;
            continue;
        }
        if call && t.sym == intern::MS_PRAGMA_OPERATOR {
            let end = closing(toks, i + 1);
            pragma_words(&toks[i + 2..end], &mut names, &mut other);
            i = end + 1;
            continue;
        }
        // A macro's only argument: `DO_PRAGMA(once)`, `PRAGMA("once")`; not
        // a parameter's name (GLib's `g_once(once, ...)` uses `(once)`).
        let alone = i > 0 && toks[i - 1].is(P::LParen) && toks.get(i + 1).is_some_and(|r| r.is(P::RParen));
        other |= match t.kind {
            Kind::Ident if params.contains(&t.sym) => false,
            Kind::Ident => [intern::PUSH_MACRO, intern::POP_MACRO, intern::INCLUDE_ALIAS].contains(&t.sym) || (t.sym == intern::ONCE && alone),
            Kind::Str if alone => {
                let text = destringize(t.sym.as_str());
                let word = text.trim_start().split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or_default();
                matches!(word, "once" | "push_macro" | "pop_macro" | "include_alias")
            }
            _ => false,
        };
        i += 1;
    }
    let pragmas = match (other, names.is_empty()) {
        (true, _) => Pragmas::Other,
        (false, false) => Pragmas::Macros(names.into_boxed_slice()),
        (false, true) => Pragmas::None,
    };
    (pragmas, operands)
}

/// What a `_Pragma` operand that macros make does (`_Pragma(STR(x))`),
/// expanded with the macros `ctx` has, `params` (the macro's, whose
/// arguments are seen where it's used) left as they are: a pragma starting
/// with a parameter is the argument's; an operand that doesn't expand to a
/// string literal is an error where it's used (no pragma runs), unless the
/// arguments or what the scan doesn't expand (`__LINE__`) are part of it.
/// Returns it with the macros the expansion looked up, whose changes
/// change it.
pub fn operand_pragmas(operand: &[Token], own: Option<Sym>, params: &[Sym], ctx: &mut dyn Context) -> (Pragmas, Vec<Sym>) {
    let param = |t: &Token| t.ident().filter(|p| params.contains(p));
    let mut input = Vec::new();
    let mut i = 0;
    while i < operand.len() {
        let t = operand[i];
        if t.is(P::Hash)
            && let Some(p) = operand.get(i + 1).and_then(param)
        {
            // `#param`: the argument's spelling.
            input.push(XTok::new(Token { kind: Kind::Str, sym: intern::intern(&format!("\"{}\"", p.as_str())), space: t.space }));
            i += 2;
            continue;
        }
        // Pasted where the macro is used.
        if t.is(P::HashHash) || t.sym == intern::VA_OPT {
            return (Pragmas::Other, Vec::new());
        }
        // A parameter is never expanded here: its hide set has it, as the
        // macro's own name has, which its expansion doesn't expand.
        input.push(XTok { t, hide: Some(Rc::new(param(&t).into_iter().chain(own).collect())) });
        i += 1;
    }
    let mut recording = Recording { ctx, looked_up: Default::default(), unknown: false };
    let mut x = Expander::new(&mut recording, false);
    let out = x.expand(input);
    let failed = x.error.is_some() || recording.unknown;
    let mut looked_up = recording.looked_up.into_inner();
    looked_up.sort();
    looked_up.dedup();
    let pragmas = match &out[..] {
        _ if failed => Pragmas::Other,
        [s] if s.t.kind == Kind::Str => {
            let words = lex::tokens(destringize(s.t.sym.as_str()).as_bytes());
            match words.first() {
                Some(w) if param(w).is_some() => Pragmas::None,
                _ => {
                    let (mut names, mut other) = (Vec::new(), false);
                    pragma_words(&words, &mut names, &mut other);
                    if other {
                        Pragmas::Other
                    } else if names.is_empty() {
                        Pragmas::None
                    } else {
                        Pragmas::Macros(names.into_boxed_slice())
                    }
                }
            }
        }
        toks if toks.iter().any(|t| param(&t.t).is_some()) => Pragmas::Other,
        _ => Pragmas::None,
    };
    (pragmas, looked_up)
}

impl Macro {
    /// What the body does through pragmas where it's expanded, with the
    /// macros `ctx` has (see `operand_pragmas`), and the macros that
    /// depends on.
    pub fn pragmas_in(&self, ctx: &mut dyn Context) -> (Pragmas, Vec<Sym>) {
        let params = self.params.as_deref().unwrap_or(&[]);
        let (mut pragmas, mut deps) = (self.pragmas.clone(), Vec::new());
        for operand in &self.pragma_operands {
            let (p, d) = operand_pragmas(operand, Some(self.name), params, ctx);
            pragmas = pragmas.and(p);
            deps.extend(d);
        }
        (pragmas, deps)
    }
}

/// A context that notes the macros looked up, and expands no dynamic ones
/// (`__LINE__`; their values where a macro is used are unknown).
struct Recording<'c> {
    ctx: &'c mut dyn Context,
    looked_up: std::cell::RefCell<Vec<Sym>>,
    /// A dynamic macro or an operator came up.
    unknown: bool,
}

impl Context for Recording<'_> {
    fn lookup(&self, name: Sym) -> Option<Arc<Macro>> {
        self.looked_up.borrow_mut().push(name);
        self.ctx.lookup(name)
    }
    fn is_defined(&self, name: Sym) -> bool {
        self.ctx.is_defined(name)
    }
    fn dynamic(&mut self, name: Sym) -> Option<Token> {
        self.unknown |= self.ctx.is_defined(name);
        None
    }
    fn has_include(&mut self, _name: &str, _angled: bool, _next: bool) -> bool {
        false
    }
    fn has_embed(&mut self, _name: &str, _angled: bool, _limit_zero: bool) -> i64 {
        0
    }
    fn is_query(&self, _op: Sym) -> bool {
        false
    }
    fn query(&mut self, _op: Sym, _arg: &str) -> Option<i64> {
        None
    }
}

/// Parses the tokens after `#define`.
pub fn parse_define(toks: &[Token]) -> Option<Macro> {
    let name = toks.first()?.ident()?;
    let rest = &toks[1..];
    if let Some(open) = rest.first()
        && open.is(P::LParen)
        && !open.space
    {
        let mut params = Vec::new();
        let mut variadic = false;
        let mut i = 1;
        loop {
            let t = rest.get(i)?;
            if t.is(P::RParen) && params.is_empty() && !variadic {
                i += 1;
                break;
            }
            if t.is(P::Ellipsis) {
                params.push(intern::VA_ARGS);
                variadic = true;
                i += 1;
            } else {
                params.push(t.ident()?);
                i += 1;
                if rest.get(i).is_some_and(|t| t.is(P::Ellipsis)) {
                    variadic = true;
                    i += 1;
                }
            }
            let sep = rest.get(i)?;
            i += 1;
            if sep.is(P::RParen) {
                break;
            }
            if !sep.is(P::Comma) || variadic {
                return None;
            }
        }
        let mut body: Vec<Token> = rest[i..].to_vec();
        if let Some(first) = body.first_mut() {
            first.space = false;
        }
        let (pragmas, operands) = pragmas(&body, true, &params);
        Some(Macro {
            name,
            params: Some(params.into_boxed_slice()),
            variadic,
            body: body.into_boxed_slice(),
            pragmas,
            pragma_operands: operands.into_boxed_slice(),
        })
    } else {
        let mut body: Vec<Token> = rest.to_vec();
        if let Some(first) = body.first_mut() {
            first.space = false;
        }
        let (pragmas, operands) = pragmas(&body, true, &[]);
        Some(Macro { name, params: None, variadic: false, body: body.into_boxed_slice(), pragmas, pragma_operands: operands.into_boxed_slice() })
    }
}

/// The macros an expansion came from.
type Hide = Option<Rc<Vec<Sym>>>;

fn hidden(h: &Hide, s: Sym) -> bool {
    h.as_ref().is_some_and(|v| v.contains(&s))
}

fn hide_union(h: &Hide, other: &Hide) -> Hide {
    match (h, other) {
        (None, o) | (o, None) => o.clone(),
        (Some(a), Some(b)) => {
            let mut v = a.as_ref().clone();
            for s in b.iter() {
                if !v.contains(s) {
                    v.push(*s);
                }
            }
            Some(Rc::new(v))
        }
    }
}

fn hide_intersection(a: &Hide, b: &Hide) -> Hide {
    match (a, b) {
        (Some(a), Some(b)) => {
            let v: Vec<Sym> = a.iter().filter(|s| b.contains(s)).copied().collect();
            (!v.is_empty()).then(|| Rc::new(v))
        }
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct XTok {
    pub t: Token,
    hide: Hide,
}

impl XTok {
    pub fn new(t: Token) -> Self {
        XTok { t, hide: None }
    }
}

/// What expansion needs from the preprocessor's state.
pub trait Context {
    fn lookup(&self, name: Sym) -> Option<Arc<Macro>>;
    /// `defined(name)`: a macro, a dynamic macro (`__LINE__`) or an
    /// operator the compiler has (`__has_include`, `__has_builtin`, ...).
    fn is_defined(&self, name: Sym) -> bool;
    /// `__LINE__`, `__FILE__`, `__COUNTER__` and the like.
    fn dynamic(&mut self, name: Sym) -> Option<Token>;
    /// `__has_include` / `__has_include_next`.
    fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool;
    /// `__has_embed`: 0 (not found), 1 (found) or 2 (empty, or `limit(0)`).
    fn has_embed(&mut self, name: &str, angled: bool, limit_zero: bool) -> i64;
    /// Whether `op` is a compiler query (`__has_builtin` and the like) the
    /// compiler has.
    fn is_query(&self, op: Sym) -> bool;
    /// A query's value; None when unknown.
    fn query(&mut self, op: Sym, arg: &str) -> Option<i64>;
    /// Whether `name` is defined otherwise than the compiler predefines
    /// it (which its answers to queries assume).
    fn redefined(&self, _name: Sym) -> bool {
        false
    }
}

/// A number token (0 and 1, the usual results, without interning).
pub(crate) fn number(v: i64) -> Token {
    let sym = match v {
        0 => intern::ZERO,
        1 => intern::ONE,
        v => intern::intern(&v.to_string()),
    };
    Token { kind: Kind::Number, sym, space: true }
}

#[cfg(test)]
fn spelling(toks: &[XTok]) -> String {
    let mut s = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0 && t.t.space {
            s.push(' ');
        }
        s.push_str(t.t.sym.as_str());
    }
    s
}

/// `#arg`: the argument's spelling as a string literal.
fn stringize(arg: &[XTok]) -> Token {
    let mut s = String::from("\"");
    for (i, t) in arg.iter().enumerate() {
        if i > 0 && t.t.space {
            s.push(' ');
        }
        let text = t.t.sym.as_str();
        if matches!(t.t.kind, Kind::Str | Kind::Char) {
            for c in text.chars() {
                if c == '"' || c == '\\' {
                    s.push('\\');
                }
                s.push(c);
            }
        } else {
            s.push_str(text);
        }
    }
    s.push('"');
    Token { kind: Kind::Str, sym: intern::intern(&s), space: false }
}

/// A step of substitution, before `##` pasting.
enum Item {
    Tok(XTok),
    /// A `##` of the macro's body.
    Paste,
    /// An empty argument next to `##` (`va`: the variadic one).
    Placemarker {
        va: bool,
    },
}

pub struct Expander<'c> {
    pub ctx: &'c mut dyn Context,
    /// `#if` mode: `defined` and the operators are evaluated.
    pub if_mode: bool,
    /// Set on malformed input (the expansion is then not trustworthy).
    pub error: Option<String>,
    budget: usize,
}

impl<'c> Expander<'c> {
    pub fn new(ctx: &'c mut dyn Context, if_mode: bool) -> Self {
        Expander { ctx, if_mode, error: None, budget: 1_000_000 }
    }

    fn fail(&mut self, msg: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(msg.into());
        }
    }

    pub fn expand_tokens(&mut self, toks: &[Token]) -> Vec<Token> {
        let x = self.expand(toks.iter().copied().map(XTok::new).collect());
        x.into_iter().map(|x| x.t).collect()
    }

    /// The operand of `defined`, `__has_include` or a query: `( ... )`,
    /// raw. For `defined`, also the bare `defined X`.
    fn operand(&mut self, input: &mut VecDeque<XTok>, bare: bool) -> Option<Vec<XTok>> {
        match input.front() {
            Some(t) if t.t.is(P::LParen) => {
                input.pop_front();
                let mut depth = 1;
                let mut out = Vec::new();
                while let Some(t) = input.pop_front() {
                    if t.t.is(P::LParen) {
                        depth += 1;
                    } else if t.t.is(P::RParen) {
                        depth -= 1;
                        if depth == 0 {
                            return Some(out);
                        }
                    }
                    out.push(t);
                }
                None
            }
            Some(t) if bare && t.t.kind == Kind::Ident => Some(vec![input.pop_front().unwrap()]),
            _ => None,
        }
    }

    /// The header name of `__has_include(...)`: `"x"`, `<x>`, or a macro
    /// expanding to one of those.
    fn header_name(&mut self, op: Vec<XTok>, expand: bool) -> Option<(String, bool)> {
        match op.first().map(|t| t.t.kind) {
            Some(Kind::Str) if op.len() == 1 => {
                let s = op[0].t.sym.as_str();
                Some((s.strip_prefix('"')?.strip_suffix('"')?.to_string(), false))
            }
            Some(Kind::HeaderName) if op.len() == 1 => Some((op[0].t.sym.as_str().to_string(), true)),
            Some(Kind::Punct(P::Lt)) => {
                let end = op.iter().position(|t| t.t.is(P::Gt))?;
                let mut name = String::new();
                for (i, t) in op[1..end].iter().enumerate() {
                    if i > 0 && t.t.space {
                        name.push(' ');
                    }
                    name.push_str(t.t.sym.as_str());
                }
                Some((name, true))
            }
            _ if expand => {
                let x = self.expand(op);
                self.header_name(x, false)
            }
            _ => None,
        }
    }

    /// Evaluates `defined`, `__has_include` and queries in `#if` mode.
    /// None: `name` is none of those.
    fn if_operator(&mut self, name: Sym, input: &mut VecDeque<XTok>) -> Option<Token> {
        if name == intern::DEFINED {
            let op = self.operand(input, true);
            return Some(match op.as_deref() {
                Some([t]) if t.t.kind == Kind::Ident => number(self.ctx.is_defined(t.t.sym) as i64),
                _ => {
                    self.fail("malformed defined()");
                    unknown()
                }
            });
        }
        if (name == intern::HAS_INCLUDE || name == intern::HAS_INCLUDE_NEXT) && self.ctx.is_defined(name) {
            let Some(op) = self.operand(input, false) else {
                self.fail("malformed __has_include()");
                return Some(unknown());
            };
            return Some(match self.header_name(op, true) {
                Some((h, angled)) => number(self.ctx.has_include(&h, angled, name == intern::HAS_INCLUDE_NEXT) as i64),
                None => {
                    self.fail("malformed __has_include()");
                    unknown()
                }
            });
        }
        if name == intern::HAS_EMBED && self.ctx.is_defined(name) {
            let Some(op) = self.operand(input, false) else {
                self.fail("malformed __has_embed()");
                return Some(unknown());
            };
            // The resource, then its parameters.
            let end = match op.first().map(|t| t.t.kind) {
                Some(Kind::Punct(P::Lt)) => op.iter().position(|t| t.t.is(P::Gt)).map_or(op.len(), |i| i + 1),
                _ => 1.min(op.len()),
            };
            let params: Vec<&str> = op[end..].iter().map(|t| t.t.sym.as_str()).collect();
            let limit_zero = params.windows(4).any(|w| w == ["limit", "(", "0", ")"]);
            return Some(match self.header_name(op[..end].to_vec(), true) {
                Some((h, angled)) => number(self.ctx.has_embed(&h, angled, limit_zero)),
                None => {
                    self.fail("malformed __has_embed()");
                    unknown()
                }
            });
        }
        if self.ctx.is_query(name) {
            let Some(op) = self.operand(input, false) else {
                self.fail(format!("malformed {}()", name.as_str()));
                return Some(unknown());
            };
            // GCC expands the operands of every query, Clang those of some:
            // a macro defined here would change what the compiler is asked.
            if op.iter().any(|t| t.t.kind == Kind::Ident && self.ctx.redefined(t.t.sym)) {
                self.fail(format!("{}() of a macro", name.as_str()));
                return Some(unknown());
            }
            let arg: String = op.iter().map(|t| t.t.sym.as_str()).collect();
            return Some(match self.ctx.query(name, &arg) {
                Some(v) => number(v),
                None => unknown(),
            });
        }
        None
    }

    pub fn expand(&mut self, input: Vec<XTok>) -> Vec<XTok> {
        let mut input: VecDeque<XTok> = input.into();
        let mut out = Vec::new();
        while let Some(t) = input.pop_front() {
            if self.budget == 0 {
                self.fail("expansion too large");
                break;
            }
            self.budget -= 1;
            if t.t.kind != Kind::Ident {
                out.push(t);
                continue;
            }
            let name = t.t.sym;
            if self.if_mode
                && let Some(mut v) = self.if_operator(name, &mut input)
            {
                v.space = t.t.space;
                out.push(XTok::new(v));
                continue;
            }
            if hidden(&t.hide, name) {
                out.push(t);
                continue;
            }
            let Some(m) = self.ctx.lookup(name) else {
                match self.ctx.dynamic(name) {
                    Some(mut d) => {
                        d.space = t.t.space;
                        out.push(XTok::new(d));
                    }
                    None => out.push(t),
                }
                continue;
            };
            let replacement = match &m.params {
                None => {
                    let hs = hide_union(&t.hide, &Some(Rc::new(vec![name])));
                    self.substitute(&m, &[], hs, t.t.space)
                }
                Some(params) => {
                    if !input.front().is_some_and(|n| n.t.is(P::LParen)) {
                        out.push(t);
                        continue;
                    }
                    let Some((args, rparen)) = self.arguments(&mut input, params.len(), m.variadic) else {
                        self.fail(format!("bad arguments for {}", name.as_str()));
                        out.push(t);
                        continue;
                    };
                    let hs = hide_union(&hide_intersection(&t.hide, &rparen), &Some(Rc::new(vec![name])));
                    self.substitute(&m, &args, hs, t.t.space)
                }
            };
            for r in replacement.into_iter().rev() {
                input.push_front(r);
            }
        }
        out
    }

    /// `( args )` of a function-like macro, and the hide set of `)`.
    fn arguments(&mut self, input: &mut VecDeque<XTok>, nparams: usize, variadic: bool) -> Option<(Vec<Vec<XTok>>, Hide)> {
        input.pop_front();
        let mut args: Vec<Vec<XTok>> = Vec::new();
        let mut cur = Vec::new();
        let mut depth = 1;
        let rparen;
        loop {
            let t = input.pop_front()?;
            if t.t.is(P::LParen) {
                depth += 1;
            } else if t.t.is(P::RParen) {
                depth -= 1;
                if depth == 0 {
                    rparen = t.hide.clone();
                    args.push(cur);
                    break;
                }
            } else if t.t.is(P::Comma) && depth == 1 && !(variadic && args.len() + 1 == nparams) {
                args.push(std::mem::take(&mut cur));
                continue;
            }
            cur.push(t);
        }
        if nparams == 0 && args.len() == 1 && args[0].is_empty() {
            args.clear();
        }
        if variadic && args.len() + 1 == nparams {
            args.push(Vec::new());
        }
        (args.len() == nparams).then_some((args, rparen))
    }

    fn substitute(&mut self, m: &Macro, args: &[Vec<XTok>], hs: Hide, space: bool) -> Vec<XTok> {
        let params: &[Sym] = m.params.as_deref().unwrap_or(&[]);
        let mut items = Vec::new();
        self.substitute_into(m, params, args, &m.body, &mut items);
        // `##` pasting.
        let mut pasted: Vec<Item> = Vec::new();
        let mut items = items.into_iter().peekable();
        while let Some(item) = items.next() {
            if !matches!(item, Item::Paste) {
                pasted.push(item);
                continue;
            }
            let Some(right) = items.next() else { break };
            let left = pasted.pop();
            match (left, right) {
                (None, r) => pasted.push(r),
                (Some(Item::Placemarker { .. }), r) => pasted.push(r),
                // GNU: `, ## __VA_ARGS__` without variadic arguments.
                (Some(Item::Tok(l)), Item::Placemarker { va: true }) if l.t.is(P::Comma) => {}
                (Some(l), Item::Placemarker { .. }) => pasted.push(l),
                (Some(Item::Tok(l)), Item::Tok(r)) => {
                    let text = format!("{}{}", l.t.sym.as_str(), r.t.sym.as_str());
                    let toks = lex::tokens(text.as_bytes());
                    if let [mut t] = toks[..] {
                        t.space = l.t.space;
                        pasted.push(Item::Tok(XTok::new(t)));
                    } else {
                        pasted.push(Item::Tok(l));
                        pasted.push(Item::Tok(r));
                    }
                }
                (Some(l), r) => {
                    pasted.push(l);
                    pasted.push(r);
                }
            }
        }
        let mut out: Vec<XTok> = pasted
            .into_iter()
            .filter_map(|i| match i {
                Item::Tok(mut t) => {
                    t.hide = hide_union(&t.hide, &hs);
                    Some(t)
                }
                _ => None,
            })
            .collect();
        if let Some(first) = out.first_mut() {
            first.t.space = space;
        }
        out
    }

    fn substitute_into(&mut self, m: &Macro, params: &[Sym], args: &[Vec<XTok>], body: &[Token], items: &mut Vec<Item>) {
        let param = |t: &Token| if m.params.is_some() { t.ident().and_then(|s| params.iter().position(|p| *p == s)) } else { None };
        let mut i = 0;
        while i < body.len() {
            let tok = body[i];
            if tok.is(P::Hash)
                && m.params.is_some()
                && let Some(pi) = body.get(i + 1).and_then(param)
            {
                items.push(Item::Tok(XTok::new(Token { space: tok.space, ..stringize(&args[pi]) })));
                i += 2;
                continue;
            }
            if tok.is(P::HashHash) {
                items.push(Item::Paste);
                i += 1;
                continue;
            }
            if tok.sym == intern::VA_OPT && m.variadic && body.get(i + 1).is_some_and(|t| t.is(P::LParen)) {
                let mut depth = 0;
                let mut j = i + 1;
                while j < body.len() {
                    if body[j].is(P::LParen) {
                        depth += 1;
                    } else if body[j].is(P::RParen) {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                let va = args.last().is_some_and(|a| !a.is_empty());
                if va {
                    self.substitute_into(m, params, args, &body[i + 2..j.min(body.len())], items);
                } else {
                    items.push(Item::Placemarker { va: false });
                }
                i = j + 1;
                continue;
            }
            if let Some(pi) = param(&tok) {
                let pasted = matches!(items.last(), Some(Item::Paste)) || body.get(i + 1).is_some_and(|t| t.is(P::HashHash));
                let arg = &args[pi];
                let va = m.variadic && pi + 1 == params.len();
                if pasted {
                    if arg.is_empty() {
                        items.push(Item::Placemarker { va });
                    } else {
                        items.extend(arg.iter().cloned().map(Item::Tok));
                    }
                } else {
                    let mut x = self.expand(arg.clone());
                    if let Some(first) = x.first_mut() {
                        first.t.space = tok.space;
                    }
                    items.extend(x.into_iter().map(Item::Tok));
                }
                i += 1;
                continue;
            }
            items.push(Item::Tok(XTok::new(tok)));
            i += 1;
        }
    }
}

fn unknown() -> Token {
    Token { kind: Kind::Unknown, sym: intern::EMPTY, space: true }
}

/// Builds macros from `#define` lines (for predefined and command-line
/// macros).
pub fn parse_defines(text: &str) -> Vec<Macro> {
    lex::lex(text.as_bytes()).dirs.into_iter().filter_map(|d| d.mac.map(|m| Arc::try_unwrap(m).unwrap_or_else(|a| (*a).clone_macro()))).collect()
}

impl Macro {
    fn clone_macro(&self) -> Macro {
        Macro {
            name: self.name,
            params: self.params.clone(),
            variadic: self.variadic,
            body: self.body.clone(),
            pragmas: self.pragmas.clone(),
            pragma_operands: self.pragma_operands.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Ctx {
        macros: HashMap<Sym, Arc<Macro>>,
    }

    impl Context for Ctx {
        fn lookup(&self, name: Sym) -> Option<Arc<Macro>> {
            self.macros.get(&name).cloned()
        }
        fn is_defined(&self, name: Sym) -> bool {
            self.macros.contains_key(&name) || name == intern::HAS_INCLUDE
        }
        fn dynamic(&mut self, name: Sym) -> Option<Token> {
            (name == intern::LINE).then(|| number(42))
        }
        fn has_include(&mut self, name: &str, angled: bool, _next: bool) -> bool {
            name == "yes.h" || (angled && name == "sys/yes.h")
        }
        fn has_embed(&mut self, name: &str, _angled: bool, limit_zero: bool) -> i64 {
            match name {
                "data.bin" if limit_zero => 2,
                "data.bin" => 1,
                _ => 0,
            }
        }
        fn is_query(&self, op: Sym) -> bool {
            op.as_str() == "__has_builtin"
        }
        fn query(&mut self, _op: Sym, arg: &str) -> Option<i64> {
            match arg {
                "__builtin_expect" => Some(1),
                "__builtin_unknown" => None,
                _ => Some(0),
            }
        }
    }

    fn ctx(defs: &str) -> Ctx {
        let mut c = Ctx::default();
        for m in parse_defines(defs) {
            c.macros.insert(m.name, Arc::new(m));
        }
        c
    }

    fn expand(defs: &str, text: &str, if_mode: bool) -> String {
        let mut c = ctx(defs);
        let mut x = Expander::new(&mut c, if_mode);
        let toks = lex::tokens(text.as_bytes());
        let out = x.expand_tokens(&toks);
        assert!(x.error.is_none(), "{:?}", x.error);
        let xt: Vec<XTok> = out.into_iter().map(XTok::new).collect();
        spelling(&xt)
    }

    #[test]
    fn standard_example_3() {
        // C17 6.10.3.5 EXAMPLE 3.
        let defs = "#define x 3\n#define f(a) f(x * (a))\n#undef x\n#define x 2\n#define g f\n#define z z[0]\n#define h g(~\n#define m(a) a(w)\n#define w 0,1\n#define t(a) a\n#define p() int\n#define q(x) x\n#define r(x,y) x ## y\n#define str(x) # x\n";
        // `#undef` isn't applied by parse_defines; the later definition wins.
        assert_eq!(expand(defs, "f(y+1) + f(f(z)) % t(t(g)(0) + t)(1);", false), "f(2 * (y+1)) + f(2 * (f(2 * (z[0])))) % f(2 * (0)) + t(1);");
        assert_eq!(expand(defs, "g(x+(3,4)-w) | h 5) & m (f)^m(m);", false), "f(2 * (2+(3,4)-0,1)) | f(2 * (~ 5)) & f(2 * (0,1))^m(0,1);");
        assert_eq!(expand(defs, "p() i[q()] = { q(1), r(2,3), r(4,), r(,5), r(,) };", false), "int i[] = { 1, 23, 4, 5, };");
        assert_eq!(expand(defs, "char c[2][6] = { str(hello), str() };", false), "char c[2][6] = { \"hello\", \"\" };");
    }

    #[test]
    fn standard_example_4() {
        // C17 6.10.3.5 EXAMPLE 4: stringizing and pasting.
        let defs = "#define str(s) # s\n#define xstr(s) str(s)\n#define debug(s, t) printf(\"x\" # s \"= %d, x\" # t \"= %s\", \\\n x ## s, x ## t)\n#define INCFILE(n) vers ## n\n#define glue(a, b) a ## b\n#define xglue(a, b) glue(a, b)\n#define HIGHLOW \"hello\"\n#define LOW LOW \", world\"\n";
        assert_eq!(expand(defs, "debug(1, 2);", false), "printf(\"x\" \"1\" \"= %d, x\" \"2\" \"= %s\", x1, x2);");
        assert_eq!(expand(defs, "xstr(INCFILE(2).h)", false), "\"vers2.h\"");
        assert_eq!(expand(defs, "glue(HIGH, LOW);", false), "\"hello\";");
        assert_eq!(expand(defs, "xglue(HIGH, LOW)", false), "\"hello\" \", world\"");
        assert_eq!(expand(defs, "str(\"a\\n\" 'b')", false), "\"\\\"a\\\\n\\\" 'b'\"");
    }

    #[test]
    fn standard_example_5_and_7() {
        // EXAMPLE 5: placemarkers.
        let defs = "#define t(x,y,z) x ## y ## z\n#define debug(...) fprintf(stderr, __VA_ARGS__)\n#define showlist(...) puts(#__VA_ARGS__)\n#define report(test, ...) ((test)?puts(#test): printf(__VA_ARGS__))\n";
        assert_eq!(
            expand(defs, "int j[] = { t(1,2,3), t(,4,5), t(6,,7), t(8,9,), t(10,,), t(,11,), t(,,12), t(,,) };", false),
            "int j[] = { 123, 45, 67, 89, 10, 11, 12, };"
        );
        assert_eq!(expand(defs, "debug(\"X = %d\\n\", x);", false), "fprintf(stderr, \"X = %d\\n\", x);");
        assert_eq!(expand(defs, "showlist(The first, second, and third items.);", false), "puts(\"The first, second, and third items.\");");
        assert_eq!(expand(defs, "report(x>y, \"x is %d but y is %d\", x, y);", false), "((x>y)?puts(\"x>y\"): printf(\"x is %d but y is %d\", x, y));");
    }

    #[test]
    fn va_opt_and_gnu_comma() {
        let defs = "#define F(a, ...) f(a __VA_OPT__(,) __VA_ARGS__)\n#define G(fmt, args...) g(fmt, ## args)\n#define H(fmt, ...) h(fmt, ##__VA_ARGS__)\n";
        assert_eq!(expand(defs, "F(1) F(1, 2, 3)", false), "f(1) f(1, 2, 3)");
        assert_eq!(expand(defs, "G(x) G(x, y)", false), "g(x) g(x, y)");
        assert_eq!(expand(defs, "H(x) H(x, y, z)", false), "h(x) h(x, y, z)");
    }

    #[test]
    fn pragmas_of_bodies() {
        // With the macros defined before.
        let of = |defs: &str| {
            let mut macros = parse_defines(defs);
            let m = macros.pop().unwrap();
            let mut c = Ctx::default();
            for m in macros {
                c.macros.insert(m.name, Arc::new(m));
            }
            m.pragmas_in(&mut c).0
        };
        let names = |n: &[&str]| Pragmas::Macros(n.iter().map(|s| intern::intern(s)).collect());
        // libc++'s _LIBCPP_PUSH_MACROS, and MSVC's spelling.
        assert_eq!(of("#define PUSH _Pragma(\"push_macro(\\\"min\\\")\") _Pragma(\"push_macro(\\\"max\\\")\")\n"), names(&["min", "max"]));
        assert_eq!(of("#define POP __pragma(pop_macro(\"min\")) __pragma(warning(pop))\n"), names(&["min"]));
        assert_eq!(of("#define D _Pragma(\"clang diagnostic push\") __pragma(warning(push))\n"), Pragmas::None);
        assert_eq!(of("#define STR(x) _Pragma(#x)\n"), Pragmas::None);
        assert_eq!(of("#define PRAGMA(x) _Pragma(x)\n"), Pragmas::None);
        assert_eq!(of("#define VERSION \"1.2\" call_once(f)\n"), Pragmas::None);
        // A parameter named `once` (GLib's g_once).
        assert_eq!(of("#define g_once(once, func) ((once)->status ? (once)->retval : func(once))\n"), Pragmas::None);
        // Operands macros make: libc++'s, Boost's and GLib's (whose pragma
        // starts with the macro's argument, seen where it's used).
        let libcxx = "#define TOSTR2(x) #x\n#define TOSTR(x) TOSTR2(x)\n";
        assert_eq!(of(&format!("{libcxx}#define IGNORED(str) _Pragma(TOSTR(clang diagnostic ignored str))\n")), Pragmas::None);
        assert_eq!(
            of(&format!(
                "{libcxx}#define SIG abi\n#define END _Pragma(TOSTR(clang attribute X.push(__attribute__((__abi_tag__(TOSTR(SIG)))), apply_to = function)))\n"
            )),
            Pragmas::None
        );
        let boost = "#define BOOST_DO_STRINGIZE(X) #X\n#define BOOST_STRINGIZE(X) BOOST_DO_STRINGIZE(X)\n";
        assert_eq!(of(&format!("{boost}#define BOOST_PRAGMA_MESSAGE(x) _Pragma(BOOST_STRINGIZE(message(x)))\n")), Pragmas::None);
        let glib = "#define G_STRINGIFY(macro_or_string) G_STRINGIFY_ARG (macro_or_string)\n#define G_STRINGIFY_ARG(contents) #contents\n";
        assert_eq!(of(&format!("{glib}#define _GLIB_GNUC_DO_PRAGMA(x) _Pragma(G_STRINGIFY (x))\n")), Pragmas::None);
        assert_eq!(of(&format!("{libcxx}#define P _Pragma(TOSTR(push_macro(\"M\")))\n")), names(&["M"]));
        // Not a string (`S` not defined yet): an error where it's used, no
        // pragma; unless an argument makes it.
        assert_eq!(of("#define P _Pragma(ONCE_STR)\n"), Pragmas::None);
        assert_eq!(of("#define P(x) _Pragma(PASS(x))\n"), Pragmas::Other);
        // (Strings aren't joined before `_Pragma` reads its operand.)
        assert_eq!(of("#define P(x) _Pragma(\"push_macro(\" #x \")\")\n"), Pragmas::None);
        for other in [
            "#define ONCE _Pragma(\"once\")\n",
            "#define ONCE __pragma(once)\n",
            "#define ALIAS _Pragma(\"include_alias(\\\"a.h\\\", \\\"b.h\\\")\")\n",
            "#define P DO_PRAGMA(push_macro(\"M\"))\n",
            "#define P DO_PRAGMA(once)\n",
            "#define P PRAGMA(\"once\")\n",
            // A macro's string (`#define ONCE_STR "once"`).
            "#define ONCE_STR \"once\"\n#define P _Pragma(ONCE_STR)\n",
            "#define TOSTR(x) #x\n#define P _Pragma(TOSTR(once))\n",
            "#define TOSTR(x) #x\n#define P(x) _Pragma(TOSTR(push_macro(x)))\n",
            "#define TOSTR(x) #x\n#define P(x, y) _Pragma(TOSTR(x ## y))\n",
        ] {
            assert_eq!(of(other), Pragmas::Other, "{other}");
        }
        // What the expansion depends on.
        let m = parse_defines("#define P(s) _Pragma(TOSTR(clang diagnostic s))\n").remove(0);
        let mut deps = m.pragmas_in(&mut ctx(libcxx)).1;
        deps.sort_by_key(|d| d.as_str());
        assert_eq!(deps, ["TOSTR", "TOSTR2", "clang", "diagnostic"].map(intern::intern));
    }

    #[test]
    fn redefined_query_operands() {
        struct Redefining(Ctx);
        impl Context for Redefining {
            fn lookup(&self, name: Sym) -> Option<Arc<Macro>> {
                self.0.lookup(name)
            }
            fn is_defined(&self, name: Sym) -> bool {
                self.0.is_defined(name)
            }
            fn dynamic(&mut self, name: Sym) -> Option<Token> {
                self.0.dynamic(name)
            }
            fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool {
                self.0.has_include(name, angled, next)
            }
            fn has_embed(&mut self, name: &str, angled: bool, limit_zero: bool) -> i64 {
                self.0.has_embed(name, angled, limit_zero)
            }
            fn is_query(&self, op: Sym) -> bool {
                self.0.is_query(op)
            }
            fn query(&mut self, op: Sym, arg: &str) -> Option<i64> {
                self.0.query(op, arg)
            }
            fn redefined(&self, name: Sym) -> bool {
                self.0.macros.contains_key(&name)
            }
        }
        let mut c = Redefining(ctx("#define B __builtin_expect\n"));
        let mut x = Expander::new(&mut c, true);
        let out = x.expand_tokens(&lex::tokens(b"__has_builtin(B) || __has_builtin(__builtin_expect)"));
        assert!(x.error.is_some());
        assert_eq!(out[0].kind, Kind::Unknown);
    }

    #[test]
    fn recursion_is_blocked() {
        let defs = "#define foo foo\n#define a b\n#define b a\n#define f(x) x f\n";
        // The `f` from f's own expansion isn't expanded again (as in GCC and
        // Clang).
        assert_eq!(expand(defs, "foo a b f(1)(2)", false), "foo a b 1 f(2)");
    }

    #[test]
    fn if_mode_operators() {
        let defs = "#define D 1\n#define FN() 1\n#define CHECK(x) defined(x)\n#define HDR <sys/yes.h>\n";
        assert_eq!(expand(defs, "defined D && defined(D) && !defined X", true), "1 && 1 && !0");
        // `defined` from a macro sees its argument expanded: FN isn't (no
        // parentheses); D is, which GCC and Clang reject too.
        assert_eq!(expand(defs, "CHECK(FN) CHECK(UNDEF)", true), "1 0");
        let mut c = ctx(defs);
        let mut x = Expander::new(&mut c, true);
        x.expand_tokens(&lex::tokens(b"CHECK(D)"));
        assert!(x.error.is_some());
        assert_eq!(expand(defs, "__has_include(\"yes.h\") + __has_include(<no.h>) + __has_include(HDR) + __has_include(<sys/yes.h>)", true), "1 + 0 + 1 + 1");
        assert_eq!(expand(defs, "__has_builtin(__builtin_expect) __LINE__", true), "1 42");
        let mut c = ctx(defs);
        let mut x = Expander::new(&mut c, true);
        let out = x.expand_tokens(&lex::tokens(b"__has_builtin(__builtin_unknown)"));
        assert_eq!(out[0].kind, Kind::Unknown);
        // Outside #if, `defined` is just an identifier.
        assert_eq!(expand(defs, "defined(D)", false), "defined(1)");
    }
}
