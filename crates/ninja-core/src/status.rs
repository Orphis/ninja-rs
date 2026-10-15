//! Build progress output, modeled on ninja's `StatusPrinter` and
//! `LinePrinter` (`[3/10] description` lines, `NINJA_STATUS`, smart terminal
//! line overwriting, `FAILED:` blocks).

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Write;
use std::time::Instant;

use crate::graph::EdgeId;
use crate::hash::HashMap;
use crate::runner::{CasOutput, ExecKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Verbosity {
    Quiet,
    NoStatusUpdate,
    #[default]
    Normal,
    Verbose,
}

/// What the status printer (and build event reporting) needs to know about
/// an edge.
#[derive(Clone, Debug, Default)]
pub struct EdgeInfo {
    pub edge: EdgeId,
    pub description: String,
    pub command: String,
    pub outputs: Vec<String>,
    pub use_console: bool,
    /// `BuildConfig::task_bindings`, as in `Task::bindings`.
    pub bindings: Vec<(String, String)>,
    /// When finished: outputs a remote execution left in the CAS.
    pub cas_outputs: Vec<CasOutput>,
}

impl EdgeInfo {
    pub fn binding(&self, key: &str) -> Option<&str> {
        self.bindings.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

pub trait Status {
    /// An edge with a command is added to the plan, or removed from it (a
    /// `restat` found its inputs unchanged). `prev_elapsed_ms` is how long
    /// its command took last time, from the build log.
    fn edge_added_to_plan(&mut self, edge: EdgeId, prev_elapsed_ms: Option<i64>);
    fn edge_removed_from_plan(&mut self, edge: EdgeId);
    fn build_edge_started(&mut self, info: &EdgeInfo, start_ms: i64);
    fn build_edge_finished(&mut self, info: &EdgeInfo, end_ms: i64, exit_code: Option<i32>, kind: &ExecKind, output: &[u8]);
    /// A started edge goes back to waiting: inputs it needed were lost from
    /// the remote store, and their edges run again first.
    fn build_edge_rewound(&mut self, _info: &EdgeInfo, _lost: &[String]) {}
    fn build_started(&mut self);
    fn build_finished(&mut self);
    fn info(&mut self, msg: &str);
    fn warning(&mut self, msg: &str);
    fn error(&mut self, msg: &str);
    fn explain(&mut self, msg: &str) {
        print_err(&format!("ninja explain: {msg}\n"));
    }
    /// The build stopped on failed commands: before saying so, name them
    /// again if other output followed them.
    fn failure_summary(&mut self) {}
}

/// Messages from work the build loop doesn't see (the remote backend,
/// include scanners), with one prefix: while a status printer is there,
/// it prints them on their own lines at its next update, never over its
/// status line; else they go to standard error at once.
pub fn message(msg: &str) {
    queue(&MESSAGES, format!("ninja-rs: {msg}\n"));
}

/// `message`'s lines: the status printers there are, and the lines
/// waiting for one.
type Messages = std::sync::Mutex<(usize, Vec<String>)>;

static MESSAGES: Messages = std::sync::Mutex::new((0, Vec::new()));

fn queue(messages: &Messages, line: String) {
    let mut m = messages.lock().unwrap_or_else(|e| e.into_inner());
    if m.0 == 0 {
        print_err(&line);
    } else {
        m.1.push(line);
    }
}

/// The lines waiting, taken; `printers` added to the printers there are.
fn take_messages(messages: &Messages, printers: isize) -> String {
    let mut m = messages.lock().unwrap_or_else(|e| e.into_inner());
    m.0 = m.0.saturating_add_signed(printers);
    std::mem::take(&mut m.1).concat()
}

/// Writes `text` to stdout, as ninja's `printf` does: an error (a pipe
/// closed by its reader, a terminal gone) loses the text, where `print!`
/// would panic and end the build without its cleanup.
pub fn print_out(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// [`print_out`] for stderr.
pub fn print_err(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// A write to a pipe its reader closed ends the process, as it ends ninja's
/// tools (`-t targets all | head`; a shell says 141): Rust ignores SIGPIPE,
/// which makes such writes fail instead. Not for builds, which go on
/// without their output (see `print_out`). On Windows, where there is no
/// SIGPIPE, the writes fail and are ignored, as ninja's.
pub fn die_on_broken_pipe() {
    // SAFETY: plain system call, restoring the default action.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Prints lines, overwriting the previous status line on smart terminals.
pub struct LinePrinter {
    smart_terminal: bool,
    /// The terminal takes escape sequences: always on POSIX; on Windows,
    /// once its console processes them.
    escapes: bool,
    supports_color: bool,
    have_blank_line: bool,
    /// Without escape sequences: the length of the status line shown, which
    /// the next one blanks out.
    last_len: usize,
    console_locked: bool,
    line_buffer: String,
    line_type_elide: bool,
    output_buffer: Vec<u8>,
}

impl Default for LinePrinter {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether to overwrite status lines on stdout, as ninja decides: on POSIX,
/// a terminal with `TERM` set and not "dumb"; on Windows, a console, unless
/// `TERM` is "dumb".
fn smart_terminal(term: Option<&OsStr>, terminal: bool) -> bool {
    let dumb = term == Some(OsStr::new("dumb"));
    terminal && !dumb && (cfg!(windows) || term.is_some())
}

/// The terminal on stdout, as ninja's `LinePrinter` asks about it.
#[cfg(unix)]
mod terminal {
    use std::io::IsTerminal;

    pub fn is_terminal() -> bool {
        std::io::stdout().is_terminal()
    }

    /// Its width, when it gives one.
    pub fn width() -> Option<usize> {
        // SAFETY: TIOCGWINSZ fills a `winsize`, nothing else.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            (libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0).then_some(usize::from(size.ws_col))
        }
    }
}

/// The console on stdout, as ninja's `LinePrinter` asks about it.
#[cfg(windows)]
mod terminal {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    #[derive(Default)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ScreenBufferInfo {
        size: Coord,
        cursor_position: Coord,
        attributes: u16,
        window: [i16; 4],
        maximum_window_size: Coord,
    }

    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleScreenBufferInfo(console: *mut c_void, info: *mut ScreenBufferInfo) -> i32;
        fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
    }

    pub fn is_terminal() -> bool {
        width().is_some()
    }

    /// The width of its screen buffer, as ninja takes it.
    pub fn width() -> Option<usize> {
        let mut info = ScreenBufferInfo::default();
        // SAFETY: plain system call, filling `info`.
        let ok = unsafe { GetConsoleScreenBufferInfo(std::io::stdout().as_raw_handle(), &mut info) } != 0;
        (ok && info.size.x > 0).then_some(info.size.x as usize)
    }

    /// Turns on escape sequences, as ninja does (and leaves them on). False
    /// if the console can't process them (before Windows 10, or in its
    /// legacy mode).
    pub fn enable_escapes() -> bool {
        let console = std::io::stdout().as_raw_handle();
        let mut mode = 0;
        // SAFETY: plain system calls.
        unsafe { GetConsoleMode(console, &mut mode) == 0 || SetConsoleMode(console, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0 }
    }
}

impl LinePrinter {
    pub fn new() -> Self {
        let smart = smart_terminal(std::env::var_os("TERM").as_deref(), terminal::is_terminal());
        // As ninja: NO_COLOR turns colors off on a terminal; CLICOLOR_FORCE
        // turns them on elsewhere unless NO_COLOR is set; FORCE_COLOR always
        // does. "0" is the same as unset.
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| v != "0");
        let no_color = set("NO_COLOR");
        let mut supports_color = smart && !no_color;
        // As ninja, colors turn on a Windows console's escape sequences, and
        // stay off when it can't take them.
        #[cfg(windows)]
        let escapes = supports_color && terminal::enable_escapes();
        #[cfg(not(windows))]
        let escapes = true;
        supports_color &= escapes;
        supports_color |= (!no_color && set("CLICOLOR_FORCE")) || set("FORCE_COLOR");
        Self {
            smart_terminal: smart,
            escapes,
            supports_color,
            have_blank_line: true,
            last_len: 0,
            console_locked: false,
            line_buffer: String::new(),
            line_type_elide: false,
            output_buffer: Vec::new(),
        }
    }

    pub fn is_smart_terminal(&self) -> bool {
        self.smart_terminal
    }
    pub fn set_smart_terminal(&mut self, v: bool) {
        self.smart_terminal = v;
    }
    pub fn supports_color(&self) -> bool {
        self.supports_color
    }

    /// Prints a status line; `elide` lines are overwritten by the next one.
    pub fn print(&mut self, line: &str, elide: bool) {
        if self.console_locked {
            self.line_buffer = line.to_string();
            self.line_type_elide = elide;
            return;
        }
        let mut out = std::io::stdout().lock();
        if self.smart_terminal {
            let _ = out.write_all(b"\r");
        }
        if self.smart_terminal && elide {
            if self.escapes {
                // As ninja: no wider than the terminal, so that it doesn't
                // wrap, then the rest of the line erased. (A line filling
                // the last column leaves the cursor on it, and loses it to
                // the erase, with ninja too.)
                let shown = terminal::width().map_or_else(|| line.to_string(), |w| elide_middle(line, w));
                let _ = out.write_all(shown.as_bytes());
                let _ = out.write_all(b"\x1b[K");
            } else {
                // A console without escape sequences (ninja writes into its
                // buffer instead): spaces blank out the rest of the previous
                // line. One column short, as filling the last one moves the
                // cursor to the next line.
                let shown = terminal::width().map_or_else(|| line.to_string(), |w| elide_middle(line, w - 1));
                let len = shown.chars().count();
                let _ = out.write_all(shown.as_bytes());
                if len < self.last_len {
                    let _ = out.write_all(" ".repeat(self.last_len - len).as_bytes());
                    let _ = out.write_all(b"\r");
                    let _ = out.write_all(shown.as_bytes());
                }
                self.last_len = len;
            }
            let _ = out.flush();
            self.have_blank_line = false;
        } else {
            if self.smart_terminal && !self.escapes && self.last_len > 0 {
                let _ = out.write_all(" ".repeat(self.last_len).as_bytes());
                let _ = out.write_all(b"\r");
            }
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            self.last_len = 0;
            self.have_blank_line = true;
        }
    }

    /// Prints arbitrary output on its own line(s).
    pub fn print_on_new_line(&mut self, data: &[u8]) {
        if self.console_locked && !self.line_buffer.is_empty() {
            self.output_buffer.extend_from_slice(self.line_buffer.as_bytes());
            self.output_buffer.push(b'\n');
            self.line_buffer.clear();
        }
        if !self.have_blank_line {
            self.print_or_buffer(b"\n");
        }
        if !data.is_empty() {
            self.print_or_buffer(data);
        }
        self.last_len = 0;
        self.have_blank_line = data.is_empty() || data.ends_with(b"\n");
    }

    fn print_or_buffer(&mut self, data: &[u8]) {
        if self.console_locked {
            self.output_buffer.extend_from_slice(data);
        } else {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(data);
            let _ = out.flush();
        }
    }

    pub fn set_console_locked(&mut self, locked: bool) {
        if locked == self.console_locked {
            return;
        }
        if locked {
            self.print_on_new_line(b"");
        }
        self.console_locked = locked;
        if !locked {
            let buf = std::mem::take(&mut self.output_buffer);
            self.print_on_new_line(&buf);
            if !self.line_buffer.is_empty() {
                let l = std::mem::take(&mut self.line_buffer);
                let elide = self.line_type_elide;
                self.print(&l, elide);
            }
        }
    }
}

/// The color sequences (`ESC [`, digits and `;`, `m`) in `chars`, as ninja's
/// `AnsiColorSequenceIterator` finds them: their start and end indices.
fn color_sequences(chars: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(seq) = chars[from..].iter().position(|&c| c == '\x1b').map(|i| from + i) {
        // The shortest one is `ESC [ m`, and something follows it.
        if seq + 4 > chars.len() {
            break;
        }
        if chars[seq + 1] != '[' {
            from = seq + 1;
            continue;
        }
        let mut end = seq + 2;
        while chars[end].is_ascii_digit() || chars[end] == ';' {
            end += 1;
            if end == chars.len() {
                return found;
            }
        }
        if chars[end] != 'm' {
            from = seq + 3;
            continue;
        }
        found.push((seq, end + 1));
        from = end + 1;
    }
    found
}

/// Shortens `s` to `width` visible characters, its middle replaced with
/// "...", as ninja's `ElideMiddleInPlace`: color sequences take no room and
/// are all kept. ninja counts bytes; characters are the same for ASCII, and
/// UTF-8 isn't cut inside one.
fn elide_middle(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    let colors = color_sequences(&chars);
    let visible = chars.len() - colors.iter().map(|(start, end)| end - start).sum::<usize>();
    if visible <= width {
        return s.to_string();
    }
    let is_visible = |i: usize| !colors.iter().any(|&(start, end)| (start..end).contains(&i));
    let ellipsis = width.min(3);
    let left = (width - ellipsis) / 2;
    let gap_end = visible - (width - ellipsis - left);
    // The left span with its color sequences, the ellipsis, the color
    // sequences of the gap (so that the right span keeps its colors), the
    // right span.
    let (mut i, mut pos) = (0, 0);
    while i < chars.len() && pos != left {
        pos += usize::from(is_visible(i));
        i += 1;
    }
    let mut out: String = chars[..i].iter().collect();
    out.push_str(&"..."[..ellipsis]);
    while i < chars.len() && pos != gap_end {
        if is_visible(i) {
            pos += 1;
        } else {
            out.push(chars[i]);
        }
        i += 1;
    }
    out.extend(&chars[i..]);
    out
}

/// Removes ANSI escape sequences (for non-terminal output).
pub fn strip_ansi_escape_codes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != 0x1b {
            out.push(input[i]);
            i += 1;
            continue;
        }
        // Only strip CSIs: ESC [ ... final byte in @-~.
        if i + 1 < input.len() && input[i + 1] == b'[' {
            i += 2;
            while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                i += 1;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

pub struct StatusPrinter {
    pub printer: LinePrinter,
    verbosity: Verbosity,
    started_edges: usize,
    finished_edges: usize,
    total_edges: usize,
    running_edges: usize,
    remote_edges: usize,
    cache_hits: usize,
    disk_cache_hits: usize,
    progress_status_format: String,
    /// `--status FMT`, parsed: replaces `NINJA_STATUS` and the description.
    status_eval: Option<crate::eval::EvalString>,
    start: Instant,
    /// The time the last edge started or finished at.
    time_millis: i64,
    /// The time the finished commands took, added up.
    cpu_time_millis: i64,
    /// When the running edges started.
    started_at: HashMap<EdgeId, i64>,
    eta: Eta,
    current_rate: SlidingRate,
    /// The outputs of the commands that failed, for `failure_summary`.
    failures: Vec<String>,
    /// Something was printed after the last failure.
    printed_since_failure: bool,
}

/// What ninja predicts the build's time from: the times the build log gives
/// for the planned edges' commands, and the time the finished ones took.
#[derive(Default)]
struct Eta {
    /// The previous times of the planned edges that have one.
    prev_elapsed: HashMap<EdgeId, i64>,
    /// The edges with a previous time, and those times added up.
    predictable_edges_total: i64,
    predictable_cpu_time_total_millis: i64,
    /// Of the edges not finished: those with a previous time, their times,
    /// and those without one.
    predictable_edges_remaining: i64,
    predictable_cpu_time_remaining_millis: i64,
    unpredictable_edges_remaining: i64,
    /// The share of the predicted total time spent so far.
    time_predicted_percentage: f64,
}

impl Eta {
    fn added(&mut self, edge: EdgeId, prev_elapsed_ms: Option<i64>) {
        match prev_elapsed_ms {
            Some(ms) => {
                self.prev_elapsed.insert(edge, ms);
                self.predictable_edges_total += 1;
                self.predictable_cpu_time_total_millis += ms;
                self.predictable_edges_remaining += 1;
                self.predictable_cpu_time_remaining_millis += ms;
            }
            None => self.unpredictable_edges_remaining += 1,
        }
    }

    fn removed(&mut self, edge: EdgeId) {
        match self.prev_elapsed.remove(&edge) {
            Some(ms) => {
                self.predictable_edges_total -= 1;
                self.predictable_cpu_time_total_millis -= ms;
                self.predictable_edges_remaining -= 1;
                self.predictable_cpu_time_remaining_millis -= ms;
            }
            None => self.unpredictable_edges_remaining -= 1,
        }
    }

    fn finished(&mut self, edge: EdgeId) {
        match self.prev_elapsed.get(&edge) {
            Some(&ms) => {
                self.predictable_edges_remaining -= 1;
                self.predictable_cpu_time_remaining_millis -= ms;
            }
            None => self.unpredictable_edges_remaining -= 1,
        }
    }
}

/// `%c`, the rate of the last commands, as ninja's `SlidingRateInfo`: over
/// as many as run at once.
struct SlidingRate {
    rate: Option<f64>,
    size: usize,
    times: VecDeque<i64>,
    last_update: Option<usize>,
}

impl SlidingRate {
    fn new(size: usize) -> Self {
        Self { rate: None, size: size.max(1), times: VecDeque::new(), last_update: None }
    }

    /// Takes the time of the `finished`th command, once.
    fn update(&mut self, finished: usize, time_millis: i64) {
        if self.last_update == Some(finished) {
            return;
        }
        self.last_update = Some(finished);
        if self.times.len() == self.size {
            self.times.pop_front();
        }
        self.times.push_back(time_millis);
        if let (Some(&front), Some(&back)) = (self.times.front(), self.times.back())
            && back != front
        {
            self.rate = Some(self.times.len() as f64 / ((back - front) as f64 / 1e3));
        }
    }
}

/// How many failed commands `failure_summary` names.
const SUMMARY_MAX_FAILURES: usize = 10;

/// The `NINJA_STATUS` placeholders.
const PLACEHOLDERS: &str = "%strufocpPeEwWRHD";

/// The variables of a `--status` format but `description`, and the
/// `NINJA_STATUS` placeholders they are.
const STATUS_VARIABLES: &[(&str, char)] = &[
    ("started", 's'),
    ("total", 't'),
    ("running", 'r'),
    ("remaining", 'u'),
    ("finished", 'f'),
    ("rate", 'o'),
    ("current_rate", 'c'),
    ("progress", 'p'),
    ("predicted_progress", 'P'),
    ("elapsed", 'w'),
    ("elapsed_seconds", 'e'),
    ("eta", 'W'),
    ("eta_seconds", 'E'),
    // ninja-rs: remote actions, remote and disk cache hits.
    ("remote", 'R'),
    ("remote_cache_hits", 'H'),
    ("disk_cache_hits", 'D'),
];

/// `[h:]mm:ss`.
fn clock(secs: i64, with_hours: bool) -> String {
    if with_hours { format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60) } else { format!("{:02}:{:02}", secs / 60, secs % 60) }
}

impl Drop for StatusPrinter {
    fn drop(&mut self) {
        eprint!("{}", take_messages(&MESSAGES, -1));
    }
}

impl StatusPrinter {
    /// `parallelism`: how many commands run at once (`-j`), which `%c`
    /// averages over.
    pub fn new(verbosity: Verbosity, parallelism: usize) -> Self {
        take_messages(&MESSAGES, 1);
        let mut printer = LinePrinter::new();
        // Don't do anything fancy in verbose mode.
        if verbosity != Verbosity::Normal {
            printer.set_smart_terminal(false);
        }
        let fmt = std::env::var("NINJA_STATUS").unwrap_or_else(|_| "[%f/%t] ".to_string());
        Self {
            printer,
            verbosity,
            started_edges: 0,
            finished_edges: 0,
            total_edges: 0,
            running_edges: 0,
            remote_edges: 0,
            cache_hits: 0,
            disk_cache_hits: 0,
            progress_status_format: fmt,
            status_eval: None,
            start: Instant::now(),
            time_millis: 0,
            cpu_time_millis: 0,
            started_at: HashMap::default(),
            eta: Eta::default(),
            current_rate: SlidingRate::new(parallelism),
            failures: Vec::new(),
            printed_since_failure: false,
        }
    }

    /// Prints `message`'s lines waiting, past the status line.
    fn print_messages(&mut self) {
        let lines = take_messages(&MESSAGES, 0);
        if !lines.is_empty() {
            self.printer.print_on_new_line(b"");
            eprint!("{lines}");
            self.printed_since_failure = true;
        }
    }

    /// Prints a failed command's `FAILED:` block: outputs, command line, and
    /// its output.
    fn print_failure(&mut self, code: i32, outputs: &str, command: &str, output: &[u8]) {
        let failed = if self.printer.supports_color() { "\x1b[31mFAILED: \x1b[0m" } else { "FAILED: " };
        let code_str = if code != 1 { format!("[code={code}] ") } else { String::new() };
        self.printer.print_on_new_line(format!("{failed}{code_str}{outputs}\n").as_bytes());
        self.printer.print_on_new_line(format!("{command}\n").as_bytes());
        if output.is_empty() {
            return;
        }
        let out = if self.printer.supports_color() { output.to_vec() } else { strip_ansi_escape_codes(output) };
        // Normalize CRLF to LF; stdout is in text mode in ninja.
        #[cfg(windows)]
        let out = String::from_utf8_lossy(&out).replace("\r\n", "\n").into_bytes();
        self.printer.print_on_new_line(&out);
    }

    /// `--status FMT`: the status line is FMT with ninja-style variables
    /// (`$finished`, `${total}`, `$description`...) expanded, instead of
    /// `NINJA_STATUS` followed by the description.
    pub fn set_status_format(&mut self, fmt: &str) -> Result<(), String> {
        // The lexer reads a value up to the end of its line.
        let input = format!("{fmt}\n");
        let mut lexer = crate::lexer::Lexer::new("--status", input.as_bytes());
        let mut eval = crate::eval::EvalString::new();
        lexer.read_var_value(&mut eval).map_err(|e| format!("invalid --status: {e}"))?;
        for part in eval.parts() {
            if let crate::eval::EvalPart::Var(name) = part
                && name != "description"
                && !STATUS_VARIABLES.iter().any(|(n, _)| n == name)
            {
                return Err(format!("unknown variable '{name}' in --status format"));
            }
        }
        self.status_eval = Some(eval);
        Ok(())
    }

    /// Fails on an unknown `NINJA_STATUS` placeholder when status lines show
    /// it, as ninja does (at its first status line).
    pub fn check_progress_status_format(&self) -> Result<(), String> {
        if self.status_eval.is_some() || matches!(self.verbosity, Verbosity::Quiet | Verbosity::NoStatusUpdate) {
            return Ok(());
        }
        let mut chars = self.progress_status_format.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                match chars.next() {
                    Some(p) if PLACEHOLDERS.contains(p) => {}
                    p => return Err(format!("unknown placeholder '%{}' in $NINJA_STATUS", p.map_or_else(String::new, String::from))),
                }
            }
        }
        Ok(())
    }

    /// The value of a `--status` variable: its placeholder's.
    fn status_variable(&mut self, name: &str, description: &str, time_millis: i64) -> String {
        if name == "description" {
            return description.to_string();
        }
        STATUS_VARIABLES.iter().find(|(n, _)| *n == name).and_then(|&(_, p)| self.placeholder(p, time_millis)).unwrap_or_default()
    }

    pub fn remote_counts(&self) -> (usize, usize) {
        (self.remote_edges, self.cache_hits)
    }

    pub fn disk_cache_hits(&self) -> usize {
        self.disk_cache_hits
    }

    /// As ninja's `RecalculateProgressPrediction`: the share of the
    /// predicted total time spent so far. Edges without a previous time are
    /// predicted to take the average time of those with one, and previous
    /// times are left out when they look off by 10 times.
    fn recalculate_progress_prediction(&mut self) {
        let eta = &mut self.eta;
        eta.time_predicted_percentage = 0.0;
        let (total, finished, cpu) = (self.total_edges as i64, self.finished_edges as i64, self.cpu_time_millis);
        let mut use_previous_times = eta.predictable_edges_remaining != 0 && eta.predictable_cpu_time_remaining_millis != 0;
        // Once enough of the build ran to tell (15 s, 5% of the edges).
        if use_previous_times && total != 0 && finished != 0 && self.time_millis >= 15_000 && finished as f64 / total as f64 >= 0.05 {
            let actual = cpu as f64 / finished as f64;
            let previous = eta.predictable_cpu_time_total_millis as f64 / eta.predictable_edges_total as f64;
            use_previous_times = previous.max(actual) / previous.min(actual) < 10.0;
        }
        let (mut known_edges, mut known_millis, mut unknown_edges) = (finished, cpu, total - finished);
        if use_previous_times {
            known_edges += eta.predictable_edges_remaining;
            known_millis += eta.predictable_cpu_time_remaining_millis;
            unknown_edges = eta.unpredictable_edges_remaining;
        }
        if known_edges == 0 {
            return;
        }
        let mut remaining_millis = known_millis as f64 / known_edges as f64 * unknown_edges as f64;
        if use_previous_times {
            remaining_millis += eta.predictable_cpu_time_remaining_millis as f64;
        }
        let total_millis = cpu as f64 + remaining_millis;
        if total_millis != 0.0 {
            eta.time_predicted_percentage = cpu as f64 / total_millis;
        }
    }

    /// The value of a `NINJA_STATUS` placeholder at `time_millis`, as ninja
    /// formats it; None for an unknown one.
    fn placeholder(&mut self, p: char, time_millis: i64) -> Option<String> {
        let secs = time_millis as f64 / 1e3;
        Some(match p {
            '%' => "%".to_string(),
            's' => self.started_edges.to_string(),
            't' => self.total_edges.to_string(),
            'r' => self.running_edges.to_string(),
            'u' => self.total_edges.saturating_sub(self.started_edges).to_string(),
            'f' => self.finished_edges.to_string(),
            // Before any time passed, ninja divides 0 by 0 and prints the C
            // library's NaN; its clock starts with the process, so it shows
            // 0.0 there.
            'o' => format!("{:.1}", if self.finished_edges == 0 { 0.0 } else { self.finished_edges as f64 / secs }),
            'c' => {
                self.current_rate.update(self.finished_edges, time_millis);
                self.current_rate.rate.map_or_else(|| "?".to_string(), |r| format!("{r:.1}"))
            }
            'p' => {
                let percent = (100 * self.finished_edges).checked_div(self.total_edges).unwrap_or(0);
                format!("{percent:3}%")
            }
            'P' => format!("{:3}%", (100.0 * self.eta.time_predicted_percentage) as i32),
            'e' | 'w' | 'E' | 'W' => {
                let predicted = self.eta.time_predicted_percentage;
                let eta_secs = if predicted != 0.0 { (time_millis as f64 / predicted - time_millis as f64) / 1e3 } else { -1.0 };
                // Both with hours if either needs them.
                let with_hours = secs >= 3600.0 || eta_secs >= 3600.0;
                let value = if matches!(p, 'e' | 'w') { secs } else { eta_secs };
                if value < 0.0 {
                    "?".to_string()
                } else if matches!(p, 'e' | 'E') {
                    format!("{value:.3}")
                } else {
                    clock(value as i64, with_hours)
                }
            }
            // ninja-rs extension: remote actions, remote and disk cache
            // hits.
            'R' => self.remote_edges.to_string(),
            'H' => self.cache_hits.to_string(),
            'D' => self.disk_cache_hits.to_string(),
            _ => return None,
        })
    }

    /// `fmt` (`NINJA_STATUS`) with its placeholders replaced, as ninja's
    /// `FormatProgressStatus`. An unknown one, fatal in ninja (see
    /// `check_progress_status_format`), stays as it is.
    pub fn format_progress_status(&mut self, fmt: &str, time_millis: i64) -> String {
        let mut out = String::new();
        let mut chars = fmt.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some(p) => match self.placeholder(p, time_millis) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push('%');
                        out.push(p);
                    }
                },
                None => out.push('%'),
            }
        }
        out
    }

    fn print_status(&mut self, info: &EdgeInfo, time_millis: i64) {
        if matches!(self.verbosity, Verbosity::Quiet | Verbosity::NoStatusUpdate) {
            return;
        }
        self.recalculate_progress_prediction();
        let force_full_command = self.verbosity == Verbosity::Verbose;
        let mut to_print = info.description.clone();
        if to_print.is_empty() || force_full_command {
            to_print = info.command.clone();
        }
        let line = match self.status_eval.take() {
            // `--status`: the description only where the format says.
            Some(eval) => {
                let line = eval.evaluate(&mut |name| self.status_variable(name, &to_print, time_millis));
                self.status_eval = Some(eval);
                line
            }
            None => {
                let fmt = std::mem::take(&mut self.progress_status_format);
                let progress = self.format_progress_status(&fmt, time_millis);
                self.progress_status_format = fmt;
                format!("{progress}{to_print}")
            }
        };
        self.printer.print(&line, !force_full_command);
    }

    /// What `failure_summary` prints, if anything: the failed commands'
    /// outputs, not their `FAILED:` blocks again, as tools reading the
    /// output (IDEs) would count those errors twice.
    fn summary(&self) -> Option<String> {
        let n = self.failures.len();
        if self.verbosity == Verbosity::Quiet || n == 0 || (n == 1 && !self.printed_since_failure) {
            return None;
        }
        let mut text = format!("ninja: {n} command{} failed:\n", if n == 1 { "" } else { "s" });
        for outputs in self.failures.iter().take(SUMMARY_MAX_FAILURES) {
            text.push_str(&format!("  {outputs}\n"));
        }
        if n > SUMMARY_MAX_FAILURES {
            text.push_str(&format!("  and {} more\n", n - SUMMARY_MAX_FAILURES));
        }
        Some(text)
    }
}

impl Status for StatusPrinter {
    fn edge_added_to_plan(&mut self, edge: EdgeId, prev_elapsed_ms: Option<i64>) {
        self.total_edges += 1;
        self.eta.added(edge, prev_elapsed_ms);
    }
    fn edge_removed_from_plan(&mut self, edge: EdgeId) {
        self.total_edges = self.total_edges.saturating_sub(1);
        self.eta.removed(edge);
    }
    fn build_edge_rewound(&mut self, info: &EdgeInfo, lost: &[String]) {
        self.print_messages();
        // Not started after all: it starts again.
        self.started_edges = self.started_edges.saturating_sub(1);
        self.running_edges = self.running_edges.saturating_sub(1);
        self.started_at.remove(&info.edge);
        if info.use_console {
            self.printer.set_console_locked(false);
        }
        if self.verbosity != Verbosity::Quiet {
            let msg = format!("ninja: rewinding {}: {} gone from the remote cache, building again\n", info.outputs.join(" "), lost.join(" "));
            self.printer.print_on_new_line(msg.as_bytes());
        }
    }
    fn build_edge_started(&mut self, info: &EdgeInfo, start_ms: i64) {
        self.print_messages();
        self.started_edges += 1;
        self.running_edges += 1;
        self.time_millis = start_ms;
        self.started_at.insert(info.edge, start_ms);
        if info.use_console || self.printer.is_smart_terminal() {
            self.print_status(info, start_ms);
        }
        if info.use_console {
            self.printer.set_console_locked(true);
        }
    }
    fn build_edge_finished(&mut self, info: &EdgeInfo, end_ms: i64, exit_code: Option<i32>, kind: &ExecKind, output: &[u8]) {
        self.print_messages();
        self.time_millis = end_ms;
        self.finished_edges += 1;
        self.cpu_time_millis += self.started_at.remove(&info.edge).map_or(0, |start| end_ms - start);
        self.eta.finished(info.edge);
        match kind {
            ExecKind::Remote => self.remote_edges += 1,
            ExecKind::RemoteCacheHit => {
                self.remote_edges += 1;
                self.cache_hits += 1
            }
            ExecKind::DiskCacheHit => self.disk_cache_hits += 1,
            _ => {}
        }
        if info.use_console {
            self.printer.set_console_locked(false);
        }
        if self.verbosity == Verbosity::Quiet {
            self.running_edges = self.running_edges.saturating_sub(1);
            return;
        }
        // As ninja: the finished edge's line still counts it as running.
        if !info.use_console {
            self.print_status(info, end_ms);
            // On a smart terminal, status lines replace each other.
            if !self.printer.is_smart_terminal() {
                self.printed_since_failure = true;
            }
        }
        self.running_edges = self.running_edges.saturating_sub(1);
        if let Some(code) = exit_code {
            let outputs = info.outputs.join(" ");
            self.print_failure(code, &outputs, &info.command, output);
            self.failures.push(outputs);
            self.printed_since_failure = false;
        } else if !output.is_empty() {
            let out = if self.printer.supports_color() { output.to_vec() } else { strip_ansi_escape_codes(output) };
            #[cfg(windows)]
            let out = {
                // Normalize CRLF to LF; stdout is in text mode in ninja.
                let s = String::from_utf8_lossy(&out).replace("\r\n", "\n");
                s.into_bytes()
            };
            self.printer.print_on_new_line(&out);
            self.printed_since_failure = true;
        }
    }
    fn build_started(&mut self) {
        self.started_edges = 0;
        self.finished_edges = 0;
        self.running_edges = 0;
        self.start = Instant::now();
    }
    fn build_finished(&mut self) {
        self.printer.set_console_locked(false);
        self.print_messages();
        self.printer.print_on_new_line(b"");
    }
    fn info(&mut self, msg: &str) {
        self.print_messages();
        self.printer.print_on_new_line(b"");
        print_out(&format!("ninja: {msg}\n"));
        self.printed_since_failure = true;
    }
    fn warning(&mut self, msg: &str) {
        self.print_messages();
        self.printer.print_on_new_line(b"");
        print_err(&format!("ninja: warning: {msg}\n"));
        self.printed_since_failure = true;
    }
    fn error(&mut self, msg: &str) {
        self.print_messages();
        self.printer.print_on_new_line(b"");
        print_err(&format!("ninja: error: {msg}\n"));
        self.printed_since_failure = true;
    }
    /// With many jobs, a failure scrolls away behind the output of the
    /// commands still running (ninja issue 2378): the failed commands are
    /// named again, unless the only one is still the last thing printed.
    fn failure_summary(&mut self) {
        if let Some(text) = self.summary() {
            self.printer.print_on_new_line(text.as_bytes());
        }
    }
}

/// Discards everything (tests).
#[derive(Default)]
pub struct NullStatus {
    pub started: Vec<String>,
}

impl Status for NullStatus {
    fn edge_added_to_plan(&mut self, _edge: EdgeId, _prev_elapsed_ms: Option<i64>) {}
    fn edge_removed_from_plan(&mut self, _edge: EdgeId) {}
    fn build_edge_started(&mut self, info: &EdgeInfo, _start_ms: i64) {
        self.started.push(info.command.clone());
    }
    fn build_edge_finished(&mut self, _: &EdgeInfo, _: i64, _: Option<i32>, _: &ExecKind, _: &[u8]) {}
    fn build_started(&mut self) {}
    fn build_finished(&mut self) {}
    fn info(&mut self, _: &str) {}
    fn warning(&mut self, _: &str) {}
    fn error(&mut self, _: &str) {}
    fn explain(&mut self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printer() -> StatusPrinter {
        StatusPrinter::new(Verbosity::Quiet, 2)
    }

    /// Runs edge `id` from `start` to `end`.
    fn run(s: &mut StatusPrinter, id: u32, start: i64, end: i64) {
        let info = EdgeInfo { edge: EdgeId(id), ..Default::default() };
        s.build_edge_started(&info, start);
        s.build_edge_finished(&info, end, None, &ExecKind::Local, b"");
    }

    /// `fmt` at the time of the last edge started or finished, after the
    /// prediction a status line makes.
    fn format(s: &mut StatusPrinter, fmt: &str) -> String {
        s.recalculate_progress_prediction();
        s.format_progress_status(fmt, s.time_millis)
    }

    #[test]
    fn format_progress() {
        let mut s = printer();
        s.total_edges = 10;
        s.started_edges = 3;
        s.finished_edges = 2;
        s.running_edges = 1;
        assert_eq!(s.format_progress_status("[%f/%t] ", 0), "[2/10] ");
        assert_eq!(s.format_progress_status("%s %r %u %p %%", 0), "3 1 7  20% %");
    }

    /// ninja's `StatusTest`s.
    #[test]
    fn format_progress_as_ninja() {
        let mut s = printer();
        s.build_started();
        assert_eq!(s.format_progress_status("[%%/e%e]", 0), "[%/e0.000]");
        assert_eq!(s.format_progress_status("[%%/e%w]", 0), "[%/e00:00]");
        assert_eq!(s.format_progress_status("[%%/s%s/t%t/r%r/u%u/f%f]", 0), "[%/s0/t0/r0/u0/f0]");
    }

    #[test]
    fn format_times() {
        let mut s = printer();
        // %E, %W and %P before anything finished: no prediction.
        s.edge_added_to_plan(EdgeId(0), None);
        assert_eq!(format(&mut s, "%e|%w|%E|%W|%P|%o|%c"), "0.000|00:00|?|?|  0%|0.0|?");
        // Elapsed time in seconds, and with hours past one.
        assert_eq!(s.format_progress_status("%e %w", 3_725_000), "3725.000 1:02:05");
        assert_eq!(s.format_progress_status("%e %w", 59_999), "59.999 00:59");
    }

    #[test]
    fn format_predictions() {
        // Without previous times: the edges left take as long as the finished
        // ones did.
        let mut s = printer();
        for id in 0..4 {
            s.edge_added_to_plan(EdgeId(id), None);
        }
        run(&mut s, 0, 0, 1000);
        assert_eq!(format(&mut s, "%P %E %W %o"), " 25% 3.000 00:03 1.0");
        // With the build log's times: 9.3 s left.
        let mut s = printer();
        s.edge_added_to_plan(EdgeId(0), Some(1000));
        for id in 1..4 {
            s.edge_added_to_plan(EdgeId(id), Some(3100));
        }
        run(&mut s, 0, 0, 1000);
        assert_eq!(format(&mut s, "%P %E %W"), "  9% 9.300 00:09");
        // An ETA past an hour shows hours in the elapsed time too.
        let mut s = printer();
        s.edge_added_to_plan(EdgeId(0), None);
        s.edge_added_to_plan(EdgeId(1), Some(7_200_500));
        run(&mut s, 0, 0, 1000);
        assert_eq!(format(&mut s, "%w %W %P"), "0:00:01 2:00:00   0%");
        // Previous times off by 10 times are left out after 15 s.
        let mut s = printer();
        for id in 0..10 {
            s.edge_added_to_plan(EdgeId(id), Some(100));
        }
        run(&mut s, 0, 0, 20_000);
        assert_eq!(format(&mut s, "%P"), " 10%");
        // A restat removing an edge from the plan takes its time out.
        let mut s = printer();
        s.edge_added_to_plan(EdgeId(0), None);
        s.edge_added_to_plan(EdgeId(1), Some(1_000_000));
        s.edge_removed_from_plan(EdgeId(1));
        run(&mut s, 0, 0, 1000);
        assert_eq!(format(&mut s, "%P %E"), "100% 0.000");
    }

    /// `%c`: the rate over the last `-j` commands.
    #[test]
    fn current_rate() {
        let mut s = printer();
        for id in 0..3 {
            s.edge_added_to_plan(EdgeId(id), None);
        }
        run(&mut s, 0, 0, 1000);
        assert_eq!(format(&mut s, "%c"), "?");
        run(&mut s, 1, 1000, 1500);
        assert_eq!(format(&mut s, "%c"), "4.0");
        // Asked again for the same commands: the same rate.
        assert_eq!(format(&mut s, "%c"), "4.0");
        run(&mut s, 2, 1500, 3500);
        assert_eq!(format(&mut s, "%c %o"), "1.0 0.9");
    }

    /// `--status` variables are the placeholders' values.
    #[test]
    fn status_variables() {
        let mut s = printer();
        s.edge_added_to_plan(EdgeId(0), None);
        s.edge_added_to_plan(EdgeId(1), None);
        run(&mut s, 0, 0, 2000);
        s.recalculate_progress_prediction();
        let vars = ["predicted_progress", "eta", "eta_seconds", "elapsed", "elapsed_seconds", "current_rate", "rate", "description"];
        let got: Vec<String> = vars.iter().map(|v| s.status_variable(v, "CC a.o", 2000)).collect();
        assert_eq!(got, [" 50%", "00:02", "2.000", "00:02", "2.000", "?", "0.5", "CC a.o"]);
        assert!(s.set_status_format("$predicted_progress $eta_seconds").is_ok());
        assert_eq!(s.set_status_format("$nope"), Err("unknown variable 'nope' in --status format".to_string()));
    }

    /// An unknown placeholder is fatal, as in ninja, where status lines show
    /// `NINJA_STATUS`.
    #[test]
    fn unknown_placeholder() {
        let mut s = StatusPrinter::new(Verbosity::Normal, 1);
        s.progress_status_format = format!("[{}]", PLACEHOLDERS.chars().map(|p| format!("%{p}")).collect::<String>());
        assert_eq!(s.check_progress_status_format(), Ok(()));
        for p in PLACEHOLDERS.chars() {
            assert!(s.placeholder(p, 0).is_some(), "%{p}");
        }
        s.progress_status_format = "[%f/%x] ".to_string();
        assert_eq!(s.check_progress_status_format(), Err("unknown placeholder '%x' in $NINJA_STATUS".to_string()));
        assert_eq!(s.format_progress_status("[%f/%x] ", 0), "[0/%x] ");
        s.progress_status_format = "[%f] %".to_string();
        assert_eq!(s.check_progress_status_format(), Err("unknown placeholder '%' in $NINJA_STATUS".to_string()));
        // Not shown: fine.
        s.verbosity = Verbosity::Quiet;
        assert_eq!(s.check_progress_status_format(), Ok(()));
        s.verbosity = Verbosity::Normal;
        assert!(s.set_status_format("$finished").is_ok());
        assert_eq!(s.check_progress_status_format(), Ok(()));
    }

    #[test]
    fn failure_summary_only_when_needed() {
        let failed = |s: &mut StatusPrinter, out: &str| {
            let info = EdgeInfo { outputs: vec![out.into()], command: format!("cc {out}"), ..Default::default() };
            s.build_edge_finished(&info, 0, Some(1), &ExecKind::Local, b"error\n");
        };
        let ok = |s: &mut StatusPrinter, output: &[u8]| {
            let info = EdgeInfo { outputs: vec!["ok.o".into()], command: "cc ok.o".into(), ..Default::default() };
            s.build_edge_finished(&info, 0, None, &ExecKind::Local, output);
        };
        // One failure, still the last thing printed: nothing to repeat.
        let mut s = StatusPrinter::new(Verbosity::Normal, 1);
        s.printer.set_smart_terminal(true);
        failed(&mut s, "a.o");
        ok(&mut s, b"");
        assert_eq!(s.summary(), None);
        // Output of another command followed it.
        ok(&mut s, b"warning\n");
        assert_eq!(s.summary().as_deref(), Some("ninja: 1 command failed:\n  a.o\n"));
        // Several failures are always named again, without their `FAILED:`
        // blocks, which tools would count twice.
        let mut s = StatusPrinter::new(Verbosity::Normal, 1);
        failed(&mut s, "a.o");
        failed(&mut s, "b.o");
        assert_eq!(s.summary().as_deref(), Some("ninja: 2 commands failed:\n  a.o\n  b.o\n"));
        s.failures.extend((0..10).map(|i| format!("{i}.o")));
        let summary = s.summary().unwrap_or_default();
        assert!(summary.starts_with("ninja: 12 commands failed:\n  a.o\n") && summary.ends_with("  7.o\n  and 2 more\n"), "{summary}");
    }

    #[test]
    fn strip_ansi() {
        assert_eq!(strip_ansi_escape_codes(b"\x1b[1maffixmgr.cxx:286:15: \x1b[0m"), b"affixmgr.cxx:286:15: ");
    }

    /// ninja's `ElideMiddle` tests.
    #[test]
    fn elide() {
        let input = "Nothing to elide in this short string.";
        assert_eq!(elide_middle(input, 80), input);
        assert_eq!(elide_middle(input, 38), input);
        let shorts: Vec<String> = (0..4).map(|w| elide_middle(input, w)).collect();
        assert_eq!(shorts, ["", ".", "..", "..."]);

        let input = "01234567890123456789";
        let cases = [(4, "...9"), (5, "0...9"), (9, "012...789"), (10, "012...6789"), (11, "0123...6789"), (19, "01234567...23456789"), (20, input)];
        for (width, want) in cases {
            assert_eq!(elide_middle(input, width), want, "{width}");
        }
    }

    #[test]
    fn elide_colors() {
        const MAGENTA: &str = "\x1b[0;35m";
        const NOTHING: &str = "\x1b[m";
        const RED: &str = "\x1b[1;31m";
        const RESET: &str = "\x1b[0m";
        let input = format!("012345{MAGENTA}67890123456789");
        assert_eq!(elide_middle(&input, 10), format!("012...{MAGENTA}6789"));
        assert_eq!(elide_middle(&input, 19), format!("012345{MAGENTA}67...23456789"));
        let input = format!("Nothing {NOTHING} string.");
        assert_eq!(elide_middle(&input, 18), input);
        assert_eq!(elide_middle(&format!("0{NOTHING}1234567890123456789"), 10), format!("0{NOTHING}12...6789"));

        let input = format!("abcd{RED}efg{RESET}hlkmnopqrstuvwxyz");
        let cases = [
            (0, format!("{RED}{RESET}")),
            (1, format!(".{RED}{RESET}")),
            (2, format!("..{RED}{RESET}")),
            (3, format!("...{RED}{RESET}")),
            (4, format!("...{RED}{RESET}z")),
            (5, format!("a...{RED}{RESET}z")),
            (6, format!("a...{RED}{RESET}yz")),
            (7, format!("ab...{RED}{RESET}yz")),
            (8, format!("ab...{RED}{RESET}xyz")),
            (9, format!("abc...{RED}{RESET}xyz")),
            (10, format!("abc...{RED}{RESET}wxyz")),
            (11, format!("abcd...{RED}{RESET}wxyz")),
            (12, format!("abcd...{RED}{RESET}vwxyz")),
            (15, format!("abcd{RED}ef...{RESET}uvwxyz")),
            (16, format!("abcd{RED}ef...{RESET}tuvwxyz")),
            (17, format!("abcd{RED}efg...{RESET}tuvwxyz")),
            (18, format!("abcd{RED}efg...{RESET}stuvwxyz")),
            (19, format!("abcd{RED}efg{RESET}h...stuvwxyz")),
        ];
        for (width, want) in cases {
            assert_eq!(elide_middle(&input, width), want, "{width}");
        }

        let input = format!("abcdef{RED}A{RESET}BC");
        let cases = [
            (4, format!("...{RED}{RESET}C")),
            (5, format!("a...{RED}{RESET}C")),
            (6, format!("a...{RED}{RESET}BC")),
            (7, format!("ab...{RED}{RESET}BC")),
            (8, format!("ab...{RED}A{RESET}BC")),
            (9, input.clone()),
        ];
        for (width, want) in cases {
            assert_eq!(elide_middle(&input, width), want, "{width}");
        }
        // Characters, not bytes.
        assert_eq!(elide_middle("ééééééééé", 7), "éé...éé");
    }

    /// As ninja: on POSIX, no status line overwriting without `TERM`.
    #[test]
    fn smart_terminal_needs() {
        let term = |t: Option<&str>, terminal: bool| smart_terminal(t.map(OsStr::new), terminal);
        assert!(term(Some("xterm-256color"), true));
        assert!(!term(Some("xterm-256color"), false));
        assert!(!term(Some("dumb"), true));
        assert_eq!(term(None, true), cfg!(windows));
    }

    /// Messages from outside the build loop wait for the status printer
    /// while there is one (printed past its status line), and are printed
    /// at once without one.
    #[test]
    fn messages_wait_for_the_printer() {
        let messages: Messages = std::sync::Mutex::new((0, Vec::new()));
        queue(&messages, "ninja-rs: before\n".into());
        assert!(messages.lock().unwrap().1.is_empty());
        take_messages(&messages, 1);
        queue(&messages, "ninja-rs: warning: one\n".into());
        queue(&messages, "ninja-rs: two\n".into());
        assert_eq!(take_messages(&messages, 0), "ninja-rs: warning: one\nninja-rs: two\n");
        assert_eq!(take_messages(&messages, 0), "");
        queue(&messages, "ninja-rs: last\n".into());
        assert_eq!(take_messages(&messages, -1), "ninja-rs: last\n");
        queue(&messages, "ninja-rs: after\n".into());
        assert_eq!(*messages.lock().unwrap(), (0, Vec::new()));
    }
}
