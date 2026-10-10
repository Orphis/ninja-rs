//! Build progress output, modeled on ninja's `StatusPrinter` and
//! `LinePrinter` (`[3/10] description` lines, `NINJA_STATUS`, smart terminal
//! line overwriting, `FAILED:` blocks).

use std::io::{IsTerminal, Write};
use std::time::Instant;

use crate::graph::EdgeId;
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
    fn edge_added_to_plan(&mut self);
    fn edge_removed_from_plan(&mut self);
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
        eprintln!("ninja explain: {msg}");
    }
}

/// Prints lines, overwriting the previous status line on smart terminals.
pub struct LinePrinter {
    smart_terminal: bool,
    supports_color: bool,
    have_blank_line: bool,
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

fn terminal_width() -> usize {
    if let Ok(c) = std::env::var("COLUMNS")
        && let Ok(n) = c.parse::<usize>()
        && n > 0
    {
        return n;
    }
    term_width_os().unwrap_or(80)
}

#[cfg(windows)]
fn term_width_os() -> Option<usize> {
    None
}

#[cfg(not(windows))]
fn term_width_os() -> Option<usize> {
    None
}

impl LinePrinter {
    pub fn new() -> Self {
        let tty = std::io::stdout().is_terminal();
        let term = std::env::var("TERM").unwrap_or_default();
        let smart = tty && term != "dumb";
        // As ninja: NO_COLOR turns colors off on a terminal; CLICOLOR_FORCE
        // turns them on elsewhere unless NO_COLOR is set; FORCE_COLOR always
        // does. "0" is the same as unset.
        let set = |name: &str| std::env::var(name).is_ok_and(|v| v != "0");
        let no_color = set("NO_COLOR");
        let supports_color = (!no_color && (smart || set("CLICOLOR_FORCE"))) || set("FORCE_COLOR");
        Self {
            smart_terminal: smart,
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
            let width = terminal_width().saturating_sub(1).max(10);
            let shown = elide_middle(line, width);
            let len = shown.chars().count();
            let _ = out.write_all(shown.as_bytes());
            if len < self.last_len {
                let _ = out.write_all(" ".repeat(self.last_len - len).as_bytes());
                let _ = out.write_all(b"\r");
                let _ = out.write_all(shown.as_bytes());
            }
            self.last_len = len;
            let _ = out.flush();
            self.have_blank_line = false;
        } else {
            if self.smart_terminal && self.last_len > 0 {
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

fn elide_middle(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    if width < 4 {
        return s.chars().take(width).collect();
    }
    let keep = width - 3;
    let left = keep / 2;
    let right = keep - left;
    let chars: Vec<char> = s.chars().collect();
    let mut out: String = chars[..left].iter().collect();
    out.push_str("...");
    out.extend(chars[n - right..].iter());
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
    time_millis: i64,
}

/// The variables of a `--status` format.
const STATUS_VARIABLES: &[&str] = &[
    "description",
    "started",
    "total",
    "running",
    "remaining",
    "finished",
    "rate",
    "current_rate",
    "progress",
    "predicted_progress",
    "elapsed",
    "elapsed_seconds",
    "eta",
    "eta_seconds",
    // ninja-rs: remote actions, remote and disk cache hits.
    "remote",
    "remote_cache_hits",
    "disk_cache_hits",
];

impl StatusPrinter {
    pub fn new(verbosity: Verbosity) -> Self {
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
        }
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
                && !STATUS_VARIABLES.contains(&name.as_str())
            {
                return Err(format!("unknown variable '{name}' in --status format"));
            }
        }
        self.status_eval = Some(eval);
        Ok(())
    }

    /// The value of a `--status` variable.
    fn status_variable(&self, name: &str, description: &str) -> String {
        let secs = self.time_millis as f64 / 1e3;
        let rate = if secs > 0.0 { self.finished_edges as f64 / secs } else { 0.0 };
        // Remaining time, from the average time per finished edge.
        let eta = (self.finished_edges > 0)
            .then(|| secs * self.total_edges.saturating_sub(self.finished_edges) as f64 / self.finished_edges as f64);
        let clock = |sec: f64| {
            let s = sec as i64;
            if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
        };
        match name {
            "description" => description.to_string(),
            "started" => self.started_edges.to_string(),
            "total" => self.total_edges.to_string(),
            "running" => self.running_edges.to_string(),
            "remaining" => self.total_edges.saturating_sub(self.started_edges).to_string(),
            "finished" => self.finished_edges.to_string(),
            "rate" | "current_rate" => {
                if self.finished_edges == 0 || secs == 0.0 {
                    "?".to_string()
                } else {
                    format!("{rate:.1}")
                }
            }
            "progress" | "predicted_progress" => {
                let pct = if self.total_edges > 0 { 100 * self.finished_edges / self.total_edges } else { 0 };
                format!("{pct:3}%")
            }
            "elapsed" => clock(secs),
            "elapsed_seconds" => format!("{secs:.3}"),
            "eta" => eta.map_or("?".to_string(), clock),
            "eta_seconds" => eta.map_or("?".to_string(), |s| format!("{s:.3}")),
            "remote" => self.remote_edges.to_string(),
            "remote_cache_hits" => self.cache_hits.to_string(),
            "disk_cache_hits" => self.disk_cache_hits.to_string(),
            _ => String::new(),
        }
    }

    pub fn remote_counts(&self) -> (usize, usize) {
        (self.remote_edges, self.cache_hits)
    }

    pub fn disk_cache_hits(&self) -> usize {
        self.disk_cache_hits
    }

    pub fn format_progress_status(&self, fmt: &str, time_millis: i64) -> String {
        let mut out = String::new();
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('s') => out.push_str(&self.started_edges.to_string()),
                Some('t') => out.push_str(&self.total_edges.to_string()),
                Some('r') => out.push_str(&self.running_edges.to_string()),
                Some('u') => out.push_str(&self.total_edges.saturating_sub(self.started_edges).to_string()),
                Some('f') => out.push_str(&self.finished_edges.to_string()),
                Some('o') => {
                    let rate = if time_millis > 0 { self.finished_edges as f64 / (time_millis as f64 / 1e3) } else { 0.0 };
                    out.push_str(&format!("{rate:.1}"));
                }
                Some('c') => out.push_str(&format!("{:.1}", 0.0)),
                Some('p') => {
                    let pct = if self.total_edges > 0 { 100 * self.finished_edges / self.total_edges } else { 0 };
                    out.push_str(&format!("{pct:3}%"));
                }
                Some('e') => out.push_str(&format!("{:.3}", time_millis as f64 / 1e3)),
                Some('w') => {
                    let s = time_millis / 1000;
                    out.push_str(&format!("{:02}:{:02}", s / 60, s % 60));
                }
                Some('E') | Some('W') => {
                    let s = if self.finished_edges > 0 {
                        (time_millis as f64 * (self.total_edges.saturating_sub(self.finished_edges)) as f64
                            / self.finished_edges as f64
                            / 1e3) as i64
                    } else {
                        0
                    };
                    out.push_str(&format!("{:02}:{:02}", s / 60, s % 60));
                }
                // ninja-rs extension: remote actions, remote and disk
                // cache hits.
                Some('R') => out.push_str(&self.remote_edges.to_string()),
                Some('H') => out.push_str(&self.cache_hits.to_string()),
                Some('D') => out.push_str(&self.disk_cache_hits.to_string()),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }

    fn print_status(&mut self, info: &EdgeInfo, time_millis: i64) {
        if matches!(self.verbosity, Verbosity::Quiet | Verbosity::NoStatusUpdate) {
            return;
        }
        let force_full_command = self.verbosity == Verbosity::Verbose;
        let mut to_print = info.description.clone();
        if to_print.is_empty() || force_full_command {
            to_print = info.command.clone();
        }
        let line = match &self.status_eval {
            // `--status`: the description only where the format says.
            Some(eval) => eval.evaluate(&mut |name| self.status_variable(name, &to_print)),
            None => format!("{}{}", self.format_progress_status(&self.progress_status_format.clone(), time_millis), to_print),
        };
        self.printer.print(&line, !force_full_command);
    }
}

impl Status for StatusPrinter {
    fn edge_added_to_plan(&mut self) {
        self.total_edges += 1;
    }
    fn edge_removed_from_plan(&mut self) {
        self.total_edges = self.total_edges.saturating_sub(1);
    }
    fn build_edge_rewound(&mut self, info: &EdgeInfo, lost: &[String]) {
        // Not started after all: it starts again.
        self.started_edges = self.started_edges.saturating_sub(1);
        self.running_edges = self.running_edges.saturating_sub(1);
        if info.use_console {
            self.printer.set_console_locked(false);
        }
        if self.verbosity != Verbosity::Quiet {
            let msg = format!("ninja: rewinding {}: {} gone from the remote cache, building again\n", info.outputs.join(" "), lost.join(" "));
            self.printer.print_on_new_line(msg.as_bytes());
        }
    }
    fn build_edge_started(&mut self, info: &EdgeInfo, start_ms: i64) {
        self.started_edges += 1;
        self.running_edges += 1;
        self.time_millis = start_ms;
        if info.use_console || self.printer.is_smart_terminal() {
            self.print_status(info, start_ms);
        }
        if info.use_console {
            self.printer.set_console_locked(true);
        }
    }
    fn build_edge_finished(&mut self, info: &EdgeInfo, end_ms: i64, exit_code: Option<i32>, kind: &ExecKind, output: &[u8]) {
        self.time_millis = end_ms;
        self.finished_edges += 1;
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
        }
        self.running_edges = self.running_edges.saturating_sub(1);
        if let Some(code) = exit_code {
            let mut outputs = String::new();
            for o in &info.outputs {
                outputs.push_str(o);
                outputs.push(' ');
            }
            let failed = if self.printer.supports_color() { "\x1b[31mFAILED: \x1b[0m" } else { "FAILED: " };
            let code_str = if code != 1 { format!("[code={code}] ") } else { String::new() };
            self.printer.print_on_new_line(format!("{failed}{code_str}{}\n", outputs.trim_end()).as_bytes());
            self.printer.print_on_new_line(format!("{}\n", info.command).as_bytes());
        }
        if !output.is_empty() {
            let out = if self.printer.supports_color() { output.to_vec() } else { strip_ansi_escape_codes(output) };
            #[cfg(windows)]
            let out = {
                // Normalize CRLF to LF; stdout is in text mode in ninja.
                let s = String::from_utf8_lossy(&out).replace("\r\n", "\n");
                s.into_bytes()
            };
            self.printer.print_on_new_line(&out);
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
        self.printer.print_on_new_line(b"");
    }
    fn info(&mut self, msg: &str) {
        self.printer.print_on_new_line(b"");
        println!("ninja: {msg}");
    }
    fn warning(&mut self, msg: &str) {
        self.printer.print_on_new_line(b"");
        eprintln!("ninja: warning: {msg}");
    }
    fn error(&mut self, msg: &str) {
        self.printer.print_on_new_line(b"");
        eprintln!("ninja: error: {msg}");
    }
}

/// Discards everything (tests).
#[derive(Default)]
pub struct NullStatus {
    pub started: Vec<String>,
}

impl Status for NullStatus {
    fn edge_added_to_plan(&mut self) {}
    fn edge_removed_from_plan(&mut self) {}
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

    #[test]
    fn format_progress() {
        let mut s = StatusPrinter::new(Verbosity::Quiet);
        s.total_edges = 10;
        s.started_edges = 3;
        s.finished_edges = 2;
        s.running_edges = 1;
        assert_eq!(s.format_progress_status("[%f/%t] ", 0), "[2/10] ");
        assert_eq!(s.format_progress_status("%s %r %u %p %%", 0), "3 1 7  20% %");
    }

    #[test]
    fn strip_ansi() {
        assert_eq!(strip_ansi_escape_codes(b"\x1b[1maffixmgr.cxx:286:15: \x1b[0m"), b"affixmgr.cxx:286:15: ");
    }

    #[test]
    fn elide() {
        assert_eq!(elide_middle("0123456789", 10), "0123456789");
        assert_eq!(elide_middle("01234567890", 10), "012...7890");
    }
}
