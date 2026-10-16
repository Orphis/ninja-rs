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
//! cl's traditional preprocessor (without `/Zc:preprocessor`: see
//! `Expander::expand_traditional`) expands otherwise, in ways Boost's
//! MSVC configuration relies on.
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
    /// The macros the body gives a pragma's words (`DO_PRAGMA(once)`): see
    /// `Found::callees`.
    pub pragma_callees: Box<[Sym]>,
    /// A `_Pragma` or `__pragma` operand of the body is an argument's
    /// (`_Pragma(x)`, `__pragma(x)`).
    pub pragma_arg: bool,
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

/// The string literal's contents (`"a\"b"` is `a"b`), as `_Pragma` reads
/// them: the prefix and the quotes gone, `\"` and `\\` unescaped.
pub(crate) fn destringize(lit: &str) -> String {
    let body = lit.trim_start_matches(|c| c != '"');
    let body = body.strip_prefix('"').unwrap_or(body);
    let mut chars = body.strip_suffix('"').unwrap_or(body).chars().peekable();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&e @ ('"' | '\\'))) => {
                out.push(e);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The pragmas that change macros or inclusion.
pub(crate) const PRAGMAS: [Sym; 4] = [intern::ONCE, intern::PUSH_MACRO, intern::POP_MACRO, intern::INCLUDE_ALIAS];

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
        // `#pragma GCC dependency`: a file, found from where it runs.
        [g, d, ..] if g.sym == intern::GCC && d.sym == intern::DEPENDENCY => *other = true,
        _ => {}
    }
}

/// Whether a pragma's words are `GCC dependency` (a file the compile reads:
/// see `scan`).
pub(crate) fn dependency_words(p: &[Token]) -> bool {
    matches!(p, [g, d, ..] if g.sym == intern::GCC && d.sym == intern::DEPENDENCY)
}

/// What `pragmas` finds in tokens.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// What they do through pragmas, wherever they are.
    pub pragmas: Pragmas,
    /// `_Pragma` operands macros make (see `operand_pragmas`).
    pub operands: Vec<Box<[Token]>>,
    /// The macros pragmas' words are an argument of (`DO_PRAGMA(once)`,
    /// `P(x, "once")`): those run a pragma if they're macros (not `if
    /// (once)`).
    pub callees: Vec<Sym>,
    /// A `_Pragma` or `__pragma` operand is one of the parameters: the
    /// pragma is the argument's.
    pub arg: bool,
}

/// The index of the `)` closing the `(` at `open`, or the end.
pub(crate) fn closing(toks: &[Token], open: usize) -> usize {
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
pub(crate) fn pragmas(toks: &[Token], literal: bool, params: &[Sym]) -> Found {
    let mut found = Found::default();
    let (mut names, mut other) = (Vec::new(), false);
    let is_param = |t: &Token| t.ident().is_some_and(|p| params.contains(&p));
    // Where the parentheses still open are.
    let mut open: Vec<usize> = Vec::new();
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
                [p] if is_param(p) => found.arg = true,
                operand => found.operands.push(operand.into()),
            }
            i = end + 1;
            continue;
        }
        if call && t.sym == intern::MS_PRAGMA_OPERATOR {
            let end = closing(toks, i + 1);
            match toks.get(i + 2) {
                Some(p) if is_param(p) => found.arg = true,
                _ => pragma_words(&toks[i + 2..end], &mut names, &mut other),
            }
            i = end + 1;
            continue;
        }
        if t.is(P::LParen) {
            open.push(i);
        } else if t.is(P::RParen) {
            open.pop();
        }
        // A whole argument of a call: `DO_PRAGMA(once)`, `P(x, once)`,
        // `PRAGMA("once")`; not a parameter's name (GLib's `g_once(once,
        // ...)` uses `(once)`).
        let argument = !open.is_empty()
            && i > 0
            && (toks[i - 1].is(P::LParen) || toks[i - 1].is(P::Comma))
            && toks.get(i + 1).is_some_and(|n| n.is(P::RParen) || n.is(P::Comma));
        let word = match t.kind {
            Kind::Ident if is_param(&t) => false,
            Kind::Ident if [intern::PUSH_MACRO, intern::POP_MACRO, intern::INCLUDE_ALIAS].contains(&t.sym) => {
                other = true;
                false
            }
            Kind::Ident => t.sym == intern::ONCE && argument,
            Kind::Str if argument => pragma_string(t.sym.as_str()),
            _ => false,
        };
        if word {
            // The call's: the identifier before its `(` (else unknown: a
            // macro's expansion may end with one).
            match open.last().and_then(|&o| o.checked_sub(1)).map(|c| toks[c]) {
                Some(c) if c.kind == Kind::Ident && !is_param(&c) => found.callees.push(c.sym),
                _ => other = true,
            }
        }
        i += 1;
    }
    found.pragmas = match (other, names.is_empty()) {
        (true, _) => Pragmas::Other,
        (false, false) => Pragmas::Macros(names.into_boxed_slice()),
        (false, true) => Pragmas::None,
    };
    found
}

/// Whether a string literal's text starts with one of the pragmas that
/// change macros or inclusion (`"once"`).
pub(crate) fn pragma_string(lit: &str) -> bool {
    let text = destringize(lit);
    let word = text.trim_start().split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or_default();
    PRAGMAS.iter().any(|p| p.as_str() == word)
}

/// What a pragma's words, as `_Pragma` reads them from a string literal,
/// do: a word given to the macro (one of `params`: `arg`) where it's used,
/// unknown text (where a dynamic macro's value is: see `Recording`).
fn string_pragmas(lit: &str, params: &[Sym], arg: &mut bool) -> Pragmas {
    let text = destringize(lit);
    if text.trim_start().starts_with(UNKNOWN_TEXT) {
        return Pragmas::Other;
    }
    let words = lex::tokens(text.as_bytes());
    if words.first().and_then(Token::ident).is_some_and(|w| params.contains(&w)) {
        *arg = true;
        return Pragmas::None;
    }
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

/// The text a string-valued dynamic macro (`__FILE__`) stands for where a
/// macro's pragma is classified (see `Recording`): its value where the
/// macro is used is unknown.
const UNKNOWN_TEXT: char = '\u{0}';

/// Tokens of a macro's body (or a `_Pragma` operand in it) to expand
/// where the macro is defined: `params` (the macro's, whose arguments are
/// seen where it's used) never expanded (their hide sets have them, as the
/// macro's own name, which its expansion doesn't expand), `#param` the
/// argument's spelling. None when pasted where it's used (`##`,
/// `__VA_OPT__`).
fn unexpanded(toks: &[Token], own: Option<Sym>, params: &[Sym]) -> Option<Vec<XTok>> {
    let param = |t: &Token| t.ident().filter(|p| params.contains(p));
    let mut input = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t.is(P::Hash)
            && let Some(p) = toks.get(i + 1).and_then(param)
        {
            input.push(XTok::new(Token { kind: Kind::Str, sym: intern::intern(&format!("\"{}\"", p.as_str())), space: t.space }));
            i += 2;
            continue;
        }
        if t.is(P::HashHash) || t.sym == intern::VA_OPT {
            return None;
        }
        input.push(XTok { hide: Some(Rc::new(param(&t).into_iter().chain(own).collect())), ..XTok::new(t) });
        i += 1;
    }
    Some(input)
}

/// What a `_Pragma` operand that macros make does (`_Pragma(STR(x))`),
/// expanded with the macros `ctx` has, `params` (the macro's, whose
/// arguments are seen where it's used) left as they are: a pragma starting
/// with a parameter is the argument's (the third value); an operand that
/// doesn't expand to a string literal is an error where it's used (no
/// pragma runs), unless the arguments are part of it. Returns it with the
/// macros the expansion looked up, whose changes change it.
pub fn operand_pragmas(operand: &[Token], own: Option<Sym>, params: &[Sym], ctx: &mut dyn Context) -> (Pragmas, Vec<Sym>, bool) {
    let param = |t: &Token| t.ident().filter(|p| params.contains(p));
    let Some(input) = unexpanded(operand, own, params) else { return (Pragmas::Other, Vec::new(), false) };
    let mut recording = Recording { ctx, looked_up: Default::default() };
    let mut x = Expander::new(&mut recording, false);
    let out = x.expand(input);
    let failed = x.error.is_some() || x.unsure();
    let mut looked_up = recording.looked_up.into_inner();
    looked_up.sort();
    looked_up.dedup();
    let mut arg = false;
    let pragmas = match &out[..] {
        _ if failed => Pragmas::Other,
        [s] if s.t.kind == Kind::Str => string_pragmas(s.t.sym.as_str(), params, &mut arg),
        toks if toks.iter().any(|t| param(&t.t).is_some()) => Pragmas::Other,
        _ => Pragmas::None,
    };
    (pragmas, looked_up, arg)
}

/// The words of the pragma a `_Pragma` operand that macros make runs (see
/// `operand_pragmas`), expanded with the macros `ctx` has; None unless
/// it's a string literal.
pub(crate) fn operand_words(operand: &[Token], ctx: &mut dyn Context) -> Option<Vec<Token>> {
    let input = unexpanded(operand, None, &[])?;
    let mut recording = Recording { ctx, looked_up: Default::default() };
    let mut x = Expander::new(&mut recording, false);
    let out = x.expand(input);
    match &out[..] {
        [s] if x.error.is_none() && !x.unsure() && s.t.kind == Kind::Str => Some(lex::tokens(destringize(s.t.sym.as_str()).as_bytes())),
        _ => None,
    }
}

/// Whether text calling a macro (`call`: its name, and its arguments) may
/// run a pragma that changes macros or inclusion (`CAT(DO_, PRAGMA)(x)`,
/// whose name pastes one whose pragma is its argument's): its expansion
/// with the macros `ctx` has runs one, or names a macro whose pragmas its
/// arguments make (see `Context::arg_pragma`); or it fails.
pub(crate) fn call_runs_pragma(call: &[Token], ctx: &mut dyn Context) -> bool {
    let mut recording = Recording { ctx, looked_up: Default::default() };
    let (out, failed) = {
        let mut x = Expander::new(&mut recording, false);
        let out = x.expand_tokens(call);
        (out, x.error.is_some() || x.unsure())
    };
    let found = pragmas(&out, true, &[]);
    failed || found.pragmas != Pragmas::None || !found.operands.is_empty() || out.iter().any(|t| t.kind == Kind::Ident && recording.ctx.arg_pragma(t.sym))
}

/// What a body that names macros whose pragmas their arguments make does
/// (`#define WRAP(x) DO_PRAGMA(x)`, `#define ONCE DO_PRAGMA(ONCE_WORD)`):
/// the pragmas of its expansion with the macros `ctx` has, parameters left
/// as they are (see `operand_pragmas`); one such macro left in it
/// (`#define DP DO_PRAGMA`) takes what follows the macro where it's used.
fn body_pragmas(m: &Macro, ctx: &mut dyn Context) -> (Pragmas, Vec<Sym>, bool) {
    let params = m.params.as_deref().unwrap_or(&[]);
    let param = |t: &Token| t.ident().filter(|p| params.contains(p));
    let Some(input) = unexpanded(&m.body, Some(m.name), params) else { return (Pragmas::Other, Vec::new(), false) };
    let mut recording = Recording { ctx, looked_up: Default::default() };
    let mut x = Expander::new(&mut recording, false);
    let out: Vec<Token> = x.expand(input).into_iter().map(|t| t.t).collect();
    let failed = x.error.is_some() || x.unsure();
    let mut looked_up = recording.looked_up.into_inner();
    looked_up.sort();
    looked_up.dedup();
    if failed {
        return (Pragmas::Other, looked_up, false);
    }
    let (mut pragmas, mut arg) = (Pragmas::None, false);
    let mut i = 0;
    while i < out.len() {
        let t = out[i];
        let call = out.get(i + 1).is_some_and(|p| p.is(P::LParen));
        if call && (t.sym == intern::PRAGMA_OPERATOR || t.sym == intern::MS_PRAGMA_OPERATOR) {
            let end = closing(&out, i + 1);
            let operand = &out[i + 2..end];
            let p = match operand {
                _ if end == out.len() => Pragmas::Other,
                [s] if t.sym == intern::PRAGMA_OPERATOR && s.kind == Kind::Str => string_pragmas(s.sym.as_str(), params, &mut arg),
                [p, ..] if param(p).is_some() => {
                    arg = true;
                    Pragmas::None
                }
                _ if t.sym == intern::MS_PRAGMA_OPERATOR => {
                    let (mut names, mut other) = (Vec::new(), false);
                    pragma_words(operand, &mut names, &mut other);
                    match (other, names.is_empty()) {
                        (true, _) => Pragmas::Other,
                        (false, true) => Pragmas::None,
                        (false, false) => Pragmas::Macros(names.into_boxed_slice()),
                    }
                }
                _ if operand.iter().any(|t| param(t).is_some()) => Pragmas::Other,
                _ => Pragmas::None,
            };
            pragmas = pragmas.and(p);
            i = end + 1;
            continue;
        }
        arg |= t.kind == Kind::Ident && param(&t).is_none() && recording.ctx.arg_pragma(t.sym);
        i += 1;
    }
    (pragmas, looked_up, arg)
}

impl Macro {
    /// What the body does through pragmas where it's expanded, with the
    /// macros `ctx` has (see `operand_pragmas`), the macros that depends
    /// on, and whether the pragma is an argument's (`DO_PRAGMA(x)`: see
    /// `Context::arg_pragma`).
    pub fn pragmas_in(&self, ctx: &mut dyn Context) -> (Pragmas, Vec<Sym>, bool) {
        let params = self.params.as_deref().unwrap_or(&[]);
        let (mut pragmas, mut deps, mut arg) = (self.pragmas.clone(), Vec::new(), self.pragma_arg);
        // A pragma's words given to a macro.
        for &c in self.pragma_callees.iter() {
            deps.push(c);
            if ctx.lookup(c).is_some() || ctx.is_unknown(c) {
                pragmas = pragmas.and(Pragmas::Other);
            }
        }
        for operand in &self.pragma_operands {
            let (p, d, a) = operand_pragmas(operand, Some(self.name), params, ctx);
            pragmas = pragmas.and(p);
            deps.extend(d);
            arg |= a;
        }
        if self.body.iter().any(|t| t.ident().is_some_and(|n| !params.contains(&n) && n != self.name && ctx.arg_pragma(n))) {
            let (p, d, a) = body_pragmas(self, ctx);
            pragmas = pragmas.and(p);
            deps.extend(d);
            arg |= a;
        }
        (pragmas, deps, arg)
    }
}

/// A context that notes the macros looked up, and expands dynamic ones
/// (`__LINE__`) as numbers or strings of unknown values (their values
/// where a macro is used): not a pragma's words.
struct Recording<'c> {
    ctx: &'c mut dyn Context,
    looked_up: std::cell::RefCell<Vec<Sym>>,
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
        if !self.ctx.is_defined(name) {
            return None;
        }
        Some(if [intern::LINE, intern::COUNTER, intern::INCLUDE_LEVEL].contains(&name) {
            number(1)
        } else {
            Token { kind: Kind::Str, sym: intern::intern(&format!("\"{UNKNOWN_TEXT}\"")), space: true }
        })
    }
    fn arg_pragma(&self, name: Sym) -> bool {
        self.ctx.arg_pragma(name)
    }
    fn traditional(&self) -> bool {
        self.ctx.traditional()
    }
    fn va_comma(&self) -> VaComma {
        self.ctx.va_comma()
    }
    fn is_unknown(&self, name: Sym) -> bool {
        self.ctx.is_unknown(name)
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
        let found = pragmas(&body, true, &params);
        Some(Macro {
            name,
            params: Some(params.into_boxed_slice()),
            variadic,
            body: body.into_boxed_slice(),
            pragmas: found.pragmas,
            pragma_operands: found.operands.into_boxed_slice(),
            pragma_callees: found.callees.into_boxed_slice(),
            pragma_arg: found.arg,
        })
    } else {
        let mut body: Vec<Token> = rest.to_vec();
        if let Some(first) = body.first_mut() {
            first.space = false;
        }
        let found = pragmas(&body, true, &[]);
        Some(Macro {
            name,
            params: None,
            variadic: false,
            body: body.into_boxed_slice(),
            pragmas: found.pragmas,
            pragma_operands: found.operands.into_boxed_slice(),
            pragma_callees: found.callees.into_boxed_slice(),
            pragma_arg: found.arg,
        })
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
    /// cl's traditional preprocessor: a comma of an argument substituted
    /// into the replacement being expanded, outside the argument's
    /// parentheses, which separates no arguments of a call there; or a
    /// parenthesis of one without its pair in it (see `substitute_traditional`).
    glued: bool,
    /// cl's traditional preprocessor: a function-like macro's name seen
    /// followed by something else than `(` (see `expand_traditional`).
    rejected: bool,
    /// cl's traditional preprocessor: a comma before an empty
    /// `__VA_ARGS__`, gone at the end of its macro's expansion.
    elided: bool,
    /// cl's traditional preprocessor: a function-like macro's name ending
    /// the expansion of a call to a macro with parameters (see
    /// `expand_traditional`).
    tail: bool,
    /// cl's traditional preprocessor: a token of an argument next to `##`
    /// but not pasted, of a call that isn't at the top: not expanded before
    /// the replacement of the call at the top (see `expand_traditional`),
    /// with the hide set it had then (not the macros' it went through).
    frozen: Option<Hide>,
    /// cl's traditional preprocessor: whitespace before it follows a macro's
    /// expansion to nothing (`EMPTY() x`), which a paste with it as second
    /// token may paste instead (`CAT(a, EMPTY() x)` is `a x`, see `paste`).
    gap: bool,
    /// cl's traditional preprocessor: the name of the macro of whose
    /// argument it is, but its first token, which cl doesn't hide for good
    /// (see `expand_traditional`).
    own: bool,
}

impl XTok {
    pub fn new(t: Token) -> Self {
        XTok { t, hide: None, glued: false, rejected: false, elided: false, tail: false, frozen: None, gap: false, own: false }
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
    /// Whether `name` is a macro whose pragma its arguments make
    /// (`DO_PRAGMA(x)`): where it's used, a macro's expansion given to it
    /// may run one (`DO_PRAGMA(ONCE_WORD)`).
    fn arg_pragma(&self, _name: Sym) -> bool {
        false
    }
    /// Whether macros expand as cl's traditional preprocessor expands them
    /// (`_MSVC_TRADITIONAL` is 1: see `Expander::expand_traditional`).
    fn traditional(&self) -> bool {
        false
    }
    /// When a comma before `__VA_ARGS__` left out goes.
    fn va_comma(&self) -> VaComma {
        VaComma::default()
    }
    /// Whether `name`'s definition is unknown here (it may be any
    /// definition it had in this scan, or none: see `scan`'s tracking of
    /// unknown values).
    fn is_unknown(&self, _name: Sym) -> bool {
        false
    }
}

/// When the compiler drops the comma before `__VA_ARGS__` (with `##`
/// before it: `, ## __VA_ARGS__`) when a call leaves the variadic arguments
/// out (`F(a)` for `F(a, ...)`, not `F(a,)`). cl's traditional
/// preprocessor drops it before an empty one whatever (see
/// `Expander::expand_traditional`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VaComma {
    /// Only when the macro has another parameter: GCC's strict modes
    /// (`-std=c11`, `-std=c++17`), and Clang's in C (C99 and later).
    pub needs_named: bool,
    /// Without `##` too: Clang's MSVC compatibility.
    pub bare: bool,
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
    /// A `##` of the macro's body; `gnu`: between a comma and the variadic
    /// parameter (`, ## __VA_ARGS__`), which keeps both (GNU's extension).
    Paste {
        gnu: bool,
    },
    /// An empty argument next to `##` (`va`: the variadic one).
    Placemarker {
        va: bool,
    },
}

/// The arguments of a call, as cl's traditional preprocessor takes them:
/// one per parameter (empty when missing), and how many were given (`#x`
/// of a missing one is nothing).
struct Args {
    toks: Vec<Vec<XTok>>,
    given: usize,
}

/// How deep cl's traditional preprocessor's expansions may nest (each
/// replacement is expanded by itself, see `expand_traditional`): deeper,
/// the expansion fails rather than take the stack.
const MAX_NESTING: usize = 200;

pub struct Expander<'c> {
    pub ctx: &'c mut dyn Context,
    /// `#if` mode: `defined` and the operators are evaluated.
    pub if_mode: bool,
    /// Set on malformed input (the expansion is then not trustworthy).
    pub error: Option<String>,
    /// The first macro of unknown definition (see `Context::is_unknown`)
    /// the expansion read: its result is then unknown as a whole (another
    /// definition may expand to anything, a `)` or a comma).
    pub tainted: Option<Sym>,
    /// An unknown value (`Kind::Unknown`: an unanswered query, `__COUNTER__`)
    /// was pasted or stringized: the result looks known but isn't.
    pub garbled: bool,
    /// How many unknown values the expansion made.
    pub unknown_values: usize,
    budget: usize,
    /// Macros expand as cl's traditional preprocessor expands them.
    traditional: bool,
    /// When a comma before `__VA_ARGS__` left out goes (the standard's
    /// expansion).
    va_comma: VaComma,
    /// The replacements being expanded, nested (traditional only).
    nesting: usize,
    /// The hide set of the call at the top whose replacement is being
    /// expanded (`Scan::TopReplacement`), which frozen tokens get there.
    top_hide: Hide,
    /// The macros whose arguments are being expanded, nested (traditional
    /// only).
    owners: Vec<Sym>,
}

impl<'c> Expander<'c> {
    pub fn new(ctx: &'c mut dyn Context, if_mode: bool) -> Self {
        let (traditional, va_comma) = (ctx.traditional(), ctx.va_comma());
        Expander {
            ctx,
            if_mode,
            error: None,
            tainted: None,
            garbled: false,
            unknown_values: 0,
            budget: 1_000_000,
            traditional,
            va_comma,
            nesting: 0,
            top_hide: None,
            owners: Vec::new(),
        }
    }

    /// An identifier met while expanding: one whose definition is unknown
    /// (whatever it's hidden by or followed by: another definition it may
    /// have would be called otherwise) makes the result unknown.
    fn taint(&mut self, name: Sym) {
        if self.tainted.is_none() && self.ctx.is_unknown(name) {
            self.tainted = Some(name);
        }
    }

    /// Whether the result can't be trusted as known: it read a macro of
    /// unknown definition, or pasted or stringized an unknown value.
    pub fn unsure(&self) -> bool {
        self.tainted.is_some() || self.garbled
    }

    /// Stringizes an argument (an unknown value in it garbles the result).
    fn stringized(&mut self, arg: &[XTok]) -> Token {
        if arg.iter().any(|t| t.t.kind == Kind::Unknown) {
            self.garbled = true;
        }
        stringize(arg)
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
    /// raw. For `defined`, also the bare `defined X`. None too with a
    /// parenthesis glued (see `substitute_traditional`).
    fn operand(&mut self, input: &mut VecDeque<XTok>, bare: bool) -> Option<Vec<XTok>> {
        match input.front() {
            Some(t) if t.t.is(P::LParen) => {
                let mut depth = 1;
                let mut out = Vec::new();
                if input.pop_front()?.glued {
                    return None;
                }
                while let Some(t) = input.pop_front() {
                    if t.glued && (t.t.is(P::LParen) || t.t.is(P::RParen)) {
                        return None;
                    }
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
                Some([t]) if t.t.kind == Kind::Ident && self.ctx.is_unknown(t.t.sym) => unknown(),
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

    /// Expands tokens (a line, or an operand) as the preprocessor does.
    pub fn expand(&mut self, input: Vec<XTok>) -> Vec<XTok> {
        if self.traditional { self.expand_traditional(input, Scan::Top) } else { self.expand_standard(input) }
    }

    /// Takes the next token, unless the budget is spent.
    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            self.fail("expansion too large");
            return false;
        }
        self.budget -= 1;
        true
    }

    /// The macro an identifier of the input names, after the operators of
    /// `#if` mode and the dynamic macros (whose values go to `out`); None
    /// when it names none or is hidden.
    fn macro_at(&mut self, t: &XTok, input: &mut VecDeque<XTok>, out: &mut Vec<XTok>) -> Option<Arc<Macro>> {
        let name = t.t.sym;
        if self.if_mode
            && let Some(mut v) = self.if_operator(name, input)
        {
            self.unknown_values += (v.kind == Kind::Unknown) as usize;
            v.space = t.t.space;
            out.push(XTok::new(v));
            return None;
        }
        if hidden(&t.hide, name) || t.rejected {
            out.push(t.clone());
            return None;
        }
        let m = self.ctx.lookup(name);
        if m.is_none() {
            match self.ctx.dynamic(name) {
                Some(mut d) => {
                    self.unknown_values += (d.kind == Kind::Unknown) as usize;
                    d.space = t.t.space;
                    out.push(XTok::new(d));
                }
                None => out.push(t.clone()),
            }
        }
        m
    }

    fn expand_standard(&mut self, input: Vec<XTok>) -> Vec<XTok> {
        let mut input: VecDeque<XTok> = input.into();
        let mut out = Vec::new();
        while let Some(t) = input.pop_front() {
            if !self.spend() {
                break;
            }
            if t.t.kind != Kind::Ident {
                out.push(t);
                continue;
            }
            let name = t.t.sym;
            self.taint(name);
            let Some(m) = self.macro_at(&t, &mut input, &mut out) else { continue };
            let replacement = match &m.params {
                None => {
                    let hs = hide_union(&t.hide, &Some(Rc::new(vec![name])));
                    self.substitute(&m, &[], hs, t.t.space, false)
                }
                Some(params) => {
                    if !input.front().is_some_and(|n| n.t.is(P::LParen)) {
                        out.push(t);
                        continue;
                    }
                    let Some((args, rparen, elided)) = self.arguments(&mut input, params.len(), m.variadic) else {
                        self.fail(format!("bad arguments for {}", name.as_str()));
                        out.push(t);
                        continue;
                    };
                    let hs = hide_union(&hide_intersection(&t.hide, &rparen), &Some(Rc::new(vec![name])));
                    let drop_comma = elided && (!self.va_comma.needs_named || params.len() > 1);
                    self.substitute(&m, &args, hs, t.t.space, drop_comma)
                }
            };
            for r in replacement.into_iter().rev() {
                input.push_front(r);
            }
        }
        out
    }

    /// `( args )` of a function-like macro, the hide set of `)`, and
    /// whether the variadic arguments were left out (`F(a)` for `F(a,
    /// ...)`, `G()` for `G(...)`).
    fn arguments(&mut self, input: &mut VecDeque<XTok>, nparams: usize, variadic: bool) -> Option<(Vec<Vec<XTok>>, Hide, bool)> {
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
        let mut elided = variadic && nparams == 1 && args.len() == 1 && args[0].is_empty();
        if variadic && args.len() + 1 == nparams {
            args.push(Vec::new());
            elided = true;
        }
        (args.len() == nparams).then_some((args, rparen, elided))
    }

    /// The replacement of a macro: `drop_comma`, a comma before
    /// `__VA_ARGS__` left out goes (see `VaComma`).
    fn substitute(&mut self, m: &Macro, args: &[Vec<XTok>], hs: Hide, space: bool, drop_comma: bool) -> Vec<XTok> {
        let params: &[Sym] = m.params.as_deref().unwrap_or(&[]);
        let mut items = Vec::new();
        self.substitute_into(m, params, args, &m.body, &mut items, drop_comma);
        self.paste(items, hs, space)
    }

    fn substitute_into(&mut self, m: &Macro, params: &[Sym], args: &[Vec<XTok>], body: &[Token], items: &mut Vec<Item>, drop_comma: bool) {
        let param = |t: &Token| if m.params.is_some() { t.ident().and_then(|s| params.iter().position(|p| *p == s)) } else { None };
        let mut i = 0;
        while i < body.len() {
            let tok = body[i];
            if tok.is(P::Hash)
                && m.params.is_some()
                && let Some(pi) = body.get(i + 1).and_then(param)
            {
                items.push(Item::Tok(XTok::new(Token { space: tok.space, ..self.stringized(&args[pi]) })));
                i += 2;
                continue;
            }
            if tok.is(P::HashHash) {
                items.push(Item::Paste { gnu: gnu_comma(m, body, i) });
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
                    self.substitute_into(m, params, args, &body[i + 2..j.min(body.len())], items, drop_comma);
                } else {
                    items.push(Item::Placemarker { va: false });
                }
                i = j + 1;
                continue;
            }
            // Clang's MSVC compatibility: `, __VA_ARGS__` too.
            if tok.is(P::Comma) && drop_comma && self.va_comma.bare && body.get(i + 1).and_then(param) == Some(params.len() - 1) {
                i += 1;
                continue;
            }
            if let Some(pi) = param(&tok) {
                let pasted = matches!(items.last(), Some(Item::Paste { .. })) || body.get(i + 1).is_some_and(|t| t.is(P::HashHash));
                let arg = &args[pi];
                let va = drop_comma && m.variadic && pi + 1 == params.len();
                if pasted {
                    if arg.is_empty() {
                        items.push(Item::Placemarker { va });
                    } else {
                        items.extend(arg.iter().cloned().map(Item::Tok));
                    }
                } else {
                    let mut x = self.expand_standard(arg.clone());
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

    /// Expansion as cl's traditional preprocessor does it, as `cl /E` shows
    /// (see the tests against it):
    /// - A macro's replacement is expanded by itself, its arguments
    ///   substituted, before what follows it is seen: a function-like
    ///   macro's name followed there by anything but `(` is rejected, never
    ///   called (`#define D(m) m EMPTY()`); one ending it is called by a `(`
    ///   following it.
    /// - Every argument is expanded, in order, used or not; `__VA_ARGS__`
    ///   is substituted expanded next to `##` too (`#define P(...) x ##
    ///   __VA_ARGS__` makes `P(PAIR)` `x1, 2`), the others as they're given.
    /// - The commas of an argument substituted into the replacement, outside
    ///   its parentheses, separate no arguments of the calls there
    ///   (`#define F(...) G(__VA_ARGS__)` gives G one argument: Boost's
    ///   `BOOST_PP_EXPAND` takes them apart again, as an argument).
    /// - At the top (`Scan::Top`: the `#if` line or `#include` operand), a
    ///   replacement's expansion is scanned again with what follows,
    ///   rejected names included: `#define A F ECHO((1, 2))` calls F there
    ///   (and not in an argument).
    /// - A comma before an empty `__VA_ARGS__` (`##` between them or not)
    ///   goes at the end of the macro's expansion, unless a call in it took
    ///   it as an argument (`f(x, __VA_ARGS__)` is `f(x )`, `ECHO(x,
    ///   __VA_ARGS__)` is `x,`); `()` is no argument (`#x` of a missing one
    ///   is nothing); `__VA_OPT__` is no operator.
    /// - The tokens of an argument substituted next to `##` but not pasted
    ///   (`ID(c)` in `CAT_I(ID(c) x, y)`, or with an empty argument
    ///   `CAT_I(ID(c), )`) of a call that isn't at the top (in a
    ///   replacement or an argument) are frozen: expanded in the
    ///   replacement of the call at the top only (`Scan::TopReplacement`),
    ///   as tokens of that macro (`ID(CAT_I(ID(c),))` is `ID(c)`,
    ///   `ID(CAT_I(CAT_I(a, b),))` is `ab`); but a function-like macro's
    ///   name frozen is called by a `(` that isn't (`CAT_I(x, y F)(1)`),
    ///   unless its arguments are being expanded (`ID(CAT_I(x, y ID)(1))` is
    ///   `xy ID(1)`).
    /// - Pasting a token that isn't a parenthesis or a comma with a token
    ///   that isn't `)` or a comma keeps both (Boost's
    ///   `BOOST_PP_TUPLE_ELEM_O_3 ## (...)`).
    ///
    /// cl takes missing arguments as empty and drops extra ones (with a
    /// warning): those fail here, which tells this model's mistakes. So do
    /// what cl expands otherwise, in ways this model doesn't follow (cl
    /// 19.51, mostly without a warning; see the tests against it, and
    /// `macros::tests::TRADITIONAL_UNMODELLED`):
    /// - A parenthesis of an argument substituted without its pair in it
    ///   (`#define H(x) F(x)` and `H(1 RP + LP 0)`) delimits a call there
    ///   or not, depending on the rest of the replacement: one after a
    ///   function-like macro's name or among a call's arguments fails.
    /// - In an argument (`Scan::Argument`), the expansion of a call to a
    ///   macro with parameters ending with a function-like macro's name
    ///   followed by more of the argument (`ECHO(ID(F) + 1)`, `ECHO(ID(F),
    ///   2)`, an argument left out too): cl keeps a part of it only (`F`), or
    ///   another argument's.
    /// - Other pastes making no token (`STR(x) ## F(y)` doesn't call F), and
    ///   pastes of whitespace after an expansion to nothing (see
    ///   `XTok::gap`).
    /// - A frozen macro's name hidden where it thaws (its expansion done,
    ///   cl may expand it), or an operator of `#if` frozen.
    /// - A macro's name in an argument of its own but first, followed by
    ///   `(` where it's hidden (`AP(x AP)` for `#define AP(m) m(2)`): cl
    ///   expands it later, when it's not.
    /// - A comma before `__VA_ARGS__` expanding to nothing but given
    ///   (`F(x, NIL)`), or before a call whose replacement starts with such
    ///   a `__VA_ARGS__` (`ECHO(a, REM())`): cl drops it, but not always. A
    ///   comma before an empty `__VA_ARGS__` that a call takes starting an
    ///   argument, or in variadic arguments after others (`REST(, x,
    ///   __VA_ARGS__)`): cl drops it too.
    /// - `F()` for `#define F(x)` in an argument (cl warns, and may leave
    ///   the call around it).
    fn expand_traditional(&mut self, input: Vec<XTok>, scan: Scan) -> Vec<XTok> {
        let mut input: VecDeque<XTok> = input.into();
        let mut out: Vec<XTok> = Vec::new();
        // Where the tokens after whitespace after an expansion to nothing go.
        let mut gaps = Vec::new();
        while let Some(mut t) = input.pop_front() {
            if !self.spend() {
                break;
            }
            let thaws = matches!(scan, Scan::Top | Scan::TopReplacement);
            if t.frozen.is_some() && thaws {
                self.thaw(&mut t);
            }
            if t.t.kind != Kind::Ident {
                out.push(t);
                continue;
            }
            let name = t.t.sym;
            self.taint(name);
            if t.tail && scan == Scan::Argument && input.front().is_some_and(|n| !n.t.is(P::LParen)) {
                self.fail(format!("{} ends an expansion in an argument, followed by more", name.as_str()));
            }
            if t.frozen.is_some() {
                let operator = [intern::DEFINED, intern::HAS_INCLUDE, intern::HAS_INCLUDE_NEXT, intern::HAS_EMBED].contains(&name) || self.ctx.is_query(name);
                if self.if_mode && operator {
                    self.fail(format!("{} of an argument next to ## in #if", name.as_str()));
                }
                let called = input.front().is_some_and(|n| n.t.is(P::LParen) && n.frozen.is_none())
                    && !hidden(&t.hide, name)
                    && !self.owners.contains(&name)
                    && !t.rejected
                    && self.ctx.lookup(name).is_some_and(|m| m.params.is_some());
                if !called {
                    out.push(t);
                    continue;
                }
                t.frozen = None;
            }
            if t.own && !thaws && hidden(&t.hide, name) && input.front().is_some_and(|n| n.t.is(P::LParen)) {
                self.fail(format!("{} of its own argument hidden", name.as_str()));
            }
            let Some(m) = self.macro_at(&t, &mut input, &mut out) else { continue };
            let one = Some(Rc::new(vec![name]));
            let (args, hs) = match &m.params {
                None => (Args { toks: Vec::new(), given: 0 }, hide_union(&t.hide, &one)),
                Some(params) => match input.front() {
                    Some(n) if n.t.is(P::LParen) && n.glued => {
                        self.fail(format!("an argument's parenthesis after {}", name.as_str()));
                        out.push(t);
                        continue;
                    }
                    Some(n) if n.t.is(P::LParen) => {
                        let (mut args, rparen) = match traditional_arguments(&mut input, params.len(), m.variadic) {
                            Ok(a) => a,
                            Err(why) => {
                                self.fail(format!("{why} for {}", name.as_str()));
                                out.push(t);
                                continue;
                            }
                        };
                        if thaws {
                            for t in args.toks.iter_mut().flatten().filter(|t| t.frozen.is_some()) {
                                self.thaw(t);
                            }
                        }
                        // `F()` for `#define F(x)`: cl warns, and in an
                        // argument expands otherwise (`XSTR(F())`).
                        if args.given == 0 && params.len() == 1 && !m.variadic && !self.owners.is_empty() {
                            self.fail(format!("no argument for {} in an argument", name.as_str()));
                        }
                        (args, hide_union(&hide_intersection(&t.hide, &rparen), &one))
                    }
                    Some(_) => {
                        t.rejected = scan != Scan::Top;
                        out.push(t);
                        continue;
                    }
                    // Called, or rejected, by what follows the replacement.
                    None => {
                        out.push(t);
                        continue;
                    }
                },
            };
            if self.nesting == MAX_NESTING {
                self.fail("macro expansions nested too deeply");
                out.push(t);
                continue;
            }
            self.nesting += 1;
            let at_top = scan == Scan::Top;
            let top_hide = if at_top { std::mem::replace(&mut self.top_hide, hs.clone()) } else { None };
            let (replacement, va_nothing) = self.substitute_traditional(&m, &args, hs, t.t.space, !at_top);
            // `__VA_ARGS__` expanding to nothing starting the replacement:
            // cl drops a comma before the call (`ECHO(a, REM())` is `a`).
            let va_first = m.params.as_ref().is_some_and(|p| m.variadic && m.body.first().and_then(Token::ident) == p.last().copied());
            if va_first && va_nothing && (out.last().is_some_and(|p: &XTok| p.t.is(P::Comma)) || (out.is_empty() && scan != Scan::Top)) {
                self.fail(format!("{}'s variadic arguments expand to nothing after a comma", name.as_str()));
            }
            let expanded = self.expand_traditional(replacement, if at_top { Scan::TopReplacement } else { Scan::Replacement });
            if at_top {
                self.top_hide = top_hide;
            }
            self.nesting -= 1;
            // A function-like macro's name ending a call's expansion (not
            // `#define OBJ F`, nor `#define G() F`: those cl takes as the
            // standard does).
            let call = m.params.as_ref().is_some_and(|p| !p.is_empty());
            let mut last = true;
            for mut r in expanded.into_iter().rev().filter(|r| !r.elided) {
                r.glued = false;
                r.rejected &= scan != Scan::Top;
                if last && call && !r.rejected && r.t.kind == Kind::Ident && self.ctx.lookup(r.t.sym).is_some_and(|f| f.params.is_some()) {
                    r.tail = true;
                }
                last = false;
                input.push_front(r);
            }
            if last && input.front().is_some_and(|n| n.t.space) {
                gaps.push(out.len());
            }
        }
        for g in gaps {
            if let Some(t) = out.get_mut(g) {
                t.gap = true;
            }
        }
        out
    }

    /// A frozen token, read where it thaws (see `expand_traditional`): a
    /// token of the call at the top, with the hide set it had when frozen.
    /// One a macro's name hides fails (cl may expand it there).
    fn thaw(&mut self, t: &mut XTok) {
        let hide = t.frozen.take().unwrap_or_default();
        if t.t.kind == Kind::Ident && hidden(&hide, t.t.sym) && self.ctx.lookup(t.t.sym).is_some() {
            self.fail(format!("{} of an argument next to ## hidden", t.t.sym.as_str()));
        }
        t.hide = hide_union(&hide, &self.top_hide);
    }

    /// `substitute`, by cl's traditional preprocessor (see
    /// `expand_traditional`); `nested`, the call isn't at the top. And
    /// whether the variadic arguments expand to nothing.
    fn substitute_traditional(&mut self, m: &Macro, args: &Args, hs: Hide, space: bool, nested: bool) -> (Vec<XTok>, bool) {
        let params: &[Sym] = m.params.as_deref().unwrap_or(&[]);
        let param = |t: &Token| if m.params.is_some() { t.ident().and_then(|s| params.iter().position(|p| *p == s)) } else { None };
        let body = &m.body;
        let va_empty = m.variadic && args.toks.last().is_some_and(Vec::is_empty);
        // Each argument expanded, in order: cl expands all, those next to
        // `##` or `#` and those left out too (`SECOND(ID(F) +1, x)` shows
        // where it fails at one, see `expand_traditional`).
        self.owners.push(m.name);
        let expanded: Vec<Vec<XTok>> =
            args.toks.iter().map(|a| if a.is_empty() { Vec::new() } else { self.expand_traditional(a.clone(), Scan::Argument) }).collect();
        self.owners.pop();
        let mut va_checked = false;
        let mut items = Vec::new();
        let mut i = 0;
        while i < body.len() {
            let tok = body[i];
            if tok.is(P::Hash)
                && let Some(pi) = body.get(i + 1).and_then(param)
            {
                if pi < args.given {
                    items.push(Item::Tok(XTok::new(Token { space: tok.space, ..self.stringized(&args.toks[pi]) })));
                }
                i += 2;
                continue;
            }
            if tok.is(P::HashHash) {
                items.push(Item::Paste { gnu: gnu_comma(m, body, i) });
                i += 1;
                continue;
            }
            // A comma before an empty `__VA_ARGS__` (`##` between them or
            // not): gone from the expansion, unless a call in it takes it.
            let va_next = |at: usize| body.get(at).and_then(param) == Some(params.len().wrapping_sub(1));
            let before_va = tok.is(P::Comma) && m.variadic && (va_next(i + 1) || (body.get(i + 1).is_some_and(|t| t.is(P::HashHash)) && va_next(i + 2)));
            if before_va && va_empty {
                items.push(Item::Tok(XTok { elided: true, ..XTok::new(tok) }));
                i += 1;
                continue;
            }
            // cl drops it too when they expand to nothing, but not always
            // (`F(x, NIL)`, not `F(x, NIL NIL)`).
            if before_va && !va_checked && expanded.last().is_some_and(Vec::is_empty) {
                va_checked = true;
                self.fail(format!("{}'s variadic arguments expand to nothing after a comma", m.name.as_str()));
            }
            if let Some(pi) = param(&tok) {
                let after = matches!(items.last(), Some(Item::Paste { .. }));
                let before = body.get(i + 1).is_some_and(|t| t.is(P::HashHash));
                let pasted = after || before;
                // Next to `##` too, cl substitutes `__VA_ARGS__` expanded
                // (`#define P(...) x ## __VA_ARGS__` makes `P(PAIR)` `x1, 2`);
                // other arguments as they're given.
                let va = |p: usize| m.variadic && p + 1 == params.len();
                let raw = pasted && !va(pi);
                let empty = |p: usize| if va(p) { expanded[p].is_empty() } else { args.toks[p].is_empty() };
                if pasted && empty(pi) {
                    items.push(Item::Placemarker { va: false });
                } else {
                    let mut x = if raw { args.toks[pi].clone() } else { expanded[pi].clone() };
                    // An argument next to `##` of a call that isn't at the
                    // top: its tokens but those pasted with a token
                    // (not with an empty argument) are frozen.
                    if raw && nested {
                        let first_pasted = after && matches!(items.len().checked_sub(2).map(|k| &items[k]), Some(Item::Tok(_)));
                        let last_pasted = before && body.get(i + 2).is_some_and(|n| param(n).is_none_or(|p| !empty(p)));
                        let end = x.len() - 1;
                        for (k, t) in x.iter_mut().enumerate() {
                            let merged = (k == 0 && first_pasted) || (k == end && last_pasted);
                            if !merged && t.frozen.is_none() {
                                t.frozen = Some(t.hide.clone());
                            }
                        }
                    }
                    // The argument's pairs of parentheses: its commas
                    // outside them, and the parentheses without their pair,
                    // are glued.
                    let mut open = Vec::new();
                    let mut paired = vec![false; x.len()];
                    for (k, t) in x.iter().enumerate() {
                        if t.t.is(P::LParen) {
                            open.push(k);
                        } else if t.t.is(P::RParen)
                            && let Some(o) = open.pop()
                        {
                            (paired[o], paired[k]) = (true, true);
                        }
                    }
                    let mut depth = 0usize;
                    for (k, (t, paired)) in x.iter_mut().zip(paired).enumerate() {
                        let paren = t.t.is(P::LParen) || t.t.is(P::RParen);
                        if paired {
                            depth = if t.t.is(P::LParen) { depth + 1 } else { depth - 1 };
                        }
                        t.glued = (paren && !paired) || (depth == 0 && t.t.is(P::Comma));
                        t.tail = false;
                        // The macro's own name, not first (`AP(x AP)` for
                        // `#define AP(m) m(2)`, not `AP(AP)`).
                        t.own = !raw && t.t.sym == m.name && t.t.kind == Kind::Ident && (k > 0 || t.gap);
                    }
                    if let Some(first) = x.first_mut().filter(|_| !pasted) {
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
        let va_nothing = m.variadic && expanded.last().is_some_and(Vec::is_empty);
        (self.paste(items, hs, space), va_nothing)
    }

    /// The tokens of a substitution, `##` pasted, with the hide set `hs`;
    /// the first spaced as the macro's name was. A paste making no token
    /// (`+ ## x`), which GCC and Clang reject and cl's conforming
    /// preprocessor warns about, fails; cl's traditional one keeps both
    /// tokens, which this model follows where it can.
    fn paste(&mut self, items: Vec<Item>, hs: Hide, space: bool) -> Vec<XTok> {
        let mut pasted: Vec<Item> = Vec::new();
        let mut items = items.into_iter().peekable();
        while let Some(item) = items.next() {
            let Item::Paste { gnu } = item else {
                pasted.push(item);
                continue;
            };
            let Some(right) = items.next() else { break };
            let left = pasted.pop();
            if let (Some(Item::Tok(l)), Item::Tok(r)) = (&left, &right)
                && (l.t.kind == Kind::Unknown || r.t.kind == Kind::Unknown)
            {
                self.garbled = true;
            }
            match (left, right) {
                (None, r) => pasted.push(r),
                (Some(Item::Placemarker { .. }), r) => pasted.push(r),
                // GNU: `, ## __VA_ARGS__` without variadic arguments.
                (Some(Item::Tok(l)), Item::Placemarker { va: true }) if l.t.is(P::Comma) => {}
                (Some(l), Item::Placemarker { .. }) => pasted.push(l),
                // cl pastes whitespace first in b (`a ## b` is `a b`, see
                // `XTok::gap`), but not always.
                (Some(Item::Tok(l)), Item::Tok(r)) if r.gap => {
                    self.fail(format!("pasting {} and whitespace before {}", l.t.sym.as_str(), r.t.sym.as_str()));
                    pasted.push(Item::Tok(l));
                    pasted.push(Item::Tok(r));
                }
                (Some(Item::Tok(l)), Item::Tok(r)) => {
                    let text = format!("{}{}", l.t.sym.as_str(), r.t.sym.as_str());
                    let toks = lex::tokens(text.as_bytes());
                    if let [mut t] = toks[..] {
                        t.space = l.t.space;
                        pasted.push(Item::Tok(XTok::new(t)));
                    } else {
                        // GNU: `, ## __VA_ARGS__` with variadic arguments.
                        // cl's traditional preprocessor keeps both tokens
                        // (Boost's `BOOST_PP_TUPLE_ELEM_O_3 ## (...)`), but
                        // pasting a parenthesis or comma first, or a `)` or
                        // comma second, it expands otherwise (`STR(x) ## F(y)`
                        // doesn't call F).
                        let delimits = |t: &XTok, open: bool| t.t.is(P::RParen) || t.t.is(P::Comma) || (open && t.t.is(P::LParen));
                        let kept = (gnu && l.t.is(P::Comma)) || (self.traditional && !delimits(&l, true) && !delimits(&r, false));
                        if !kept {
                            self.fail(format!("pasting {} and {} makes no token", l.t.sym.as_str(), r.t.sym.as_str()));
                        }
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
}

/// What cl's traditional preprocessor expands (see
/// `Expander::expand_traditional`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scan {
    /// A line: text, an `#if` expression, an `#include` operand.
    Top,
    /// The replacement of a call at the top, by itself.
    TopReplacement,
    /// Another macro's replacement, by itself.
    Replacement,
    /// An argument, before it's substituted.
    Argument,
}

/// Whether the `##` at `i` of a macro's body is GNU's `, ## __VA_ARGS__`,
/// which keeps both when there are variadic arguments.
fn gnu_comma(m: &Macro, body: &[Token], i: usize) -> bool {
    m.variadic && i > 0 && body[i - 1].is(P::Comma) && body.get(i + 1).and_then(Token::ident) == m.params.as_deref().and_then(<[Sym]>::last).copied()
}

/// `( args )` of a function-like macro, as cl's traditional preprocessor
/// takes them (see `Expander::expand_traditional`): commas glued separate
/// none; and the hide set of `)`. The arguments are glued, rejected and
/// tails nowhere. An error for another number of arguments than the macro
/// takes (`()` is none, and the variadic ones may be left out), a
/// parenthesis glued, or a comma before an empty `__VA_ARGS__` starting
/// one.
fn traditional_arguments(input: &mut VecDeque<XTok>, nparams: usize, variadic: bool) -> Result<(Args, Hide), &'static str> {
    input.pop_front();
    let mut toks: Vec<Vec<XTok>> = Vec::new();
    let mut cur = Vec::new();
    let mut depth = 1;
    let rparen;
    loop {
        let Some(mut t) = input.pop_front() else { return Err("bad arguments") };
        if t.glued && (t.t.is(P::LParen) || t.t.is(P::RParen)) {
            return Err("an argument's parenthesis in the arguments");
        }
        if t.t.is(P::LParen) {
            depth += 1;
        } else if t.t.is(P::RParen) {
            depth -= 1;
            if depth == 0 {
                rparen = t.hide.clone();
                toks.push(cur);
                break;
            }
        } else if t.t.is(P::Comma) && depth == 1 && !t.glued && !(variadic && toks.len() + 1 == nparams) {
            toks.push(std::mem::take(&mut cur));
            continue;
        }
        // A comma before an empty `__VA_ARGS__` starting an argument, or in
        // the variadic ones after others (`#define R(...) REST(, x,
        // __VA_ARGS__)`, for `#define REST(a, ...) __VA_ARGS__`): cl drops it
        // there, or later (`R()` is `x`, but `ECHO(x, __VA_ARGS__)` keeps it).
        if t.elided && (cur.is_empty() || (variadic && nparams > 1 && toks.len() + 1 == nparams)) {
            return Err("a comma before an empty __VA_ARGS__ in an argument");
        }
        t.glued = false;
        t.rejected = false;
        t.elided = false;
        t.tail = false;
        cur.push(t);
    }
    if toks.len() == 1 && toks[0].is_empty() {
        toks.clear();
    }
    let given = toks.len();
    // The variadic arguments may be left out, as may the only argument
    // (`()` is none; for `(x)`, cl warns).
    let fine = given == nparams || (given == 0 && nparams == 1) || (variadic && given + 1 == nparams);
    if !fine {
        return Err("bad arguments");
    }
    toks.resize_with(nparams, Vec::new);
    Ok((Args { toks, given }, rparen))
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
            pragma_callees: self.pragma_callees.clone(),
            pragma_arg: self.pragma_arg,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Ctx {
        macros: HashMap<Sym, Arc<Macro>>,
        traditional: bool,
        va_comma: VaComma,
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
        fn traditional(&self) -> bool {
            self.traditional
        }
        fn va_comma(&self) -> VaComma {
            self.va_comma
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

    /// Tokens as one string, single spaces between them and none in string
    /// literals (whose spaces stringizing the expansion of an argument puts
    /// otherwise than cl).
    pub(crate) fn spelled(toks: &[Token]) -> String {
        toks.iter().map(|t| if t.kind == Kind::Str { t.sym.as_str().replace(' ', "") } else { t.sym.as_str().to_string() }).collect::<Vec<_>>().join(" ")
    }

    /// The tokens text expands to, by cl's traditional preprocessor or the
    /// standard's (see `spelled`), or the error.
    pub(crate) fn expanded(defs: &str, text: &str, traditional: bool, if_mode: bool) -> Result<String, String> {
        let mut c = Ctx { traditional, ..ctx(defs) };
        let mut x = Expander::new(&mut c, if_mode);
        let out = x.expand_tokens(&lex::tokens(text.as_bytes()));
        x.error.take().map_or_else(|| Ok(spelled(&out)), Err)
    }

    fn traditional(defs: &str, text: &str, if_mode: bool) -> Result<String, String> {
        expanded(defs, text, true, if_mode)
    }

    /// Cases of cl's traditional preprocessor, as `cl /E` expands them
    /// (cl 19.51; `driver::tests::real_cl_traditional` checks them against
    /// cl): macros (after `TRADITIONAL_DEFS`), text, its expansion.
    pub(crate) const TRADITIONAL_DEFS: &str = "#define F(x, y) [x|y]\n#define ECHO(...) __VA_ARGS__\n#define EXPAND(x) x\n#define PAIR 1, 2\n#define NIL\n#define EMPTYF()\n\
        #define STR(x) #x\n#define XSTR(x) STR(x)\n#define DEFER(m) m EMPTYF()\n#define CAT(a, b) CAT_I(a, b)\n#define CAT_I(a, b) a ## b\n";
    pub(crate) const TRADITIONAL_CASES: &[(&str, &str, &str)] = &[
        // An argument's commas separate no arguments where it's
        // substituted: any argument, `__VA_ARGS__` or not.
        ("#define G(x) F(x, 3)\n", "G(PAIR)", "[1, 2|3]"),
        ("#define V(...) F(__VA_ARGS__, 3)\n", "V(1, 2)", "[1, 2|3]"),
        ("#define V(a, ...) F(__VA_ARGS__, a)\n", "V(0, 1, 2)", "[1, 2|0]"),
        ("#define H(m, x) m(x, 0)\n", "H(F, PAIR)", "[1, 2|0]"),
        ("#define V(...) F(__VA_ARGS__ ## x, y)\n", "V(1, 2)", "[1, 2x|y]"),
        // But inside its parentheses they do; and taken as an argument
        // again, they do.
        ("#define K(x) F x\n", "K((11, 12))", "[11|12]"),
        ("#define H(m, x) m x\n", "H(F, (1, 2))", "[1|2]"),
        ("#define V(...) EXPAND(F(__VA_ARGS__))\n", "V(1, 2)", "[1|2]"),
        ("#define V(...) CAT(F(__VA_ARGS__),)\n", "V(1, 2)", "[1|2]"),
        ("#define V(...) ECHO(F(__VA_ARGS__))\n", "V(1, 2)", "[1|2]"),
        ("#define CNT(...) CNT_I(__VA_ARGS__, 3, 2, 1, 0)\n#define CNT_I(a, b, c, n, ...) n\n", "CNT(a, b) CNT(a) CNT()", "1 1 1"),
        ("#define CNT(...) ECHO(CNT_I(__VA_ARGS__, 3, 2, 1, 0))\n#define CNT_I(a, b, c, n, ...) n\n", "CNT(a, b) CNT(a) CNT()", "2 1 1"),
        // A name ending a replacement is called by what follows it,
        // with the commas of that replacement's arguments glued.
        ("#define V(...) ECHO(F)(__VA_ARGS__, 3)\n", "V(1, 2)", "[1, 2|3]"),
        ("#define OBJ F\n#define V(x) OBJ(x, 3)\n", "V(PAIR)", "[1, 2|3]"),
        ("#define V(x) ECHO(ECHO(F))(x, 3)\n", "V(PAIR)", "[1, 2|3]"),
        // One followed by anything else is rejected, but where it's
        // scanned again at the top (where they're not glued).
        ("#define V(x) DEFER(F)(x)\n", "V(PAIR)", "[1|2]"),
        ("#define V(x) OBJ NIL (x)\n#define OBJ F\n", "V(PAIR)", "[1|2]"),
        ("#define V(x) ECHO(F NIL)(x)\n", "V(PAIR)", "[1|2]"),
        ("#define A F ECHO((1, 2))\n", "A", "[1|2]"),
        ("#define A F NIL (1, 2)\n", "A", "[1|2]"),
        ("#define LP (\n#define A F LP 1, 2)\n", "A XSTR(A)", "[1|2] \"F ( 1, 2)\""),
        ("#define IMPL1(p, v) one(p, v)\n#define DO(s, b) CAT(IMPL, s) ECHO((\"Hello\", b))\n", "DO(1, \"World\")", "one(\"Hello\", \"World\")"),
        ("", "DEFER(F)(1, 2) F NIL (1, 2) XSTR(DEFER(F)(1, 2))", "[1|2] F (1, 2) \"F (1, 2)\""),
        ("#define T5 F EMPTYF NIL () (1, 2)\n", "XSTR(T5) T5", "\"F EMPTYF () (1, 2)\" F (1, 2)"),
        ("#define J F ECHO\n", "J((1, 2))", "F (1, 2)"),
        // Rejected in an argument: in what it's substituted into too,
        // unless taken as an argument again.
        ("#define AP(x) x(1, 2)\n", "XSTR(AP(F NIL))", "\"F (1, 2)\""),
        ("#define AQ(x) ECHO(x)(1, 2)\n", "XSTR(AQ(F NIL))", "\"[1|2]\""),
        // A comma before an empty `__VA_ARGS__` goes at the end of the
        // expansion: not one a call there takes.
        ("#define E(fmt, ...) f(fmt, __VA_ARGS__)\n", "E(x) E(x,) E(x, y)", "f(x) f(x) f(x, y)"),
        ("#define E(a, ...) g(a, __VA_ARGS__, __VA_ARGS__) h(a,, __VA_ARGS__) k(a, (__VA_ARGS__))\n", "E(x)", "g(x) h(x,) k(x, ())"),
        ("#define E(...) g(__VA_ARGS__, end)\n", "E()", "g(, end)"),
        ("#define E(fmt, ...) f(fmt, ## __VA_ARGS__)\n", "E(x) E(x, y)", "f(x) f(x,y)"),
        (
            "#define SHOW(...) [__VA_ARGS__]\n#define A1(...) SHOW(x, ## __VA_ARGS__, 3)\n#define A2(...) SHOW(x, __VA_ARGS__, 3)\n#define A3(...) SHOW(x, __VA_ARGS__)\n",
            "A1() A2() A3()",
            "[x,, 3] [x, , 3] [x,]",
        ),
        (
            "#define SHOW(...) [__VA_ARGS__]\n#define A12(...) DEFER(SHOW)(x, __VA_ARGS__)\n#define A13(...) x, __VA_ARGS__\n#define A14(...) ECHO(x, __VA_ARGS__)\n\
             #define A15(...) EXPAND(f(x, __VA_ARGS__))\n#define A16(...) (x, __VA_ARGS__)\n#define A17(...) SHOW A16(__VA_ARGS__)\n#define A18(...) x, ## __VA_ARGS__\n",
            "A12() SHOW(A13()) A13() A14() A15() A17() SHOW(A18()) A18()",
            "[x] [x] x x, f(x, ) [x] [x] x",
        ),
        (
            "#define ARGC_I(a, b, c, d, n, ...) n\n#define ARGC(...) ARGC_I(x, ## __VA_ARGS__, 3, 2, 1, 0)\n#define BARE(...) ARGC_I(x, __VA_ARGS__, 3, 2, 1, 0)\n",
            "ARGC() BARE() ARGC(y) BARE(y)",
            "1 1 1 1",
        ),
        // `()` is no argument: `#x` is then nothing.
        ("#define S1(...) #__VA_ARGS__\n#define S2(x, y) #x #y\n", "S1() S1(,) S2(,) S2(a,)", "\",\" \"\" \"\" \"a\" \"\""),
        ("#define V(a, ...) <a #__VA_ARGS__>\n", "V(x) V(x,) V(x, y)", "<x> <x \"\"> <x \"y\">"),
        ("#define V(...) [__VA_ARGS__]\n", "V() V(,)", "[] [,]"),
        // No `__VA_OPT__`.
        ("#define VO(a, ...) f(a __VA_OPT__(,) __VA_ARGS__)\n", "VO(x, y)", "f(x __VA_OPT__(,) y)"),
        // As the standard's otherwise.
        ("#define f(x) x f\n#define foo foo\n", "f(1)(2) foo", "1 f(2) foo"),
        (
            "#define BOOST_PP_CAT(a, b) BOOST_PP_CAT_I(a, b)\n#define BOOST_PP_CAT_I(a, b) BOOST_PP_CAT_II(~, a ## b)\n#define BOOST_PP_CAT_II(p, res) res\n#define AB(x) <ab x>\n",
            "BOOST_PP_CAT(A, B)(3) BOOST_PP_CAT(F, (1, 2))",
            "<ab 3> [1|2]",
        ),
        // Pasting what makes no token keeps both (Boost's
        // `BOOST_PP_TUPLE_ELEM_II(m, args)`, `BOOST_PP_CAT(m ## args,)`).
        ("#define PC(a, b) a ## b\n#define G1(x) <x>\n", "PC(G1, (3)) PC(x G1, (3)) PC(+, 1) PC(1, +)", "<3> x <3> +1 1+"),
        ("#define TE(m, args) CAT(m ## args,)\n#define M2(a, b) [a b]\n", "TE(M2, (1, 2))", "[1 2]"),
        // The rest of an argument next to `##`, in a call that isn't at
        // the top, is expanded in the replacement of the call at the top,
        // as its tokens; at the top, in the replacement of that call.
        (
            "#define PAST(a, b) a ## b\n",
            "EXPAND(PAST(EXPAND(c) NIL, )) PAST(EXPAND(c) NIL, ) EXPAND(PAST(PAST(a, b) NIL, )) EXPAND(PAST(, EXPAND(c)))",
            "EXPAND(c) c ab EXPAND(c)",
        ),
        ("", "CAT_I(CAT_I(a, b) NIL, ) EXPAND(CAT_I(CAT_I(a, b) NIL, ))", "CAT_I(a, b) ab"),
        ("#define PAST(a, b) a ## b\n#define SHOW_X(a, b) [a|b]\n", "EXPAND(PAST(SHOW_, X(1, EXPAND(c)))) PAST(SHOW_, X(1, EXPAND(c)))", "[1|EXPAND(c)] [1|c]"),
        (
            "#define PAST(a, b) a ## b\n#define R6 PAST(EXPAND(c) NIL, )\n#define R7 EXPAND(R6)\n",
            "R7 ECHO(R6) EXPAND(R6) EXPAND(ECHO(R6))",
            "c c EXPAND(c) EXPAND(c)",
        ),
        (
            "#define IIF(c) CAT(IIF_, c)\n#define WHEN(c) IIF(c)(EXPAND, EAT)\n#define EAT(...)\n#define R3 CAT_I(WHEN(G1), )\n",
            "CAT_I(WHEN(G1), ) R3 ECHO(CAT_I(WHEN(G1), ))",
            "CAT_I(IIF_, G1)(EXPAND, EAT) IIF_G1(EXPAND, EAT) IIF_G1(EXPAND, EAT)",
        ),
        // A macro's name hidden for good: in a replacement (but in an
        // argument of its own, see `TRADITIONAL_UNMODELLED`).
        (
            "#define OUT(x) IN(x)\n#define IN(y) y(5)\n#define G1(x) <x>\n#define G2(x) G1 G2 (x)\n",
            "EXPAND(OUT(z OUT)) EXPAND(OUT(G1 OUT)) ECHO(G2(1))",
            "z OUT(5) G1 OUT(5) G1 G2 (1)",
        ),
        // A frozen name called by a `(` that isn't, unless its arguments
        // are being expanded.
        (
            "#define PAST(a, b) a ## b\n#define G7(x) PAST(a, z x)(c)\n",
            "EXPAND(G7(EXPAND)) EXPAND(PAST(a, z EXPAND)(c)) ECHO(G7(EXPAND))",
            "az EXPAND(c) az EXPAND(c) az c",
        ),
        // `__VA_ARGS__` expanded next to `##` too (GNU's `, ##
        // __VA_ARGS__` or not).
        ("#define VGX(a, ...) f(a, ## __VA_ARGS__)\n", "VGX(, VGX(q)) VGX(x, y EXPAND(c))", "f(,f(q )) f(x,y c)"),
        ("#define PV(...) x ## __VA_ARGS__\n#define PV2(a, b) a ## b\n", "PV(PAIR) PV() PV2(x, PAIR)", "x1, 2 x xPAIR"),
    ];

    /// Calls cl warns about (it takes missing arguments as empty and drops
    /// extra ones: `G(PAIR)` is `[1, 2|]`), which fail here: the macros
    /// (after `TRADITIONAL_DEFS`) and the calls.
    pub(crate) const TRADITIONAL_REFUSED: (&str, &[&str]) = (
        "#define G(x) F(x)\n#define V(...) F(__VA_ARGS__)\n#define W(x, y) F(x y)\n#define COMMA ,\n",
        &["F(1)", "F(1, 2, 3)", "F()", "EXPAND(1, 2)", "ECHO(EXPAND())", "G(PAIR)", "V(1, 2)", "W(1 COMMA, 2)"],
    );

    /// What cl expands without a warning, but otherwise than this model,
    /// which fails there (see `Expander::expand_traditional`): macros
    /// (after `TRADITIONAL_DEFS`), text, and cl's expansion (cl 19.51;
    /// `driver::tests::real_cl_traditional` checks it).
    pub(crate) const TRADITIONAL_UNMODELLED: &[(&str, &str, &str)] = &[
        // A parenthesis of an argument without its pair there.
        ("#define LP (\n#define RP )\n#define G1(a) [a]\n#define H(x) G1(x)\n", "H(1 RP + LP 0)", "[1 ) + ( 0]"),
        ("#define RP )\n#define G1(a) [a]\n#define H8(x) G1(1 x\n", "H8(RP) 2)", "[1] 2)"),
        ("#define LP (\n#define G1(a) [a]\n#define H7(x) G1 x 1)\n", "H7(LP)", "[1]"),
        // A call's expansion ending with a function-like macro's name,
        // followed by more of an argument, given or left out.
        ("#define ID(x) x\n#define G1(x) <x>\n", "ECHO(ID(G1) + 1)", "G1"),
        ("#define ID(x) x\n#define G1(x) <x>\n", "ECHO(1, ID(G1) 2)", "G1"),
        ("#define G1(x) <x>\n#define GF(x) x G1\n", "ECHO(GF(1) + 1)", "1"),
        ("#define ID(x) x\n#define G1(x) <x>\n#define SECOND(a, b) b\n", "SECOND(ID(G1) + 1, y)", "G1"),
        // Its name frozen first (`CALL`, called by WHEN's `(`).
        (
            "#define SHOW(...) [__VA_ARGS__]\n#define NARG_I(a, b, c, d, n, ...) n\n#define XNARG(...) ECHO(NARG_I(__VA_ARGS__, 4, 3, 2, 1, 0))\n\
             #define IIF(c) CAT(IIF_, c)\n#define WHEN(c) IIF(c)(EXPAND, EAT)\n#define EAT(...)\n#define HEAD_I(x) x EAT\n#define CALL(m, ...) m(__VA_ARGS__)\n",
            "SHOW(XNARG(WHEN(HEAD_I CALL) DEFER, ))",
            "[EAT]",
        ),
        // A paste making no token after a call (F isn't called).
        ("#define G1(x) <x>\n#define SP2(b) STR(q) ## b\n", "SP2(G1(2))", "\"q\"G1(2)"),
        // A macro's name in its own argument (but first), hidden there.
        ("#define AP2(m) m(2)\n#define G1(x) <x>\n", "EXPAND(AP2(G1 AP2))", "G1 2(2)"),
        ("#define AP2(m) m(2)\n", "EXPAND(AP2(x AP2))", "x 2(2)"),
        // A macro's name pasted in an argument, hidden where it thaws.
        ("#define PAST(a, b) a ## b\n#define R8 PAST(R8 x, )\n", "ECHO(R8)", "R8 x x"),
        // Variadic arguments expanding to nothing after a comma.
        ("#define REM(...) __VA_ARGS__\n", "ECHO(a, REM())", "a"),
        ("#define E2(fmt, ...) f(fmt, __VA_ARGS__)\n", "E2(x, NIL)", "f(x )"),
        // One empty taken in variadic arguments after others.
        ("#define REST(a, ...) __VA_ARGS__\n#define A22(...) REST(, x, __VA_ARGS__)\n", "A22()", "x"),
        // Whitespace after an expansion to nothing, pasted.
        ("", "CAT(x, EMPTYF() 1)", "x 1"),
        ("#define IIF(c) CAT(IIF_, c)\n", "IIF(EMPTYF() 1)", "IIF_ 1"),
    ];

    /// cl's traditional preprocessor, as `cl /E` expands these (cl 19.51;
    /// `driver::tests::real_cl_traditional` checks them against cl).
    #[test]
    fn traditional_preprocessor() {
        let spaced = |s: &str| spelled(&lex::tokens(s.as_bytes()));
        for (more, text, want) in TRADITIONAL_CASES {
            assert_eq!(traditional(&format!("{TRADITIONAL_DEFS}{more}"), text, false), Ok(spaced(want)), "{more}{text}");
        }
        let (more, refused) = TRADITIONAL_REFUSED;
        for text in refused {
            assert!(traditional(&format!("{TRADITIONAL_DEFS}{more}"), text, false).is_err(), "{text}");
        }
        for (more, text, _) in TRADITIONAL_UNMODELLED {
            assert!(traditional(&format!("{TRADITIONAL_DEFS}{more}"), text, false).is_err(), "{more}{text}");
        }
        // In `#if`.
        let defs = "#define G(x, y) x + y\n#define ECHO(...) __VA_ARGS__\n#define EMPTYF()\n#define DEFER(m) m EMPTYF()\n#define PAIR 1, 2\n";
        assert_eq!(traditional(&format!("{defs}#define G1 G ECHO((1, 2))\n"), "G1 == 3", true), Ok(spaced("1 + 2 == 3")));
        assert_eq!(traditional(&format!("{defs}#define Q(x) DEFER(G)(x)\n"), "Q(PAIR) == 3", true), Ok(spaced("1 + 2 == 3")));
        // What cl evaluates otherwise (1, `F1`, `F1`, `10 + 2`): see
        // `TRADITIONAL_UNMODELLED`.
        let defs = "#define LP (\n#define RP )\n#define F(a) (a)*2\n#define H(x) F(x)\n#define ECHO(...) __VA_ARGS__\n#define ID(x) x\n#define F1(x) [x]\n\
            #define SECOND(a, b) b\n#define CAT(a, b) CAT_I(a, b)\n#define CAT_I(a, b) a ## b\n#define NIL\n#define V_ 10 +\n#define V_2 7\n";
        for text in ["H(1 RP + LP 0) == 2", "ECHO(ID(F1) + 1) == 1", "SECOND(ID(F1) + 1, 1) == 1", "CAT(V_, NIL 2) == 12"] {
            assert!(traditional(defs, text, true).is_err(), "{text}");
        }
        // `F()` for `#define F(x)` in an argument (cl warns, and leaves
        // XSTR: `XSTR(<>)`); an operator of `#if` in an argument next to
        // `##` (expanded later).
        let defs = "#define F1(x) <x>\n#define STR(x) #x\n#define XSTR(x) STR(x)\n#define PAST(a, b) a ## b\n#define ID(x) x\n";
        assert!(traditional(defs, "XSTR(F1())", false).is_err());
        assert_eq!(traditional(defs, "F1()", false), Ok(spaced("<>")));
        assert!(traditional(defs, "ID(PAST(defined(X) Y, ))", true).is_err());
        // An argument's parenthesis without its pair delimiting an
        // operator's operand.
        assert!(traditional("#define RP )\n#define D(x) defined(x)\n", "D(A RP)", true).is_err());
        // Pastes making no token: GCC and Clang reject them, cl's
        // conforming preprocessor warns; not GNU's `, ## __VA_ARGS__`.
        let defs = "#define CAT_I(a, b) a ## b\n#define ONE 1\n#define G(a, ...) g(a, ## __VA_ARGS__)\n";
        assert!(expanded(defs, "CAT_I(+, ONE)", false, true).is_err());
        assert_eq!(expanded(defs, "CAT_I(+, ONE)", true, true), Ok(spaced("+ 1")));
        assert_eq!(expanded(defs, "G(x, y) G(x)", false, false), Ok(spaced("g(x, y) g(x)")));
        // Nested too deeply: an error, not the stack.
        let deep: String = (0..300).map(|i| format!("#define D{i}(x) D{}(x)\n", i + 1)).collect();
        assert!(traditional(&deep, "D0(1)", false).unwrap_err().contains("nested"));
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

    /// `_Pragma`'s operand (C11 6.10.9): one quote off each end (and the
    /// prefix), then `\"` and `\\` unescaped; a quote escaped at the end
    /// stays.
    #[test]
    fn destringized() {
        for (lit, text) in [
            (r#""a\"b""#, r#"a"b"#),
            (r#""GCC dependency \"f\"""#, r#"GCC dependency "f""#),
            (r#"L"push_macro(\"M\")""#, r#"push_macro("M")"#),
            (r#"u8"x\\""#, r"x\"),
            (r#""\\\"""#, r#"\""#),
            (r#""a\nb""#, r"a\nb"),
            (r#""""#, ""),
        ] {
            assert_eq!(destringize(lit), text, "{lit}");
        }
    }

    /// The comma before `__VA_ARGS__` left out, as GCC, Clang and cl drop
    /// it (GCC 13, Clang 19 and 22, cl 19.51; the differential cases
    /// `va_comma*` check them): not before one given empty (`G(a,)`).
    #[test]
    fn va_comma_rules() {
        let defs = "#define E(...) g(x, __VA_ARGS__)\n#define F(a, ...) g(a, __VA_ARGS__)\n#define G(a, ...) g(a, ## __VA_ARGS__)\n#define H(...) g(x, ## __VA_ARGS__)\n";
        let text = "E() F(a) F(a,) G(a) G(a,) H() H(y)";
        let with = |va_comma: VaComma| {
            let mut c = Ctx { va_comma, ..ctx(defs) };
            let mut x = Expander::new(&mut c, false);
            spelled(&x.expand_tokens(&lex::tokens(text.as_bytes())))
        };
        let spaced = |s: &str| spelled(&lex::tokens(s.as_bytes()));
        // GCC's and Clang's GNU modes, cl's conforming preprocessor.
        assert_eq!(with(VaComma::default()), spaced("g(x,) g(a,) g(a,) g(a) g(a,) g(x) g(x,y)"));
        // GCC's strict modes, Clang's for C.
        assert_eq!(with(VaComma { needs_named: true, bare: false }), spaced("g(x,) g(a,) g(a,) g(a) g(a,) g(x,) g(x,y)"));
        // Clang's MSVC compatibility (clang-cl).
        assert_eq!(with(VaComma { needs_named: false, bare: true }), spaced("g(x) g(a) g(a,) g(a) g(a,) g(x) g(x,y)"));
        assert_eq!(with(VaComma { needs_named: true, bare: true }), spaced("g(x,) g(a) g(a,) g(a) g(a,) g(x,) g(x,y)"));
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
        // Words given to what isn't a macro.
        assert_eq!(of("#define CHECK(o) if (once) o\n"), Pragmas::None);
        assert_eq!(of("#define P DO_PRAGMA(once)\n"), Pragmas::None);
        // A TODO message with the place: not a pragma's words, whatever
        // they are where it's used.
        let todo = "#define STR(x) #x\n#define XSTR(x) STR(x)\n";
        assert_eq!(of(&format!("{todo}#define TODO(m) _Pragma(XSTR(message(__FILE__ \":\" XSTR(__LINE__) \": \" m)))\n")), Pragmas::None);
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
            "#define DO_PRAGMA(x) _Pragma(#x)\n#define P DO_PRAGMA(once)\n",
            "#define PRAGMA(x) _Pragma(x)\n#define P PRAGMA(\"once\")\n",
            "#define P2(a, b) b\n#define P P2(x, once)\n",
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
