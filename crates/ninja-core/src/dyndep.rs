//! Dyndep files: dynamically discovered implicit inputs and outputs of
//! edges (ninja's `DyndepParser` and `DyndepLoader`). CMake uses them for
//! C++20 modules and Fortran.

use crate::hash::HashMap;

use crate::canon::canonicalize_path;
use crate::eval::EvalString;
use crate::graph::{EdgeId, NodeId, State};
use crate::lexer::{Lexer, Token};
use crate::parser::{expect, parse_let, parse_version};

#[derive(Debug, Default, Clone)]
pub struct Dyndeps {
    pub used: bool,
    pub restat: bool,
    pub implicit_inputs: Vec<NodeId>,
    pub implicit_outputs: Vec<NodeId>,
}

pub type DyndepFile = HashMap<EdgeId, Dyndeps>;

/// Parses dyndep file contents into `ddf`, creating nodes as needed.
pub fn parse_dyndep(state: &mut State, filename: &str, input: &[u8], ddf: &mut DyndepFile) -> Result<(), String> {
    let mut lexer = Lexer::new(filename, input);
    let mut have_version = false;
    loop {
        let token = lexer.read_token();
        match token {
            Token::Build => {
                if !have_version {
                    return Err(lexer.error("expected 'ninja_dyndep_version = ...'"));
                }
                parse_edge(state, &mut lexer, ddf)?;
            }
            Token::Ident => {
                lexer.unread_token();
                if have_version {
                    return Err(lexer.error(&format!("unexpected {}", token.name())));
                }
                let (name, value) = parse_let(&mut lexer)?;
                if name != "ninja_dyndep_version" {
                    return Err(lexer.error("expected 'ninja_dyndep_version = ...'"));
                }
                let version = value.evaluate(&mut |_| String::new());
                let (major, minor) = parse_version(&version);
                if major != 1 || minor != 0 {
                    return Err(lexer.error(&format!("unsupported 'ninja_dyndep_version = {version}'")));
                }
                have_version = true;
            }
            Token::Error => return Err(lexer.error(lexer.describe_last_error())),
            Token::Teof => {
                if !have_version {
                    return Err(lexer.error("expected 'ninja_dyndep_version = ...'"));
                }
                return Ok(());
            }
            Token::Newline => {}
            t => return Err(lexer.error(&format!("unexpected {}", t.name()))),
        }
    }
}

fn read_path_list(lexer: &mut Lexer) -> Result<Vec<EvalString>, String> {
    let mut v = Vec::new();
    loop {
        let mut e = EvalString::new();
        lexer.read_path(&mut e)?;
        if e.is_empty() {
            return Ok(v);
        }
        v.push(e);
    }
}

fn eval(e: &EvalString) -> String {
    // Dyndep files have no variables other than the version.
    e.evaluate(&mut |_| String::new())
}

fn parse_edge(state: &mut State, lexer: &mut Lexer, ddf: &mut DyndepFile) -> Result<(), String> {
    // One explicit output, which must already have an edge.
    let mut out0 = EvalString::new();
    lexer.read_path(&mut out0)?;
    if out0.is_empty() {
        return Err(lexer.error("expected path"));
    }
    let path = eval(&out0);
    if path.is_empty() {
        return Err(lexer.error("empty path"));
    }
    let (path, _) = canonicalize_path(&path);
    let edge = match state.lookup_node(&path).and_then(|n| state.node(n).in_edge) {
        Some(e) => e,
        None => return Err(lexer.error(&format!("no build statement exists for '{path}'"))),
    };
    if ddf.contains_key(&edge) {
        return Err(lexer.error(&format!("multiple statements for '{path}'")));
    }
    let mut out = EvalString::new();
    lexer.read_path(&mut out)?;
    if !out.is_empty() {
        return Err(lexer.error("explicit outputs not supported"));
    }
    let outs = if lexer.peek_token(Token::Pipe) { read_path_list(lexer)? } else { Vec::new() };
    expect(lexer, Token::Colon)?;
    match lexer.read_ident() {
        Some(r) if r == "dyndep" => {}
        _ => return Err(lexer.error("expected build command name 'dyndep'")),
    }
    let mut inp = EvalString::new();
    lexer.read_path(&mut inp)?;
    if !inp.is_empty() {
        return Err(lexer.error("explicit inputs not supported"));
    }
    let ins = if lexer.peek_token(Token::Pipe) { read_path_list(lexer)? } else { Vec::new() };
    if lexer.peek_token(Token::Pipe2) {
        return Err(lexer.error("order-only inputs not supported"));
    }
    expect(lexer, Token::Newline)?;
    let mut dd = Dyndeps::default();
    if lexer.peek_token(Token::Indent) {
        let (key, val) = parse_let(lexer)?;
        if key != "restat" {
            return Err(lexer.error("binding is not 'restat'"));
        }
        dd.restat = !eval(&val).is_empty();
    }
    for i in &ins {
        let p = eval(i);
        if p.is_empty() {
            return Err(lexer.error("empty path"));
        }
        let (p, bits) = canonicalize_path(&p);
        dd.implicit_inputs.push(state.get_node(&p, bits));
    }
    for o in &outs {
        let p = eval(o);
        if p.is_empty() {
            return Err(lexer.error("empty path"));
        }
        let (p, bits) = canonicalize_path(&p);
        dd.implicit_outputs.push(state.get_node(&p, bits));
    }
    ddf.insert(edge, dd);
    Ok(())
}

/// Applies dyndep information loaded from `node` to the edges that name it
/// in their `dyndep` binding (ninja's `DyndepLoader::LoadDyndeps`).
pub fn apply_dyndeps(state: &mut State, node: NodeId, ddf: &mut DyndepFile) -> Result<(), String> {
    state.node_mut(node).dyndep_pending = false;
    let out_edges = state.node(node).out_edges.clone();
    for edge in out_edges {
        if state.edge(edge).dyndep != Some(node) {
            continue;
        }
        let Some(dd) = ddf.get_mut(&edge) else {
            let out0 = state.path(state.edge(edge).outputs[0]).to_string();
            return Err(format!("'{out0}' not mentioned in its dyndep file '{}'", state.path(node)));
        };
        dd.used = true;
        let dd = dd.clone();
        update_edge(state, edge, &dd)?;
    }
    for (edge, dd) in ddf.iter() {
        if !dd.used {
            let out0 = state.path(state.edge(*edge).outputs[0]).to_string();
            return Err(format!(
                "dyndep file '{}' mentions output '{out0}' whose build statement does not have a dyndep binding for the file",
                state.path(node)
            ));
        }
    }
    Ok(())
}

fn update_edge(state: &mut State, edge: EdgeId, dd: &Dyndeps) -> Result<(), String> {
    if dd.restat {
        state.edge_mut(edge).env.get_or_insert_with(HashMap::default).insert("restat".into(), "1".into());
    }
    for &n in &dd.implicit_outputs {
        if state.node(n).in_edge.is_some() {
            return Err(format!("multiple rules generate {}", state.path(n)));
        }
        state.node_mut(n).in_edge = Some(edge);
    }
    {
        let e = state.edge_mut(edge);
        e.outputs.extend_from_slice(&dd.implicit_outputs);
        e.implicit_outs += dd.implicit_outputs.len();
        let pos = e.inputs.len() - e.order_only_deps;
        e.inputs.splice(pos..pos, dd.implicit_inputs.iter().copied());
        e.implicit_deps += dd.implicit_inputs.len();
    }
    for &n in &dd.implicit_inputs {
        state.node_mut(n).out_edges.push(edge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ManifestParser, MemFiles, ParserOptions};

    fn setup() -> State {
        let mut state = State::new();
        let files = MemFiles::default();
        ManifestParser::new(&mut state, &files, ParserOptions::default())
            .parse_test("rule touch\n  command = touch $out\nbuild out otherout: touch\n")
            .unwrap();
        state
    }

    fn parse(state: &mut State, input: &str) -> Result<DyndepFile, String> {
        let mut ddf = DyndepFile::default();
        parse_dyndep(state, "input", input.as_bytes(), &mut ddf)?;
        Ok(ddf)
    }

    #[test]
    fn empty() {
        let mut s = setup();
        assert_eq!(parse(&mut s, "").unwrap_err(), "input:1: expected 'ninja_dyndep_version = ...'\n");
    }

    #[test]
    fn version() {
        let mut s = setup();
        assert!(parse(&mut s, "ninja_dyndep_version = 1\n").unwrap().is_empty());
        assert!(parse(&mut s, "ninja_dyndep_version = 1.0\n").unwrap().is_empty());
        assert!(parse(&mut s, "ninja_dyndep_version = 1-extra\n").is_ok());
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 0\n").unwrap_err(),
            "input:1: unsupported 'ninja_dyndep_version = 0'\nninja_dyndep_version = 0\n                        ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1.1\n").unwrap_err(),
            "input:1: unsupported 'ninja_dyndep_version = 1.1'\nninja_dyndep_version = 1.1\n                          ^ near here"
        );
    }

    #[test]
    fn errors() {
        let mut s = setup();
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild missing: dyndep\n").unwrap_err(),
            "input:2: no build statement exists for 'missing'\nbuild missing: dyndep\n             ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild out: dyndep\nbuild out: dyndep\n").unwrap_err(),
            "input:3: multiple statements for 'out'\nbuild out: dyndep\n         ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild out: dyndep in\n").unwrap_err(),
            "input:2: explicit inputs not supported\nbuild out: dyndep in\n                    ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild out out2: dyndep\n").unwrap_err(),
            "input:2: explicit outputs not supported\nbuild out out2: dyndep\n              ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild out: dyndep || in\n").unwrap_err(),
            "input:2: order-only inputs not supported\nbuild out: dyndep || in\n                  ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild out: touch\n").unwrap_err(),
            "input:2: expected build command name 'dyndep'\nbuild out: touch\n           ^ near here"
        );
        assert_eq!(
            parse(&mut s, "ninja_dyndep_version = 1\nbuild out: dyndep\n  not_restat = 1\n").unwrap_err(),
            "input:3: binding is not 'restat'\n  not_restat = 1\n                ^ near here"
        );
    }

    #[test]
    fn implicit_in_and_out_and_restat() {
        let mut s = setup();
        let ddf = parse(&mut s, "ninja_dyndep_version = 1\nbuild out | impout: dyndep | impin\n  restat = 1\n").unwrap();
        let e = s.node(s.lookup_node("out").unwrap()).in_edge.unwrap();
        let dd = &ddf[&e];
        assert!(dd.restat);
        assert_eq!(dd.implicit_outputs.len(), 1);
        assert_eq!(s.path(dd.implicit_outputs[0]), "impout");
        assert_eq!(s.path(dd.implicit_inputs[0]), "impin");
    }

    #[test]
    fn other_output() {
        let mut s = setup();
        let ddf = parse(&mut s, "ninja_dyndep_version = 1\nbuild otherout: dyndep\n").unwrap();
        assert_eq!(ddf.len(), 1);
    }

    #[test]
    fn apply_updates_edge() {
        let mut state = State::new();
        let files = MemFiles::default();
        ManifestParser::new(&mut state, &files, ParserOptions::default())
            .parse_test("rule r\n  command = r $in\nbuild out: r in || dd\n  dyndep = dd\nbuild in: r\nbuild dd: r\n")
            .unwrap();
        let mut ddf = DyndepFile::default();
        parse_dyndep(&mut state, "dd", b"ninja_dyndep_version = 1\nbuild out | out.imp: dyndep | in.imp\n  restat = 1\n", &mut ddf).unwrap();
        let dd = state.lookup_node("dd").unwrap();
        apply_dyndeps(&mut state, dd, &mut ddf).unwrap();
        let e = state.node(state.lookup_node("out").unwrap()).in_edge.unwrap();
        let edge = state.edge(e);
        let ins: Vec<&str> = edge.inputs.iter().map(|&n| state.path(n)).collect();
        assert_eq!(ins, vec!["in", "in.imp", "dd"]);
        assert_eq!(edge.implicit_deps, 1);
        assert_eq!(edge.order_only_deps, 1);
        let outs: Vec<&str> = edge.outputs.iter().map(|&n| state.path(n)).collect();
        assert_eq!(outs, vec!["out", "out.imp"]);
        assert!(state.edge_binding_bool(e, "restat"));
        assert!(!state.node(dd).dyndep_pending);
    }

    #[test]
    fn missing_edge_in_dyndep_file() {
        let mut state = State::new();
        let files = MemFiles::default();
        ManifestParser::new(&mut state, &files, ParserOptions::default())
            .parse_test("rule r\n  command = r\nbuild out1: r || dd\n  dyndep = dd\nbuild out2: r\n")
            .unwrap();
        let mut ddf = DyndepFile::default();
        parse_dyndep(&mut state, "dd", b"ninja_dyndep_version = 1\n", &mut ddf).unwrap();
        let dd = state.lookup_node("dd").unwrap();
        assert_eq!(apply_dyndeps(&mut state, dd, &mut ddf).unwrap_err(), "'out1' not mentioned in its dyndep file 'dd'");
        let mut ddf = DyndepFile::default();
        parse_dyndep(&mut state, "dd", b"ninja_dyndep_version = 1\nbuild out1: dyndep\nbuild out2: dyndep\n", &mut ddf).unwrap();
        assert_eq!(
            apply_dyndeps(&mut state, dd, &mut ddf).unwrap_err(),
            "dyndep file 'dd' mentions output 'out2' whose build statement does not have a dyndep binding for the file"
        );
    }
}
