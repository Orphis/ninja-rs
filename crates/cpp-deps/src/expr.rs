//! `#if` expressions, after macro expansion: C's integer arithmetic on
//! `intmax_t`/`uintmax_t` (64 bits), with the usual arithmetic conversions.
//! A value may be unknown (an unanswered compiler query): `&&`, `||` and
//! `?:` still decide when the known side is enough.

use crate::intern;
use crate::lex::{Kind, P, Token};

/// The language's keywords that matter in `#if`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dialect {
    /// `true` and `false` are 1 and 0 (C++, C23), not identifiers.
    pub true_false: bool,
    /// `and`, `or`, `not`, `bitand`, `bitor`, `xor`, `compl`, `not_eq`
    /// are operators (C++; MSVC only in conforming mode, `/permissive-`).
    /// In C they're `<iso646.h>` macros, expanded before evaluation.
    pub alt_operators: bool,
}

impl Dialect {
    pub const C: Dialect = Dialect { true_false: false, alt_operators: false };
    pub const CXX: Dialect = Dialect { true_false: true, alt_operators: true };
}

/// The operator an alternative token spells.
fn alternative(name: &str) -> Option<P> {
    Some(match name {
        "and" => P::AndAnd,
        "or" => P::OrOr,
        "not" => P::Not,
        "bitand" => P::Amp,
        "bitor" => P::Pipe,
        "xor" => P::Caret,
        "compl" => P::Tilde,
        "not_eq" => P::Ne,
        // Assignments: an error in #if, as `&=` would be.
        "and_eq" | "or_eq" | "xor_eq" => P::Other,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Val {
    /// The value's bits, and whether it is unsigned.
    Known(u64, bool),
    Unknown,
}

impl Val {
    fn int(v: i64) -> Val {
        Val::Known(v as u64, false)
    }
    fn bool(b: bool) -> Val {
        Val::int(b as i64)
    }
    /// True, false, or unknown.
    pub fn truth(self) -> Option<bool> {
        match self {
            Val::Known(v, _) => Some(v != 0),
            Val::Unknown => None,
        }
    }
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    dialect: Dialect,
}

type R = Result<Val, String>;

/// An integer literal's value and signedness.
fn integer(s: &str) -> Result<Val, String> {
    let s = s.replace('\'', "");
    let lower = s.to_ascii_lowercase();
    let (digits, radix) = if let Some(h) = lower.strip_prefix("0x") {
        (h, 16)
    } else if let Some(b) = lower.strip_prefix("0b") {
        (b, 2)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (&lower[1..], 8)
    } else {
        (lower.as_str(), 10)
    };
    let end = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
    let (num, suffix) = digits.split_at(end);
    if !suffix.chars().all(|c| matches!(c, 'u' | 'l' | 'z' | 'w' | 'b')) || (num.is_empty() && radix != 8) {
        return Err(format!("invalid integer literal {s}"));
    }
    let v = if num.is_empty() { 0 } else { u64::from_str_radix(num, radix).map_err(|_| format!("integer literal too large: {s}"))? };
    let unsigned = suffix.contains('u') || v > i64::MAX as u64;
    Ok(Val::Known(v, unsigned))
}

/// A character literal's value (of its first character, as compilers do
/// for these purposes).
fn character(s: &str) -> Result<Val, String> {
    let (wide, body) = match s.find('\'') {
        Some(i) => (i > 0, &s[i + 1..]),
        None => return Err(format!("invalid character literal {s}")),
    };
    let body = body.strip_suffix('\'').unwrap_or(body);
    let mut chars = body.chars().peekable();
    let mut values = Vec::new();
    while let Some(c) = chars.next() {
        let v = if c == '\\' {
            match chars.next() {
                Some('n') => 10,
                Some('t') => 9,
                Some('r') => 13,
                Some('a') => 7,
                Some('b') => 8,
                Some('f') => 12,
                Some('v') => 11,
                Some('e') => 27,
                Some('x') => {
                    let mut v = 0u32;
                    while let Some(d) = chars.peek().and_then(|c| c.to_digit(16)) {
                        v = v.wrapping_mul(16) + d;
                        chars.next();
                    }
                    v
                }
                Some(d @ '0'..='7') => {
                    let mut v = d.to_digit(8).unwrap();
                    for _ in 0..2 {
                        match chars.peek().and_then(|c| c.to_digit(8)) {
                            Some(d) => {
                                v = v * 8 + d;
                                chars.next();
                            }
                            None => break,
                        }
                    }
                    v
                }
                Some(other) => other as u32,
                None => return Err(format!("invalid character literal {s}")),
            }
        } else {
            c as u32
        };
        values.push(v);
    }
    match values.as_slice() {
        // Narrow plain characters are signed chars.
        [v] if !wide => Ok(Val::int(*v as u8 as i8 as i64)),
        [v] => Ok(Val::int(*v as i64)),
        // Multi-character: GCC and Clang shift each in.
        vs => Ok(Val::int(vs.iter().fold(0i64, |acc, v| (acc << 8) | (*v as i64 & 0xff)) as i32 as i64)),
    }
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }
    fn eat(&mut self, p: P) -> bool {
        if self.peek().is_some_and(|t| t.is(p)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn primary(&mut self) -> R {
        let t = *self.peek().ok_or("missing expression")?;
        self.pos += 1;
        match t.kind {
            Kind::Number => integer(t.sym.as_str()),
            Kind::Char => character(t.sym.as_str()),
            Kind::Unknown => Ok(Val::Unknown),
            Kind::Ident => {
                // Identifiers left after expansion are 0, `true` and
                // `false` are C++ keywords (and C23's).
                if t.sym == intern::TRUE && self.dialect.true_false {
                    Ok(Val::int(1))
                } else {
                    if self.peek().is_some_and(|n| n.is(P::LParen)) {
                        return Err(format!("function-like {} in #if", t.sym.as_str()));
                    }
                    Ok(Val::int(0))
                }
            }
            Kind::Punct(P::LParen) => {
                let v = self.conditional()?;
                if !self.eat(P::RParen) {
                    return Err("missing )".into());
                }
                Ok(v)
            }
            _ => Err(format!("unexpected '{}' in #if", t.sym.as_str())),
        }
    }

    fn unary(&mut self) -> R {
        let Some(t) = self.peek() else { return Err("missing expression".into()) };
        let op = match t.kind {
            Kind::Punct(p @ (P::Plus | P::Minus | P::Not | P::Tilde)) => p,
            _ => return self.primary(),
        };
        self.pos += 1;
        let v = self.unary()?;
        Ok(match (op, v) {
            (_, Val::Unknown) => Val::Unknown,
            (P::Plus, v) => v,
            (P::Minus, Val::Known(x, u)) => Val::Known(x.wrapping_neg(), u),
            (P::Not, Val::Known(x, _)) => Val::bool(x == 0),
            (P::Tilde, Val::Known(x, u)) => Val::Known(!x, u),
            _ => unreachable!(),
        })
    }

    fn binary_op(&self) -> Option<(P, u8)> {
        let p = match self.peek()?.kind {
            Kind::Punct(p) => p,
            _ => return None,
        };
        let prec = match p {
            P::Star | P::Slash | P::Percent => 10,
            P::Plus | P::Minus => 9,
            P::Shl | P::Shr => 8,
            P::Lt | P::Gt | P::Le | P::Ge => 7,
            P::EqEq | P::Ne => 6,
            P::Amp => 5,
            P::Caret => 4,
            P::Pipe => 3,
            P::AndAnd => 2,
            P::OrOr => 1,
            _ => return None,
        };
        Some((p, prec))
    }

    /// Precedence climbing over the binary operators.
    fn binary(&mut self, min: u8) -> R {
        let mut lhs = self.unary()?;
        while let Some((op, prec)) = self.binary_op() {
            if prec < min {
                break;
            }
            self.pos += 1;
            let rhs = self.binary(prec + 1)?;
            lhs = apply(op, lhs, rhs)?;
        }
        Ok(lhs)
    }

    fn conditional(&mut self) -> R {
        let c = self.binary(1)?;
        if !self.eat(P::Question) {
            // The comma operator (allowed by GCC and Clang in #if).
            if self.eat(P::Comma) {
                return self.conditional();
            }
            return Ok(c);
        }
        let a = self.conditional()?;
        if !self.eat(P::Colon) {
            return Err("missing : in ?:".into());
        }
        let b = self.conditional()?;
        let unsigned = matches!(a, Val::Known(_, true)) || matches!(b, Val::Known(_, true));
        let conv = |v: Val| match v {
            Val::Known(x, _) => Val::Known(x, unsigned),
            u => u,
        };
        Ok(match c.truth() {
            Some(true) => conv(a),
            Some(false) => conv(b),
            None if a == b => a,
            None => Val::Unknown,
        })
    }
}

fn apply(op: P, a: Val, b: Val) -> R {
    // Short-circuits decide with one known side.
    match (op, a.truth(), b.truth()) {
        (P::AndAnd, Some(false), _) | (P::AndAnd, _, Some(false)) => return Ok(Val::int(0)),
        (P::AndAnd, Some(true), Some(true)) => return Ok(Val::int(1)),
        (P::OrOr, Some(true), _) | (P::OrOr, _, Some(true)) => return Ok(Val::int(1)),
        (P::OrOr, Some(false), Some(false)) => return Ok(Val::int(0)),
        (P::AndAnd | P::OrOr, _, _) => return Ok(Val::Unknown),
        _ => {}
    }
    let (Val::Known(x, ux), Val::Known(y, uy)) = (a, b) else { return Ok(Val::Unknown) };
    let u = ux || uy;
    let (sx, sy) = (x as i64, y as i64);
    Ok(match op {
        P::Star => Val::Known(x.wrapping_mul(y), u),
        P::Slash | P::Percent => {
            if y == 0 {
                return Err("division by zero in #if".into());
            }
            let v = match (op, u) {
                (P::Slash, true) => x / y,
                (P::Slash, false) => sx.wrapping_div(sy) as u64,
                (_, true) => x % y,
                (_, false) => sx.wrapping_rem(sy) as u64,
            };
            Val::Known(v, u)
        }
        P::Plus => Val::Known(x.wrapping_add(y), u),
        P::Minus => Val::Known(x.wrapping_sub(y), u),
        P::Shl => Val::Known(if y >= 64 { 0 } else { x << y }, ux),
        P::Shr => Val::Known(
            match (ux, y >= 64) {
                (true, true) => 0,
                (true, false) => x >> y,
                (false, true) => (sx >> 63) as u64,
                (false, false) => (sx >> y) as u64,
            },
            ux,
        ),
        P::Lt => Val::bool(if u { x < y } else { sx < sy }),
        P::Gt => Val::bool(if u { x > y } else { sx > sy }),
        P::Le => Val::bool(if u { x <= y } else { sx <= sy }),
        P::Ge => Val::bool(if u { x >= y } else { sx >= sy }),
        P::EqEq => Val::bool(x == y),
        P::Ne => Val::bool(x != y),
        P::Amp => Val::Known(x & y, u),
        P::Caret => Val::Known(x ^ y, u),
        P::Pipe => Val::Known(x | y, u),
        _ => unreachable!(),
    })
}

/// Evaluates an expanded `#if` expression.
pub fn eval(toks: &[Token], dialect: Dialect) -> Result<Val, String> {
    let alt;
    let toks = if dialect.alt_operators && toks.iter().any(|t| t.kind == Kind::Ident && alternative(t.sym.as_str()).is_some()) {
        alt = toks
            .iter()
            .map(|t| match (t.kind, alternative(t.sym.as_str())) {
                (Kind::Ident, Some(p)) => Token { kind: Kind::Punct(p), ..*t },
                _ => *t,
            })
            .collect::<Vec<_>>();
        &alt[..]
    } else {
        toks
    };
    let mut p = Parser { toks, pos: 0, dialect };
    let v = p.conditional()?;
    if p.pos != toks.len() {
        return Err(format!("unexpected '{}' in #if", toks[p.pos].sym.as_str()));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tokens;

    fn ev(s: &str) -> Result<Val, String> {
        eval(&tokens(s.as_bytes()), Dialect::CXX)
    }

    #[test]
    fn arithmetic() {
        assert_eq!(ev("1 + 2 * 3"), Ok(Val::int(7)));
        assert_eq!(ev("(1 + 2) * 3 == 9 && 10 / 3 == 3 && 10 % 3 == 1"), Ok(Val::int(1)));
        assert_eq!(ev("-1 < 0"), Ok(Val::int(1)));
        assert_eq!(ev("-1 < 0u"), Ok(Val::int(0)));
        assert_eq!(ev("0xFFFFFFFFFFFFFFFF == -1"), Ok(Val::int(1)));
        assert_eq!(ev("1 << 4 | 1"), Ok(Val::int(17)));
        assert_eq!(ev("~0 >> 63"), Ok(Val::int(-1)));
        assert_eq!(ev("~0u >> 63"), Ok(Val::Known(1, true)));
        assert_eq!(ev("0b101 == 5 && 010 == 8 && 1'000 == 1000ull && 0 == 0L"), Ok(Val::int(1)));
        assert_eq!(ev("'a' == 97 && '\\n' == 10 && '\\x41' == 65 && '\\377' < 0 && L'\\377' > 0"), Ok(Val::int(1)));
        assert_eq!(ev("1 ? 2 : 3"), Ok(Val::int(2)));
        assert_eq!(ev("0 ? 2 : 3u"), Ok(Val::Known(3, true)));
        assert_eq!(ev("undefined_ident + true + false"), Ok(Val::int(1)));
        assert_eq!(ev("1, 2"), Ok(Val::int(2)));
    }

    #[test]
    fn dialects() {
        assert_eq!(ev("1 and not 0 and (2 bitand 3) == 2 and (1 bitor 2) == 3 and (3 xor 1) == 2 and compl 0 == -1 and 1 not_eq 2"), Ok(Val::int(1)));
        assert_eq!(ev("0 or 1"), Ok(Val::int(1)));
        assert!(ev("1 and_eq 2").is_err());
        // In C, they're identifiers (0) unless <iso646.h> defined them.
        let c = |s: &str| eval(&tokens(s.as_bytes()), Dialect::C);
        assert!(c("1 and 1").is_err());
        assert_eq!(c("true"), Ok(Val::int(0)));
        assert_eq!(c("not"), Ok(Val::int(0)));
        assert_eq!(eval(&tokens(b"true"), Dialect { true_false: true, alt_operators: false }), Ok(Val::int(1)));
    }

    #[test]
    fn errors() {
        assert!(ev("1 +").is_err());
        assert!(ev("(1").is_err());
        assert!(ev("1 / 0").is_err());
        assert!(ev("f(1)").is_err());
        assert!(ev("1.5").is_err());
        assert!(ev("1 2").is_err());
    }

    #[test]
    fn unknowns() {
        let u = Token { kind: Kind::Unknown, sym: intern::EMPTY, space: true };
        let with = |s: &str| {
            let mut t = tokens(s.as_bytes());
            for x in &mut t {
                if x.sym.as_str() == "U" {
                    *x = u;
                }
            }
            eval(&t, Dialect::CXX)
        };
        assert_eq!(with("U"), Ok(Val::Unknown));
        assert_eq!(with("0 && U"), Ok(Val::int(0)));
        assert_eq!(with("U && 0"), Ok(Val::int(0)));
        assert_eq!(with("1 || U"), Ok(Val::int(1)));
        assert_eq!(with("U || 0"), Ok(Val::Unknown));
        assert_eq!(with("U + 1"), Ok(Val::Unknown));
        assert_eq!(with("U ? 1 : 1"), Ok(Val::int(1)));
        assert_eq!(with("!U"), Ok(Val::Unknown));
    }
}
