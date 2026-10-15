//! Manifest (`build.ninja`) parser, ported from ninja's `ManifestParser`.

use crate::hash::HashMap;

use crate::canon::canonicalize_path;
use crate::eval::EvalString;
use crate::graph::{NodeId, ROOT_SCOPE, Rule, ScopeId, State};
use crate::lexer::{Lexer, Token};

/// Reads manifest files (real disk or an in-memory fake in tests).
pub trait FileReader {
    /// Returns the file contents, or an error message like
    /// "No such file or directory".
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PhonyCycleAction {
    #[default]
    Warn,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct ParserOptions {
    pub phony_cycle_action: PhonyCycleAction,
    pub quiet: bool,
}

/// The version of ninja whose manifest language we implement, for
/// `ninja_required_version` checks and `--version`.
pub const NINJA_VERSION: &str = "1.13.1";

/// Starts the errors ninja reports as fatal (`ninja: fatal: ...`) rather
/// than as errors.
pub const FATAL: &str = "fatal: ";

/// Parses "major.minor[...]" like ninja's `ParseVersion` (atoi semantics).
pub fn parse_version(v: &str) -> (i32, i32) {
    fn atoi(s: &str) -> i32 {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().unwrap_or(0)
    }
    let major = atoi(v);
    let minor = match v.find('.') {
        Some(i) => atoi(&v[i + 1..]),
        None => 0,
    };
    (major, minor)
}

pub struct ManifestParser<'s, 'r> {
    pub state: &'s mut State,
    reader: &'r dyn FileReader,
    options: ParserOptions,
    pub warnings: Vec<String>,
}

impl<'s, 'r> ManifestParser<'s, 'r> {
    pub fn new(state: &'s mut State, reader: &'r dyn FileReader, options: ParserOptions) -> Self {
        Self { state, reader, options, warnings: Vec::new() }
    }

    /// Loads and parses `filename` into the root scope.
    pub fn load(&mut self, filename: &str) -> Result<(), String> {
        let contents = self.reader.read_file(filename).map_err(|e| format!("loading '{filename}': {e}"))?;
        self.parse(filename, &contents, ROOT_SCOPE)
    }

    /// Parses `input` as if it were a file named "input" (tests).
    pub fn parse_test(&mut self, input: &str) -> Result<(), String> {
        self.parse("input", input.as_bytes(), ROOT_SCOPE)
    }

    fn warn(&mut self, msg: String) {
        if !self.options.quiet {
            eprintln!("ninja: warning: {msg}");
        }
        self.warnings.push(msg);
    }

    pub fn parse(&mut self, filename: &str, input: &[u8], scope: ScopeId) -> Result<(), String> {
        let mut lexer = Lexer::new(filename, input);
        loop {
            let token = lexer.read_token();
            match token {
                Token::Pool => self.parse_pool(&mut lexer, scope)?,
                Token::Build => self.parse_edge(&mut lexer, scope)?,
                Token::Rule => self.parse_rule(&mut lexer, scope)?,
                Token::Default => self.parse_default(&mut lexer, scope)?,
                Token::Ident => {
                    lexer.unread_token();
                    let (name, value) = parse_let(&mut lexer)?;
                    let value = self.eval_in_scope(&value, scope);
                    if name == "ninja_required_version" {
                        check_ninja_version(&value)?;
                    }
                    self.state.add_binding(scope, &name, value);
                }
                Token::Include => self.parse_file_include(&mut lexer, scope, false)?,
                Token::Subninja => self.parse_file_include(&mut lexer, scope, true)?,
                Token::Error => return Err(lexer.error(lexer.describe_last_error())),
                Token::Teof => return Ok(()),
                Token::Newline => {}
                t => return Err(lexer.error(&format!("unexpected {}", t.name()))),
            }
        }
    }

    fn eval_in_scope(&self, e: &EvalString, scope: ScopeId) -> String {
        e.evaluate(&mut |v| self.state.lookup_variable(scope, v))
    }

    fn parse_pool(&mut self, lexer: &mut Lexer, scope: ScopeId) -> Result<(), String> {
        let name = lexer.read_ident().ok_or_else(|| lexer.error("expected pool name"))?;
        expect_token(lexer, Token::Newline)?;
        if self.state.lookup_pool(&name).is_some() {
            return Err(lexer.error(&format!("duplicate pool '{name}'")));
        }
        let mut depth: Option<usize> = None;
        while lexer.peek_token(Token::Indent) {
            let (key, value) = parse_let(lexer)?;
            if key == "depth" {
                let s = self.eval_in_scope(&value, scope);
                match s.parse::<i64>() {
                    Ok(d) if d >= 0 => depth = Some(d as usize),
                    _ => return Err(lexer.error("invalid pool depth")),
                }
            } else {
                return Err(lexer.error(&format!("unexpected variable '{key}'")));
            }
        }
        let depth = depth.ok_or_else(|| lexer.error("expected 'depth =' line"))?;
        self.state.add_pool(&name, depth);
        Ok(())
    }

    fn parse_rule(&mut self, lexer: &mut Lexer, scope: ScopeId) -> Result<(), String> {
        let name = lexer.read_ident().ok_or_else(|| lexer.error("expected rule name"))?;
        expect_token(lexer, Token::Newline)?;
        if self.state.lookup_rule_current_scope(scope, &name).is_some() {
            return Err(lexer.error(&format!("duplicate rule '{name}'")));
        }
        let mut bindings = HashMap::default();
        while lexer.peek_token(Token::Indent) {
            let (key, value) = parse_let(lexer)?;
            if Rule::is_reserved_binding(&key) {
                bindings.insert(key, value);
            } else {
                return Err(lexer.error(&format!("unexpected variable '{key}'")));
            }
        }
        let empty = |k: &str| bindings.get(k).is_none_or(|e: &EvalString| e.is_empty());
        if empty("rspfile") != empty("rspfile_content") {
            return Err(lexer.error("rspfile and rspfile_content need to be both specified"));
        }
        if empty("command") {
            return Err(lexer.error("expected 'command =' line"));
        }
        self.state.add_rule(scope, Rule { name, bindings, phony: false });
        Ok(())
    }

    fn parse_default(&mut self, lexer: &mut Lexer, scope: ScopeId) -> Result<(), String> {
        let mut eval = EvalString::new();
        lexer.read_path(&mut eval)?;
        if eval.is_empty() {
            return Err(lexer.error("expected target name"));
        }
        loop {
            let path = self.eval_in_scope(&eval, scope);
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let (path, _) = canonicalize_path(&path);
            self.state.add_default(&path).map_err(|e| lexer.error(&e))?;
            eval.clear();
            lexer.read_path(&mut eval)?;
            if eval.is_empty() {
                break;
            }
        }
        expect_token(lexer, Token::Newline)
    }

    fn read_paths(lexer: &mut Lexer, out: &mut Vec<EvalString>) -> Result<usize, String> {
        let mut n = 0;
        loop {
            let mut e = EvalString::new();
            lexer.read_path(&mut e)?;
            if e.is_empty() {
                return Ok(n);
            }
            out.push(e);
            n += 1;
        }
    }

    fn parse_edge(&mut self, lexer: &mut Lexer, scope: ScopeId) -> Result<(), String> {
        let mut outs = Vec::new();
        Self::read_paths(lexer, &mut outs)?;
        let mut implicit_outs = 0;
        if lexer.peek_token(Token::Pipe) {
            implicit_outs = Self::read_paths(lexer, &mut outs)?;
        }
        if outs.is_empty() {
            return Err(lexer.error("expected path"));
        }
        expect_token(lexer, Token::Colon)?;
        let rule_name = lexer.read_ident().ok_or_else(|| lexer.error("expected build command name"))?;
        let rule = self.state.lookup_rule(scope, &rule_name).ok_or_else(|| lexer.error(&format!("unknown build rule '{rule_name}'")))?;

        let mut ins = Vec::new();
        Self::read_paths(lexer, &mut ins)?;
        let mut implicit = 0;
        if lexer.peek_token(Token::Pipe) {
            implicit = Self::read_paths(lexer, &mut ins)?;
        }
        let mut order_only = 0;
        if lexer.peek_token(Token::Pipe2) {
            order_only = Self::read_paths(lexer, &mut ins)?;
        }
        let mut validations = Vec::new();
        if lexer.peek_token(Token::PipeAt) {
            Self::read_paths(lexer, &mut validations)?;
        }
        expect_token(lexer, Token::Newline)?;

        // Bindings on edges are rare, so allocate per-edge envs only when needed.
        let mut env: Option<HashMap<String, String>> = None;
        while lexer.peek_token(Token::Indent) {
            let (key, val) = parse_let(lexer)?;
            let v = val.into_evaluated(&mut |v| self.state.lookup_variable(scope, v));
            env.get_or_insert_with(HashMap::default).insert(key, v);
        }

        let edge = self.state.add_edge(rule, scope);
        self.state.edge_mut(edge).env = env;

        let pool_name = self.state.edge_binding(edge, "pool").map_err(|e| lexer.error(&e))?;
        if !pool_name.is_empty() {
            let pool = self.state.lookup_pool(&pool_name).ok_or_else(|| lexer.error(&format!("unknown pool name '{pool_name}'")))?;
            self.state.edge_mut(edge).pool = pool;
        }

        // Paths are evaluated in the edge's environment.
        let eval_edge = |state: &State, e: &EvalString| -> String { e.evaluate(&mut |v| edge_scope_lookup(state, edge, scope, v)) };
        for o in &outs {
            let path = eval_edge(self.state, o);
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let (path, bits) = canonicalize_path(&path);
            self.state.add_out(edge, &path, bits).map_err(|e| lexer.error(&e))?;
        }
        self.state.edge_mut(edge).implicit_outs = implicit_outs;

        for i in &ins {
            let path = eval_edge(self.state, i);
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let (path, bits) = canonicalize_path(&path);
            self.state.add_in(edge, &path, bits);
        }
        {
            let e = self.state.edge_mut(edge);
            e.implicit_deps = implicit;
            e.order_only_deps = order_only;
        }
        for v in &validations {
            let path = eval_edge(self.state, v);
            if path.is_empty() {
                return Err(lexer.error("empty path"));
            }
            let (path, bits) = canonicalize_path(&path);
            self.state.add_validation(edge, &path, bits);
        }

        if self.options.phony_cycle_action == PhonyCycleAction::Warn && self.state.maybe_phonycycle_diagnostic(edge) {
            // CMake 2.8.12.x and 3.0.x incorrectly write phony build statements
            // that reference themselves. Filter them out.
            let out = self.state.edge(edge).outputs[0];
            let before = self.state.edge(edge).inputs.len();
            self.state.edge_mut(edge).inputs.retain(|&i| i != out);
            if self.state.edge(edge).inputs.len() != before {
                self.state.node_mut(out).out_edges.retain(|&e| e != edge);
                let p = self.state.path(out).to_string();
                self.warn(format!("phony target '{p}' names itself as an input; ignoring [-w phonycycle=warn]"));
            }
        }

        // Look up, validate, and save any dyndep binding.
        let dyndep = self.state.unescaped_dyndep(edge);
        if !dyndep.is_empty() {
            let (path, bits) = canonicalize_path(&dyndep);
            let n: NodeId = self.state.get_node(&path, bits);
            self.state.node_mut(n).dyndep_pending = true;
            self.state.edge_mut(edge).dyndep = Some(n);
            if !self.state.edge(edge).inputs.contains(&n) {
                return Err(lexer.error(&format!("dyndep '{path}' is not an input")));
            }
        }
        Ok(())
    }

    fn parse_file_include(&mut self, lexer: &mut Lexer, scope: ScopeId, new_scope: bool) -> Result<(), String> {
        let mut eval = EvalString::new();
        lexer.read_path(&mut eval)?;
        let path = self.eval_in_scope(&eval, scope);
        let contents = self.reader.read_file(&path).map_err(|e| lexer.error(&format!("loading '{path}': {e}")))?;
        let sub_scope = if new_scope { self.state.new_scope(Some(scope)) } else { scope };
        self.parse(&path, &contents, sub_scope)?;
        expect_token(lexer, Token::Newline)
    }
}

/// Variable lookup for paths of a build statement: the edge's own bindings,
/// then the enclosing scope chain (no rule bindings, as in ninja where paths
/// are evaluated in the edge's `BindingEnv`).
fn edge_scope_lookup(state: &State, edge: crate::graph::EdgeId, scope: ScopeId, var: &str) -> String {
    if let Some(env) = &state.edge(edge).env
        && let Some(v) = env.get(var)
    {
        return v.clone();
    }
    state.lookup_variable(scope, var)
}

fn expect_token(lexer: &mut Lexer, expected: Token) -> Result<(), String> {
    let token = lexer.read_token();
    if token != expected {
        let msg = format!("expected {}, got {}{}", expected.name(), token.name(), expected.error_hint());
        return Err(lexer.error(&msg));
    }
    Ok(())
}

pub(crate) fn parse_let(lexer: &mut Lexer) -> Result<(String, EvalString), String> {
    let key = lexer.read_ident().ok_or_else(|| lexer.error("expected variable name"))?;
    expect_token(lexer, Token::Equals)?;
    let mut value = EvalString::new();
    lexer.read_var_value(&mut value)?;
    Ok((key, value))
}

pub(crate) fn expect(lexer: &mut Lexer, t: Token) -> Result<(), String> {
    expect_token(lexer, t)
}

fn check_ninja_version(required: &str) -> Result<(), String> {
    let (bin_major, bin_minor) = parse_version(NINJA_VERSION);
    let (file_major, file_minor) = parse_version(required);
    if bin_major > file_major {
        // Newer major versions may break compatibility; warn only.
        eprintln!(
            "ninja: warning: ninja executable version ({NINJA_VERSION}) greater than build file ninja_required_version ({required}); versions may be incompatible."
        );
        return Ok(());
    }
    if (bin_major == file_major && bin_minor < file_minor) || bin_major < file_major {
        return Err(format!("{FATAL}ninja version ({NINJA_VERSION}) incompatible with build file ninja_required_version version ({required})."));
    }
    Ok(())
}

/// A `FileReader` over an in-memory map (tests).
#[derive(Default)]
pub struct MemFiles {
    pub files: HashMap<String, Vec<u8>>,
    pub reads: std::cell::RefCell<Vec<String>>,
}

impl MemFiles {
    pub fn create(&mut self, path: &str, contents: &str) {
        self.files.insert(path.to_string(), contents.as_bytes().to_vec());
    }
}

impl FileReader for MemFiles {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.reads.borrow_mut().push(path.to_string());
        self.files.get(path).cloned().ok_or_else(|| "No such file or directory".to_string())
    }
}

/// Reads from the real filesystem.
pub struct RealFileReader;

impl FileReader for RealFileReader {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
            _ => e.to_string(),
        })
    }
}
