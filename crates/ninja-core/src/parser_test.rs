//! Manifest parser tests, ported from ninja's manifest_parser_test.cc.

use crate::graph::{ROOT_SCOPE, State};
use crate::parser::{ManifestParser, MemFiles, ParserOptions, PhonyCycleAction};

fn quiet() -> ParserOptions {
    ParserOptions { quiet: true, ..Default::default() }
}

fn parse_with(state: &mut State, files: &MemFiles, input: &str) -> Result<(), String> {
    ManifestParser::new(state, files, quiet()).parse_test(input)
}

fn assert_parse(input: &str) -> State {
    let mut state = State::new();
    let files = MemFiles::default();
    parse_with(&mut state, &files, input).unwrap();
    verify_graph(&state);
    state
}

fn parse_err(input: &str) -> String {
    let mut state = State::new();
    let files = MemFiles::default();
    parse_with(&mut state, &files, input).unwrap_err()
}

/// Checks graph invariants, like ninja's `VerifyGraph`.
fn verify_graph(state: &State) {
    for e in &state.edges {
        assert!(!e.outputs.is_empty(), "edge with no outputs");
        for &i in &e.inputs {
            assert!(state.node(i).out_edges.contains(&e.id), "in-node lacks out edge");
        }
        for &o in &e.outputs {
            assert_eq!(state.node(o).in_edge, Some(e.id), "out-node lacks in edge");
        }
    }
    for n in &state.nodes {
        if let Some(e) = n.in_edge {
            assert!(state.edge(e).outputs.iter().any(|&o| state.path(o) == n.path));
        }
    }
}

fn cmd(state: &State, i: usize) -> String {
    state.evaluate_command(crate::graph::EdgeId(i as u32), false).unwrap()
}

fn edge_of(state: &State, path: &str) -> crate::graph::EdgeId {
    state.node(state.lookup_node(path).unwrap()).in_edge.unwrap()
}

#[test]
fn empty() {
    assert_parse("");
}

#[test]
fn rules() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\n\nrule date\n  command = date > $out\n\nbuild result: cat in_1.cc in-2.O\n");
    assert_eq!(s.rules.len(), 3);
    let r = &s.rules[1];
    assert_eq!(r.name, "cat");
    assert_eq!(r.bindings["command"].serialize(), "[cat ][$in][ > ][$out]");
}

#[test]
fn rule_attributes() {
    assert_parse(
        "rule cat\n  command = a\n  depfile = a\n  deps = a\n  description = a\n  generator = a\n  restat = a\n  rspfile = a\n  rspfile_content = a\n",
    );
}

#[test]
fn ignore_indented_comments() {
    let s = assert_parse(
        "  #indented comment\nrule cat\n  command = cat $in > $out\n  #generator = 1\n  restat = 1 # comment\n  #comment\nbuild result: cat in_1.cc in-2.O\n  #comment\n",
    );
    assert_eq!(s.rules.len(), 2);
    let e = edge_of(&s, "result");
    assert!(s.edge_binding_bool(e, "restat"));
    assert!(!s.edge_binding_bool(e, "generator"));
}

#[test]
fn ignore_indented_blank_lines() {
    let s = assert_parse("  \nrule cat\n  command = cat $in > $out\n  \nbuild result: cat in_1.cc in-2.O\n  \nvariable=1\n");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "variable"), "1");
}

#[test]
fn response_files() {
    let s = assert_parse(
        "rule cat_rsp\n  command = cat $rspfile > $out\n  rspfile = $rspfile\n  rspfile_content = $in\n\nbuild out: cat_rsp in\n  rspfile=out.rsp\n",
    );
    let r = &s.rules[1];
    assert_eq!(r.bindings["command"].serialize(), "[cat ][$rspfile][ > ][$out]");
    assert_eq!(r.bindings["rspfile"].serialize(), "[$rspfile]");
    assert_eq!(r.bindings["rspfile_content"].serialize(), "[$in]");
}

#[test]
fn in_newline() {
    let s = assert_parse("rule cat_rsp\n  command = cat $in_newline > $out\n\nbuild out: cat_rsp in in2\n  rspfile=out.rsp\n");
    assert_eq!(cmd(&s, 0), "cat in\nin2 > out");
}

#[test]
fn variables() {
    let s = assert_parse(
        "l = one-letter-test\nrule link\n  command = ld $l $extra $with_under -o $out $in\n\nextra = -pthread\nwith_under = -under\nbuild a: link b c\nnested1 = 1\nnested2 = $nested1/2\nbuild supernested: link x\n  extra = $nested2/3\n",
    );
    assert_eq!(s.edges.len(), 2);
    assert_eq!(cmd(&s, 0), "ld one-letter-test -pthread -under -o a b c");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "nested2"), "1/2");
    assert_eq!(cmd(&s, 1), "ld one-letter-test 1/2/3 -under -o supernested x");
}

#[test]
fn variable_scope() {
    let s = assert_parse("foo = bar\nrule cmd\n  command = cmd $foo $in $out\n\nbuild inner: cmd a\n  foo = baz\nbuild outer: cmd b\n\n");
    assert_eq!(cmd(&s, 0), "cmd baz a inner");
    assert_eq!(cmd(&s, 1), "cmd bar b outer");
}

#[test]
fn continuation() {
    let s = assert_parse("rule link\n  command = foo bar $\n    baz\n\nbuild a: link c $\n d e f\n");
    assert_eq!(s.rules[1].bindings["command"].serialize(), "[foo bar baz]");
}

#[test]
fn backslash() {
    let s = assert_parse("foo = bar\\baz\nfoo2 = bar\\ baz\n");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "foo"), "bar\\baz");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "foo2"), "bar\\ baz");
}

#[test]
fn comment() {
    let s = assert_parse("# this is a comment\nfoo = not # a comment\n");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "foo"), "not # a comment");
}

#[test]
fn dollars() {
    let s = assert_parse("rule foo\n  command = ${out}bar$$baz$$$\nblah\nx = $$dollar\nbuild $x: foo y\n");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "x"), "$dollar");
    if cfg!(windows) {
        assert_eq!(cmd(&s, 0), "$dollarbar$baz$blah");
    } else {
        assert_eq!(cmd(&s, 0), "'$dollar'bar$baz$blah");
    }
}

#[test]
fn escape_spaces() {
    let s = assert_parse("rule spaces\n  command = something\nbuild foo$ bar: spaces $$one two$$$ three\n");
    assert!(s.lookup_node("foo bar").is_some());
    let e = &s.edges[0];
    assert_eq!(s.path(e.outputs[0]), "foo bar");
    assert_eq!(s.path(e.inputs[0]), "$one");
    assert_eq!(s.path(e.inputs[1]), "two$ three");
    assert_eq!(cmd(&s, 0), "something");
}

#[test]
fn canonicalize_file() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild out: cat in/1 in//2\nbuild in/1: cat\nbuild in/2: cat\n");
    assert!(s.lookup_node("in/1").is_some());
    assert!(s.lookup_node("in/2").is_some());
    assert!(s.lookup_node("in//1").is_none());
    assert!(s.lookup_node("in//2").is_none());
}

#[cfg(windows)]
#[test]
fn canonicalize_file_backslashes() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild out: cat in\\1 in\\\\2\nbuild in\\1: cat\nbuild in\\2: cat\n");
    let n = s.lookup_node("in/1").unwrap();
    assert_eq!(s.node(n).slash_bits, 1);
    let n = s.lookup_node("in/2").unwrap();
    assert_eq!(s.node(n).slash_bits, 1);
    assert!(s.lookup_node("in//1").is_none());
}

#[test]
fn path_variables() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\ndir = out\nbuild $dir/exe: cat src\n");
    assert!(s.lookup_node("$dir/exe").is_none());
    assert!(s.lookup_node("out/exe").is_some());
}

#[test]
fn canonicalize_paths() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild ./out.o: cat ./bar/baz/../foo.cc\n");
    assert!(s.lookup_node("./out.o").is_none());
    assert!(s.lookup_node("out.o").is_some());
    assert!(s.lookup_node("./bar/baz/../foo.cc").is_none());
    assert!(s.lookup_node("bar/foo.cc").is_some());
}

#[test]
fn duplicate_edge_with_multiple_outputs_error() {
    assert_eq!(
        parse_err("rule cat\n  command = cat $in > $out\nbuild out1 out2: cat in1\nbuild out1: cat in2\nbuild final: cat out1\n"),
        "input:5: multiple rules generate out1\n"
    );
}

#[test]
fn duplicate_edge_in_included_file() {
    let mut files = MemFiles::default();
    files.create("sub.ninja", "rule cat\n  command = cat $in > $out\nbuild out1 out2: cat in1\nbuild out1: cat in2\nbuild final: cat out1\n");
    let mut state = State::new();
    assert_eq!(parse_with(&mut state, &files, "subninja sub.ninja\n").unwrap_err(), "sub.ninja:5: multiple rules generate out1\n");
}

#[test]
fn phony_self_reference_ignored() {
    let s = assert_parse("build a: phony a\n");
    let n = s.lookup_node("a").unwrap();
    let e = s.node(n).in_edge.unwrap();
    assert!(s.edge(e).inputs.is_empty());
    assert!(s.node(n).out_edges.is_empty());
}

#[test]
fn phony_self_reference_with_order_only_input_kept() {
    let s = assert_parse("build a: phony b c || a\n");
    let n = s.lookup_node("a").unwrap();
    let e = s.edge(s.node(n).in_edge.unwrap());
    assert_eq!(e.inputs.len(), 3);
    assert_eq!(e.inputs[2], n);
    assert_eq!(e.order_only_deps, 1);
}

#[test]
fn phony_self_reference_kept() {
    let mut state = State::new();
    let files = MemFiles::default();
    let opts = ParserOptions { phony_cycle_action: PhonyCycleAction::Error, quiet: true };
    ManifestParser::new(&mut state, &files, opts).parse_test("build a: phony a\n").unwrap();
    let n = state.lookup_node("a").unwrap();
    let e = state.edge(state.node(n).in_edge.unwrap());
    assert_eq!(e.inputs, vec![n]);
}

#[test]
fn reserved_words() {
    assert_parse("rule build\n  command = rule run $out\nbuild subninja: build include default foo.cc\ndefault subninja\n");
}

#[test]
fn errors() {
    let cases: &[(&str, &str)] = &[
        ("subn", "input:1: expected '=', got eof\nsubn\n    ^ near here"),
        ("foobar", "input:1: expected '=', got eof\nfoobar\n      ^ near here"),
        ("x 3", "input:1: expected '=', got identifier\nx 3\n  ^ near here"),
        ("x = 3", "input:1: unexpected EOF\nx = 3\n     ^ near here"),
        ("x = 3\ny 2", "input:2: expected '=', got identifier\ny 2\n  ^ near here"),
        ("x = $", "input:1: bad $-escape (literal $ must be written as $$)\nx = $\n    ^ near here"),
        ("x = $\n $[\n", "input:2: bad $-escape (literal $ must be written as $$)\n $[\n ^ near here"),
        ("x = a$\n b$\n $\n", "input:4: unexpected EOF\n"),
        ("build\n", "input:1: expected path\nbuild\n     ^ near here"),
        ("build x: y z\n", "input:1: unknown build rule 'y'\nbuild x: y z\n         ^ near here"),
        ("build x:: y z\n", "input:1: expected build command name\nbuild x:: y z\n        ^ near here"),
        ("rule cat\n  command = cat ok\nbuild x: cat $\n :\n", "input:4: expected newline, got ':'\n :\n ^ near here"),
        ("rule cat\n", "input:2: expected 'command =' line\n"),
        ("rule cat\n  command = echo\nrule cat\n  command = echo\n", "input:3: duplicate rule 'cat'\nrule cat\n        ^ near here"),
        ("rule cat\n  command = echo\n  rspfile = cat.rsp\n", "input:4: rspfile and rspfile_content need to be both specified\n"),
        ("rule cat\n  command = ${fafsd\nfoo = bar\n", "input:2: bad $-escape (literal $ must be written as $$)\n  command = ${fafsd\n            ^ near here"),
        ("rule cat\n  command = cat\nbuild $.: cat foo\n", "input:3: bad $-escape (literal $ must be written as $$)\nbuild $.: cat foo\n      ^ near here"),
        (
            "rule cat\n  command = cat\nbuild $: cat foo\n",
            "input:3: expected ':', got newline ($ also escapes ':')\nbuild $: cat foo\n                ^ near here",
        ),
        ("rule %foo\n", "input:1: expected rule name\nrule %foo\n     ^ near here"),
        ("rule cc\n  command = foo\n  othervar = bar\n", "input:3: unexpected variable 'othervar'\n  othervar = bar\n                ^ near here"),
        ("rule cc\n  command = foo\n  && bar", "input:3: expected variable name\n  && bar\n  ^ near here"),
        ("default\n", "input:1: expected target name\ndefault\n       ^ near here"),
        ("default nonexistent\n", "input:1: unknown target 'nonexistent'\ndefault nonexistent\n                   ^ near here"),
        ("rule r\n  command = r\nbuild b: r\ndefault b:\n", "input:4: expected newline, got ':'\ndefault b:\n         ^ near here"),
        ("default $a\n", "input:1: empty path\ndefault $a\n          ^ near here"),
        ("rule r\n  command = r\nbuild $a: r $c\n", "input:4: empty path\n"),
        ("rule r\n  command = r\n  \n  generator = 1\n", "input:4: unexpected indent\n"),
        ("pool\n", "input:1: expected pool name\npool\n    ^ near here"),
        ("pool foo\n", "input:2: expected 'depth =' line\n"),
        ("pool foo\n  depth = 4\npool foo\n", "input:3: duplicate pool 'foo'\npool foo\n        ^ near here"),
        ("pool foo\n  depth = -1\n", "input:2: invalid pool depth\n  depth = -1\n            ^ near here"),
        ("pool foo\n  bar = 1\n", "input:2: unexpected variable 'bar'\n  bar = 1\n         ^ near here"),
        ("rule run\n  command = echo\n  pool = unnamed_pool\nbuild out: run in\n", "input:5: unknown pool name 'unnamed_pool'\n"),
    ];
    for (input, expected) in cases {
        assert_eq!(&parse_err(input), expected, "input: {input:?}");
    }
}

#[test]
fn missing_input() {
    let mut state = State::new();
    let files = MemFiles::default();
    let err = ManifestParser::new(&mut state, &files, quiet()).load("build.ninja").unwrap_err();
    assert_eq!(err, "loading 'build.ninja': No such file or directory");
}

#[test]
fn multiple_outputs() {
    assert_parse("rule cc\n  command = foo\n  depfile = bar\nbuild a.o b.o: cc c.cc\n");
    assert_parse("rule cc\n  command = foo\n  deps = gcc\nbuild a.o b.o: cc c.cc\n");
}

#[test]
fn sub_ninja() {
    let mut files = MemFiles::default();
    files.create("test.ninja", "var = inner\nbuild $builddir/inner: varref\n");
    let mut s = State::new();
    parse_with(
        &mut s,
        &files,
        "builddir = some_dir/\nrule varref\n  command = varref $var\nvar = outer\nbuild $builddir/outer: varref\nsubninja test.ninja\nbuild $builddir/outer2: varref\n",
    )
    .unwrap();
    assert_eq!(*files.reads.borrow(), vec!["test.ninja"]);
    assert!(s.lookup_node("some_dir/outer").is_some());
    assert!(s.lookup_node("some_dir/inner").is_some());
    assert_eq!(s.edges.len(), 3);
    assert_eq!(cmd(&s, 0), "varref outer");
    assert_eq!(cmd(&s, 1), "varref inner");
    assert_eq!(cmd(&s, 2), "varref outer");
}

#[test]
fn missing_sub_ninja() {
    assert_eq!(parse_err("subninja foo.ninja\n"), "input:1: loading 'foo.ninja': No such file or directory\nsubninja foo.ninja\n                  ^ near here");
}

#[test]
fn duplicate_rule_in_different_subninjas() {
    let mut files = MemFiles::default();
    files.create("test.ninja", "rule cat\n  command = cat\n");
    let mut s = State::new();
    parse_with(&mut s, &files, "rule cat\n  command = cat\nsubninja test.ninja\n").unwrap();
}

#[test]
fn duplicate_rule_in_different_subninjas_with_include() {
    let mut files = MemFiles::default();
    files.create("rules.ninja", "rule cat\n  command = cat\n");
    files.create("test.ninja", "include rules.ninja\nbuild x : cat\n");
    let mut s = State::new();
    parse_with(&mut s, &files, "include rules.ninja\nsubninja test.ninja\nbuild y : cat\n").unwrap();
}

#[test]
fn include() {
    let mut files = MemFiles::default();
    files.create("include.ninja", "var = inner\n");
    let mut s = State::new();
    parse_with(&mut s, &files, "var = outer\ninclude include.ninja\n").unwrap();
    assert_eq!(*files.reads.borrow(), vec!["include.ninja"]);
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "var"), "inner");
}

#[test]
fn broken_include() {
    let mut files = MemFiles::default();
    files.create("include.ninja", "build\n");
    let mut s = State::new();
    assert_eq!(parse_with(&mut s, &files, "include include.ninja\n").unwrap_err(), "include.ninja:1: expected path\nbuild\n     ^ near here");
}

#[test]
fn implicit() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild foo: cat bar | baz\n");
    assert!(s.edge(edge_of(&s, "foo")).is_implicit(1));
}

#[test]
fn order_only() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild foo: cat bar || baz\n");
    assert!(s.edge(edge_of(&s, "foo")).is_order_only(1));
}

#[test]
fn validations() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild foo: cat bar |@ baz\n");
    let e = s.edge(edge_of(&s, "foo"));
    assert_eq!(e.validations.len(), 1);
    assert_eq!(s.path(e.validations[0]), "baz");
}

#[test]
fn implicit_output() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild foo | imp: cat bar\n");
    let e = s.edge(edge_of(&s, "imp"));
    assert_eq!(e.outputs.len(), 2);
    assert!(e.is_implicit_out(1));
}

#[test]
fn implicit_output_empty() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild foo | : cat bar\n");
    let e = s.edge(edge_of(&s, "foo"));
    assert_eq!(e.outputs.len(), 1);
    assert!(!e.is_implicit_out(0));
}

#[test]
fn implicit_output_dupe_error() {
    assert_eq!(
        parse_err("rule cat\n  command = cat $in > $out\nbuild foo baz | foo baq foo: cat bar\n"),
        "input:4: foo is defined as an output multiple times\n"
    );
    assert_eq!(
        parse_err("rule cat\n  command = cat $in > $out\nbuild foo foo foo | foo foo foo foo: cat bar\n"),
        "input:4: foo is defined as an output multiple times\n"
    );
}

#[test]
fn no_explicit_output() {
    assert_parse("rule cat\n  command = cat $in > $out\nbuild | imp : cat bar\n");
}

#[test]
fn default_default() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild a: cat foo\nbuild b: cat foo\nbuild c: cat foo\nbuild d: cat foo\n");
    assert_eq!(s.default_nodes().unwrap().len(), 4);
}

#[test]
fn default_default_cycle() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild a: cat a\n");
    assert_eq!(s.default_nodes().unwrap_err(), "could not determine root nodes of build graph");
}

#[test]
fn default_statements() {
    let s = assert_parse(
        "rule cat\n  command = cat $in > $out\nbuild a: cat foo\nbuild b: cat foo\nbuild c: cat foo\nbuild d: cat foo\nthird = c\ndefault a b\ndefault $third\n",
    );
    let nodes = s.default_nodes().unwrap();
    let names: Vec<&str> = nodes.iter().map(|&n| s.path(n)).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn utf8() {
    assert_parse("rule utf8\n  command = true\n  description = compilaci\u{f3}\n");
}

#[test]
fn crlf() {
    assert_parse("# comment with crlf\r\n");
    assert_parse("foo = foo\nbar = bar\r\n");
    assert_parse("pool link_pool\r\n  depth = 15\r\n\r\nrule xyz\r\n  command = something$expand \r\n  description = YAY!\r\n");
}

#[test]
fn dyndep_not_specified() {
    let s = assert_parse("rule cat\n  command = cat $in > $out\nbuild result: cat in\n");
    assert!(s.edge(edge_of(&s, "result")).dyndep.is_none());
}

#[test]
fn dyndep_not_input() {
    assert_eq!(parse_err("rule touch\n  command = touch $out\nbuild result: touch\n  dyndep = notin\n"), "input:5: dyndep 'notin' is not an input\n");
}

#[test]
fn dyndep_inputs() {
    for input in [
        "rule cat\n  command = cat $in > $out\nbuild result: cat in\n  dyndep = in\n",
        "rule cat\n  command = cat $in > $out\nbuild result: cat in | dd\n  dyndep = dd\n",
        "rule cat\n  command = cat $in > $out\nbuild result: cat in || dd\n  dyndep = dd\n",
        "rule cat\n  command = cat $in > $out\n  dyndep = $in\nbuild result: cat in\n",
    ] {
        let s = assert_parse(input);
        let e = s.edge(edge_of(&s, "result"));
        let dd = e.dyndep.unwrap();
        assert!(s.node(dd).dyndep_pending);
    }
}

#[test]
fn lookup_variable() {
    let s = assert_parse("foo = World\nbar = Hello $foo\n");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "foo"), "World");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "bar"), "Hello World");
    assert_eq!(s.lookup_variable(ROOT_SCOPE, "zoo"), "");
}

#[test]
fn rule_variable_cycle_is_an_error() {
    let s = assert_parse("rule r\n  command = $description\n  description = $command\nbuild o: r\n");
    let err = s.evaluate_command(crate::graph::EdgeId(0), false).unwrap_err();
    assert!(err.starts_with("cycle in rule variables:"), "{err}");
}

#[test]
fn edge_bindings_take_precedence_over_rule() {
    // A file-scope variable named like a rule binding wins over the rule's
    // value when the edge has no bindings of its own (ninja quirk).
    let s = assert_parse("rule r\n  command = c\n  description = rule\ndescription = file\nbuild a: r\nbuild b: r\n  x = 1\n");
    assert_eq!(s.edge_binding(edge_of(&s, "a"), "description").unwrap(), "file");
    assert_eq!(s.edge_binding(edge_of(&s, "b"), "description").unwrap(), "rule");
}
