//! Command-line parsing compatible with ninja's getopt usage, plus the
//! remote execution flags.

use ninja_core::status::Verbosity;

#[derive(Clone, Debug)]
pub struct Options {
    pub working_dir: Option<String>,
    pub input_file: String,
    pub parallelism: usize,
    /// `usize::MAX` for `-k 0`.
    pub failures_allowed: usize,
    pub dry_run: bool,
    pub verbosity: Verbosity,
    pub explain: bool,
    pub keep_depfile: bool,
    pub keep_rsp: bool,
    /// `-d stats`: print operation counts and timings at exit.
    pub stats: bool,
    /// `-d nostatcache`: don't batch stats per directory.
    pub no_stat_cache: bool,
    pub phonycycle_err: bool,
    pub tool: Option<String>,
    pub tool_args: Vec<String>,
    pub targets: Vec<String>,
    pub remote: RemoteFlags,
}

#[derive(Clone, Debug, Default)]
pub struct RemoteFlags {
    pub executor: Option<String>,
    pub instance_name: String,
    pub headers: Vec<(String, String)>,
    pub jobs: usize,
    pub exec_root: Option<String>,
    pub mnemonic_suffixes: Vec<String>,
    pub local_fallback: bool,
    pub verbose: bool,
}

pub enum ParseOutcome {
    Run(Box<Options>),
    /// Print this and exit with the code (e.g. --version, -h).
    Exit(String, i32),
}

pub fn guess_parallelism() -> usize {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    match n {
        0 | 1 => 2,
        2 => 3,
        n => n + 2,
    }
}

pub const USAGE: &str = "usage: ninja-rs [options] [targets...]

if targets are unspecified, builds the 'default' target (see manual).

options:
  --version      print ninja version (\"{version}\")
  -v, --verbose  show all command lines while building
  --quiet        don't show progress status, just command output

  -C DIR   change to DIR before doing anything else
  -f FILE  specify input build file [default=build.ninja]

  -j N     run N jobs in parallel (0 means infinity) [default={jobs} on this system]
  -k N     keep going until N jobs fail (0 means infinity) [default=1]
  -l N     do not start new jobs if the load average is greater than N (ignored)
  -n       dry run (don't run commands but act like they succeeded)

  -d MODE  enable debugging (use '-d list' to list modes)
  -t TOOL  run a subtool (use '-t list' to list subtools)
    terminates toplevel options; further flags are passed to the tool
  -w FLAG  adjust warnings (use '-w list' to list warnings)

remote execution (REAPI v2; only for build.ninja files with rbe_version = 1):
  --remote_executor=URL      grpc://host:port or grpcs://host:port
  --remote_instance_name=S   REAPI instance name
  --remote_header=K=V        extra gRPC header (repeatable)
  --remote_jobs=N            remote actions in flight [default=50]
  --exec_root=DIR            fixed exec root for remote actions (default: per action)
  --remote_mnemonics=A,B     mnemonic suffixes allowed to run remotely [default=Compile]
  --[no]remote_local_fallback  run locally when remote execution fails [default=on]
  --remote_verbose           log remote decisions and cache hits
";

pub fn usage() -> String {
    USAGE
        .replace("{version}", ninja_core::parser::NINJA_VERSION)
        .replace("{jobs}", &guess_parallelism().to_string())
}

fn parse_int(flag: &str, v: &str) -> Result<i64, String> {
    v.parse::<i64>().map_err(|_| format!("invalid {flag} parameter"))
}

pub fn parse_args(args: &[String]) -> Result<ParseOutcome, String> {
    let mut o = Options {
        working_dir: None,
        input_file: "build.ninja".into(),
        parallelism: guess_parallelism(),
        failures_allowed: 1,
        dry_run: false,
        verbosity: Verbosity::Normal,
        explain: false,
        keep_depfile: false,
        keep_rsp: false,
        stats: false,
        no_stat_cache: false,
        phonycycle_err: false,
        tool: None,
        tool_args: Vec::new(),
        targets: Vec::new(),
        remote: RemoteFlags { jobs: 50, mnemonic_suffixes: vec!["Compile".into()], local_fallback: true, ..Default::default() },
    };
    let mut i = 0;
    let mut positional = Vec::new();
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            positional.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let value = |i: &mut usize| -> Result<String, String> {
                if let Some(v) = &inline {
                    return Ok(v.clone());
                }
                let v = args.get(*i).cloned().ok_or_else(|| format!("option '--{name}' requires an argument"))?;
                *i += 1;
                Ok(v)
            };
            match name {
                "version" => return Ok(ParseOutcome::Exit(format!("{}\n", ninja_core::parser::NINJA_VERSION), 0)),
                "verbose" => o.verbosity = Verbosity::Verbose,
                "quiet" => o.verbosity = Verbosity::NoStatusUpdate,
                "help" => return Ok(ParseOutcome::Exit(usage(), 1)),
                "remote_executor" => o.remote.executor = Some(value(&mut i)?),
                "remote_instance_name" => o.remote.instance_name = value(&mut i)?,
                "remote_header" => {
                    let v = value(&mut i)?;
                    let (k, val) = v.split_once('=').ok_or_else(|| format!("--remote_header expects KEY=VALUE, got '{v}'"))?;
                    o.remote.headers.push((k.to_string(), val.to_string()));
                }
                "remote_jobs" => {
                    let v = value(&mut i)?;
                    let n = parse_int("--remote_jobs", &v)?;
                    if n <= 0 {
                        return Err("invalid --remote_jobs parameter".into());
                    }
                    o.remote.jobs = n as usize;
                }
                "exec_root" => o.remote.exec_root = Some(value(&mut i)?),
                "remote_mnemonics" => {
                    o.remote.mnemonic_suffixes =
                        value(&mut i)?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect()
                }
                "remote_local_fallback" => o.remote.local_fallback = true,
                "noremote_local_fallback" => o.remote.local_fallback = false,
                "remote_verbose" => o.remote.verbose = true,
                _ => return Err(format!("unrecognized option '--{name}'")),
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            // Short options, getopt style: "-j8", "-j 8", "-nv".
            let chars: Vec<char> = a[1..].chars().collect();
            let mut ci = 0;
            while ci < chars.len() {
                let c = chars[ci];
                ci += 1;
                let takes_arg = matches!(c, 'd' | 'f' | 'j' | 'k' | 'l' | 't' | 'w' | 'C');
                let arg = if takes_arg {
                    if ci < chars.len() {
                        let v: String = chars[ci..].iter().collect();
                        ci = chars.len();
                        Some(v)
                    } else {
                        let v = args.get(i).cloned().ok_or_else(|| format!("option requires an argument -- '{c}'"))?;
                        i += 1;
                        Some(v)
                    }
                } else {
                    None
                };
                match c {
                    'd' => match arg.as_deref().unwrap() {
                        "explain" => o.explain = true,
                        "keepdepfile" => o.keep_depfile = true,
                        "keeprsp" => o.keep_rsp = true,
                        "stats" => o.stats = true,
                        "nostatcache" => o.no_stat_cache = true,
                        "list" => {
                            return Ok(ParseOutcome::Exit(
                                "debugging modes:\n  stats        print operation counts/timing info\n  explain      explain what caused a command to execute\n  keepdepfile  don't delete depfiles after they're read by ninja\n  keeprsp      don't delete @response files on success\n  nostatcache  don't batch stat() calls per directory and cache them\nmultiple modes can be enabled via -d FOO -d BAR\n".into(),
                                1,
                            ));
                        }
                        other => return Err(format!("unknown debug setting '{other}'")),
                    },
                    'f' => o.input_file = arg.unwrap(),
                    'j' => {
                        let n = parse_int("-j", arg.as_deref().unwrap())?;
                        if n < 0 {
                            return Err("invalid -j parameter".into());
                        }
                        // We want to run N jobs in parallel. For N = 0, INT_MAX
                        // is close enough to infinite for most sane builds.
                        o.parallelism = if n > 0 { n as usize } else { i32::MAX as usize };
                    }
                    'k' => {
                        let n = parse_int("-k", arg.as_deref().unwrap())?;
                        // We want to go until N jobs fail, which means we should
                        // allow N failures and then stop. For N <= 0, INT_MAX is
                        // close enough to infinite for most sane builds.
                        o.failures_allowed = if n > 0 { n as usize } else { i32::MAX as usize };
                    }
                    'l' => {
                        arg.as_deref().unwrap().parse::<f64>().map_err(|_| "-l parameter not numeric: did you mean -l 0.0?".to_string())?;
                    }
                    'n' => o.dry_run = true,
                    't' => {
                        o.tool = arg;
                        // -t terminates toplevel options; the rest goes to the tool.
                        o.tool_args = args[i..].to_vec();
                        i = args.len();
                    }
                    'v' => o.verbosity = Verbosity::Verbose,
                    'q' => o.verbosity = Verbosity::Quiet,
                    'w' => match arg.as_deref().unwrap() {
                        "phonycycle=err" => o.phonycycle_err = true,
                        "phonycycle=warn" => o.phonycycle_err = false,
                        "dupbuild=err" | "dupbuild=warn" => {}
                        "list" => {
                            return Ok(ParseOutcome::Exit(
                                "warning flags:\n  phonycycle={err,warn}  phony build statement references itself\n".into(),
                                1,
                            ));
                        }
                        other => return Err(format!("unknown warning flag '{other}'")),
                    },
                    'C' => o.working_dir = arg,
                    'h' => return Ok(ParseOutcome::Exit(usage(), 1)),
                    other => return Err(format!("invalid option -- '{other}'\n{}", usage())),
                }
                if o.tool.is_some() && i == args.len() && c == 't' {
                    break;
                }
            }
            continue;
        }
        positional.push(a.clone());
    }
    o.targets = positional;
    Ok(ParseOutcome::Run(Box::new(o)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Options {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_args(&v).unwrap() {
            ParseOutcome::Run(o) => *o,
            ParseOutcome::Exit(m, _) => panic!("exit: {m}"),
        }
    }

    #[test]
    fn ninja_flags() {
        let o = p(&["-C", "build", "-j8", "-k", "0", "-nv", "-f", "x.ninja", "all", "foo"]);
        assert_eq!(o.working_dir.as_deref(), Some("build"));
        assert_eq!(o.parallelism, 8);
        assert_eq!(o.failures_allowed, i32::MAX as usize);
        assert!(o.dry_run);
        assert_eq!(o.verbosity, Verbosity::Verbose);
        assert_eq!(o.input_file, "x.ninja");
        assert_eq!(o.targets, vec!["all", "foo"]);
    }

    #[test]
    fn tool_terminates_options() {
        let o = p(&["-Cbuild", "-t", "clean", "-g", "foo"]);
        assert_eq!(o.tool.as_deref(), Some("clean"));
        assert_eq!(o.tool_args, vec!["-g", "foo"]);
        assert!(o.targets.is_empty());
    }

    #[test]
    fn remote_flags() {
        let o = p(&[
            "--remote_executor=grpc://localhost:8980",
            "--remote_instance_name",
            "main",
            "--remote_header=x-a=b=c",
            "--remote_header=k=v",
            "--remote_jobs=12",
            "--noremote_local_fallback",
            "--remote_mnemonics=Compile,Archive",
        ]);
        assert_eq!(o.remote.executor.as_deref(), Some("grpc://localhost:8980"));
        assert_eq!(o.remote.instance_name, "main");
        assert_eq!(o.remote.headers, vec![("x-a".to_string(), "b=c".to_string()), ("k".into(), "v".into())]);
        assert_eq!(o.remote.jobs, 12);
        assert!(!o.remote.local_fallback);
        assert_eq!(o.remote.mnemonic_suffixes, vec!["Compile", "Archive"]);
    }

    #[test]
    fn version() {
        let v = vec!["--version".to_string()];
        assert!(matches!(parse_args(&v).unwrap(), ParseOutcome::Exit(_, 0)));
    }
}
