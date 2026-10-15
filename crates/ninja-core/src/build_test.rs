//! Build engine tests, ported from ninja's build_test.cc (a subset), with a
//! fake command runner that simulates commands on a virtual filesystem.

use std::sync::{Arc, Mutex};

use crate::build::{BuildConfig, Builder};
use crate::build_log::BuildLog;
use crate::deps_log::DepsLog;
use crate::disk::{Disk, VirtualDisk};
use crate::graph::{EdgeId, Existence, State};
use crate::parser::{ManifestParser, MemFiles, ParserOptions};
use crate::runner::{AbortFuture, CommandRunner, ExecKind, ExitStatus, Task, TaskResult, WaitFuture};
use crate::status::{NullStatus, Verbosity};

struct FakeRunner<'a> {
    fs: &'a VirtualDisk,
    commands_ran: Arc<Mutex<Vec<String>>>,
    active: Vec<Task>,
    max_active: usize,
    /// Edges of the "lose-once" rule that already lost their inputs.
    lost: Vec<EdgeId>,
}

impl CommandRunner for FakeRunner<'_> {
    fn can_run_more(&self) -> usize {
        if self.active.len() < self.max_active { usize::MAX } else { 0 }
    }

    fn start_command(&mut self, task: Task) -> Result<(), String> {
        self.commands_ran.lock().unwrap().push(task.command.clone());
        let fs = self.fs;
        match task.rule.as_str() {
            "cat" | "cat_rsp" | "cc" | "touch" | "lose-once" | "lose-always" | "touch-interrupt" | "touch-fail-tick2" | "cp_multi_msvc" | "cp_multi_gcc" => {
                for o in &task.outputs {
                    fs.create(o, "");
                }
            }
            "true" | "fail" | "interrupt" | "console" => {}
            "cp" => {
                if let Some(c) = fs.read_file(&task.inputs[0]).unwrap() {
                    fs.write_file(&task.outputs[0], &c).unwrap();
                }
            }
            // Like cp, and also writes the input to `d.d`.
            "cp-deps" => {
                if let Some(c) = fs.read_file(&task.inputs[0]).unwrap() {
                    fs.write_file(&task.outputs[0], &c).unwrap();
                    fs.write_file("d.d", &c).unwrap();
                }
            }
            // Writes the task's inputs to its output.
            "list-inputs" => fs.create(&task.outputs[0], &task.inputs.join(" ")),
            "touch-implicit-dep-out" => {
                let dep = task.binding("test_dependency").unwrap_or_default().to_string();
                fs.tick();
                fs.create(&dep, "");
                fs.tick();
                for o in &task.outputs {
                    fs.create(o, "");
                }
            }
            "generate-depfile" => {
                let dep = task.binding("test_dependency").unwrap_or_default().to_string();
                let depfile = task.depfile.clone().unwrap();
                let mut contents = String::new();
                for o in &task.outputs {
                    contents.push_str(&format!("{o}: {dep}\n"));
                    fs.create(o, "");
                }
                fs.create(&depfile, &contents);
            }
            other => return Err(format!("unknown command rule {other}")),
        }
        self.active.push(task);
        // Allow tests to control the order by the name of the first output.
        self.active.sort_by(|a, b| a.outputs[0].cmp(&b.outputs[0]));
        Ok(())
    }

    fn wait_for_command(&mut self) -> WaitFuture<'_> {
        // Interrupted commands stay active, as in ninja's fake runner.
        if let Some(last) = self.active.last()
            && (last.rule == "interrupt" || last.rule == "touch-interrupt")
        {
            let edge = last.edge;
            return Box::pin(async move {
                Some(TaskResult {
                    edge,
                    status: ExitStatus::Interrupted,
                    output: Vec::new(),
                    kind: ExecKind::Local,
                    cas_outputs: Vec::new(),
                    lost_inputs: Vec::new(),
                })
            });
        }
        let now = self.fs.now();
        // "lose-once" reports its inputs lost the first time it runs,
        // "lose-always" every time (action rewinding).
        if let Some(task) = self.active.last()
            && (task.rule == "lose-always" || (task.rule == "lose-once" && !self.lost.contains(&task.edge)))
        {
            let task = self.active.pop().unwrap();
            self.lost.push(task.edge);
            let r = TaskResult {
                edge: task.edge,
                status: ExitStatus::Failure(1),
                output: Vec::new(),
                kind: ExecKind::Remote,
                cas_outputs: Vec::new(),
                lost_inputs: task.inputs.clone(),
            };
            return Box::pin(async move { Some(r) });
        }
        let r = self.active.pop().map(|task| {
            let mut output = Vec::new();
            let status = match task.rule.as_str() {
                "touch-fail-tick2" if now == 2 => ExitStatus::Failure(1),
                "console" => {
                    if task.use_console {
                        ExitStatus::Success
                    } else {
                        ExitStatus::Failure(1)
                    }
                }
                "fail" => ExitStatus::Failure(1),
                "cp_multi_msvc" => {
                    for i in &task.inputs {
                        output.extend_from_slice(format!("{}{}\n", task.msvc_deps_prefix, i).as_bytes());
                    }
                    ExitStatus::Success
                }
                _ => ExitStatus::Success,
            };
            TaskResult { edge: task.edge, status, output, kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() }
        });
        Box::pin(async move { r })
    }

    fn active_edges(&self) -> Vec<EdgeId> {
        self.active.iter().map(|t| t.edge).collect()
    }

    fn abort(&mut self) -> AbortFuture<'_> {
        self.active.clear();
        Box::pin(std::future::ready(()))
    }
}

struct BuildTest {
    state: State,
    fs: VirtualDisk,
    config: BuildConfig,
    commands_ran: Arc<Mutex<Vec<String>>>,
    build_log: Option<BuildLog>,
    deps_log: Option<DepsLog>,
}

const CAT_RULE: &str = "rule cat\n  command = cat $in > $out\n";

impl BuildTest {
    fn new() -> Self {
        let mut t = Self::empty();
        t.parse("build cat1: cat in1\nbuild cat2: cat in1 in2\nbuild cat12: cat cat1 cat2\n");
        t.fs.create("in1", "");
        t.fs.create("in2", "");
        t
    }

    fn empty() -> Self {
        let mut t = Self {
            state: State::new(),
            fs: VirtualDisk::new(),
            config: BuildConfig { verbosity: Verbosity::Quiet, task_bindings: vec!["test_dependency".into()], ..Default::default() },
            commands_ran: Arc::new(Mutex::new(Vec::new())),
            build_log: None,
            deps_log: None,
        };
        t.parse(CAT_RULE);
        t
    }

    fn with_log() -> Self {
        let mut t = Self::new();
        t.build_log = Some(BuildLog::new());
        t
    }

    fn parse(&mut self, manifest: &str) {
        let files = MemFiles::default();
        ManifestParser::new(&mut self.state, &files, ParserOptions { quiet: true, ..Default::default() }).parse_test(manifest).unwrap();
    }

    fn commands(&self) -> Vec<String> {
        self.commands_ran.lock().unwrap().clone()
    }

    fn clear(&mut self) {
        self.commands_ran.lock().unwrap().clear();
        self.state.reset();
    }

    /// Marks a path dirty (and missing, for source files).
    fn dirty(&mut self, path: &str) {
        let n = match self.state.lookup_node(path) {
            Some(n) => n,
            None => self.state.get_node(path, 0),
        };
        let node = self.state.node_mut(n);
        node.dirty = true;
        if node.in_edge.is_none() {
            node.exists = Existence::Missing;
            node.mtime = 0;
        }
    }

    /// Adds targets; returns Ok(up_to_date) or the AddTarget error.
    fn add(&mut self, targets: &[&str]) -> Result<bool, String> {
        let mut status = NullStatus::default();
        let mut b = Builder::new(&mut self.state, &self.fs, self.build_log.as_mut(), self.deps_log.as_mut(), &self.config, &mut status);
        for t in targets {
            b.add_target_by_name(t)?;
        }
        Ok(b.already_up_to_date())
    }

    /// AddTarget + Build, like ninja's tests do.
    fn build(&mut self, targets: &[&str]) -> Result<(), String> {
        let mut status = NullStatus::default();
        let mut b = Builder::new(&mut self.state, &self.fs, self.build_log.as_mut(), self.deps_log.as_mut(), &self.config, &mut status);
        for t in targets {
            b.add_target_by_name(t)?;
        }
        if b.already_up_to_date() {
            return Ok(());
        }
        let mut runner = FakeRunner { fs: &self.fs, commands_ran: self.commands_ran.clone(), active: Vec::new(), max_active: 1, lost: Vec::new() };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(b.build(&mut runner)).map_err(|e| e.message)
    }
}

#[test]
fn no_work() {
    let mut t = BuildTest::new();
    t.fs.create("cat1", "");
    t.fs.create("cat2", "");
    t.fs.create("cat12", "");
    assert!(t.add(&["cat12"]).unwrap());
}

#[test]
fn one_step() {
    let mut t = BuildTest::new();
    t.dirty("cat1");
    t.build(&["cat1"]).unwrap();
    assert_eq!(t.commands(), vec!["cat in1 > cat1"]);
}

#[test]
fn two_step() {
    let mut t = BuildTest::new();
    t.build(&["cat12"]).unwrap();
    let c = t.commands();
    assert_eq!(c.len(), 3);
    assert!((c[0] == "cat in1 > cat1" && c[1] == "cat in1 in2 > cat2") || (c[1] == "cat in1 > cat1" && c[0] == "cat in1 in2 > cat2"));
    assert_eq!(c[2], "cat cat1 cat2 > cat12");
    t.fs.tick();
    // Modifying in2 requires rebuilding one intermediate file and the final file.
    t.fs.create("in2", "");
    t.state.reset();
    t.build(&["cat12"]).unwrap();
    let c = t.commands();
    assert_eq!(&c[3..], &["cat in1 in2 > cat2", "cat cat1 cat2 > cat12"]);
}

#[test]
fn two_outputs() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nbuild out1 out2: touch in.txt\n");
    t.fs.create("in.txt", "");
    t.build(&["out1"]).unwrap();
    assert_eq!(t.commands(), vec!["touch out1 out2"]);
}

#[test]
fn implicit_output() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nbuild out | out.imp: touch in.txt\n");
    t.fs.create("in.txt", "");
    t.build(&["out.imp"]).unwrap();
    assert_eq!(t.commands(), vec!["touch out out.imp"]);
}

#[test]
fn multi_out_in() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nbuild in1 otherfile: touch in\nbuild out: touch in | in1\n");
    t.fs.create("in", "");
    t.fs.tick();
    t.fs.create("in1", "");
    t.build(&["out"]).unwrap();
}

#[test]
fn chain() {
    let mut t = BuildTest::new();
    t.parse("build c2: cat c1\nbuild c3: cat c2\nbuild c4: cat c3\nbuild c5: cat c4\n");
    t.fs.create("c1", "");
    t.build(&["c5"]).unwrap();
    assert_eq!(t.commands().len(), 4);
    t.clear();
    assert!(t.add(&["c5"]).unwrap());
    t.fs.tick();
    t.fs.create("c3", "");
    t.clear();
    assert!(!t.add(&["c5"]).unwrap());
    t.state.reset();
    t.build(&["c5"]).unwrap();
    assert_eq!(t.commands().len(), 2); // 3->4, 4->5
}

#[test]
fn missing_input() {
    let mut t = BuildTest::new();
    t.dirty("in1");
    assert_eq!(t.add(&["cat1"]).unwrap_err(), "'in1', needed by 'cat1', missing and no known rule to make it");
}

#[test]
fn missing_target() {
    let mut t = BuildTest::new();
    assert_eq!(t.add(&["meow"]).unwrap_err(), "unknown target: 'meow'");
}

#[test]
fn missing_input_target() {
    let mut t = BuildTest::new();
    t.dirty("in1");
    assert_eq!(t.add(&["in1"]).unwrap_err(), "'in1' missing and no known rule to make it");
}

#[test]
fn dep_file_ok() {
    let mut t = BuildTest::new();
    let orig_edges = t.state.edges.len();
    t.parse("rule cc\n  command = cc $in\n  depfile = $out.d\nbuild foo.o: cc foo.c\n");
    let edge = EdgeId((t.state.edges.len() - 1) as u32);
    t.fs.create("foo.c", "");
    t.fs.create("foo.o", "");
    t.dirty("bar.h");
    t.fs.create("foo.o.d", "foo.o: blah.h bar.h\n");
    t.add(&["foo.o"]).unwrap();
    assert_eq!(t.state.edges.len(), orig_edges + 1);
    let s = &t.state;
    assert!(!s.node(s.lookup_node("foo.o").unwrap()).generated_by_dep_loader);
    assert!(!s.node(s.lookup_node("foo.c").unwrap()).generated_by_dep_loader);
    assert!(s.node(s.lookup_node("blah.h").unwrap()).generated_by_dep_loader);
    assert!(s.node(s.lookup_node("bar.h").unwrap()).generated_by_dep_loader);
    assert_eq!(s.edge(edge).inputs.len(), 3);
    assert_eq!(s.evaluate_command(edge, false).unwrap(), "cc foo.c");
}

#[test]
fn dep_file_parse_error() {
    let mut t = BuildTest::new();
    t.parse("rule cc\n  command = cc $in\n  depfile = $out.d\nbuild foo.o: cc foo.c\n");
    t.fs.create("foo.c", "");
    t.fs.create("foo.o", "");
    t.fs.create("foo.o.d", "randomtext\n");
    assert_eq!(t.add(&["foo.o"]).unwrap_err(), "foo.o.d: expected ':' in depfile");
}

#[test]
fn encounter_ready_twice() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nbuild c: touch\nbuild b: touch || c\nbuild a: touch | b || c\n");
    t.fs.create("b", "");
    t.build(&["a"]).unwrap();
    assert_eq!(t.commands().len(), 2);
}

#[test]
fn order_only_deps() {
    let mut t = BuildTest::new();
    t.parse("rule cc\n  command = cc $in\n  depfile = $out.d\nbuild foo.o: cc foo.c || otherfile\n");
    let edge = EdgeId((t.state.edges.len() - 1) as u32);
    t.fs.create("foo.c", "");
    t.fs.create("foo.o", "");
    t.fs.create("otherfile", "");
    t.fs.create("foo.o.d", "foo.o: blah.h bar.h\n");
    // explicit dep dirty, expect a rebuild.
    t.build(&["foo.o"]).unwrap();
    {
        let s = &t.state;
        let e = s.edge(edge);
        assert_eq!(e.inputs.len(), 4);
        assert_eq!(e.implicit_deps, 2);
        assert_eq!(e.order_only_deps, 1);
        let names: Vec<&str> = e.inputs.iter().map(|&n| s.path(n)).collect();
        assert_eq!(names, vec!["foo.c", "blah.h", "bar.h", "otherfile"]);
    }
    assert_eq!(t.commands().len(), 1);

    t.fs.tick();
    t.fs.create("foo.o.d", "foo.o: blah.h bar.h\n");
    // implicit dep dirty, expect a rebuild.
    t.fs.create("blah.h", "");
    t.fs.create("bar.h", "");
    t.clear();
    t.build(&["foo.o"]).unwrap();
    assert_eq!(t.commands().len(), 1);

    t.fs.tick();
    t.fs.create("foo.o.d", "foo.o: blah.h bar.h\n");
    // order only dep dirty, no rebuild.
    t.fs.create("otherfile", "");
    t.clear();
    assert!(t.add(&["foo.o"]).unwrap());

    // implicit dep missing, expect rebuild.
    t.fs.remove_file("bar.h").unwrap();
    t.clear();
    t.build(&["foo.o"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn rebuild_order_only_deps() {
    let mut t = BuildTest::new();
    t.parse("rule cc\n  command = cc $in\nrule true\n  command = true\nbuild oo.h: cc oo.h.in\nbuild foo.o: cc foo.c || oo.h\n");
    t.fs.create("foo.c", "");
    t.fs.create("oo.h.in", "");
    // foo.o and order-only dep dirty, build both.
    t.build(&["foo.o"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    // all clean, no rebuild.
    t.clear();
    assert!(t.add(&["foo.o"]).unwrap());
    // order-only dep missing, build it only.
    t.fs.remove_file("oo.h").unwrap();
    t.clear();
    t.build(&["foo.o"]).unwrap();
    assert_eq!(t.commands(), vec!["cc oo.h.in"]);
    t.fs.tick();
    // order-only dep dirty, build it only.
    t.fs.create("oo.h.in", "");
    t.clear();
    t.build(&["foo.o"]).unwrap();
    assert_eq!(t.commands(), vec!["cc oo.h.in"]);
}

#[test]
fn phony() {
    let mut t = BuildTest::new();
    t.parse("build out: cat bar.cc\nbuild all: phony out\n");
    t.fs.create("bar.cc", "");
    t.build(&["all"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn phony_no_work() {
    let mut t = BuildTest::new();
    t.parse("build out: cat bar.cc\nbuild all: phony out\n");
    t.fs.create("bar.cc", "");
    t.fs.create("out", "");
    assert!(t.add(&["all"]).unwrap());
}

#[test]
fn phony_self_reference() {
    let mut t = BuildTest::new();
    t.parse("build a: phony a\n");
    assert!(t.add(&["a"]).unwrap());
}

fn phony_use_case(i: usize) {
    let mut t = BuildTest::new();
    t.parse(
        "rule touch\n command = touch $out\nbuild notreal: phony blank\nbuild phony1: phony notreal\nbuild phony2: phony\nbuild phony3: phony blank\nbuild phony4: phony notreal\nbuild phony5: phony\nbuild phony6: phony blank\n\nbuild test1: touch phony1\nbuild test2: touch phony2\nbuild test3: touch phony3\nbuild test4: touch phony4\nbuild test5: touch phony5\nbuild test6: touch phony6\n",
    );
    t.fs.create("blank", "");
    t.build(&["test1", "test2", "test3", "test4", "test5", "test6"]).unwrap();
    let ci = format!("test{i}");
    // Tests 1, 3, 4, and 6 should rebuild when the input is updated.
    if i != 2 && i != 5 {
        let phony_node = t.state.lookup_node(&format!("phony{i}")).unwrap();
        let _ = phony_node;
        t.state.reset();
        t.commands_ran.lock().unwrap().clear();
        t.fs.tick();
        t.fs.create("blank", ""); // a "real" file
        t.build(&[&ci]).unwrap();
        assert_eq!(t.commands(), vec![format!("touch {ci}")]);

        t.state.reset();
        t.commands_ran.lock().unwrap().clear();
        assert!(t.add(&[&ci]).unwrap());
    } else {
        // Tests 2 and 5: Expect dependents to always rebuild.
        t.state.reset();
        t.commands_ran.lock().unwrap().clear();
        t.fs.tick();
        t.build(&[&ci]).unwrap();
        assert_eq!(t.commands(), vec![format!("touch {ci}")]);

        t.state.reset();
        t.commands_ran.lock().unwrap().clear();
        t.build(&[&ci]).unwrap();
        assert_eq!(t.commands(), vec![format!("touch {ci}")]);
    }
}

#[test]
fn phony_use_cases() {
    for i in 1..=6 {
        phony_use_case(i);
    }
}

#[test]
fn fail() {
    let mut t = BuildTest::new();
    t.parse("rule fail\n  command = fail\nbuild out1: fail\n");
    assert_eq!(t.build(&["out1"]).unwrap_err(), "subcommand failed");
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn swallow_failures() {
    let mut t = BuildTest::new();
    t.parse("rule fail\n  command = fail\nbuild out1: fail\nbuild out2: fail\nbuild out3: fail\nbuild all: phony out1 out2 out3\n");
    // Swallow two failures, die on the third.
    t.config.failures_allowed = 3;
    assert_eq!(t.build(&["all"]).unwrap_err(), "subcommands failed");
    assert_eq!(t.commands().len(), 3);
}

#[test]
fn swallow_failures_limit() {
    let mut t = BuildTest::new();
    t.parse("rule fail\n  command = fail\nbuild out1: fail\nbuild out2: fail\nbuild out3: fail\nbuild final: cat out1 out2 out3\n");
    // Swallow ten failures; we should stop before building final.
    t.config.failures_allowed = 11;
    assert_eq!(t.build(&["final"]).unwrap_err(), "cannot make progress due to previous errors");
    assert_eq!(t.commands().len(), 3);
}

#[test]
fn swallow_failures_pool() {
    let mut t = BuildTest::new();
    t.parse("pool failpool\n  depth = 1\nrule fail\n  command = fail\n  pool = failpool\nbuild out1: fail\nbuild out2: fail\nbuild out3: fail\nbuild final: cat out1 out2 out3\n");
    t.config.failures_allowed = 11;
    assert_eq!(t.build(&["final"]).unwrap_err(), "cannot make progress due to previous errors");
    assert_eq!(t.commands().len(), 3);
}

#[test]
fn pool_with_depth_one_serializes() {
    let mut t = BuildTest::new();
    t.parse("pool foobar\n  depth = 1\nrule poolcat\n  command = cat $in > $out\n  pool = foobar\nbuild out1: cat in\nbuild out2: cat in\n");
    t.parse("rule touch\n  command = touch $out\n  pool = foobar\nbuild p1: touch\nbuild p2: touch\nbuild all: phony p1 p2\n");
    // With unlimited runner capacity, the pool still serializes edges.
    let mut status = NullStatus::default();
    let mut b = Builder::new(&mut t.state, &t.fs, None, None, &t.config, &mut status);
    b.add_target_by_name("all").unwrap();
    let mut runner = FakeRunner { fs: &t.fs, commands_ran: t.commands_ran.clone(), active: Vec::new(), max_active: 10, lost: Vec::new() };
    struct Check<'a> {
        inner: FakeRunner<'a>,
        max_seen: usize,
    }
    impl CommandRunner for Check<'_> {
        fn can_run_more(&self) -> usize {
            self.inner.can_run_more()
        }
        fn start_command(&mut self, task: Task) -> Result<(), String> {
            self.inner.start_command(task)?;
            self.max_seen = self.max_seen.max(self.inner.active.len());
            Ok(())
        }
        fn wait_for_command(&mut self) -> WaitFuture<'_> {
            self.inner.wait_for_command()
        }
        fn active_edges(&self) -> Vec<EdgeId> {
            self.inner.active_edges()
        }
        fn abort(&mut self) -> AbortFuture<'_> {
            self.inner.abort()
        }
    }
    runner.max_active = 10;
    let mut check = Check { inner: runner, max_seen: 0 };
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(b.build(&mut check)).unwrap();
    assert_eq!(check.max_seen, 1);
    assert_eq!(t.commands().len(), 2);
}

#[test]
fn pool_edges_ready_but_not_wanted() {
    let manifest = "pool some_pool\n  depth = 4\nrule touch\n  command = touch $out\n  pool = some_pool\nrule cc\n  command = touch grit\n\nbuild B.d.stamp: cc | x\nbuild C.stamp: touch B.d.stamp\nbuild final.stamp: touch || C.stamp\n";
    let mut t = BuildTest::empty();
    t.parse(manifest);
    t.fs.create("x", "");
    t.build(&["final.stamp"]).unwrap();
    t.fs.remove_file("B.d.stamp").unwrap();
    let mut t2 = BuildTest::empty();
    t2.fs = VirtualDisk::new();
    for p in ["x", "C.stamp", "final.stamp"] {
        t2.fs.create(p, "");
    }
    t2.parse(manifest);
    t2.build(&["final.stamp"]).unwrap();
    let pool = t2.state.lookup_pool("some_pool").unwrap();
    assert_eq!(t2.state.pools[pool.0 as usize].current_use, 0);
}

#[test]
fn not_in_log_but_on_disk() {
    let mut t = BuildTest::with_log();
    t.parse("rule cc\n  command = cc\nbuild out1: cc in\n");
    // Create input/output that would be considered up to date when not
    // considering the command line hash.
    t.fs.create("in", "");
    t.fs.create("out1", "");
    // Because it's not in the log, it should not be up-to-date until we
    // build again.
    assert!(!t.add(&["out1"]).unwrap());
    t.state.reset();
    t.build(&["out1"]).unwrap();
    t.clear();
    assert!(t.add(&["out1"]).unwrap());
}

#[test]
fn rebuild_after_failure() {
    let mut t = BuildTest::with_log();
    t.parse(
        "rule touch-fail-tick2
  command = touch-fail-tick2
build out1: touch-fail-tick2 in
",
    );
    t.fs.create("in", "");
    // Run once successfully to get out1 in the log.
    t.build(&["out1"]).unwrap();
    assert_eq!(t.commands().len(), 1);
    t.clear();
    t.fs.tick();
    t.fs.create("in", "");
    // Run again with a failure that updates the output file timestamp.
    assert_eq!(t.build(&["out1"]).unwrap_err(), "subcommand failed");
    assert_eq!(t.commands().len(), 1);
    t.clear();
    t.fs.tick();
    // Run again, should rerun even though the output file is up to date on disk.
    assert!(!t.add(&["out1"]).unwrap());
    t.state.reset();
    t.build(&["out1"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn restat_test() {
    let mut t = BuildTest::with_log();
    t.parse(
        "rule true\n  command = true\n  restat = 1\nrule cc\n  command = cc\n  restat = 1\nbuild out1: cc in\nbuild out2: true out1\nbuild out3: cat out2\n",
    );
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    t.fs.create("out3", "");
    t.fs.tick();
    t.fs.create("in", "");
    // Do a pre-build so that there's commands in the log for the outputs.
    t.build(&["out3"]).unwrap();
    assert_eq!(t.commands().len(), 3);
    t.clear();
    t.fs.tick();
    t.fs.create("in", "");
    // "cc" touches out1, so we should build out2. But because "true" does not
    // touch out2, we should cancel the build of out3.
    t.build(&["out3"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    // If we run again, it should be a no-op, because the build log has
    // recorded that we've already built out2 with an input timestamp of 2
    // (from out1).
    t.clear();
    assert!(t.add(&["out3"]).unwrap());
    t.fs.tick();
    t.fs.create("in", "");
    // The build log entry should not, however, prevent us from rebuilding
    // out2 if out1 changes.
    t.clear();
    t.build(&["out3"]).unwrap();
    assert_eq!(t.commands().len(), 2);
}

#[test]
fn restat_missing_file() {
    let mut t = BuildTest::with_log();
    t.parse("rule true\n  command = true\n  restat = 1\nrule cc\n  command = cc\nbuild out1: true in\nbuild out2: cc out1\n");
    t.fs.create("in", "");
    t.fs.create("out2", "");
    t.build(&["out2"]).unwrap();
    t.clear();
    t.fs.tick();
    t.fs.create("in", "");
    t.fs.create("out2", "");
    // Run a build, expect only the first command to run. It doesn't touch its
    // output (due to being the "true" command), so we shouldn't run the
    // dependent build.
    t.build(&["out2"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn restat_single_dependent_output_dirty() {
    let mut t = BuildTest::with_log();
    t.parse(
        "rule true\n  command = true\n  restat = 1\nrule touch\n  command = touch\nbuild out1: true in\nbuild out2 out3: touch out1\nbuild out4: touch out2\n",
    );
    t.fs.create("in", "");
    t.build(&["out4"]).unwrap();
    assert_eq!(t.commands().len(), 3);
    t.fs.tick();
    t.fs.create("in", "");
    t.fs.remove_file("out3").unwrap();
    t.clear();
    t.build(&["out4"]).unwrap();
    assert_eq!(t.commands().len(), 3);
}

#[test]
fn dry_run_all_commands_shown() {
    let mut t = BuildTest::with_log();
    t.config.dry_run = true;
    t.parse(
        "rule true\n  command = true\n  restat = 1\nrule cc\n  command = cc\n  restat = 1\nbuild out1: cc in\nbuild out2: true out1\nbuild out3: cat out2\n",
    );
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    t.fs.create("out3", "");
    t.fs.tick();
    t.fs.create("in", "");
    // "cc" touches out1, so we should build out2. But because "true" does not
    // touch out2, we should cancel the build of out3.
    let mut status = NullStatus::default();
    let mut b = Builder::new(&mut t.state, &t.fs, t.build_log.as_mut(), None, &t.config, &mut status);
    b.add_target_by_name("out3").unwrap();
    let mut runner = crate::runner::DryRunRunner::default();
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(b.build(&mut runner)).unwrap();
    assert_eq!(status.started.len(), 3);
}

#[test]
fn rsp_file_success() {
    let mut t = BuildTest::new();
    t.parse(
        "rule cat_rsp\n  command = cat $rspfile > $out\n  rspfile = $rspfile\n  rspfile_content = $long_command\nbuild out1: cat in\nbuild out2: cat_rsp in\n  rspfile = out 2.rsp\n  long_command = Some very long command\n",
    );
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    t.fs.tick();
    t.fs.create("in", "");
    t.build(&["out1", "out2"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    // The RSP file was created and then removed.
    assert!(t.fs.removed().iter().any(|p| p == "out 2.rsp"));
    assert!(!t.fs.exists("out 2.rsp"));
}

#[test]
fn rsp_file_failure_keeps_file() {
    let mut t = BuildTest::new();
    t.parse("rule fail\n  command = fail\n  rspfile = $rspfile\n  rspfile_content = $long_command\nbuild out: fail in\n  rspfile = out.rsp\n  long_command = Another very long command\n");
    t.fs.create("out", "");
    t.fs.tick();
    t.fs.create("in", "");
    assert_eq!(t.build(&["out"]).unwrap_err(), "subcommand failed");
    // The RSP file was created but not removed.
    assert_eq!(t.fs.contents("out.rsp").unwrap(), "Another very long command");
}

#[test]
fn rsp_file_cmd_line_change() {
    let mut t = BuildTest::with_log();
    t.parse("rule cat_rsp\n  command = cat $rspfile > $out\n  rspfile = $rspfile\n  rspfile_content = $long_command\nbuild out: cat_rsp in\n  rspfile = out.rsp\n  long_command = Original very long command\n");
    t.fs.create("out", "");
    t.fs.tick();
    t.fs.create("in", "");
    // 1. Build for the 1st time (-> populate log)
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands().len(), 1);
    // 2. Build again (no change)
    t.clear();
    assert!(t.add(&["out"]).unwrap());
    // 3. Alter the entry in the logfile (to simulate a change in the command
    // line between 2 builds).
    let mut entry = t.build_log.as_ref().unwrap().lookup_by_output("out").unwrap().clone();
    entry.command_hash = crate::build_log::hash_command("cat out.rsp > out;rspfile=Altered very long command");
    t.build_log.as_mut().unwrap().record(entry).unwrap();
    t.clear();
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn interrupt_cleanup() {
    let mut t = BuildTest::new();
    t.parse("rule interrupt\n  command = interrupt\nrule touch-interrupt\n  command = touch-interrupt\nbuild out1: interrupt in1\nbuild out2: touch-interrupt in2\n");
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    t.fs.tick();
    t.fs.create("in1", "");
    t.fs.create("in2", "");
    // An untouched output of an interrupted command should be retained.
    assert_eq!(t.build(&["out1"]).unwrap_err(), "interrupted by user");
    assert!(t.fs.stat("out1").unwrap() > 0);
    // A touched output of an interrupted command should be deleted.
    t.clear();
    assert_eq!(t.build(&["out2"]).unwrap_err(), "interrupted by user");
    assert_eq!(t.fs.stat("out2").unwrap(), 0);
}

#[test]
fn stat_failure_aborts_build() {
    let mut t = BuildTest::new();
    let too_long = "i".repeat(400);
    t.parse(&format!("build {too_long}: cat {too_long}\n"));
    t.fs.set_stat_error(&too_long, "stat failed");
    assert_eq!(t.add(&[&too_long]).unwrap_err(), "stat failed");
}

#[test]
fn phony_with_no_inputs() {
    let mut t = BuildTest::new();
    t.parse("build nonexistent: phony\nbuild out1: cat || nonexistent\nbuild out2: cat nonexistent\n");
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    // out1 should be up to date even though its input is dirty, because its
    // order-only dependency has nothing to do.
    assert!(t.add(&["out1"]).unwrap());
    // out2 should still be out of date though, because its input is dirty.
    t.clear();
    t.build(&["out2"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn deps_gcc_with_empty_depfile_errors_out() {
    let mut t = BuildTest::new();
    t.deps_log = Some(DepsLog::new());
    t.parse("rule cc\n  command = cc\n  deps = gcc\nbuild out: cc\n");
    t.dirty("out");
    let err = t.build(&["out"]).unwrap_err();
    assert_eq!(err, "subcommand failed");
}

#[test]
fn console() {
    let mut t = BuildTest::new();
    t.parse("rule console\n  command = console\n  pool = console\nbuild cons: console in.txt\n");
    t.fs.create("in.txt", "");
    t.build(&["cons"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn deps_msvc_and_gcc_recorded() {
    let mut t = BuildTest::new();
    t.deps_log = Some(DepsLog::new());
    t.parse("rule cp_multi_msvc\n  command = cp_multi_msvc $in\n  deps = msvc\n  msvc_deps_prefix = using \nbuild out: cp_multi_msvc in1 in2\n");
    t.build(&["out"]).unwrap();
    let deps = t.deps_log.as_ref().unwrap().get("out").unwrap();
    assert_eq!(deps.paths().collect::<Vec<_>>(), vec!["in1", "in2"]);
}

#[test]
fn depfile_gcc_recorded_and_deleted() {
    let mut t = BuildTest::new();
    t.deps_log = Some(DepsLog::new());
    t.parse("rule generate-depfile\n  command = gen $out\n  depfile = $out.d\n  deps = gcc\nbuild out: generate-depfile in1\n  test_dependency = dep.h\n");
    t.fs.create("dep.h", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.deps_log.as_ref().unwrap().get("out").unwrap().paths().collect::<Vec<_>>(), vec!["dep.h"]);
    assert!(!t.fs.exists("out.d"));
    // Now the deps log knows dep.h: touching it triggers a rebuild.
    t.clear();
    assert!(t.add(&["out"]).unwrap());
    t.fs.tick();
    t.fs.create("dep.h", "");
    t.clear();
    assert!(!t.add(&["out"]).unwrap());
}

#[test]
fn wrong_output_in_depfile_causes_rebuild() {
    let mut t = BuildTest::new();
    t.parse("rule cc\n  command = cc $in\n  depfile = $out.d\nbuild foo.o: cc foo.c\n");
    t.fs.create("foo.c", "");
    t.fs.create("foo.o", "");
    t.fs.create("header.h", "");
    t.fs.create("foo.o.d", "bar.o.d: header.h\n");
    t.build(&["foo.o"]).unwrap();
    assert_eq!(t.commands().len(), 1);
}

#[test]
fn dyndep_missing_and_no_rule() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nbuild out: touch || dd\n  dyndep = dd\n");
    assert_eq!(t.add(&["out"]).unwrap_err(), "loading 'dd': No such file or directory");
}

#[test]
fn dyndep_ready_implicit_connection() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nbuild tmp: touch || dd\n  dyndep = dd\nbuild out: touch || dd\n  dyndep = dd\n");
    t.fs.create("dd", "ninja_dyndep_version = 1\nbuild out | out.imp: dyndep | tmp.imp\nbuild tmp | tmp.imp: dyndep\n");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["touch tmp tmp.imp", "touch out out.imp"]);
}

#[test]
fn dyndep_ready_syntax_error() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nbuild out: touch || dd\n  dyndep = dd\n");
    t.fs.create("dd", "build out: dyndep\n");
    assert_eq!(t.add(&["out"]).unwrap_err(), "dd:1: expected 'ninja_dyndep_version = ...'\n");
}

#[test]
fn dyndep_ready_circular() {
    let mut t = BuildTest::new();
    t.parse("rule r\n  command = unused\nbuild out: r in || dd\n  dyndep = dd\nbuild in: r circ\n");
    t.fs.create("dd", "ninja_dyndep_version = 1\nbuild out | circ: dyndep\n");
    t.fs.create("out", "");
    assert_eq!(t.add(&["out"]).unwrap_err(), "dependency cycle: circ -> in -> circ");
}

#[test]
fn dyndep_build() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out: touch || dd\n  dyndep = dd\n");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out: dyndep\n");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch out"]);
}

#[test]
fn dyndep_build_syntax_error() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out: touch || dd\n  dyndep = dd\n");
    t.fs.create("dd-in", "build out: dyndep\n");
    assert_eq!(t.build(&["out"]).unwrap_err(), "dd:1: expected 'ninja_dyndep_version = ...'\n");
}

#[test]
fn dyndep_build_unrelated_output() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild unrelated: touch || dd\nbuild out: touch unrelated || dd\n  dyndep = dd\n");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out: dyndep\n");
    t.fs.tick();
    t.fs.create("out", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch unrelated", "touch out"]);
}

#[test]
fn dyndep_build_discover_new_output() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out: touch in || dd\n  dyndep = dd\n");
    t.fs.create("in", "");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out | out.imp: dyndep\n");
    t.fs.tick();
    t.fs.create("out", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch out out.imp"]);
}

#[test]
fn dyndep_build_discover_new_output_with_multiple_rules() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out1 | out-twice.imp: touch in\nbuild out2: touch in || dd\n  dyndep = dd\n");
    t.fs.create("in", "");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out2 | out-twice.imp: dyndep\n");
    t.fs.tick();
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    assert_eq!(t.build(&["out1", "out2"]).unwrap_err(), "multiple rules generate out-twice.imp");
}

#[test]
fn dyndep_build_discover_new_input() {
    let mut t = BuildTest::new();
    t.parse(
        "rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild in: touch\nbuild out: touch || dd\n  dyndep = dd\n",
    );
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out: dyndep | in\n");
    t.fs.tick();
    t.fs.create("out", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch in", "touch out"]);
}

#[test]
fn dyndep_build_discover_implicit_connection() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild tmp: touch || dd\n  dyndep = dd\nbuild out: touch || dd\n  dyndep = dd\n");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out | out.imp: dyndep | tmp.imp\nbuild tmp | tmp.imp: dyndep\n");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch tmp tmp.imp", "touch out out.imp"]);
}

#[test]
fn dyndep_build_discover_now_want_edge() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild tmp: touch || dd\n  dyndep = dd\nbuild out: touch tmp || dd\n  dyndep = dd\n");
    t.fs.create("tmp", "");
    t.fs.create("out", "");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out: dyndep\nbuild tmp | tmp.imp: dyndep\n");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch tmp tmp.imp", "touch out out.imp"]);
}

#[test]
fn dyndep_build_discover_now_want_edge_and_dependent() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild tmp: touch || dd\n  dyndep = dd\nbuild out: touch tmp\n");
    t.fs.create("tmp", "");
    t.fs.create("out", "");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild tmp | tmp.imp: dyndep\n");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "touch tmp tmp.imp", "touch out out.imp"]);
}

#[test]
fn dyndep_build_discover_circular() {
    let mut t = BuildTest::new();
    t.parse("rule r\n  command = unused\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out: r in || dd\n  depfile = out.d\n  dyndep = dd\nbuild in: r || dd\n  dyndep = dd\n");
    t.fs.create("out.d", "out: inimp\n");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out | circ: dyndep\nbuild in: dyndep | circ\n");
    t.fs.create("out", "");
    let err = t.build(&["out"]).unwrap_err();
    // Depending on when cycle detection happens, the cycle is reported from
    // a different starting point.
    assert!(err == "dependency cycle: circ -> in -> circ" || err == "dependency cycle: in -> circ -> in", "{err}");
}

#[test]
fn dyndep_build_discover_restat() {
    let mut t = BuildTest::with_log();
    t.parse("rule touch\n  command = touch $out\nrule true\n  command = true\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out1: true in || dd\n  dyndep = dd\nbuild out2: cat out1\n");
    t.fs.create("out1", "");
    t.fs.create("out2", "");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out1: dyndep\n  restat = 1\n");
    t.fs.tick();
    t.fs.create("in", "");
    // Do a pre-build so that there's commands in the log for the outputs,
    // otherwise, the lack of an entry in the build log will cause "out2" to
    // rebuild regardless of restat.
    t.build(&["out2"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd-in dd", "true", "cat out1 > out2"]);
    t.clear();
    t.fs.tick();
    t.fs.create("in", "");
    // We touched "in", so we should build "out1". But because "true" does not
    // touch "out1", we should cancel the build of "out2".
    t.build(&["out2"]).unwrap();
    assert_eq!(t.commands(), vec!["true"]);
}

#[test]
fn dyndep_build_discover_scheduled_edge() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild out1 | out1.imp: touch\nbuild zdd: cp zdd-in\n  verify_active_edge = out1\nbuild out2: cp out1 || zdd\n  dyndep = zdd\n");
    t.fs.create("zdd-in", "ninja_dyndep_version = 1\nbuild out2: dyndep | out1.imp\n");
    // Enable concurrent builds so that we can load the dyndep file while
    // another edge is still active.
    let mut status = NullStatus::default();
    let mut b = Builder::new(&mut t.state, &t.fs, None, None, &t.config, &mut status);
    b.add_target_by_name("out2").unwrap();
    let mut runner = FakeRunner { fs: &t.fs, commands_ran: t.commands_ran.clone(), active: Vec::new(), max_active: 2, lost: Vec::new() };
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(b.build(&mut runner)).unwrap();
    assert_eq!(t.commands(), vec!["touch out1 out1.imp", "cp zdd-in zdd", "cp out1 out2"]);
}

#[test]
fn dyndep_two_level_direct() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out $out.imp\nrule cp\n  command = cp $in $out\nbuild dd1: cp dd1-in\nbuild out1 | out1.imp: touch || dd1\n  dyndep = dd1\nbuild dd2: cp dd2-in || dd1\nbuild out2: touch || dd2\n  dyndep = dd2\n");
    t.fs.create("out1.imp", "");
    t.fs.create("out2", "");
    t.fs.create("out2.imp", "");
    t.fs.create("dd1-in", "ninja_dyndep_version = 1\nbuild out1: dyndep\n");
    t.fs.create("dd2-in", "");
    t.fs.create("dd2", "ninja_dyndep_version = 1\nbuild out2 | out2.imp: dyndep | out1.imp\n");
    t.build(&["out2"]).unwrap();
    assert_eq!(t.commands(), vec!["cp dd1-in dd1", "touch out1 out1.imp", "touch out2 out2.imp"]);
}

#[test]
fn dyndep_build_multiple() {
    let mut t = BuildTest::new();
    t.parse("rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out1: touch in1 || dd\n  dyndep = dd\nbuild out2: touch in2 || dd\n  dyndep = dd\nbuild outs: phony out1 out2\n");
    t.fs.create("in1", "");
    t.fs.create("in2", "");
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out1: dyndep\nbuild out2: dyndep\n");
    t.build(&["outs"]).unwrap();
    let c = t.commands();
    assert_eq!(c[0], "cp dd-in dd");
    assert_eq!(c.len(), 3);
}

#[test]
fn validation() {
    let mut t = BuildTest::new();
    t.parse("build out: cat in |@ validate\nbuild validate: cat in2\n");
    t.fs.create("in", "");
    t.fs.create("in2", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    // Test touching "in" only rebuilds "out" ("validate" doesn't depend on "out").
    t.fs.tick();
    t.fs.create("in", "");
    t.clear();
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cat in > out"]);
    // Test touching "in2" only rebuilds "validate".
    t.fs.tick();
    t.fs.create("in2", "");
    t.clear();
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cat in2 > validate"]);
}

#[test]
fn validation_depends_on_output() {
    let mut t = BuildTest::new();
    t.parse("build out: cat in |@ validate\nbuild validate: cat in2 | out\n");
    t.fs.create("in", "");
    t.fs.create("in2", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    // Test touching "in" rebuilds "out" and "validate".
    t.fs.tick();
    t.fs.create("in", "");
    t.clear();
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    // Test touching "in2" only rebuilds "validate".
    t.fs.tick();
    t.fs.create("in2", "");
    t.clear();
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cat in2 > validate"]);
}

#[test]
fn validation_circular() {
    let mut t = BuildTest::new();
    t.parse("build out: cat in |@ out2\nbuild out2: cat in2 |@ out\n");
    t.fs.create("in", "");
    t.fs.create("in2", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands().len(), 2);
    t.fs.tick();
    t.fs.create("in", "");
    t.clear();
    t.build(&["out"]).unwrap();
    assert_eq!(t.commands(), vec!["cat in > out"]);
}

#[test]
fn validation_with_circular_dependency() {
    let mut t = BuildTest::new();
    t.parse("build out: cat in |@ validate\nbuild validate: cat validate_in | out\nbuild validate_in: cat validate\n");
    t.fs.create("in", "");
    assert_eq!(t.add(&["out"]).unwrap_err(), "dependency cycle: validate -> validate_in -> validate");
}

#[test]
fn cycle_detection() {
    let mut t = BuildTest::new();
    t.parse("build a: cat b\nbuild b: cat c\nbuild c: cat a\n");
    assert_eq!(t.add(&["a"]).unwrap_err(), "dependency cycle: a -> b -> c -> a");
}

#[test]
fn generator_command_change_does_not_rebuild() {
    let mut t = BuildTest::with_log();
    t.parse("rule touch\n  command = touch $out $extra\n  generator = 1\nbuild out: touch in\n");
    t.fs.create("in", "");
    t.build(&["out"]).unwrap();
    t.clear();
    t.parse("build out2: touch in\n  extra = changed\n");
    assert!(t.add(&["out"]).unwrap());
}

#[test]
fn command_change_rebuilds() {
    let mut t = BuildTest::with_log();
    t.parse("rule touch\n  command = touch $out\nbuild out: touch in\n");
    t.fs.create("in", "");
    t.build(&["out"]).unwrap();
    t.clear();
    assert!(t.add(&["out"]).unwrap());
    // Change the hash in the log as if the command changed.
    let mut e = t.build_log.as_ref().unwrap().lookup_by_output("out").unwrap().clone();
    e.command_hash ^= 1;
    t.build_log.as_mut().unwrap().record(e).unwrap();
    t.clear();
    assert!(!t.add(&["out"]).unwrap());
}

#[test]
fn critical_path_prefers_long_chains() {
    // With one slot, the longer chain is started first.
    let mut t = BuildTest::empty();
    t.parse("rule touch\n  command = touch $out\nbuild a1: touch\nbuild a2: touch a1\nbuild a3: touch a2\nbuild b1: touch\nbuild all: phony a3 b1\n");
    t.build(&["all"]).unwrap();
    assert_eq!(t.commands()[0], "touch a1");
}

#[test]
fn rewinds_lost_inputs() {
    let mut t = BuildTest::empty();
    t.parse("rule lose-once\n  command = lose $in > $out\nbuild a: cat in1\nbuild b: lose-once a\n");
    t.fs.create("in1", "");
    t.fs.tick();
    t.fs.create("a", "");
    t.build(&["b"]).unwrap();
    // b lost a: a's edge ran again, then b.
    assert_eq!(t.commands(), ["lose a > b", "cat in1 > a", "lose a > b"]);
}

#[test]
fn rewinding_needs_a_producer_and_stops() {
    // A lost source file can't be rebuilt.
    let mut t = BuildTest::empty();
    t.parse("rule lose-once\n  command = lose $in > $out\nbuild b: lose-once in1\n");
    t.fs.create("in1", "");
    assert!(t.build(&["b"]).is_err());
    assert_eq!(t.commands(), ["lose in1 > b"]);

    // Inputs lost again and again: the edge gives up.
    let mut t = BuildTest::empty();
    t.parse("rule lose-always\n  command = lose $in > $out\nbuild a: cat in1\nbuild b: lose-always a\n");
    t.fs.create("in1", "");
    t.fs.tick();
    t.fs.create("a", "");
    assert!(t.build(&["b"]).is_err());
    assert_eq!(t.commands().iter().filter(|c| c.starts_with("lose")).count(), 4);
}

/// ninja issue 2743: `g.h` is an output a dyndep file adds to `out`'s edge.
/// `a.o` reads it; `out` is missing. Built in either order, or through a
/// phony, `a.o` must be rebuilt after `out`, even though the scan meets
/// `g.h` as a source before loading the dyndep file.
fn dyndep_output_consumed_earlier(targets: &[&str], phony: bool) -> Vec<String> {
    let mut t = BuildTest::empty();
    t.parse("rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd: cp dd-in\nbuild out: touch || dd\n  dyndep = dd\nbuild a.o: touch a.c g.h || dd\n");
    if phony {
        t.parse("build all: phony a.o out\n");
    }
    t.fs.create("dd-in", "ninja_dyndep_version = 1\nbuild out | g.h: dyndep\n");
    t.fs.create("a.c", "");
    t.fs.tick();
    t.fs.create("dd", "ninja_dyndep_version = 1\nbuild out | g.h: dyndep\n");
    t.fs.tick();
    t.fs.create("g.h", "");
    t.fs.tick();
    t.fs.create("a.o", "");
    t.fs.tick();
    t.build(targets).unwrap();
    t.commands()
}

#[test]
fn dyndep_output_consumed_by_edge_scanned_earlier() {
    let want = vec!["touch out".to_string(), "touch a.o".to_string()];
    assert_eq!(dyndep_output_consumed_earlier(&["out", "a.o"], false), want);
    assert_eq!(dyndep_output_consumed_earlier(&["a.o", "out"], false), want);
    assert_eq!(dyndep_output_consumed_earlier(&["all"], true), want);
}

/// ninja issue 2653: `out1` is a dyndep output of `stamp1` and a dyndep
/// input of `stamp2`, whose build statement comes first. When `source`
/// changes, `stamp2` must be rebuilt after `stamp1`.
#[test]
fn dyndep_output_to_dyndep_input_in_any_order() {
    for targets in [&["stamp2", "stamp1"][..], &["stamp1", "stamp2"][..], &["all"][..]] {
        let mut t = BuildTest::empty();
        t.parse("rule touch\n  command = touch $out\nrule cp\n  command = cp $in $out\nbuild dd1: cp dd1-in\nbuild dd2: cp dd2-in\nbuild stamp2: touch || dd1 dd2\n  dyndep = dd2\nbuild stamp1: touch || dd1\n  dyndep = dd1\nbuild all: phony stamp2 stamp1\n");
        let dd1 = "ninja_dyndep_version = 1\nbuild stamp1 | out1: dyndep | source\n";
        let dd2 = "ninja_dyndep_version = 1\nbuild stamp2 | out2: dyndep | out1\n";
        t.fs.create("dd1-in", dd1);
        t.fs.create("dd2-in", dd2);
        t.fs.create("dd1", dd1);
        t.fs.create("dd2", dd2);
        t.fs.create("source", "");
        t.fs.tick();
        t.fs.create("out1", "");
        t.fs.create("stamp1", "");
        t.fs.tick();
        t.fs.create("out2", "");
        t.fs.create("stamp2", "");
        t.fs.tick();
        t.fs.create("source", "");
        t.fs.tick();
        t.build(targets).unwrap();
        assert_eq!(t.commands(), vec!["touch stamp1", "touch stamp2"], "targets {targets:?}");
    }
}

/// ninja 0b115d8: a dirty edge doesn't read its depfile into the graph.
#[test]
fn manifest_input_dirty_no_depfile_load() {
    let mut t = BuildTest::empty();
    t.parse(
        "rule deprule
  depfile = dep.d
  command = unused
build a b: deprule input
",
    );
    t.fs.create(
        "dep.d",
        "a: input_deps
",
    );
    t.fs.create("input", "");
    t.fs.create("input_deps", "");
    assert!(!t.add(&["a"]).unwrap());
    let a = t.state.lookup_node("a").unwrap();
    let edge = t.state.node(a).in_edge.unwrap();
    let inputs: Vec<&str> = t.state.edge(edge).inputs.iter().map(|&i| t.state.path(i)).collect();
    assert_eq!(inputs, vec!["input"]);
}

/// ninja issue 2666: a stale depfile doesn't make a dependency cycle.
#[test]
fn cycle_with_old_depfile() {
    let mut t = BuildTest::empty();
    t.parse(
        "rule cp
  command = cp $in $out
rule cp-deps
  command = cp-deps $in $out
build b: cp-deps a
  depfile = d.d
build c: cp b
build d: cp c
",
    );
    // The depfile this run generates, and last run's: d -> c -> b -> d.
    t.fs.create("a", "b: X");
    t.fs.create("d.d", "b: d");
    t.fs.create("X", "");
    t.build(&["d"]).unwrap();
    assert_eq!(t.commands(), vec!["cp-deps a b", "cp b c", "cp c d"]);
    assert_eq!(t.fs.read_file("d.d").unwrap().unwrap(), b"b: X");
}

/// The deps of an edge the scan found dirty still reach its command, as
/// hints (remote actions send them).
#[test]
fn dirty_edge_gets_discovered_deps_as_hints() {
    let mut t = BuildTest::empty();
    t.parse(
        "rule list-inputs
  command = list $in
  depfile = $out.d
build out: list-inputs in
",
    );
    t.fs.create("out", "");
    t.fs.create(
        "out.d",
        "out: h1.h sub/../h2.h
",
    );
    t.fs.tick();
    t.fs.create("in", "");
    t.build(&["out"]).unwrap();
    assert_eq!(t.fs.read_file("out").unwrap().unwrap(), b"in h1.h h2.h");
}

const RESTAT_STALE_DEPFILE: &str = "rule true
  command = true $in $out
  restat = 1
rule cp
  command = cp $in $out
  depfile = $out.d
build B: true A
";

/// ninja PR 2842: C is dirty only because of B, whose restat command
/// leaves it unchanged; C's depfile then decides, and its input X is newer.
#[test]
fn restat_pseudo_stale_depfile() {
    let mut t = BuildTest::empty();
    t.parse(RESTAT_STALE_DEPFILE);
    t.parse(
        "build C: cp B
",
    );
    t.fs.create("B", "");
    t.fs.create("C", "");
    t.fs.create(
        "C.d", "C: X
",
    );
    t.fs.tick();
    t.fs.create("A", "");
    t.fs.create("X", "");
    t.build(&["C"]).unwrap();
    assert_eq!(t.commands(), vec!["true A B", "cp B C"]);

    // B is restatted unchanged again, and C is up to date with X: C is
    // cleaned.
    t.clear();
    t.fs.tick();
    t.fs.create("A", "");
    t.fs.tick();
    t.build(&["C"]).unwrap();
    assert_eq!(t.commands(), vec!["true A B"]);
}

/// Like `restat_pseudo_stale_depfile`, with X built by the manifest: its
/// edge joins the plan before C.
#[test]
fn restat_pseudo_stale_depfile_generated() {
    let mut t = BuildTest::empty();
    t.parse(RESTAT_STALE_DEPFILE);
    t.parse(
        "build X: cat in_X
build C: cp B
",
    );
    t.fs.create("B", "");
    t.fs.create("C", "");
    t.fs.create(
        "C.d", "C: X
",
    );
    t.fs.tick();
    t.fs.create("A", "");
    t.fs.tick();
    t.fs.create("in_X", "");
    t.build(&["C"]).unwrap();
    assert_eq!(t.commands(), vec!["true A B", "cat in_X > X", "cp B C"]);
}

/// A cycle through C that only its depfile closes is found when the
/// depfile is read after the restat.
#[test]
fn restat_pseudo_stale_depfile_cycle() {
    let mut t = BuildTest::empty();
    t.parse(RESTAT_STALE_DEPFILE);
    t.parse(
        "build X: cat in_X
build C: cp B
build in_X: cat C
",
    );
    t.fs.create("B", "");
    t.fs.create("C", "");
    t.fs.create(
        "C.d", "C: X
",
    );
    t.fs.tick();
    t.fs.create("A", "");
    t.fs.tick();
    t.fs.create("in_X", "");
    let err = t.build(&["C"]).unwrap_err();
    assert!(err.starts_with("dependency cycle: "), "{err}");
}
