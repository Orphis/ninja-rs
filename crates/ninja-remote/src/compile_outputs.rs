//! Files a compile writes besides its object and its depfile: split DWARF
//! (`.dwo`), coverage notes (`.gcno`), time traces, compilation database
//! fragments (`-MJ`), serialized diagnostics, stack usage, optimization
//! records. A remote action returns only the files it asks for: these are
//! asked for too. Flags writing files that can't be named here (`-save-temps`,
//! cl's listings, a PDB several compiles share) make the action run locally.

/// `path` with the extension of its file name replaced by `ext` (added
/// when it has none), as compilers name the files they write next to their
/// output.
fn with_extension(path: &str, ext: &str) -> String {
    let name_start = path.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let stem_end = path[name_start..].rfind('.').filter(|&i| i > 0).map_or(path.len(), |i| name_start + i);
    format!("{}.{ext}", &path[..stem_end])
}

/// The files `argv`, a compile command (response files expanded), writes
/// besides its output and depfile, as named in it (relative to its working
/// directory, or absolute). `msvc`: a cl-style command line. An error means
/// the command writes files that can't be named here.
pub fn side_outputs(argv: &[String], msvc: bool) -> Result<Vec<String>, String> {
    let unknown = |a: &str| Err(format!("{a} writes files the action can't return"));
    let program = argv.first().map(|a| a.rsplit(['/', '\\']).next().unwrap_or(a).to_ascii_lowercase()).unwrap_or_default();
    let clang_cl = program.contains("clang");
    let mut output: Option<String> = None;
    let mut files = Vec::new();
    let mut split_dwarf = false;
    let mut coverage = false;
    let mut notes: Option<String> = None;
    let mut time_trace: Option<Option<String>> = None;
    let mut stack_usage = false;
    let mut opt_record: Option<String> = None;
    let mut opt_record_file: Option<String> = None;
    let mut make_deps = false;
    let mut make_deps_file: Option<String> = None;
    let mut pdb = false;
    // An option's value: glued to it, or the next argument.
    let value = |glued: &str, i: &mut usize| -> Option<String> {
        if glued.is_empty() {
            *i += 1;
            argv.get(*i - 1).cloned()
        } else {
            Some(glued.trim_start_matches('=').to_string())
        }
    };
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        i += 1;
        // clang-cl hands `/clang:` arguments to the driver as they are.
        let (a, cl) = match arg.strip_prefix("/clang:").or_else(|| arg.strip_prefix("-clang:")) {
            Some(rest) if msvc => (rest, false),
            _ => (arg, msvc),
        };
        if cl && let Some(o) = a.strip_prefix('/').or_else(|| a.strip_prefix('-')) {
            if let Some(p) = o.strip_prefix("Fo") {
                output = value(p, &mut i);
                continue;
            }
            if o == "Zi" || o == "ZI" {
                pdb = true;
                continue;
            }
            if let Some(p) = o.strip_prefix("sourceDependencies") {
                if p.starts_with(':') {
                    continue; // `/sourceDependencies:directives`, never alone.
                }
                match value(p, &mut i) {
                    Some(v) if v == "-" => {}
                    Some(v) if !v.ends_with(['/', '\\']) => files.push(v),
                    _ => return unknown(arg),
                }
                continue;
            }
            if o == "P"
                || o.starts_with("FA")
                || o.starts_with("Fa")
                || o.starts_with("Fi")
                || o.starts_with("doc")
                || o.starts_with("Fr")
                || o.starts_with("FR")
            {
                return unknown(arg);
            }
            // Options of both (`-o`, `-ftime-trace`) below; not `-MD`, the
            // runtime library to cl.
            if a.starts_with("-M") || a.starts_with("/") {
                continue;
            }
        }
        if a == "-o" || (a.starts_with("-o") && !cl) {
            output = value(&a[2..], &mut i);
        } else if a == "-gsplit-dwarf" || a == "-gsplit-dwarf=split" {
            split_dwarf = true;
        } else if a == "-gsplit-dwarf=single" || a == "-gno-split-dwarf" {
            split_dwarf = false;
        } else if a == "--coverage" || a == "-coverage" || a == "-ftest-coverage" {
            coverage = true;
        } else if a == "-fno-test-coverage" {
            coverage = false;
        } else if let Some(p) = a.strip_prefix("-fprofile-note=") {
            notes = Some(p.to_string());
        } else if a == "-ftime-trace" {
            time_trace = Some(None);
        } else if let Some(p) = a.strip_prefix("-ftime-trace=") {
            if p.ends_with(['/', '\\']) || std::path::Path::new(p).is_dir() {
                return unknown(arg);
            }
            time_trace = Some(Some(p.to_string()));
        } else if let Some(p) = a.strip_prefix("-MJ") {
            files.extend(value(p, &mut i));
        } else if let Some(p) = a.strip_prefix("--serialize-diagnostics") {
            files.extend(value(p, &mut i));
        } else if a == "-fstack-usage" {
            stack_usage = true;
        } else if a == "-fsave-optimization-record" {
            opt_record = Some("yaml".into());
        } else if let Some(f) = a.strip_prefix("-fsave-optimization-record=") {
            opt_record = Some(f.to_string());
        } else if let Some(p) = a.strip_prefix("-foptimization-record-file=") {
            opt_record_file = Some(p.to_string());
        } else if a == "-MD" || a == "-MMD" {
            make_deps = true;
        } else if let Some(p) = a.strip_prefix("-MF") {
            make_deps_file = value(p, &mut i);
        } else if a.starts_with("-save-temps") || a.starts_with("-fdump-") || a == "-fcallgraph-info" || a.starts_with("-fcallgraph-info=") || a == "-aux-info"
        {
            return unknown(arg);
        } else if matches!(
            a,
            "-MT" | "-MQ" | "-Xclang" | "-mllvm" | "-Xlinker" | "-Xassembler" | "-Xpreprocessor" | "-D" | "-U" | "-I" | "-include" | "-isystem"
        ) {
            // Their value isn't an option.
            i += 1;
        }
    }
    // A PDB cl.exe writes for each compile, shared by several: not one
    // compile's output. clang-cl puts the debug information in the object.
    if pdb && !clang_cl {
        return unknown("/Zi");
    }
    let next_to_output = |ext: &str| -> Result<String, String> {
        output.as_deref().map(|o| with_extension(o, ext)).ok_or_else(|| format!("no output to name the .{ext} file after"))
    };
    if split_dwarf {
        files.push(next_to_output("dwo")?);
    }
    if coverage {
        files.push(match notes {
            Some(n) => n,
            None => next_to_output("gcno")?,
        });
    }
    if let Some(trace) = time_trace {
        files.push(match trace {
            Some(t) => t,
            None => next_to_output("json")?,
        });
    }
    if stack_usage {
        files.push(next_to_output("su")?);
    }
    if let Some(format) = opt_record.filter(|_| opt_record_file.is_none()) {
        files.push(next_to_output(&format!("opt.{format}"))?);
    }
    files.extend(opt_record_file);
    if make_deps {
        files.push(match make_deps_file {
            Some(f) => f,
            None => next_to_output("d")?,
        });
    } else {
        files.extend(make_deps_file);
    }
    files.retain(|f| f != "-");
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outs(cmd: &str, msvc: bool) -> Result<Vec<String>, String> {
        side_outputs(&cmd.split(' ').map(str::to_string).collect::<Vec<_>>(), msvc)
    }

    #[test]
    fn names() {
        assert_eq!(with_extension("obj/a.c.o", "dwo"), "obj/a.c.dwo");
        assert_eq!(with_extension("obj/a", "dwo"), "obj/a.dwo");
        assert_eq!(with_extension("obj.d/.hidden", "su"), "obj.d/.hidden.su");
        assert_eq!(with_extension(r"obj\a.obj", "json"), r"obj\a.json");
    }

    #[test]
    fn gcc_style() {
        let base = "clang -c a.c -o obj/a.c.o";
        assert_eq!(outs(base, false).unwrap(), Vec::<String>::new());
        assert_eq!(outs(&format!("{base} -g -gsplit-dwarf"), false).unwrap(), ["obj/a.c.dwo"]);
        assert_eq!(outs(&format!("{base} -gsplit-dwarf -gsplit-dwarf=single"), false).unwrap(), Vec::<String>::new());
        assert_eq!(outs(&format!("{base} --coverage -ftime-trace"), false).unwrap(), ["obj/a.c.gcno", "obj/a.c.json"]);
        assert_eq!(outs(&format!("{base} -ftest-coverage -fprofile-note=n.gcno"), false).unwrap(), ["n.gcno"]);
        assert_eq!(outs(&format!("{base} -MJ obj/a.json -fstack-usage"), false).unwrap(), ["obj/a.json", "obj/a.c.su"]);
        assert_eq!(outs(&format!("{base} -fsave-optimization-record"), false).unwrap(), ["obj/a.c.opt.yaml"]);
        // The depfile, named or not; ninja's own is filtered out later.
        assert_eq!(outs(&format!("{base} -MD -MT obj/a.c.o -MF obj/a.c.o.d"), false).unwrap(), ["obj/a.c.o.d"]);
        assert_eq!(outs(&format!("{base} -MMD"), false).unwrap(), ["obj/a.c.d"]);
        assert_eq!(outs("gcc -c a.c -oobj/a.o -gsplit-dwarf", false).unwrap(), ["obj/a.dwo"]);
        assert!(outs(&format!("{base} -save-temps"), false).is_err());
        assert!(outs(&format!("{base} -fdump-tree-all"), false).is_err());
        assert!(outs("clang -c a.c -gsplit-dwarf", false).is_err());
        // A macro named like an option is no option.
        assert_eq!(outs(&format!("{base} -D -ftime-trace"), false).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn cl_style() {
        let base = "clang-cl /c a.c /Foobj/a.obj /MD /Zi";
        assert_eq!(outs(base, true).unwrap(), Vec::<String>::new());
        assert_eq!(outs(&format!("{base} /clang:-gsplit-dwarf /clang:-MJobj/a.json"), true).unwrap(), ["obj/a.json", "obj/a.dwo"]);
        assert_eq!(outs(&format!("{base} -ftime-trace /sourceDependencies obj/a.deps.json"), true).unwrap(), ["obj/a.deps.json", "obj/a.json"]);
        assert!(outs(&format!("{base} /FAs"), true).is_err());
        assert!(outs(&format!("{base} /P"), true).is_err());
        assert!(outs("cl.exe /c a.c /Foobj/a.obj /Zi /Fdtarget.pdb", true).is_err());
        assert_eq!(outs("cl.exe /c a.c /Foobj/a.obj /Z7", true).unwrap(), Vec::<String>::new());
    }
}
