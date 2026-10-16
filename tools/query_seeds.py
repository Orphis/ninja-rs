#!/usr/bin/env python3
"""Writes the compiler queries every cpp-deps profile asks in its first
probe (`crates/cpp-deps/src/query_seeds.txt`): the `__has_builtin(x)`,
`__has_feature(x)`, `__has_attribute(x)`... that GCC's and Clang's tables
know, that the C and C++ standards name, and that real headers ask.

Each line is `op arg`, then tags saying where the query comes from:

- `std:c++`, `std:c23`: a standard attribute of the C++ draft, of C23;
- `gcc`, `clang`: in GCC's tables, in Clang's (the spellings their
  attribute names normalize from, `__nodiscard__` for `nodiscard`,
  included);
- `used:<set>`: a header set asks it.

Run it with `python3 -I`; the sources are arguments:

    python3 -I tools/query_seeds.py \\
        --llvm ~/llvm-project \\
        --gcc <dir> --gcc-version 16.2.0 \\
        --headers msvc-stl "MSVC STL 14.51.36231" <include dir> \\
        --headers glibc "glibc 2.39" @<list of files> \\
        ... \\
        --out crates/cpp-deps/src/query_seeds.txt

`--llvm` is an llvm-project checkout (its version from
`cmake/Modules/LLVMVersion.cmake`). `--gcc` names a directory of GCC's
files, each named after its path in GCC's repository with `/` as `_`
(`gcc_builtins.def`). A header set's paths are directories (every file
under them) or `@file`s listing files, one per line.

Names some tables build with macros (GCC's builtins, Clang's traits) are
expanded by a small C preprocessor, and Clang's TableGen files are
evaluated by a small TableGen interpreter: what can't be (a file that
isn't there, an iterator GCC generates at build time) is reported, not
guessed.
"""

import argparse
import os
import re
import sys
from collections import defaultdict

# The operators answered by the compiler (`probe::OPERATORS` without those
# the scanner answers: `__has_include`, `__has_include_next`,
# `__has_embed`; and `__building_module`, whose answer is the compile's).
QUERY_OPERATORS = [
    "__has_builtin",
    "__has_constexpr_builtin",
    "__has_feature",
    "__has_extension",
    "__has_attribute",
    "__has_cpp_attribute",
    "__has_c_attribute",
    "__has_declspec_attribute",
    "__has_warning",
    "__is_identifier",
    "__is_target_arch",
    "__is_target_vendor",
    "__is_target_os",
    "__is_target_environment",
    "__is_target_variant_os",
    "__is_target_variant_environment",
]

# The standard attributes (names only: the compiler gives the values).
STD_CXX_ATTRIBUTES = [
    "assume", "carries_dependency", "deprecated", "fallthrough", "indeterminate", "likely",
    "maybe_unused", "no_unique_address", "nodiscard", "noreturn", "unlikely",
]
STD_C23_ATTRIBUTES = [
    "deprecated", "fallthrough", "maybe_unused", "nodiscard", "noreturn", "_Noreturn",
    "unsequenced", "reproducible",
]


class SourceError(Exception):
    pass


def read(path):
    with open(path, encoding="utf-8", errors="replace", newline="") as f:
        return f.read().replace("\r\n", "\n")


# -- C: tokens and a small preprocessor ------------------------------------

C_TOKEN = re.compile(
    r"""
    (?P<ws>[ \t\r\f\v]+)
  | (?P<nl>\n)
  | (?P<comment>//[^\n]*|/\*.*?\*/)
  | (?P<str>(?:u8|u|U|L)?"(?:\\.|[^"\\\n])*")
  | (?P<chr>(?:u8|u|U|L)?'(?:\\.|[^'\\\n])*')
  | (?P<num>\.?[0-9](?:[eEpP][+-]|[0-9A-Za-z_.])*)
  | (?P<id>[A-Za-z_$][A-Za-z0-9_$]*)
  | (?P<punct>\#\#|::|->|\.\.\.|<<=|>>=|[-+*/%&|^!=<>]=|&&|\|\||<<|>>|\+\+|--|.)
""",
    re.S | re.X,
)


class Tok:
    __slots__ = ("kind", "text", "space", "hide")

    def __init__(self, kind, text, space=False, hide=frozenset()):
        self.kind, self.text, self.space, self.hide = kind, text, space, hide

    def __repr__(self):
        return self.text


def c_lines(text):
    """The logical lines of a C source, as lists of tokens (comments are
    spaces; a comment spanning lines ends the line, which is all these
    sources need)."""
    text = text.replace("\\\n", "")
    lines, line, space = [], [], False
    for m in C_TOKEN.finditer(text):
        kind = m.lastgroup
        if kind in ("ws", "comment"):
            space = True
            if kind == "comment" and "\n" in m.group():
                lines.append(line)
                line = []
            continue
        if kind == "nl":
            lines.append(line)
            line, space = [], False
            continue
        line.append(Tok(kind, m.group(), space))
        space = False
    lines.append(line)
    return lines


def c_tokens(text):
    return [t for line in c_lines(text) for t in line]


class Macro:
    def __init__(self, params, variadic, body):
        self.params, self.variadic, self.body = params, variadic, body


def split_args(tokens, open_index):
    """The arguments of the call whose `(` is at `open_index`: (lists of
    tokens, index of the `)`), or None when it isn't closed."""
    depth, args, cur = 0, [], []
    for i in range(open_index + 1, len(tokens)):
        t = tokens[i]
        if t.text == "(":
            depth += 1
        elif t.text == ")":
            if depth == 0:
                args.append(cur)
                return args, i
            depth -= 1
        elif t.text == "," and depth == 0:
            args.append(cur)
            cur = []
            continue
        cur.append(t)
    return None


def stringize(tokens):
    s = "".join((" " if t.space and i else "") + t.text for i, t in enumerate(tokens))
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


class Preprocessor:
    """Enough of a C preprocessor for tables built with macros: object- and
    function-like macros (`#`, `##`, `__VA_ARGS__`), conditionals,
    `#include "..."` of the files `resolve` finds. The calls of the
    `collect` macros are recorded (with their arguments expanded) instead
    of expanded."""

    def __init__(self, resolve, collect=(), defines=None):
        self.resolve, self.collect = resolve, set(collect)
        self.macros = {}
        for name, body in (defines or {}).items():
            self.macros[name] = Macro(None, False, c_tokens(body))
        self.calls = []
        self.missing = []
        self.output = []

    def run(self, path, depth=0):
        text_run = []

        def flush():
            if text_run:
                self.output.extend(self.expand(list(text_run)))
                text_run.clear()

        stack = []  # (taking this branch, a branch was taken, parent active)
        active = True
        for line in c_lines(read(path)):
            if line and line[0].text == "#":
                d = line[1].text if len(line) > 1 else ""
                rest = line[2:]
                if d in ("if", "ifdef", "ifndef"):
                    if not active:
                        take = False
                    elif d == "if":
                        take = self.evaluate(rest)
                    else:
                        take = self.defined(rest[0].text) == (d == "ifdef")
                    stack.append([take, take, active])
                    active = active and take
                elif d in ("elif", "else"):
                    frame = stack[-1]
                    take = frame[2] and not frame[1] and (d == "else" or self.evaluate(rest))
                    frame[0] = take
                    frame[1] = frame[1] or take
                    active = frame[2] and take
                elif d == "endif":
                    active = stack.pop()[2]
                elif not active:
                    pass
                elif d == "define":
                    flush()
                    self.define(rest)
                elif d == "undef":
                    flush()
                    if rest[0].text not in self.collect:
                        self.macros.pop(rest[0].text, None)
                elif d == "include":
                    flush()
                    name = rest[0].text if rest else ""
                    found = self.resolve(path, name.strip('"<>')) if name.startswith('"') else None
                    if found:
                        self.run(found, depth + 1)
                    else:
                        self.missing.append(name)
                continue
            if active:
                text_run.extend(line)
        flush()
        return self.output

    def defined(self, name):
        return name in self.macros or name in self.collect

    def define(self, rest):
        name = rest[0].text
        if name in self.collect:
            return
        if len(rest) > 1 and rest[1].text == "(" and not rest[1].space:
            close = next(i for i in range(2, len(rest)) if rest[i].text == ")")
            params = [t.text for t in rest[2:close] if t.text != ","]
            variadic = bool(params) and params[-1] == "..."
            if variadic:
                params[-1] = "__VA_ARGS__"
            self.macros[name] = Macro(params, variadic, rest[close + 1:])
        else:
            self.macros[name] = Macro(None, False, rest[1:])

    def evaluate(self, tokens):
        out, i = [], 0
        while i < len(tokens):
            t = tokens[i]
            if t.text == "defined":
                if tokens[i + 1].text == "(":
                    name, i = tokens[i + 2].text, i + 4
                else:
                    name, i = tokens[i + 1].text, i + 2
                out.append(Tok("num", "1" if self.defined(name) else "0"))
                continue
            out.append(t)
            i += 1
        expr = []
        for t in self.expand(out):
            if t.kind == "id":
                expr.append("0")
            elif t.kind == "num":
                expr.append(t.text.rstrip("uUlL"))
            else:
                expr.append({"&&": " and ", "||": " or ", "!": " not ", "/": "//"}.get(t.text, t.text))
        try:
            return bool(eval("".join(expr), {"__builtins__": {}}))
        except Exception as e:
            raise SourceError(f"#if {' '.join(t.text for t in tokens)}: {e}")

    def expand(self, tokens):
        """Expands tokens; collected calls go to `calls`."""
        pending = list(reversed(tokens))
        out = []
        while pending:
            t = pending.pop()
            name = t.text
            if t.kind != "id" or name in t.hide or (name not in self.macros and name not in self.collect):
                out.append(t)
                continue
            m = self.macros.get(name)
            if m is not None and m.params is None:
                pending.extend(reversed(self.substitute(m, [], t.hide | {name}, t.space)))
                continue
            if not pending or pending[-1].text != "(":
                out.append(t)
                continue
            # The call's arguments, which may come from expansions.
            depth, args, cur = 0, [], []
            pending.pop()
            while True:
                if not pending:
                    raise SourceError(f"unterminated call of {name}")
                a = pending.pop()
                if a.text == "(":
                    depth += 1
                elif a.text == ")":
                    if depth == 0:
                        args.append(cur)
                        close = a
                        break
                    depth -= 1
                elif a.text == "," and depth == 0 and not (m and m.variadic and len(args) == len(m.params) - 1):
                    args.append(cur)
                    cur = []
                    continue
                cur.append(a)
            if m is None:
                self.calls.append((name, [self.expand(a) for a in args]))
                continue
            if args == [[]] and not m.params:
                args = []
            hide = (t.hide & close.hide) | {name}
            pending.extend(reversed(self.substitute(m, args, hide, t.space)))
        return out

    def substitute(self, m, args, hide, space):
        params = m.params or []
        if len(args) < len(params):
            args = args + [[] for _ in range(len(params) - len(args))]
        body = m.body
        pieces = []  # lists of tokens; `##` between them pastes
        i = 0
        while i < len(body):
            t = body[i]
            if t.text == "#" and i + 1 < len(body) and body[i + 1].text in params:
                pieces.append([Tok("str", stringize(args[params.index(body[i + 1].text)]), t.space)])
                i += 2
                continue
            if t.text == "##":
                pieces.append("##")
                i += 1
                continue
            if t.text in params:
                arg = args[params.index(t.text)]
                pasted = (i > 0 and body[i - 1].text == "##") or (i + 1 < len(body) and body[i + 1].text == "##")
                toks = [Tok(a.kind, a.text, a.space, a.hide) for a in (arg if pasted else self.expand(arg))]
                if toks:
                    toks[0].space = t.space
                pieces.append(toks)
                i += 1
                continue
            pieces.append([Tok(t.kind, t.text, t.space)])
            i += 1
        result = []
        paste = False
        for p in pieces:
            if p == "##":
                paste = True
                continue
            if paste and result and p:
                joined = result.pop().text + p[0].text
                new = c_tokens(joined)
                result.extend(new)
                result.extend(p[1:])
            else:
                result.extend(p)
            paste = False
        out = []
        for k, t in enumerate(result):
            out.append(Tok(t.kind, t.text, space if k == 0 else t.space, t.hide | hide))
        return out


def strings(tokens):
    """The concatenated string literals of tokens, or None if there are
    other tokens."""
    if not tokens or any(t.kind != "str" for t in tokens):
        return None
    return "".join(t.text[t.text.index('"') + 1 : -1] for t in tokens)


def initializer(tokens, name):
    """The fields of the braced initializer of the variable `name`
    (`name = { ... }` or `name[...] = { ... }`), each a list of tokens
    without its own braces."""
    return [f[1:-1] if f and f[0].text == "{" else f for f in fields(initializer_tokens(tokens, name))]


def braced(tokens):
    """The groups in braces of tokens (without the braces)."""
    groups, depth, start = [], 0, 0
    for i, t in enumerate(tokens):
        if t.text == "{":
            if depth == 0:
                start = i + 1
            depth += 1
        elif t.text == "}":
            depth -= 1
            if depth == 0:
                groups.append(tokens[start:i])
    return groups


def initializer_tokens(tokens, name):
    """The tokens in the braces of the initializer of `name`."""
    for i in range(len(tokens) - 2):
        if tokens[i].text != name or tokens[i].kind != "id":
            continue
        j = i + 1
        if tokens[j].text == "[":
            while j < len(tokens) and tokens[j].text != "]":
                j += 1
            j += 1
        if j + 1 >= len(tokens) or tokens[j].text != "=" or tokens[j + 1].text != "{":
            continue
        depth = 0
        for k in range(j + 1, len(tokens)):
            depth += {"{": 1, "}": -1}.get(tokens[k].text, 0)
            if depth == 0:
                return tokens[j + 2 : k]
    raise SourceError(f"no initializer of {name}")


def fields(tokens):
    """Tokens split at their top-level commas."""
    out, cur, depth = [], [], 0
    for t in tokens:
        if t.text in ("(", "{", "["):
            depth += 1
        elif t.text in (")", "}", "]"):
            depth -= 1
        elif t.text == "," and depth == 0:
            out.append(cur)
            cur = []
            continue
        cur.append(t)
    if cur:
        out.append(cur)
    return out


def function_body(tokens, name):
    """The tokens of the definition of the function `name`."""
    for i in range(len(tokens) - 1):
        if tokens[i].text != name or tokens[i + 1].text != "(":
            continue
        r = split_args(tokens, i + 1)
        if r is None or r[1] + 1 >= len(tokens) or tokens[r[1] + 1].text != "{":
            continue
        depth = 0
        for k in range(r[1] + 1, len(tokens)):
            depth += {"{": 1, "}": -1}.get(tokens[k].text, 0)
            if depth == 0:
                return tokens[r[1] + 2 : k]
    raise SourceError(f"no definition of {name}")


# -- TableGen: a small interpreter -----------------------------------------

TD_TOKEN = re.compile(
    r"""
    (?P<ws>\s+)
  | (?P<comment>//[^\n]*)
  | (?P<bcomment>/\*)
  | (?P<code>\[\{)
  | (?P<str>"(?:\\.|[^"\\\n])*")
  | (?P<num>-?(?:0x[0-9a-fA-F]+|0b[01]+|[0-9]+)(?![A-Za-z_0-9]))
  | (?P<id>[A-Za-z_][A-Za-z0-9_]*|[0-9]+[A-Za-z_][A-Za-z0-9_]*)
  | (?P<bang>![a-z]+)
  | (?P<punct>\.\.\.|[<>\[\]{}(),;:=.\#?+\-$])
""",
    re.X,
)


TD_DIRECTIVE = re.compile(r"#[ \t]*(\w+)[ \t]*(\w*)[^\n]*")


def td_tokens(text, path, defines=None):
    """TableGen's tokens, after its preprocessor (`#define NAME`,
    `#ifdef`, `#ifndef`, `#else`, `#endif`: include guards)."""
    defines = set() if defines is None else defines
    toks, i = [], 0
    line_start = True
    conds = []  # (active, parent active)
    active = True
    while i < len(text):
        m = TD_TOKEN.match(text, i)
        if not m:
            raise SourceError(f"{path}: can't read {text[i:i + 20]!r}")
        kind = m.lastgroup
        if kind == "bcomment":
            depth, j = 1, m.end()
            while depth:
                a, b = text.find("/*", j), text.find("*/", j)
                if b < 0:
                    raise SourceError(f"{path}: unterminated comment")
                if 0 <= a < b:
                    depth, j = depth + 1, a + 2
                else:
                    depth, j = depth - 1, b + 2
            i = j
            continue
        if kind == "code":
            j = text.find("}]", m.end())
            if active:
                toks.append(("code", text[m.end() : j]))
            i = j + 2
            line_start = False
            continue
        if kind == "punct" and m.group() == "#" and line_start:
            d = TD_DIRECTIVE.match(text, i)
            if d and d.group(1) in ("ifdef", "ifndef", "else", "endif", "define", "undef"):
                word, name = d.group(1), d.group(2)
                if word in ("ifdef", "ifndef"):
                    conds.append([(name in defines) == (word == "ifdef"), active])
                    active = active and conds[-1][0]
                elif word == "else":
                    conds[-1][0] = not conds[-1][0]
                    active = conds[-1][1] and conds[-1][0]
                elif word == "endif":
                    active = conds.pop()[1]
                elif active and word == "define":
                    defines.add(name)
                elif active:
                    defines.discard(name)
                i = d.end()
                continue
        if kind == "ws":
            line_start = line_start or "\n" in m.group()
        elif kind != "comment":
            line_start = False
            if active:
                toks.append((kind, m.group()))
        i = m.end()
    return toks


class TDError(Exception):
    pass


UNSET = object()


class Thunk:
    __slots__ = ("ast", "env", "value", "busy")

    def __init__(self, ast, env):
        self.ast, self.env, self.value, self.busy = ast, env, UNSET, False


class Record:
    def __init__(self, td, name):
        self.td, self.name = td, name
        self.fields = {}
        self.classes = set()

    def get(self, field):
        f = self.fields.get(field, UNSET)
        if f is UNSET:
            raise TDError(f"{self.name} has no {field}")
        if isinstance(f, Thunk):
            if f.value is UNSET:
                if f.busy:
                    raise TDError(f"{self.name}.{field} refers to itself")
                f.busy = True
                try:
                    f.value = self.td.evaluate(f.ast, f.env)
                finally:
                    f.busy = False
            return f.value
        return f

    def __repr__(self):
        return f"<{self.name}>"


class Env:
    def __init__(self, parent=None, record=None, vars=None):
        self.parent, self.record, self.vars = parent, record, vars or {}

    def lookup(self, name):
        env = self
        while env is not None:
            if name in env.vars:
                return env.vars[name]
            if env.record is not None:
                if name == "NAME":
                    return env.record.name
                if name in env.record.fields:
                    return env.record.get(name)
            env = env.parent
        return UNSET


class TableGen:
    """Evaluates TableGen files: classes, defs (named or not), `let ... in`,
    `foreach`, `defvar`, class instances in values, `#` pastes and the
    usual bang operators. Fields are evaluated when read."""

    def __init__(self, include_dirs):
        self.include_dirs = include_dirs
        self.classes = {}
        self.defs = {}
        self.records = []
        self.anonymous = 0
        self.defines = set()

    def load(self, path):
        self.toks = td_tokens(read(path), path, self.defines)
        self.pos = 0
        self.path = path
        self.statements(Env(), [], top=True)

    # Parsing.

    def peek(self, k=0):
        p = self.pos + k
        return self.toks[p] if p < len(self.toks) else ("eof", "")

    def next(self):
        t = self.peek()
        self.pos += 1
        return t

    def expect(self, text):
        t = self.next()
        if t[1] != text:
            raise TDError(f"{self.path}: expected {text!r}, got {t[1]!r} near token {self.pos}")
        return t

    def accept(self, text):
        if self.peek()[1] == text:
            self.pos += 1
            return True
        return False

    def statements(self, env, lets, top=False):
        while True:
            kind, text = self.peek()
            if kind == "eof":
                if not top:
                    raise TDError(f"{self.path}: unexpected end")
                return
            if text == "}" and not top:
                return
            self.statement(env, lets)

    def block_or_statement(self, env, lets):
        if self.accept("{"):
            self.statements(env, lets)
            self.expect("}")
        else:
            self.statement(env, lets)

    def statement(self, env, lets):
        kind, text = self.next()
        if text == "include":
            name = self.next()[1].strip('"')
            for d in [os.path.dirname(self.path)] + self.include_dirs:
                p = os.path.join(d, name)
                if os.path.exists(p):
                    saved = (self.toks, self.pos, self.path)
                    self.load(p)
                    self.toks, self.pos, self.path = saved
                    return
            raise TDError(f"{self.path}: can't find {name}")
        if text == "class":
            self.class_def()
        elif text == "def":
            self.def_(env, lets)
        elif text == "let":
            items = []
            while True:
                name = self.next()[1]
                if self.accept("{") or self.accept("<"):
                    raise TDError(f"{self.path}: bit ranges in let aren't supported")
                self.expect("=")
                items.append((name, self.value(), env))
                if not self.accept(","):
                    break
            self.expect("in")
            self.block_or_statement(env, lets + items)
        elif text == "foreach":
            var = self.next()[1]
            self.expect("=")
            values = self.evaluate(self.value(), env)
            self.expect("in")
            start = self.pos
            if not isinstance(values, list):
                raise TDError(f"{self.path}: foreach over {values!r}")
            for v in values:
                self.pos = start
                self.block_or_statement(Env(env, vars={var: v}), lets)
        elif text == "defvar":
            name = self.next()[1]
            self.expect("=")
            env.vars[name] = self.evaluate(self.value(), env)
            self.expect(";")
        elif text in ("multiclass", "defm", "defset", "if", "deftype"):
            raise TDError(f"{self.path}: {text} isn't supported")
        elif text in ("assert", "dump"):
            while self.next()[1] != ";":
                pass
        elif text == ";":
            pass
        else:
            raise TDError(f"{self.path}: unexpected {text!r}")

    def type_(self):
        self.next()
        if self.accept("<"):
            depth = 1
            while depth:
                t = self.next()[1]
                depth += {"<": 1, ">": -1}.get(t, 0)

    def template_params(self):
        params = []
        if self.accept("<"):
            while True:
                self.type_()
                name = self.next()[1]
                default = self.value() if self.accept("=") else None
                params.append((name, default))
                if self.accept(">"):
                    break
                self.expect(",")
        return params

    def parents(self):
        parents = []
        if self.accept(":"):
            while True:
                name = self.next()[1]
                args = self.args("<", ">") if self.peek()[1] == "<" else []
                parents.append((name, args))
                if not self.accept(","):
                    break
        return parents

    def args(self, open_, close):
        self.expect(open_)
        args = []
        if self.accept(close):
            return args
        while True:
            args.append(self.value())
            if self.accept(close):
                return args
            self.expect(",")

    def body(self):
        """Items: (name, value or None, is a let)."""
        items = []
        if self.accept(";"):
            return items
        self.expect("{")
        while not self.accept("}"):
            if self.accept("let"):
                name = self.next()[1]
                self.expect("=")
                items.append((name, self.value(), True))
                self.expect(";")
            elif self.accept("defvar"):
                name = self.next()[1]
                self.expect("=")
                items.append(("defvar " + name, self.value(), True))
                self.expect(";")
            elif self.peek()[1] in ("assert", "dump"):
                while self.next()[1] != ";":
                    pass
            else:
                self.accept("field")
                self.type_()
                name = self.next()[1]
                v = self.value() if self.accept("=") else None
                items.append((name, v, False))
                self.expect(";")
        return items

    def class_def(self):
        name = self.next()[1]
        params = self.template_params()
        parents = self.parents()
        items = self.body()
        self.classes[name] = (params, parents, items)

    def def_(self, env, lets):
        name_ast = None
        if self.peek()[1] not in (":", "{", ";"):
            name_ast = self.value(paste_names=True)
        parents = self.parents()
        items = self.body()
        if name_ast is None:
            self.anonymous += 1
            name = f"anonymous_{self.anonymous}"
        elif name_ast[0] == "id":
            name = name_ast[1]
        else:
            # `comi#Cmp`: the variables of `foreach` pasted.
            name = self.evaluate(name_ast, env, paste_names=True)
        rec = Record(self, name)
        renv = Env(env, record=rec)
        for cls, args in parents:
            self.apply(rec, cls, [self.evaluate(a, renv) for a in args])
        self.apply_items(rec, items, renv)
        for field, ast, lenv in lets:
            rec.fields[field] = Thunk(ast, Env(lenv, record=rec))
        self.defs[name] = rec
        self.records.append(rec)

    def apply(self, rec, cls, argvals):
        if cls not in self.classes:
            raise TDError(f"no class {cls}")
        params, parents, items = self.classes[cls]
        # Its parameters, then the record's fields (then the defs: see
        # `evaluate`).
        cenv = Env(Env(record=rec))
        for i, (pname, default) in enumerate(params):
            if i < len(argvals):
                cenv.vars[pname] = argvals[i]
            elif default is not None:
                cenv.vars[pname] = self.evaluate(default, cenv)
            else:
                raise TDError(f"{cls} needs {pname}")
        for pcls, pargs in parents:
            self.apply(rec, pcls, [self.evaluate(a, cenv) for a in pargs])
        self.apply_items(rec, items, cenv)
        rec.classes.add(cls)

    def apply_items(self, rec, items, env):
        for name, ast, is_let in items:
            if name.startswith("defvar "):
                env.vars[name[7:]] = self.evaluate(ast, env)
            elif ast is not None:
                rec.fields[name] = Thunk(ast, env)
            elif name not in rec.fields:
                rec.fields[name] = None

    # Values: parsed to tuples, evaluated later.

    def value(self, paste_names=False):
        parts = [self.simple(paste_names)]
        while True:
            if self.peek()[1] == "#":
                self.next()
                parts.append(self.simple(paste_names))
            elif self.peek()[0] == "str" and parts[-1][0] == "str":
                parts.append(self.simple(paste_names))
            else:
                break
        return parts[0] if len(parts) == 1 else ("paste", parts)

    def simple(self, paste_names=False):
        kind, text = self.next()
        if kind == "str":
            v = ("str", bytes(text[1:-1], "utf-8").decode("unicode_escape"))
        elif kind == "num":
            v = ("int", int(text, 0))
        elif kind == "code":
            v = ("str", text)
        elif text == "?":
            v = ("unset",)
        elif text == "[":
            items = []
            while not self.accept("]"):
                items.append(self.value())
                if not self.accept(","):
                    self.expect("]")
                    break
            if self.peek()[1] == "<":
                self.next()
                self.type_()
                self.expect(">")
            v = ("list", items)
        elif text == "{":
            items = []
            if not self.accept("}"):
                while True:
                    items.append(self.value())
                    if self.accept("}"):
                        break
                    self.expect(",")
            v = ("bits", items)
        elif text == "(":
            depth = 1
            while depth:
                t = self.next()[1]
                depth += {"(": 1, ")": -1}.get(t, 0)
            v = ("opaque", "dag")
        elif kind == "bang":
            typ = None
            if self.peek()[1] == "<":
                self.next()
                typ = self.next()[1]
                if self.peek()[1] == "<":
                    depth = 1
                    self.next()
                    while depth:
                        t = self.next()[1]
                        depth += {"<": 1, ">": -1}.get(t, 0)
                self.expect(">")
            v = ("bang", text[1:], typ, self.args("(", ")"))
        elif kind == "id":
            if self.peek()[1] == "<" and not paste_names:
                v = ("inst", text, self.args("<", ">"))
            else:
                v = ("id", text)
        else:
            raise TDError(f"{self.path}: unexpected {text!r} in a value")
        while self.peek()[1] == "." and self.peek(1)[0] == "id":
            self.next()
            v = ("field", v, self.next()[1])
        if self.peek()[1] in ("{", "[") and not paste_names and v[0] in ("id", "field"):
            # A bit range or list slice: not needed.
            close = "}" if self.next()[1] == "{" else "]"
            while self.next()[1] != close:
                pass
            v = ("opaque", "slice")
        return v

    def evaluate(self, ast, env, paste_names=False):
        k = ast[0]
        if k in ("str", "int"):
            return ast[1]
        if k == "unset":
            return None
        if k == "list":
            return [self.evaluate(a, env) for a in ast[1]]
        if k == "bits":
            return [self.evaluate(a, env) for a in ast[1]]
        if k == "id":
            v = env.lookup(ast[1])
            if v is UNSET:
                v = self.defs.get(ast[1], UNSET)
            if v is UNSET:
                if paste_names:
                    return ast[1]
                raise TDError(f"unknown {ast[1]}")
            return v
        if k == "paste":
            out = ""
            for p in ast[1]:
                v = self.evaluate(p, env, paste_names=True)
                if isinstance(v, Record):
                    v = v.name
                out += str(v)
            return out
        if k == "field":
            r = self.evaluate(ast[1], env)
            if not isinstance(r, Record):
                raise TDError(f"field of {r!r}")
            return r.get(ast[2])
        if k == "inst":
            self.anonymous += 1
            rec = Record(self, f"anonymous_{self.anonymous}")
            self.apply(rec, ast[1], [self.evaluate(a, env) for a in ast[2]])
            self.records.append(rec)
            return rec
        if k == "bang":
            return self.bang(ast[1], ast[2], [lambda a=a: self.evaluate(a, env) for a in ast[3]])
        raise TDError(f"can't evaluate {k}")

    def bang(self, op, typ, args):
        if op == "listconcat":
            return [x for a in args for x in a()]
        if op == "strconcat":
            return "".join(str(a()) for a in args)
        if op == "if":
            return args[1]() if args[0]() else args[2]()
        if op == "eq":
            return int(args[0]() == args[1]())
        if op == "ne":
            return int(args[0]() != args[1]())
        if op == "not":
            return int(not args[0]())
        if op == "and":
            return int(all(a() for a in args))
        if op == "or":
            return int(any(a() for a in args))
        if op == "empty":
            return int(not args[0]())
        if op == "size":
            return len(args[0]())
        if op == "cast":
            v = args[0]()
            return self.defs[v] if isinstance(v, str) and typ in self.classes else v
        raise TDError(f"!{op} isn't supported")

    def derived(self, cls):
        return [r for r in self.records if cls in r.classes]


# -- The sources ------------------------------------------------------------


class Seeds:
    def __init__(self):
        self.tags = defaultdict(set)  # (op, arg) -> tags
        self.by_source = defaultdict(set)  # source -> {(op, arg)}
        self.names = defaultdict(set)  # source -> names before spellings
        self.notes = []

    def add(self, source, tag, op, arg, name=None):
        if op not in QUERY_OPERATORS:
            raise SourceError(f"{source}: unknown operator {op}")
        if not arg or re.search(r"\s", arg):
            raise SourceError(f"{source}: bad argument {arg!r}")
        self.tags[(op, arg)].add(tag)
        self.by_source[source].add((op, arg))
        self.names[source].add(name or arg)

    def note(self, text):
        self.notes.append(text)
        print(f"note: {text}", file=sys.stderr)


IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")


def reserved(name):
    """`__name__`, which GCC and Clang take for `name` (an attribute's, a
    feature's), unless it already has underscores."""
    return [] if name.startswith("_") or name.endswith("_") else [f"__{name}__"]


def attribute_spellings(scope, name, clang):
    """An attribute's spellings: as named, and the reserved one system
    headers use, which the compilers normalize: `__name__` without a
    scope, `__gnu__::__name__` for `gnu::`, and Clang's `_Clang::__name__`
    for `clang::`."""
    if not scope:
        return [name] + reserved(name)
    out = [f"{scope}::{name}"]
    if scope == "gnu":
        out += [f"__gnu__::{r}" for r in reserved(name)]
    if scope == "clang" and clang:
        out += [f"_Clang::{r}" for r in reserved(name)]
    return out


def clang_sources(seeds, llvm):
    basic = os.path.join(llvm, "clang", "include", "clang", "Basic")
    include = os.path.join(llvm, "clang", "include")

    def resolve(including, name):
        for d in (os.path.dirname(including), include):
            p = os.path.join(d, name)
            if os.path.exists(p):
                return p
        return None

    def source(file):
        return f"clang:{file}"

    # __has_feature, __has_extension.
    pp = Preprocessor(resolve, collect=["FEATURE", "EXTENSION"])
    pp.run(os.path.join(basic, "Features.def"))
    for macro, args in pp.calls:
        name = args[0][0].text
        for arg in [name] + reserved(name):
            seeds.add(source("Features.def"), "clang", "__has_extension", arg, name)
            if macro == "FEATURE":
                seeds.add(source("Features.def"), "clang", "__has_feature", arg, name)

    # Attributes.
    td = TableGen([include])
    td.load(os.path.join(basic, "Attr.td"))
    attrs = td.derived("Attr")
    for rec in attrs:
        for sp in rec.get("Spellings"):
            variety, name = sp.get("Variety"), sp.get("Name")
            flat = []
            if variety in ("GCC", "Clang", "ClangGCC"):
                scopes = {"GCC": ["gnu"], "Clang": ["clang"], "ClangGCC": ["clang", "gnu"]}[variety]
                flat.append(("__has_attribute", ""))
                flat += [("__has_cpp_attribute", s) for s in scopes]
                if sp.get("AllowInC"):
                    flat += [("__has_c_attribute", s) for s in scopes]
            elif variety == "GNU":
                flat.append(("__has_attribute", ""))
            elif variety == "CXX11":
                flat.append(("__has_cpp_attribute", sp.get("Namespace")))
            elif variety == "C23":
                flat.append(("__has_c_attribute", sp.get("Namespace")))
            elif variety == "Declspec":
                seeds.add(source("Attr.td"), "clang", "__has_declspec_attribute", name)
            for op, scope in flat:
                for arg in attribute_spellings(scope, name, clang=True):
                    seeds.add(source("Attr.td"), "clang", op, arg, name)

    # __has_warning: the groups, and those diagnostics name in place.
    def groups(file, label):
        toks = td_tokens(read(os.path.join(basic, file)), file)
        for i in range(len(toks) - 2):
            if toks[i] == ("id", "DiagGroup") and toks[i + 1][1] == "<" and toks[i + 2][0] == "str":
                seeds.add(source(label), "clang", "__has_warning", f'"-W{toks[i + 2][1][1:-1]}"')

    groups("DiagnosticGroups.td", "DiagnosticGroups.td")
    for f in sorted(os.listdir(basic)):
        if re.match(r"Diagnostic\w*Kinds\.td\Z", f):
            groups(f, "Diagnostic*Kinds.td")

    # __has_builtin: the builtins of the generic, x86 and Arm tables.
    for file in ["Builtins.td", "BuiltinsX86.td", "BuiltinsX86_64.td", "BuiltinsAArch64.td"]:
        td = TableGen([include])
        td.load(os.path.join(basic, file))
        for rec in td.derived("Builtin"):
            spellings = rec.get("Spellings")
            prefix = rec.get("RequiredNamePrefix")
            prefix = prefix.get("Spelling") if isinstance(prefix, Record) else ""
            affixes, as_prefix = [""], False
            if "Template" in rec.classes:
                affixes, as_prefix = rec.get("Affixes"), rec.get("AsPrefix")
            for affix in affixes:
                for s in spellings:
                    full = affix + s if as_prefix else s + affix
                    names = [prefix + full]
                    if "LibBuiltin" in rec.classes and rec.get("AddBuiltinPrefixedAlias"):
                        names.append("__builtin_" + full)
                    # (`Consteval` implies `Constexpr`.)
                    constexpr = {"Constexpr", "Consteval"} & {a.name for a in rec.get("Attributes") if isinstance(a, Record)}
                    for n in names:
                        seeds.add(source(file), "clang", "__has_builtin", n)
                        if constexpr:
                            seeds.add(source(file), "clang", "__has_constexpr_builtin", n)
    builtin_macros = ["BUILTIN", "LANGBUILTIN", "TARGET_BUILTIN", "TARGET_HEADER_BUILTIN", "LIBBUILTIN"]
    for file, defines in [("BuiltinsARM.def", {}), ("BuiltinsAArch64NeonSVEBridge.def", {"GET_SVE_BUILTINS": "1"})]:
        pp = Preprocessor(resolve, collect=builtin_macros, defines=defines)
        pp.run(os.path.join(basic, file))
        for _, args in pp.calls:
            seeds.add(source(file), "clang", "__has_builtin", args[0][0].text)
        for m in pp.missing:
            seeds.note(f"{file}: {m} isn't in the sources (generated at build time): its builtins are left out")

    # __has_builtin: type traits and the `__builtin_` keywords.
    traits = ["TYPE_TRAIT_1", "TYPE_TRAIT_2", "TYPE_TRAIT_N", "ARRAY_TYPE_TRAIT", "EXPRESSION_TRAIT"]
    pp = Preprocessor(resolve, collect=traits + ["TRANSFORM_TYPE_TRAIT_DEF", "KEYWORD", "ALIAS"])
    pp.run(os.path.join(basic, "TokenKinds.def"))
    keywords = set()
    for macro, args in pp.calls:
        # (A trait spelled `/*EmptySpellingName*/` is internal.)
        if macro in traits and args[0]:
            keywords.add(args[0][0].text)
        elif macro == "TRANSFORM_TYPE_TRAIT_DEF":
            keywords.add("__" + args[1][0].text)
        elif macro == "KEYWORD" and args[0] and args[0][0].text.startswith("__builtin_"):
            keywords.add(args[0][0].text)
    for macro, args in pp.calls:
        # Another spelling of a trait is the trait.
        if macro == "ALIAS" and args[1][0].text in keywords:
            keywords.add(strings(args[0]))
    for k in keywords:
        seeds.add(source("TokenKinds.def"), "clang", "__has_builtin", k)
    td = TableGen([include])
    td.load(os.path.join(basic, "BuiltinTemplates.td"))
    for rec in td.derived("BuiltinTemplate"):
        seeds.add(source("BuiltinTemplates.td"), "clang", "__has_builtin", rec.name)
    with open(os.path.join(llvm, "cmake", "Modules", "LLVMVersion.cmake")) as f:
        v = dict(re.findall(r"set\(LLVM_VERSION_(MAJOR|MINOR|PATCH) (\d+)\)", f.read()))
    return f"{v['MAJOR']}.{v['MINOR']}.{v['PATCH']}"


def gcc_sources(seeds, gcc):
    def path(repo_path):
        return os.path.join(gcc, repo_path.replace("/", "_"))

    def need(repo_path):
        p = path(repo_path)
        if not os.path.exists(p):
            raise SourceError(f"{p} is missing")
        return p

    def resolve(including, name):
        # A file named after its repository path: `gcc_config_i386_x.def`.
        base = os.path.basename(including)
        dirs = base.split("_")[:-1]
        while True:
            p = os.path.join(gcc, "_".join(dirs + [name]))
            if os.path.exists(p):
                return p
            if not dirs:
                return None
            dirs.pop()

    def source(file):
        return f"gcc:{file}"

    # Builtins: DEF_BUILTIN (ENUM, NAME, CLASS, TYPE, LIBTYPE, BOTH_P,
    # FALLBACK_P, NONANSI_P, ATTRS, IMPLICIT, COND).
    pp = Preprocessor(resolve, collect=["DEF_BUILTIN"])
    pp.run(need("gcc/builtins.def"))
    for _, args in pp.calls:
        name = strings(args[1])
        cond = " ".join(t.text for t in args[10])
        # (Internal ones aren't names: `__builtin_unreachable trap`.)
        if name is None or not IDENT.match(name) or cond == "false":
            continue
        seeds.add(source("builtins.def"), "gcc", "__has_builtin", name)
        both = " ".join(t.text for t in args[5])
        if both != "false" and name.startswith("__builtin_"):
            seeds.add(source("builtins.def"), "gcc", "__has_builtin", name[len("__builtin_") :])
    for m in pp.missing:
        seeds.note(f"gcc/builtins.def: {m} isn't in the sources: its builtins are left out")

    # x86: BDESC (mask, mask2, icode, name, code, comparison, flag), its name
    # the only string (internal ones end with a space).
    pp = Preprocessor(resolve, collect=["BDESC", "BDESC_FIRST", "BDESC_END"])
    pp.run(need("gcc/config/i386/i386-builtin.def"))
    for macro, args in pp.calls:
        for a in args:
            s = strings(a)
            if s and IDENT.match(s):
                seeds.add(source("i386-builtin.def"), "gcc", "__has_builtin", s)

    # AArch64: the AdvSIMD builtins (`__builtin_aarch64_<name><mode>`, with
    # `_<signature>` when an operand is unsigned or polynomial), and those
    # named in full.
    pp = Preprocessor(resolve)
    toks = pp.run(need("gcc/config/aarch64/aarch64-builtins.cc"))
    # The operands' qualifiers of each kind of builtin (`TYPES_BINOPU` is
    # `(aarch64_types_binopu_qualifiers)`).
    qualifiers = {}
    for i in range(len(toks) - 1):
        name = toks[i].text
        if toks[i].kind == "id" and name.endswith("_qualifiers") and toks[i + 1].text == "[" and name not in qualifiers:
            try:
                qualifiers[name] = [" ".join(t.text for t in f) for f in initializer(toks, name)]
            except SourceError:
                pass
    # The entries (`{#N #A, ...}`), between the calls of the iterators that
    # would have made more (`BUILTIN_VDQ_I (BINOP, addp, 0, DEFAULT)`).
    data = initializer_tokens(toks, "aarch64_simd_builtin_data")
    skipped = sum(1 for t in data if t.kind == "id" and t.text.startswith("BUILTIN_"))
    for f in braced(data):
        f = fields(f)
        name = strings(f[0])
        quals = next((t.text for t in f[4] if t.text in qualifiers), None)
        if name is None or quals is None:
            raise SourceError(f"aarch64_simd_builtin_data: can't read {f}")
        # A character per operand (`type_signature`), the result's first
        # unless it's void.
        q = qualifiers[quals]
        if q and q[0] == "qualifier_void":
            q = q[1:]
        sig = "".join("u" if "unsigned" in x else "p" if "poly" in x else "s" for x in q)
        full = f"__builtin_aarch64_{name}" + (f"_{sig}" if set(sig) - {"s"} else "")
        seeds.add(source("aarch64-simd-builtins.def"), "gcc", "__has_builtin", full)
    i = 0
    while i < len(toks):
        j = i
        while j < len(toks) and toks[j].kind == "str":
            j += 1
        if j > i:
            s = strings(toks[i:j])
            if s.startswith("__builtin_aarch64_") and "%" not in s and not s.startswith("__builtin_aarch64_simd_"):
                seeds.add(source("aarch64-builtins.cc"), "gcc", "__has_builtin", s)
            i = j
        else:
            i += 1
    if skipped:
        seeds.note(
            f"gcc/config/aarch64: {skipped} BUILTIN_<iterator> lines of aarch64-simd-builtins.def left out: "
            "their modes come from aarch64-builtin-iterators.h, generated from iterators.md at build time"
        )
    for m in sorted(set(pp.missing)):
        if m.endswith('.def"'):
            seeds.note(f"gcc/config/aarch64/aarch64-builtins.cc: {m} isn't in the sources: its builtins are left out")

    # The keywords __has_builtin knows (`names_builtin_p`).
    rids = set()
    for file in ["gcc/c/c-decl.cc", "gcc/cp/cp-objcp-common.cc"]:
        body = function_body(c_tokens(read(need(file))), "names_builtin_p")
        rids |= {body[i + 1].text for i in range(len(body) - 1) if body[i].text == "case"}
    for f in initializer(c_tokens(read(need("gcc/c-family/c-common.cc"))), "c_common_reswords"):
        f = fields(f)
        if f[1][0].text in rids:
            seeds.add(source("c-common.cc"), "gcc", "__has_builtin", strings(f[0]))

    # __has_feature, __has_extension (`HF_FLAG_EXT`: an extension only).
    for file, table in [
        ("gcc/c-family/c-common.cc", "has_feature_table"),
        ("gcc/cp/cp-objcp-common.cc", "cp_feature_table"),
        ("gcc/c/c-objc-common.cc", "c_feature_table"),
    ]:
        label = source(os.path.basename(file))
        for f in initializer(c_tokens(read(need(file))), table):
            f = fields(f)
            name = strings(f[0])
            for arg in [name] + reserved(name):
                seeds.add(label, "gcc", "__has_extension", arg, name)
                if not any(t.text == "HF_FLAG_EXT" for t in f[1]):
                    seeds.add(label, "gcc", "__has_feature", arg, name)

    # Attributes: the tables, and the scopes they have.
    for file, lang in [
        ("gcc/c-family/c-attribs.cc", "c c++"),
        ("gcc/cp/tree.cc", "c++"),
        ("gcc/c/c-decl.cc", "c"),
        ("gcc/config/i386/i386-options.cc", "c c++"),
    ]:
        toks = c_tokens(read(need(file)))
        label = source(os.path.basename(file))
        for i in range(len(toks) - 4):
            if toks[i].text != "scoped_attribute_specs" or toks[i + 2].text != "=":
                continue
            # `{ "gnu", { table } }`; `nullptr` for the standard ones.
            scope, table = initializer(toks, toks[i + 1].text)
            scope = strings(scope) or ""
            if scope.strip() == "internal":
                continue
            for e in initializer(toks, table[0].text):
                name = strings(e[:1])
                if not name or not IDENT.match(name):
                    continue
                ops = []
                if not scope:
                    ops = [("__has_cpp_attribute" if lang == "c++" else "__has_c_attribute", "")]
                else:
                    if scope == "gnu":
                        ops.append(("__has_attribute", ""))
                    if "c++" in lang:
                        ops.append(("__has_cpp_attribute", scope))
                    if "c" in lang.split():
                        ops.append(("__has_c_attribute", scope))
                for op, s in ops:
                    for arg in attribute_spellings(s, name, clang=False):
                        seeds.add(label, "gcc", op, arg, name)


def standard_sources(seeds):
    for name in STD_CXX_ATTRIBUTES:
        seeds.add("std:c++", "std:c++", "__has_cpp_attribute", name)
    for name in STD_C23_ATTRIBUTES:
        # C23 makes `__name__` the same attribute.
        for arg in [name] + reserved(name):
            seeds.add("std:c23", "std:c23", "__has_c_attribute", arg, name)


# -- What headers ask -------------------------------------------------------


def header_files(paths):
    for p in paths:
        if p.startswith("@"):
            with open(p[1:]) as f:
                yield from (line.strip() for line in f if line.strip())
        elif os.path.isdir(p):
            for root, dirs, files in os.walk(p):
                dirs.sort()
                for f in sorted(files):
                    yield os.path.join(root, f)
        else:
            yield p


def directives(path):
    """A header's `#if`, `#elif` and `#define` lines, as tokens."""
    try:
        text = read(path)
    except (OSError, UnicodeError):
        return
    if "__has_" not in text and "__is_" not in text and "#define" not in text:
        return
    for line in c_lines(text):
        if len(line) > 2 and line[0].text == "#" and line[1].text in ("if", "elif", "define"):
            yield line


def literal(arg):
    """A query's argument as the scanner spells it (its tokens, joined), if
    it's a name, a scoped name or a string."""
    text = "".join(t.text for t in arg)
    if len(arg) == 1 and arg[0].kind in ("id", "str"):
        return text
    if len(arg) == 3 and arg[0].kind == "id" and arg[1].text == "::" and arg[2].kind == "id":
        return text
    return None


def wrapped(line, wrappers):
    """What a `#define` makes its macro if it wraps an operator: a macro of
    one parameter whose replacement is a query (or another wrapper's call)
    of it, maybe scoped: `#define _GLIBCXX_HAS_BUILTIN(B) __has_builtin(B)`,
    `#define _HAS_MSVC_ATTRIBUTE(x) __has_cpp_attribute(msvc::x)`. As
    (operator, text before the parameter, text after)."""
    if line[1].text != "define" or len(line) < 10 or line[3].text != "(" or line[3].space or line[5].text != ")":
        return []
    param, body = line[4].text, line[6:]
    # (Negated too: libc++'s `#define __has_keyword(__x) !(__is_identifier(__x))`.)
    while len(body) > 2 and (body[0].text == "!" or (body[0].text == "(" and body[-1].text == ")")):
        body = body[1:] if body[0].text == "!" else body[1:-1]
    if len(body) < 4 or body[1].text != "(" or body[-1].text != ")":
        return []
    inner = body[2:-1]
    at = [i for i, t in enumerate(inner) if t.text == param]
    if len(at) != 1 or any(t.kind != "id" and t.text != "::" for t in inner):
        return []
    before = "".join(t.text for t in inner[: at[0]])
    after = "".join(t.text for t in inner[at[0] + 1 :])
    head = body[0].text
    out = [(head, before, after)] if head in QUERY_OPERATORS else []
    return out + [(op, b + before, after + a) for op, b, a in wrappers.get(head, ())]


def harvest(seeds, label, paths):
    """The queries the headers ask, with a literal argument: in conditions
    and macro definitions, directly or through a macro wrapping an operator
    (see `wrapped`)."""
    files = list(header_files(paths))
    lines = {f: list(directives(f)) for f in files}
    wrappers = defaultdict(set)
    changed = True
    while changed:
        changed = False
        for f in files:
            for line in lines[f]:
                for w in wrapped(line, wrappers):
                    if line[2].text not in QUERY_OPERATORS and w not in wrappers[line[2].text]:
                        wrappers[line[2].text].add(w)
                        changed = True
    for f in files:
        for line in lines[f]:
            params = set()
            toks = line[2:]
            if line[1].text == "define":
                toks = line[3:]
                if toks and toks[0].text == "(" and not toks[0].space:
                    close = next(i for i, t in enumerate(toks) if t.text == ")")
                    params = {t.text for t in toks[1:close]}
                    toks = toks[close + 1 :]
            for i, t in enumerate(toks):
                forms = [(t.text, "", "")] if t.text in QUERY_OPERATORS else wrappers.get(t.text, ())
                if not forms or i + 1 >= len(toks) or toks[i + 1].text != "(":
                    continue
                r = split_args(toks, i + 1)
                if r is None or len(r[0]) != 1 or any(a.text in params for a in r[0][0]):
                    continue
                arg = literal(r[0][0])
                if arg is None:
                    continue
                for op, before, after in forms:
                    if (op == "__has_warning") != arg.startswith('"') or (op == "__has_warning" and not arg.startswith('"-W')):
                        continue
                    seeds.add(f"used:{label}", f"used:{label}", op, before + arg + after)
    return {k: sorted(v) for k, v in wrappers.items()}


# -- Output -----------------------------------------------------------------


TAG_ORDER = ["std:c++", "std:c23", "gcc", "clang"]


def tag_key(tag):
    return (TAG_ORDER.index(tag), "") if tag in TAG_ORDER else (len(TAG_ORDER), tag)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--llvm", required=True, help="an llvm-project checkout")
    ap.add_argument("--gcc", required=True, help="GCC's files, named after their repository paths")
    ap.add_argument("--gcc-version", required=True)
    ap.add_argument("--headers", nargs="+", action="append", default=[], metavar="NAME VERSION PATH", help="a header set")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    seeds = Seeds()
    llvm_version = clang_sources(seeds, a.llvm)
    gcc_sources(seeds, a.gcc)
    standard_sources(seeds)
    sets = []
    for h in a.headers:
        if len(h) < 3:
            ap.error("--headers takes a name, a version and paths")
        name, version, paths = h[0], h[1], h[2:]
        wrappers = harvest(seeds, name, paths)
        sets.append((name, version))
        if wrappers:
            print(f"{name}: wrappers {', '.join(f'{k} {v}' for k, v in sorted(wrappers.items()))}", file=sys.stderr)

    lines = []
    for (op, arg), tags in sorted(seeds.tags.items()):
        lines.append(f"{op} {arg}  {' '.join(sorted(tags, key=tag_key))}\n")
    header = [
        "# The compiler queries every cpp-deps profile asks in its first probe",
        "# (see driver.rs), each with where it comes from. Generated by",
        "# tools/query_seeds.py (don't edit): `op arg  tags`, sorted.",
        "#",
        "# Tags:",
        "#   std:c++, std:c23  a standard attribute of the C++ draft, of C23",
        f"#   clang             LLVM {llvm_version}'s tables (clang/include/clang/Basic: Features.def,",
        "#                     Attr.td, DiagnosticGroups.td and the groups diagnostics name,",
        "#                     Builtins.td, BuiltinsX86.td, BuiltinsX86_64.td, BuiltinsAArch64.td,",
        "#                     BuiltinsARM.def, BuiltinsAArch64NeonSVEBridge.def, TokenKinds.def's",
        "#                     traits and __builtin_ keywords, BuiltinTemplates.td)",
        f"#   gcc               GCC {a.gcc_version}'s tables (builtins.def, sync-builtins.def,",
        "#                     i386-builtin.def, aarch64-simd-builtins.def, aarch64-builtins.cc,",
        "#                     c-common.cc, c-objc-common.cc, cp-objcp-common.cc, c-attribs.cc,",
        "#                     cp/tree.cc, c-decl.cc, i386-options.cc)",
        "#   used:<set>        asked by a header set:",
    ]
    for name, version in sets:
        header.append(f"#                       {name}: {version}")
    header.append("#")
    text = "\n".join(header) + "\n" + "".join(lines)
    with open(a.out, "w", newline="\n") as f:
        f.write(text)

    # The report.
    print(f"{len(lines)} entries, {len(text.encode())} bytes", file=sys.stderr)
    print("per source (entries, names):", file=sys.stderr)
    for source in sorted(seeds.by_source):
        print(f"  {source}: {len(seeds.by_source[source])}, {len(seeds.names[source])}", file=sys.stderr)
    per_op = defaultdict(int)
    for op, _ in seeds.tags:
        per_op[op] += 1
    print("per operator:", file=sys.stderr)
    for op in QUERY_OPERATORS:
        if per_op[op]:
            print(f"  {op}: {per_op[op]}", file=sys.stderr)
    kinds = defaultdict(int)
    for tags in seeds.tags.values():
        std = any(t.startswith("std:") for t in tags)
        g, c = "gcc" in tags, "clang" in tags
        used = any(t.startswith("used:") for t in tags)
        kinds["standard"] += std
        kinds["gcc and clang"] += g and c
        kinds["gcc only"] += g and not c
        kinds["clang only"] += c and not g
        kinds["used, in neither table nor standard"] += used and not (g or c or std)
    print("kinds:", file=sys.stderr)
    for k, v in kinds.items():
        print(f"  {k}: {v}", file=sys.stderr)


if __name__ == "__main__":
    try:
        main()
    except (SourceError, TDError) as e:
        sys.exit(f"query_seeds.py: {e}")
