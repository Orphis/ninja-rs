//! Tokenizer for the ninja manifest and dyndep languages, ported from ninja's
//! re2c lexer (`lexer.in.cc`).

use crate::eval::EvalString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Error,
    Build,
    Colon,
    Default,
    Equals,
    Ident,
    Include,
    Indent,
    Newline,
    Pipe,
    Pipe2,
    PipeAt,
    Pool,
    Rule,
    Subninja,
    Teof,
}

impl Token {
    pub fn name(self) -> &'static str {
        match self {
            Token::Error => "lexing error",
            Token::Build => "'build'",
            Token::Colon => "':'",
            Token::Default => "'default'",
            Token::Equals => "'='",
            Token::Ident => "identifier",
            Token::Include => "'include'",
            Token::Indent => "indent",
            Token::Newline => "newline",
            Token::Pipe2 => "'||'",
            Token::Pipe => "'|'",
            Token::PipeAt => "'|@'",
            Token::Pool => "'pool'",
            Token::Rule => "'rule'",
            Token::Subninja => "'subninja'",
            Token::Teof => "eof",
        }
    }

    pub fn error_hint(self) -> &'static str {
        match self {
            Token::Colon => " ($ also escapes ':')",
            _ => "",
        }
    }
}

fn is_varname_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.'
}

fn is_simple_varname_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

pub struct Lexer<'a> {
    filename: String,
    input: &'a [u8],
    ofs: usize,
    last_token: Option<usize>,
}

impl<'a> Lexer<'a> {
    pub fn new(filename: &str, input: &'a [u8]) -> Self {
        Self { filename: filename.to_string(), input, ofs: 0, last_token: None }
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    fn at(&self, i: usize) -> u8 {
        // A NUL past the end, like ninja's NUL-terminated input.
        self.input.get(i).copied().unwrap_or(0)
    }

    /// Formats an error with the current position, like ninja's
    /// `Lexer::Error`: `file:line: message` followed by the line and a caret.
    pub fn error(&self, message: &str) -> String {
        let last = self.last_token.unwrap_or(self.ofs).min(self.input.len());
        let mut line = 1;
        let mut line_start = 0;
        for (i, &c) in self.input[..last].iter().enumerate() {
            if c == b'\n' {
                line += 1;
                line_start = i + 1;
            }
        }
        let col = last - line_start;
        let mut out = format!("{}:{}: {}\n", self.filename, line, message);
        const TRUNCATE_COLUMN: usize = 72;
        if col > 0 && col < TRUNCATE_COLUMN {
            let mut len = 0;
            let mut truncated = true;
            while len < TRUNCATE_COLUMN {
                let c = self.at(line_start + len);
                if c == 0 || c == b'\n' {
                    truncated = false;
                    break;
                }
                len += 1;
            }
            out.push_str(&String::from_utf8_lossy(&self.input[line_start..(line_start + len).min(self.input.len())]));
            if truncated {
                out.push_str("...");
            }
            out.push('\n');
            out.push_str(&" ".repeat(col));
            out.push_str("^ near here");
        }
        out
    }

    pub fn describe_last_error(&self) -> &'static str {
        if let Some(t) = self.last_token
            && self.at(t) == b'\t'
        {
            return "tabs are not allowed, use spaces";
        }
        "lexing error"
    }

    pub fn unread_token(&mut self) {
        if let Some(t) = self.last_token {
            self.ofs = t;
        }
    }

    pub fn read_token(&mut self) -> Token {
        let mut p = self.ofs;
        let start;
        let token;
        loop {
            let s = p;
            // Comment: [ ]*"#"[^\0\n]*"\n"  (also accepted at EOF)
            let mut q = s;
            while self.at(q) == b' ' {
                q += 1;
            }
            if self.at(q) == b'#' {
                while self.at(q) != 0 && self.at(q) != b'\n' {
                    q += 1;
                }
                if self.at(q) == b'\n' {
                    q += 1;
                }
                p = q;
                continue;
            }
            // Newlines with leading spaces.
            if self.at(q) == b'\n' {
                p = q + 1;
                start = s;
                token = Token::Newline;
                break;
            }
            if self.at(q) == b'\r' && self.at(q + 1) == b'\n' {
                p = q + 2;
                start = s;
                token = Token::Newline;
                break;
            }
            if q > s {
                p = q;
                start = s;
                token = Token::Indent;
                break;
            }
            start = s;
            let c = self.at(s);
            if c == 0 && s >= self.input.len() {
                p = s;
                token = Token::Teof;
                break;
            }
            match c {
                b'=' => {
                    p = s + 1;
                    token = Token::Equals;
                }
                b':' => {
                    p = s + 1;
                    token = Token::Colon;
                }
                b'|' => {
                    if self.at(s + 1) == b'@' {
                        p = s + 2;
                        token = Token::PipeAt;
                    } else if self.at(s + 1) == b'|' {
                        p = s + 2;
                        token = Token::Pipe2;
                    } else {
                        p = s + 1;
                        token = Token::Pipe;
                    }
                }
                c if is_varname_char(c) => {
                    let mut e = s;
                    while is_varname_char(self.at(e)) {
                        e += 1;
                    }
                    p = e;
                    token = match &self.input[s..e] {
                        b"build" => Token::Build,
                        b"pool" => Token::Pool,
                        b"rule" => Token::Rule,
                        b"default" => Token::Default,
                        b"include" => Token::Include,
                        b"subninja" => Token::Subninja,
                        _ => Token::Ident,
                    };
                }
                _ => {
                    p = s + 1;
                    token = Token::Error;
                }
            }
            break;
        }
        self.last_token = Some(start);
        self.ofs = p;
        if token != Token::Newline && token != Token::Teof {
            self.eat_whitespace();
        }
        token
    }

    pub fn peek_token(&mut self, t: Token) -> bool {
        if self.read_token() == t {
            return true;
        }
        self.unread_token();
        false
    }

    fn eat_whitespace(&mut self) {
        loop {
            let p = self.ofs;
            if self.at(p) == b' ' {
                self.ofs += 1;
            } else if self.at(p) == b'$' && self.at(p + 1) == b'\n' {
                self.ofs += 2;
            } else if self.at(p) == b'$' && self.at(p + 1) == b'\r' && self.at(p + 2) == b'\n' {
                self.ofs += 3;
            } else {
                break;
            }
        }
    }

    pub fn read_ident(&mut self) -> Option<String> {
        let start = self.ofs;
        let mut e = start;
        while is_varname_char(self.at(e)) {
            e += 1;
        }
        self.last_token = Some(start);
        if e == start {
            return None;
        }
        let s = String::from_utf8_lossy(&self.input[start..e]).into_owned();
        self.ofs = e;
        self.eat_whitespace();
        Some(s)
    }

    pub fn read_path(&mut self, eval: &mut EvalString) -> Result<(), String> {
        self.read_eval_string(eval, true)
    }

    pub fn read_var_value(&mut self, eval: &mut EvalString) -> Result<(), String> {
        self.read_eval_string(eval, false)
    }

    fn read_eval_string(&mut self, eval: &mut EvalString, path: bool) -> Result<(), String> {
        let mut p = self.ofs;
        let mut start;
        loop {
            start = p;
            let c = self.at(p);
            match c {
                0 if p >= self.input.len() => {
                    self.last_token = Some(start);
                    return Err(self.error("unexpected EOF"));
                }
                b'\r' if self.at(p + 1) == b'\n' => {
                    if !path {
                        p += 2;
                    }
                    break;
                }
                b' ' | b':' | b'|' | b'\n' => {
                    if path {
                        break;
                    }
                    if c == b'\n' {
                        p += 1;
                        break;
                    }
                    eval.add_text(if c == b' ' {
                        " "
                    } else if c == b':' {
                        ":"
                    } else {
                        "|"
                    });
                    p += 1;
                }
                b'$' => {
                    let n = self.at(p + 1);
                    match n {
                        b'$' => {
                            eval.add_text("$");
                            p += 2;
                        }
                        b' ' => {
                            eval.add_text(" ");
                            p += 2;
                        }
                        b'\n' => {
                            p += 2;
                            while self.at(p) == b' ' {
                                p += 1;
                            }
                        }
                        b'\r' if self.at(p + 2) == b'\n' => {
                            p += 3;
                            while self.at(p) == b' ' {
                                p += 1;
                            }
                        }
                        b'{' => {
                            let mut e = p + 2;
                            while is_varname_char(self.at(e)) {
                                e += 1;
                            }
                            if e > p + 2 && self.at(e) == b'}' {
                                eval.add_var(&String::from_utf8_lossy(&self.input[p + 2..e]));
                                p = e + 1;
                            } else {
                                self.last_token = Some(start);
                                return Err(self.error("bad $-escape (literal $ must be written as $$)"));
                            }
                        }
                        b':' => {
                            eval.add_text(":");
                            p += 2;
                        }
                        b'^' => {
                            eval.add_text("\n");
                            p += 2;
                        }
                        n if is_simple_varname_char(n) => {
                            let mut e = p + 1;
                            while is_simple_varname_char(self.at(e)) {
                                e += 1;
                            }
                            eval.add_var(&String::from_utf8_lossy(&self.input[p + 1..e]));
                            p = e;
                        }
                        _ => {
                            self.last_token = Some(start);
                            return Err(self.error("bad $-escape (literal $ must be written as $$)"));
                        }
                    }
                }
                b'\r' | 0 => {
                    self.last_token = Some(start);
                    return Err(self.error(self.describe_last_error()));
                }
                _ => {
                    // In a value, ' ', ':' and '|' are plain text: keep them
                    // in the run instead of appending them one at a time.
                    let ends_run = |c: u8| match c {
                        b'$' | b'\r' | b'\n' | 0 => true,
                        b' ' | b':' | b'|' => path,
                        _ => false,
                    };
                    let mut e = p;
                    while !ends_run(self.at(e)) {
                        e += 1;
                    }
                    // Stop at a NUL only at the real end; embedded NULs are errors.
                    eval.add_text(&String::from_utf8_lossy(&self.input[p..e]));
                    p = e;
                }
            }
        }
        self.last_token = Some(start);
        self.ofs = p;
        if path {
            self.eat_whitespace();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_var_value() {
        let mut l = Lexer::new("input", b"plain text $var $VaR ${x}\n");
        let mut e = EvalString::new();
        l.read_var_value(&mut e).unwrap();
        assert_eq!(e.serialize(), "[plain text ][$var][ ][$VaR][ ][$x]");
    }

    #[test]
    fn read_eval_string_escapes() {
        let mut l = Lexer::new("input", b"$ $$ab c$: $\ncde\n");
        let mut e = EvalString::new();
        l.read_var_value(&mut e).unwrap();
        assert_eq!(e.serialize(), "[ $ab c: cde]");
    }

    #[test]
    fn read_ident() {
        let mut l = Lexer::new("input", b"foo baR baz_123 foo-bar");
        assert_eq!(l.read_ident().as_deref(), Some("foo"));
        assert_eq!(l.read_ident().as_deref(), Some("baR"));
        assert_eq!(l.read_ident().as_deref(), Some("baz_123"));
        assert_eq!(l.read_ident().as_deref(), Some("foo-bar"));
    }

    #[test]
    fn read_ident_curlies() {
        // Verify that ReadIdent includes dots in the name, but in an expansion
        // $bar.dots stops at the dot.
        let mut l = Lexer::new("input", b"foo.dots $bar.dots ${bar.dots}\n");
        assert_eq!(l.read_ident().as_deref(), Some("foo.dots"));
        let mut e = EvalString::new();
        l.read_var_value(&mut e).unwrap();
        assert_eq!(e.serialize(), "[$bar][.dots ][$bar.dots]");
    }

    #[test]
    fn error() {
        let mut l = Lexer::new("input", b"foo$\nbad $");
        let mut e = EvalString::new();
        let err = l.read_var_value(&mut e).unwrap_err();
        assert_eq!(err, "input:2: bad $-escape (literal $ must be written as $$)\nbad $\n    ^ near here");
    }

    #[test]
    fn comment_eof() {
        // Make sure we don't run off the end of the string.
        let mut l = Lexer::new("input", b"# foo");
        assert_eq!(l.read_token(), Token::Teof);
    }

    #[test]
    fn tabs() {
        let mut l = Lexer::new("input", b"   \tfoobar");
        assert_eq!(l.read_token(), Token::Indent);
        assert_eq!(l.read_token(), Token::Error);
        assert_eq!(l.describe_last_error(), "tabs are not allowed, use spaces");
    }

    #[test]
    fn keywords_vs_idents() {
        let mut l = Lexer::new("input", b"build buildfoo rule\n");
        assert_eq!(l.read_token(), Token::Build);
        assert_eq!(l.read_token(), Token::Ident);
        assert_eq!(l.read_token(), Token::Rule);
        assert_eq!(l.read_token(), Token::Newline);
        assert_eq!(l.read_token(), Token::Teof);
    }
}
