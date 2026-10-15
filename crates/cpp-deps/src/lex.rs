//! Lexing a file down to its directives.
//!
//! Only directive lines matter for dependencies, so the lexer tokenizes
//! those and skips everything else. Skipping must still be exact about what
//! hides a `#`: block comments (also across lines), line comments, string
//! and character literals, raw string literals (across lines), digit
//! separators (`1'000` is no character literal) and line splices
//! (backslash-newline). Lines end with a newline, a carriage return and a
//! newline, or a carriage return alone (as GCC and Clang read them). The
//! result is cached per file version and shared by every compile that
//! includes the file.

use std::sync::{Arc, OnceLock};

use crate::intern::{self, Sym};
use crate::macros::{self, Macro};

/// Punctuators the preprocessor distinguishes; everything else is `Other`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum P {
    LParen,
    RParen,
    Comma,
    Hash,
    HashHash,
    Ellipsis,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    Ne,
    Not,
    Tilde,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Shl,
    Shr,
    Amp,
    AndAnd,
    Pipe,
    OrOr,
    Caret,
    Question,
    Colon,
    ColonColon,
    Dot,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ident,
    Number,
    Char,
    Str,
    /// `<...>` after `#include` (its contents, without the brackets).
    HeaderName,
    Punct(P),
    /// A byte that is no token of its own (e.g. a stray `\` or `@`).
    Other,
    /// Placeholder during `##` pasting (an empty macro argument).
    Placemarker,
    /// A value the expression evaluator can't know (an unanswered query).
    Unknown,
}

/// A preprocessing token: its kind, spelling and whether whitespace came
/// before it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    pub sym: Sym,
    pub space: bool,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", if self.space { " " } else { "" }, self.sym.as_str())
    }
}

impl Token {
    pub fn is(&self, p: P) -> bool {
        self.kind == Kind::Punct(p)
    }
    pub fn ident(&self) -> Option<Sym> {
        (self.kind == Kind::Ident).then_some(self.sym)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirKind {
    Define,
    Undef,
    Include,
    IncludeNext,
    Import,
    /// C23's `#embed` (the file is read as data).
    Embed,
    If,
    Ifdef,
    Ifndef,
    Elif,
    Elifdef,
    Elifndef,
    Else,
    Endif,
    /// `#pragma`, and `_Pragma("...")` operators written in text (a line of
    /// text that may run one through a macro is marked with a `_Pragma`
    /// token alone).
    Pragma,
    /// `#line`, and GNU's line markers (`# 33 "file"`).
    Line,
    /// `#error`, `#warning`, `#ident`, unknown directives, `#`.
    Other,
}

#[derive(Debug)]
pub struct Directive {
    pub kind: DirKind,
    /// The tokens after the directive's name (for `#include <x>`, one
    /// `HeaderName`).
    pub toks: Box<[Token]>,
    pub line: u32,
    /// For `#define`: the parsed macro (None when malformed).
    pub mac: Option<Arc<Macro>>,
}

#[derive(Debug, Default)]
pub struct FileDirectives {
    pub dirs: Vec<Directive>,
    /// The include guard: the whole file is `#ifndef G` ... `#endif` with
    /// nothing outside, so including it again while `G` is defined does
    /// nothing.
    pub guard: Option<Sym>,
    /// The file is UTF-16 (a byte order mark), which compilers like cl
    /// read, and this lexer doesn't: its directives are unknown.
    pub utf16: bool,
    /// A carriage return not followed by a newline ends a line here, as in
    /// GCC and Clang; cl reads such files otherwise.
    pub lone_cr: bool,
    /// The file has a trigraph (`??=`, `??/`...), which this lexer doesn't
    /// replace: its directives are unknown when trigraphs are on.
    pub trigraphs: bool,
    /// A line of text is an assembler's `.include` or `.incbin` (in `.S`
    /// files), which reads a file the preprocessor doesn't see.
    pub asm_include: bool,
    /// A hash of the file's bytes, which tells files with the same
    /// contents (with their sizes) without reading them again (see
    /// `#pragma once` in `scan`).
    pub hash: u64,
    /// The names text may call (see `name_hash`), sorted: text isn't
    /// expanded, but a call of a macro whose pragma its arguments make is
    /// seen (see `Context::arg_pragma`).
    pub calls: Box<[u64]>,
    /// The tokens of the text, when a scan looks at such a call: read
    /// again then (rarely), once (None: the file changed since).
    pub text: OnceLock<Option<Box<[Token]>>>,
}

/// A name's hash (see `FileDirectives::calls`).
pub(crate) fn name_hash(name: &[u8]) -> u64 {
    // FNV-1a.
    name.iter().fold(0xcbf2_9ce4_8422_2325, |h, &c| (h ^ u64::from(c)).wrapping_mul(0x0100_0000_01b3))
}

/// Adds the hashes of the names a line of text may call to `out`: those
/// followed by `(`, or ending the line (the call may go on on the next).
/// (Names in its comments and strings too.)
fn calls(text: &[u8], out: &mut Vec<u64>) {
    let mut i = 0;
    while i < text.len() {
        if !is_ident_start(text[i]) || (i > 0 && is_ident_char(text[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < text.len() && is_ident_char(text[i]) {
            i += 1;
        }
        let mut j = i;
        loop {
            match text.get(j) {
                Some(b' ' | b'\t' | b'\r' | b'\n' | b'\\') => j += 1,
                Some(b'(') | None => {
                    out.push(name_hash(&text[start..i]));
                    break;
                }
                Some(_) => break,
            }
        }
    }
}

/// A hash of a file's bytes (see `FileDirectives::hash`).
pub(crate) fn content_hash(content: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::hash::DefaultHasher::new();
    h.write(content);
    h.finish()
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

const PUNCTS: &[(&str, P)] = &[
    ("%:%:", P::HashHash),
    ("...", P::Ellipsis),
    ("<<=", P::Other),
    (">>=", P::Other),
    ("->*", P::Other),
    ("<=>", P::Other),
    ("##", P::HashHash),
    ("%:", P::Hash),
    ("<:", P::Other),
    (":>", P::Other),
    ("<%", P::Other),
    ("%>", P::Other),
    ("::", P::ColonColon),
    (".*", P::Other),
    ("->", P::Other),
    ("++", P::Other),
    ("--", P::Other),
    ("<<", P::Shl),
    (">>", P::Shr),
    ("<=", P::Le),
    (">=", P::Ge),
    ("==", P::EqEq),
    ("!=", P::Ne),
    ("&&", P::AndAnd),
    ("||", P::OrOr),
    ("*=", P::Other),
    ("/=", P::Other),
    ("%=", P::Other),
    ("+=", P::Other),
    ("-=", P::Other),
    ("&=", P::Other),
    ("^=", P::Other),
    ("|=", P::Other),
    ("(", P::LParen),
    (")", P::RParen),
    (",", P::Comma),
    ("#", P::Hash),
    ("<", P::Lt),
    (">", P::Gt),
    ("!", P::Not),
    ("~", P::Tilde),
    ("+", P::Plus),
    ("-", P::Minus),
    ("*", P::Star),
    ("/", P::Slash),
    ("%", P::Percent),
    ("&", P::Amp),
    ("|", P::Pipe),
    ("^", P::Caret),
    ("?", P::Question),
    (":", P::Colon),
    (".", P::Dot),
    ("=", P::Other),
    ("[", P::Other),
    ("]", P::Other),
    ("{", P::Other),
    ("}", P::Other),
    (";", P::Other),
];

struct Lexer<'a> {
    b: &'a [u8],
    pos: usize,
    /// Newlines counted up to `line_pos`.
    line: u32,
    line_pos: usize,
}

impl<'a> Lexer<'a> {
    /// The position after any line splices at `p`.
    fn splice(&self, mut p: usize) -> usize {
        let b = self.b;
        while p < b.len() && b[p] == b'\\' {
            let mut q = p + 1;
            while q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                q += 1;
            }
            if q < b.len() && b[q] == b'\n' {
                p = q + 1;
            } else if q + 1 < b.len() && b[q] == b'\r' && b[q + 1] == b'\n' {
                p = q + 2;
            } else {
                break;
            }
        }
        p
    }

    fn peek(&mut self) -> Option<u8> {
        self.pos = self.splice(self.pos);
        self.b.get(self.pos).copied()
    }

    /// The character after the current one (splices skipped).
    fn peek2(&mut self) -> Option<u8> {
        self.pos = self.splice(self.pos);
        let p = self.splice(self.pos + 1);
        self.b.get(p).copied()
    }

    fn bump(&mut self) {
        self.pos = self.splice(self.pos) + 1;
    }

    fn line_at(&mut self, p: usize) -> u32 {
        if p >= self.line_pos {
            self.line += self.b[self.line_pos..p].iter().filter(|&&c| c == b'\n').count() as u32;
            self.line_pos = p;
        }
        self.line + 1
    }

    /// Skips a block comment starting at `/*` (the position of `/`).
    fn skip_block_comment(&mut self) {
        self.bump();
        self.bump();
        let b = self.b;
        while self.pos < b.len() {
            if b[self.pos] == b'*' {
                let p = self.splice(self.pos + 1);
                if p < b.len() && b[p] == b'/' {
                    self.pos = p + 1;
                    return;
                }
            }
            self.pos += 1;
        }
    }

    /// Skips a line comment, up to (not including) the newline ending it.
    fn skip_line_comment(&mut self) {
        let b = self.b;
        while self.pos < b.len() {
            match b[self.pos] {
                b'\n' => return,
                b'\\' => {
                    let p = self.splice(self.pos);
                    self.pos = if p == self.pos { p + 1 } else { p };
                }
                _ => self.pos += 1,
            }
        }
    }

    /// Skips spaces and comments within a line. Returns whether any were
    /// skipped.
    fn skip_space(&mut self) -> bool {
        let start = self.pos;
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | 0x0b | 0x0c) => self.bump(),
                Some(b'/') => match self.peek2() {
                    Some(b'*') => self.skip_block_comment(),
                    Some(b'/') => self.skip_line_comment(),
                    _ => break,
                },
                _ => break,
            }
        }
        self.pos != start
    }

    /// Whether the `"` at `q` starts a raw string literal: the identifier
    /// right before it is a raw prefix.
    fn raw_prefix(&self, q: usize) -> bool {
        let mut s = q;
        while s > 0 && is_ident_char(self.b[s - 1]) {
            s -= 1;
        }
        matches!(&self.b[s..q], b"R" | b"u8R" | b"uR" | b"UR" | b"LR")
    }

    /// Whether the `'` at `q` is a digit separator: the token before it is
    /// a number.
    fn digit_separator(&self, q: usize) -> bool {
        let mut s = q;
        while s > 0 && (is_ident_char(self.b[s - 1]) || self.b[s - 1] == b'.' || self.b[s - 1] == b'\'') {
            s -= 1;
        }
        s < q && (self.b[s].is_ascii_digit() || (self.b[s] == b'.' && s + 1 < q && self.b[s + 1].is_ascii_digit()))
    }

    /// Skips a raw string literal whose `"` is at `self.pos`. False (nothing
    /// skipped) when the delimiter is malformed.
    fn skip_raw_string(&mut self) -> bool {
        let b = self.b;
        let start = self.pos + 1;
        let mut p = start;
        while p < b.len() && p - start <= 16 && !matches!(b[p], b'(' | b')' | b'\\' | b' ' | b'\t' | b'\n' | b'"') {
            p += 1;
        }
        if p >= b.len() || b[p] != b'(' {
            return false;
        }
        let delim = &b[start..p];
        let mut q = p + 1;
        while q < b.len() {
            if b[q] == b')' && b[q + 1..].starts_with(delim) && b.get(q + 1 + delim.len()) == Some(&b'"') {
                self.pos = q + 2 + delim.len();
                return true;
            }
            q += 1;
        }
        self.pos = b.len();
        true
    }

    /// Skips a string or character literal whose quote is at `self.pos`;
    /// an unterminated one ends at the end of the line.
    fn skip_quoted(&mut self, quote: u8) {
        let b = self.b;
        self.pos += 1;
        while self.pos < b.len() {
            match b[self.pos] {
                b'\\' => {
                    let p = self.splice(self.pos);
                    self.pos = if p == self.pos { p + 2 } else { p };
                }
                b'\n' => return,
                c if c == quote => {
                    self.pos += 1;
                    return;
                }
                _ => self.pos += 1,
            }
        }
    }

    /// Skips the rest of a line of text, and its newline.
    fn skip_text_line(&mut self) {
        let b = self.b;
        while self.pos < b.len() {
            match b[self.pos] {
                b'\n' => {
                    self.pos += 1;
                    return;
                }
                b'\\' => {
                    let p = self.splice(self.pos);
                    self.pos = if p == self.pos { p + 1 } else { p };
                }
                b'"' => {
                    if !(self.raw_prefix(self.pos) && self.skip_raw_string()) {
                        self.skip_quoted(b'"');
                    }
                }
                b'\'' => {
                    if self.digit_separator(self.pos) {
                        self.pos += 1;
                    } else {
                        self.skip_quoted(b'\'');
                    }
                }
                b'/' => {
                    let p = self.splice(self.pos + 1);
                    match b.get(p) {
                        Some(b'*') => self.skip_block_comment(),
                        Some(b'/') => self.skip_line_comment(),
                        _ => self.pos += 1,
                    }
                }
                _ => self.pos += 1,
            }
        }
    }

    /// The bytes of `[start, self.pos)` without line splices.
    fn text(&self, start: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pos - start);
        let mut p = start;
        while p < self.pos {
            let q = self.splice(p);
            if q != p {
                p = q;
                continue;
            }
            out.push(self.b[p]);
            p += 1;
        }
        out
    }

    fn take_ident_chars(&mut self) {
        while let Some(c) = self.peek() {
            if !is_ident_char(c) {
                break;
            }
            self.bump();
        }
    }

    /// A string or character literal (prefix already consumed), with its
    /// user-defined suffix.
    fn lex_quoted(&mut self, raw: bool) {
        let q = self.peek().unwrap();
        if raw && q == b'"' && self.skip_raw_string() {
        } else {
            self.bump();
            loop {
                match self.peek() {
                    None | Some(b'\n') => break,
                    Some(b'\\') => {
                        self.bump();
                        if self.peek().is_some_and(|c| c != b'\n') {
                            self.bump();
                        }
                    }
                    Some(c) if c == q => {
                        self.bump();
                        break;
                    }
                    _ => self.bump(),
                }
            }
        }
        if self.peek().is_some_and(is_ident_start) {
            self.take_ident_chars();
        }
    }

    /// The next token of a directive line, or None at its end.
    fn lex_token(&mut self) -> Option<Token> {
        let space = self.skip_space();
        let c = self.peek()?;
        if c == b'\n' {
            return None;
        }
        let start = self.pos;
        let kind = if is_ident_start(c) {
            self.take_ident_chars();
            let t = self.text(start);
            match (self.peek(), t.as_slice()) {
                (Some(b'"'), b"R" | b"u8R" | b"uR" | b"UR" | b"LR") => {
                    self.lex_quoted(true);
                    Kind::Str
                }
                (Some(b'"'), b"u8" | b"u" | b"U" | b"L") => {
                    self.lex_quoted(false);
                    Kind::Str
                }
                (Some(b'\''), b"u8" | b"u" | b"U" | b"L") => {
                    self.lex_quoted(false);
                    Kind::Char
                }
                _ => Kind::Ident,
            }
        } else if c.is_ascii_digit() || (c == b'.' && self.peek2().is_some_and(|d| d.is_ascii_digit())) {
            self.bump();
            while let Some(d) = self.peek() {
                if matches!(d, b'e' | b'E' | b'p' | b'P') && matches!(self.peek2(), Some(b'+' | b'-')) {
                    self.bump();
                    self.bump();
                } else if is_ident_char(d) || d == b'.' || (d == b'\'' && self.peek2().is_some_and(is_ident_char)) {
                    // A digit separator only before a digit or letter.
                    self.bump();
                } else {
                    break;
                }
            }
            Kind::Number
        } else if c == b'"' {
            self.lex_quoted(false);
            Kind::Str
        } else if c == b'\'' {
            self.lex_quoted(false);
            Kind::Char
        } else {
            // Punctuators, longest first (they hold no splices in practice).
            let rest = &self.b[self.pos..];
            match PUNCTS.iter().find(|(s, _)| rest.starts_with(s.as_bytes())) {
                Some((s, p)) => {
                    self.pos += s.len();
                    Kind::Punct(*p)
                }
                None => {
                    self.bump();
                    Kind::Other
                }
            }
        };
        Some(Token { kind, sym: intern::intern_bytes(&self.text(start)), space })
    }

    /// `<...>` after `#include`: everything up to `>` on the line.
    fn lex_header_name(&mut self) -> Option<Token> {
        let space = self.skip_space();
        if self.peek() != Some(b'<') {
            return None;
        }
        self.bump();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b'>' || c == b'\n' {
                break;
            }
            self.bump();
        }
        let name = self.text(start);
        if self.peek() == Some(b'>') {
            self.bump();
        }
        Some(Token { kind: Kind::HeaderName, sym: intern::intern_bytes(&name), space })
    }

    /// The rest of a directive line, after its name, and the newline.
    fn directive_tokens(&mut self, kind: DirKind) -> Vec<Token> {
        let mut toks = Vec::new();
        if matches!(kind, DirKind::Include | DirKind::IncludeNext | DirKind::Import | DirKind::Embed)
            && let Some(t) = self.lex_header_name()
        {
            toks.push(t);
        }
        if matches!(kind, DirKind::Other | DirKind::Line) {
            self.skip_text_line();
            return toks;
        }
        while let Some(t) = self.lex_token() {
            toks.push(t);
        }
        if self.peek() == Some(b'\n') {
            self.bump();
        }
        toks
    }
}

fn directive_kind(name: &[u8]) -> DirKind {
    match name {
        b"define" => DirKind::Define,
        b"undef" => DirKind::Undef,
        b"include" => DirKind::Include,
        b"include_next" => DirKind::IncludeNext,
        b"import" => DirKind::Import,
        b"embed" => DirKind::Embed,
        b"if" => DirKind::If,
        b"ifdef" => DirKind::Ifdef,
        b"ifndef" => DirKind::Ifndef,
        b"elif" => DirKind::Elif,
        b"elifdef" => DirKind::Elifdef,
        b"elifndef" => DirKind::Elifndef,
        b"else" => DirKind::Else,
        b"endif" => DirKind::Endif,
        b"pragma" => DirKind::Pragma,
        b"line" => DirKind::Line,
        _ => DirKind::Other,
    }
}

/// Lexes a file's directives.
pub fn lex(content: &[u8]) -> FileDirectives {
    let hash = content_hash(content);
    if content.starts_with(&[0xff, 0xfe]) || content.starts_with(&[0xfe, 0xff]) {
        return FileDirectives { utf16: true, hash, ..Default::default() };
    }
    // A UTF-8 byte order mark (as editors on Windows write) is not text:
    // left in, it would hide a directive on the first line.
    let content = content.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(content);
    // A lone carriage return ends a line: made a newline, which keeps
    // every offset.
    let lone_cr = content.contains(&b'\r') && content.iter().enumerate().any(|(i, &c)| c == b'\r' && content.get(i + 1) != Some(&b'\n'));
    let unix;
    let content = if lone_cr {
        unix = content.iter().enumerate().map(|(i, &c)| if c == b'\r' && content.get(i + 1) != Some(&b'\n') { b'\n' } else { c }).collect::<Vec<u8>>();
        &unix[..]
    } else {
        content
    };
    let trigraphs = content.windows(3).any(|w| w[0] == b'?' && w[1] == b'?' && b"=/'()!<>-".contains(&w[2]));
    let mut lx = Lexer { b: content, pos: 0, line: 0, line_pos: 0 };
    let mut dirs = Vec::new();
    // Whether text (not directives) appeared before the first directive,
    // and after the last one: for include guards.
    let mut text_first = false;
    let mut text_last = false;
    let mut asm_include = false;
    let mut called = Vec::new();
    loop {
        lx.skip_space();
        let Some(c) = lx.peek() else { break };
        if c == b'\n' {
            lx.bump();
            continue;
        }
        let hash = c == b'#' || (c == b'%' && lx.peek2() == Some(b':'));
        if !hash {
            if dirs.is_empty() {
                text_first = true;
            }
            text_last = true;
            let start = lx.pos;
            lx.skip_text_line();
            let text = &content[start..lx.pos];
            asm_include |= [&b".include"[..], b".incbin"]
                .iter()
                .any(|d| text.strip_prefix(*d).is_some_and(|rest| rest.first().is_some_and(|c| c.is_ascii_whitespace() || *c == b'"')));
            calls(text, &mut called);
            if may_run_pragma(text) {
                let line = lx.line_at(start);
                let toks = tokens(text);
                dirs.extend(pragma_operators(&toks).into_iter().map(|toks| Directive {
                    kind: DirKind::Pragma,
                    toks: toks.into_boxed_slice(),
                    line,
                    mac: None,
                }));
                // `_Pragma(OPERAND)` with an operand macros make: what it
                // does depends on the macros defined there.
                let found = macros::pragmas(&toks, false, &[]);
                dirs.extend(found.operands.into_iter().map(|operand| {
                    let mut toks = vec![
                        Token { kind: Kind::Ident, sym: intern::PRAGMA_OPERATOR, space: false },
                        Token { kind: Kind::Punct(P::LParen), sym: intern::intern("("), space: false },
                    ];
                    toks.extend(operand);
                    toks.push(Token { kind: Kind::Punct(P::RParen), sym: intern::intern(")"), space: false });
                    Directive { kind: DirKind::Pragma, toks: toks.into_boxed_slice(), line, mac: None }
                }));
                // A pragma from a macro (`DO_PRAGMA(push_macro("M"))`,
                // `__pragma(once)`): text isn't expanded, so it's marked.
                if found.pragmas != macros::Pragmas::None {
                    let marker = Token { kind: Kind::Ident, sym: intern::PRAGMA_OPERATOR, space: false };
                    dirs.push(Directive { kind: DirKind::Pragma, toks: Box::new([marker]), line, mac: None });
                } else if !found.callees.is_empty() {
                    // Pragmas' words given to these (`DO_PRAGMA(once)`, not
                    // `if (once)`): one if a macro (see `CALLEES`).
                    let marker = Token { kind: Kind::Unknown, sym: CALLEES, space: false };
                    let callees = found.callees.iter().map(|&c| Token { kind: Kind::Ident, sym: c, space: true });
                    dirs.push(Directive { kind: DirKind::Pragma, toks: std::iter::once(marker).chain(callees).collect(), line, mac: None });
                }
            }
            continue;
        }
        let line = lx.line_at(lx.pos);
        lx.bump();
        if c == b'%' {
            lx.bump();
        }
        lx.skip_space();
        let start = lx.pos;
        if lx.peek().is_some_and(is_ident_start) {
            lx.take_ident_chars();
        }
        let name = lx.text(start);
        let kind = match lx.peek() {
            _ if !name.is_empty() => directive_kind(&name),
            Some(d) if d.is_ascii_digit() => DirKind::Line,
            _ => DirKind::Other,
        };
        let toks = lx.directive_tokens(kind);
        let mac = if kind == DirKind::Define { macros::parse_define(&toks).map(Arc::new) } else { None };
        text_last = false;
        dirs.push(Directive { kind, toks: toks.into_boxed_slice(), line, mac });
    }
    let guard = if text_first || text_last { None } else { include_guard(&dirs) };
    called.sort_unstable();
    called.dedup();
    FileDirectives { dirs, guard, utf16: false, lone_cr, trigraphs, asm_include, hash, calls: called.into_boxed_slice(), text: OnceLock::new() }
}

/// The first token of a `#pragma` the lexer makes for text that gives a
/// pragma's words to the macros its other tokens name (see
/// `macros::Found::callees`): no text lexes as it.
pub(crate) const CALLEES: Sym = intern::EMPTY;

/// Whether a line of text may run a pragma that changes macros or
/// inclusion, `_Pragma("...")` or through a macro (see `macros::pragmas`):
/// it has `_Pragma` or one of those pragmas' names.
fn may_run_pragma(text: &[u8]) -> bool {
    text.iter().enumerate().any(|(i, &c)| {
        let rest = &text[i..];
        match c {
            b'o' => rest.starts_with(b"once"),
            b'_' => rest.starts_with(b"_Pragma") || rest.starts_with(b"_macro") || rest.starts_with(b"_alias"),
            _ => false,
        }
    })
}

/// The `_Pragma("...")` operators of a line of text, each as the tokens
/// of the `#pragma` it stands for (macros that expand to them aren't
/// seen: text isn't expanded).
fn pragma_operators(toks: &[Token]) -> Vec<Vec<Token>> {
    let mut out = Vec::new();
    for w in toks.windows(4) {
        if let [p, l, s, r] = w
            && p.sym == intern::PRAGMA_OPERATOR
            && l.is(P::LParen)
            && s.kind == Kind::Str
            && r.is(P::RParen)
        {
            out.push(tokens(macros::destringize(s.sym.as_str()).as_bytes()));
        }
    }
    out
}

/// The macro of an include guard: the first directive is `#ifndef G` (or
/// `#if !defined G`), whose `#endif` is the last directive, without an
/// `#else` or `#elif` of its own.
pub(crate) fn include_guard(dirs: &[Directive]) -> Option<Sym> {
    let first = dirs.first()?;
    let g = match (first.kind, &first.toks[..]) {
        (DirKind::Ifndef, [t]) => t.ident()?,
        (DirKind::If, [n, d, t]) if n.is(P::Not) && d.sym == intern::DEFINED => t.ident()?,
        (DirKind::If, [n, d, l, t, r]) if n.is(P::Not) && d.sym == intern::DEFINED && l.is(P::LParen) && r.is(P::RParen) => t.ident()?,
        _ => return None,
    };
    let mut depth = 0usize;
    for (i, d) in dirs.iter().enumerate() {
        match d.kind {
            DirKind::If | DirKind::Ifdef | DirKind::Ifndef => depth += 1,
            DirKind::Endif => {
                depth -= 1;
                if depth == 0 {
                    return (i == dirs.len() - 1).then_some(g);
                }
            }
            DirKind::Elif | DirKind::Elifdef | DirKind::Elifndef | DirKind::Else if depth == 1 => return None,
            _ => {}
        }
        if depth == 0 {
            return None;
        }
    }
    None
}

/// The tokens of a file's text: its lines that aren't directives (which
/// `lex` skips; see `FileDirectives::text`).
pub(crate) fn text_tokens(content: &[u8]) -> Vec<Token> {
    let content = content.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(content);
    let mut lx = Lexer { b: content, pos: 0, line: 0, line_pos: 0 };
    let mut out = Vec::new();
    loop {
        lx.skip_space();
        let Some(c) = lx.peek() else { break };
        if c == b'\n' {
            lx.bump();
            continue;
        }
        let directive = c == b'#' || (c == b'%' && lx.peek2() == Some(b':'));
        let start = lx.pos;
        lx.skip_text_line();
        if !directive {
            out.extend(tokens(&lx.text(start)));
        }
    }
    out
}

/// Lexes a line of tokens (for `##` pasting and command-line macros).
pub fn tokens(text: &[u8]) -> Vec<Token> {
    let mut lx = Lexer { b: text, pos: 0, line: 0, line_pos: 0 };
    let mut out = Vec::new();
    while let Some(t) = lx.lex_token() {
        out.push(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(fd: &FileDirectives) -> Vec<DirKind> {
        fd.dirs.iter().map(|d| d.kind).collect()
    }

    fn spell(toks: &[Token]) -> Vec<&'static str> {
        toks.iter().map(|t| t.sym.as_str()).collect()
    }

    #[test]
    fn finds_directives_only() {
        let fd = lex(b"int a; // #include \"no\"\n  #  include <a/b.h>\n/* #define NO\n */ #define YES 1\nchar *s = \"#include <no2.h>\";\n#if A\n#endif\n");
        assert_eq!(kinds(&fd), vec![DirKind::Include, DirKind::Define, DirKind::If, DirKind::Endif]);
        assert_eq!(fd.dirs[0].toks[0].kind, Kind::HeaderName);
        assert_eq!(fd.dirs[0].toks[0].sym.as_str(), "a/b.h");
        assert_eq!(spell(&fd.dirs[1].toks), vec!["YES", "1"]);
        // The line of the `#`, after the comment.
        assert_eq!(fd.dirs[1].line, 4);
        assert_eq!(fd.dirs[2].line, 6);
    }

    #[test]
    fn hidden_hashes() {
        // A raw string spanning lines, a digit separator before a block
        // comment, a character literal with a quote, a splice.
        let src = b"auto r = R\"x(\n#include <raw.h>\n)x\";\nint n = 1'000; /* c\n#include <comment.h>\n*/\nchar q = '\\''; char h = '#';\nint s = 1 \\\n#include <spliced.h>\n#include <real.h>\n";
        let fd = lex(src);
        assert_eq!(kinds(&fd), vec![DirKind::Include]);
        assert_eq!(fd.dirs[0].toks[0].sym.as_str(), "real.h");
    }

    #[test]
    fn directive_lines() {
        let fd = lex(b"#define F(a, ...) a ## __VA_ARGS__ /* comment\n continued */ + 1\n#define G (x)\n#if defined(F) && \\\n  X >= 0x10'00ull\n#endif\n%:define DIGRAPH <:\n");
        let f = fd.dirs[0].mac.as_ref().unwrap();
        assert_eq!(f.params.as_deref().map(|p| p.len()), Some(2));
        assert!(f.variadic);
        assert_eq!(spell(&f.body), vec!["a", "##", "__VA_ARGS__", "+", "1"]);
        assert!(fd.dirs[1].mac.as_ref().unwrap().params.is_none());
        assert_eq!(spell(&fd.dirs[2].toks), vec!["defined", "(", "F", ")", "&&", "X", ">=", "0x10'00ull"]);
        assert_eq!(fd.dirs[4].kind, DirKind::Define);
    }

    #[test]
    fn literals() {
        let t = tokens(b"u8\"a\\\"b\" L'x' R\"d(a)\"b)d\" \"s\"_ud 1.5e+3 .5 a<<=b");
        assert_eq!(spell(&t), vec!["u8\"a\\\"b\"", "L'x'", "R\"d(a)\"b)d\"", "\"s\"_ud", "1.5e+3", ".5", "a", "<<=", "b"]);
        assert_eq!(t[0].kind, Kind::Str);
        assert_eq!(t[1].kind, Kind::Char);
        assert_eq!(t[2].kind, Kind::Str);
    }

    #[test]
    fn line_endings() {
        // A lone carriage return ends a line (and a splice), as a newline
        // and a carriage return and a newline do.
        let fd = lex(b"#pragma once\r#include \"a.h\"\r\n#define X 1 \\\r  + 2\rint y;\r#if X\n#endif\r");
        assert_eq!(kinds(&fd), vec![DirKind::Pragma, DirKind::Include, DirKind::Define, DirKind::If, DirKind::Endif]);
        assert_eq!(spell(&fd.dirs[2].toks), vec!["X", "1", "+", "2"]);
        assert_eq!(fd.dirs.iter().map(|d| d.line).collect::<Vec<_>>(), vec![1, 2, 3, 6, 7]);
        assert!(fd.lone_cr);
        assert!(!lex(b"#include \"a.h\"\r\n").lone_cr);
    }

    #[test]
    fn line_directives_and_trigraphs() {
        let fd = lex(b"#line 10 \"x.c\"\n# 33 \"y.c\" 2\n#\n#ident \"x\"\n");
        assert_eq!(kinds(&fd), vec![DirKind::Line, DirKind::Line, DirKind::Other, DirKind::Other]);
        assert!(!fd.trigraphs);
        for t in [&b"??=include <a.h>\n"[..], b"/* *??/\n/\n", b"char c = '??/'';\n", b"x ??- y\n"] {
            assert!(lex(t).trigraphs, "{:?}", String::from_utf8_lossy(t));
        }
        assert!(!lex(b"x ? y : z ?? w;\nint q = a ?: b;\n").trigraphs);
        // An assembler's includes.
        assert!(lex(b"#include \"a.h\"\n  .include \"macros.s\"\n").asm_include);
        assert!(lex(b".incbin \"blob.bin\"\n").asm_include);
        assert!(!lex(b".includes x\nint include;\n").asm_include);
    }

    #[test]
    fn pragmas_in_text() {
        // Written in text: run. From macros: marked (text isn't expanded).
        let fd = lex(b"_Pragma(\"push_macro(\\\"M\\\")\") int x;\nDO_PRAGMA(pop_macro(\"M\"))\n__pragma(once)\nPRAGMA(\"once\")\n\
              PRAGMA(GCC diagnostic push)\nint push_macro_count;\nstd::call_once(flag, f); // once\nstatic_assert(false, \"T must occur \" \"once in Types\");\n\
              _Pragma(STR(once)) int y;\n");
        let pragmas: Vec<Vec<&str>> = fd.dirs.iter().map(|d| spell(&d.toks)).collect();
        // An operand macros make: kept for the scan, which knows them.
        let operand = vec!["_Pragma", "(", "STR", "(", "once", ")", ")"];
        // Words given to a macro: a pragma if it's one (not `if (once)`).
        assert_eq!(pragmas, vec![vec!["push_macro", "(", "\"M\"", ")"], vec!["_Pragma"], vec!["_Pragma"], vec!["", "PRAGMA"], operand]);
        assert!(fd.dirs.iter().all(|d| d.kind == DirKind::Pragma));
        let fd = lex(b"if (once) f();\nP2(x, once)\nP3(\"once\", y)\n(once)\n");
        let pragmas: Vec<Vec<&str>> = fd.dirs.iter().map(|d| spell(&d.toks)).collect();
        assert_eq!(pragmas, vec![vec!["", "if"], vec!["", "P2"], vec!["", "P3"], vec!["_Pragma"]]);
    }

    /// The names text may call, and its tokens.
    #[test]
    fn text_calls() {
        let text = b"#define F(x) G(x)\nint a = F(1) + H (2);\nCALL\n  (3);\nvoid f(int x);\n";
        let fd = lex(text);
        let mut names: Vec<u64> = ["F", "H", "CALL", "f"].iter().map(|n| name_hash(n.as_bytes())).collect();
        names.sort_unstable();
        assert_eq!(&*fd.calls, &names[..]);
        let toks: Vec<&str> = text_tokens(text).iter().map(|t| t.sym.as_str()).collect();
        assert_eq!(toks.join(" "), "int a = F ( 1 ) + H ( 2 ) ; CALL ( 3 ) ; void f ( int x ) ;");
    }

    #[test]
    fn guards() {
        assert_eq!(lex(b"// c\n#ifndef G\n#define G\nint x;\n#endif // G\n").guard, Some(intern::intern("G")));
        assert_eq!(lex(b"#if !defined(H)\n#define H\n#if X\n#endif\n#endif\n").guard, Some(intern::intern("H")));
        assert_eq!(lex(b"int y;\n#ifndef G\n#define G\n#endif\n").guard, None);
        assert_eq!(lex(b"#ifndef G\n#define G\n#endif\nint z;\n").guard, None);
        assert_eq!(lex(b"#ifndef G\n#define G\n#else\n#endif\n").guard, None);
        assert_eq!(lex(b"#ifndef G\n#endif\n#ifndef K\n#endif\n").guard, None);
    }
}
