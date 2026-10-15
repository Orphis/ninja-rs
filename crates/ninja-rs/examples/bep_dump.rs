//! Prints a build event file (`--build_event_binary_file`) one event per
//! line, checking that it's a valid stream: `bep_dump <file>`.

#![allow(clippy::disallowed_macros)] // It prints what it found.

use rbe_bes::proto::build_event_stream::build_event::Payload;
use rbe_bes::proto::build_event_stream::build_event_id::Id;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bep_dump <file>");
    let events = ninja_rs::bes::read_binary_file(std::path::Path::new(&path)).expect("reading the file");
    for e in &events {
        let id = match e.id.as_ref().and_then(|i| i.id.as_ref()) {
            Some(Id::Progress(_)) => "progress".to_string(),
            Some(Id::Configuration(c)) => format!("configuration {}", c.id),
            Some(Id::TargetConfigured(t)) => format!("configured {}", t.label),
            Some(Id::TargetCompleted(t)) => format!("completed {} [{}]", t.label, t.configuration.as_ref().map_or("", |c| c.id.as_str())),
            Some(Id::NamedSet(s)) => format!("file set {}", s.id),
            Some(Id::ActionCompleted(a)) => format!("action {} ({})", a.primary_output, a.label),
            Some(other) => format!("{other:?}").chars().take(60).collect(),
            None => "?".into(),
        };
        let detail = match &e.payload {
            Some(Payload::Progress(p)) if !p.stderr.is_empty() || !p.stdout.is_empty() => format!(": {}", p.stderr.lines().next().unwrap_or_default()),
            Some(Payload::Configuration(c)) => format!(": {}", c.mnemonic),
            Some(Payload::Completed(c)) => format!(": success={}", c.success),
            Some(Payload::Aborted(a)) => format!(": aborted ({})", a.description),
            Some(Payload::NamedSetOfFiles(f)) => {
                format!(": {:?}", f.files.iter().map(|f| format!("{}/{}", f.path_prefix.join("/"), f.name)).collect::<Vec<_>>())
            }
            Some(Payload::BuildMetrics(m)) => format!(": actions created {}", m.action_summary.as_ref().map_or(0, |a| a.actions_created)),
            _ => String::new(),
        };
        if !(id == "progress" && detail.is_empty()) {
            println!("{id}{detail}");
        }
    }
    match rbe_bes::events::validate(&events) {
        Ok(()) => println!("valid stream of {} events", events.len()),
        Err(e) => println!("INVALID: {e}"),
    }
}
