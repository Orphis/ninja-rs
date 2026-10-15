//! Builds a valid Build Event Protocol stream.
//!
//! BEP consumers rely on a few invariants, which [`EventStream`] maintains
//! (and [`validate`] checks):
//! - the first event is `BuildStarted`;
//! - every other event was announced as a child of an earlier event. Events
//!   nobody announced (configurations, named file sets, failed actions) are
//!   announced by a `Progress` event first, as Bazel does;
//! - no event is posted twice, and every announced event is eventually posted
//!   (`Aborted` stands in for those that never happened);
//! - the last event, and only it, has `last_message` set.

use std::collections::HashSet;

use prost::Message;

use crate::proto::build_event_stream as bep;
use bep::build_event::Payload;
use bep::build_event_id::{self as eid, Id};

/// Constructors for event ids.
pub mod id {
    use super::*;

    fn wrap(id: Id) -> bep::BuildEventId {
        bep::BuildEventId { id: Some(id) }
    }
    pub fn started() -> bep::BuildEventId {
        wrap(Id::Started(eid::BuildStartedId {}))
    }
    pub fn progress(n: i32) -> bep::BuildEventId {
        wrap(Id::Progress(eid::ProgressId { opaque_count: n }))
    }
    pub fn unstructured_command_line() -> bep::BuildEventId {
        wrap(Id::UnstructuredCommandLine(eid::UnstructuredCommandLineId {}))
    }
    pub fn structured_command_line(label: &str) -> bep::BuildEventId {
        wrap(Id::StructuredCommandLine(eid::StructuredCommandLineId { command_line_label: label.into() }))
    }
    pub fn options_parsed() -> bep::BuildEventId {
        wrap(Id::OptionsParsed(eid::OptionsParsedId {}))
    }
    pub fn workspace_status() -> bep::BuildEventId {
        wrap(Id::WorkspaceStatus(eid::WorkspaceStatusId {}))
    }
    pub fn build_metadata() -> bep::BuildEventId {
        wrap(Id::BuildMetadata(eid::BuildMetadataId {}))
    }
    pub fn workspace() -> bep::BuildEventId {
        wrap(Id::Workspace(eid::WorkspaceConfigId {}))
    }
    pub fn pattern(patterns: &[String]) -> bep::BuildEventId {
        wrap(Id::Pattern(eid::PatternExpandedId { pattern: patterns.to_vec() }))
    }
    pub fn configuration(config: &str) -> bep::BuildEventId {
        wrap(Id::Configuration(eid::ConfigurationId { id: config.into() }))
    }
    pub fn target_configured(label: &str) -> bep::BuildEventId {
        wrap(Id::TargetConfigured(eid::TargetConfiguredId { label: label.into(), aspect: String::new() }))
    }
    pub fn target_completed(label: &str, config: &str) -> bep::BuildEventId {
        wrap(Id::TargetCompleted(eid::TargetCompletedId {
            label: label.into(),
            configuration: Some(eid::ConfigurationId { id: config.into() }),
            aspect: String::new(),
        }))
    }
    pub fn named_set(set: &str) -> bep::BuildEventId {
        wrap(Id::NamedSet(eid::NamedSetOfFilesId { id: set.into() }))
    }
    pub fn action_completed(primary_output: &str, label: &str, config: &str) -> bep::BuildEventId {
        wrap(Id::ActionCompleted(eid::ActionCompletedId {
            primary_output: primary_output.into(),
            label: label.into(),
            configuration: Some(eid::ConfigurationId { id: config.into() }),
        }))
    }
    pub fn build_finished() -> bep::BuildEventId {
        wrap(Id::BuildFinished(eid::BuildFinishedId {}))
    }
    pub fn build_tool_logs() -> bep::BuildEventId {
        wrap(Id::BuildToolLogs(eid::BuildToolLogsId {}))
    }
    pub fn build_metrics() -> bep::BuildEventId {
        wrap(Id::BuildMetrics(eid::BuildMetricsId {}))
    }
}

fn key(id: &bep::BuildEventId) -> Vec<u8> {
    id.encode_to_vec()
}

#[derive(Default)]
pub struct EventStream {
    announced: HashSet<Vec<u8>>,
    /// Announced ids in order, to abort the ones never posted.
    announced_order: Vec<bep::BuildEventId>,
    posted: HashSet<Vec<u8>>,
    /// The announced, not yet posted, progress event.
    next_progress: i32,
    started: bool,
    finished: bool,
}

impl EventStream {
    pub fn new() -> Self {
        Self::default()
    }

    fn announce(&mut self, children: &[bep::BuildEventId]) {
        for c in children {
            if self.announced.insert(key(c)) {
                self.announced_order.push(c.clone());
            }
        }
    }

    fn event(&mut self, id: bep::BuildEventId, children: Vec<bep::BuildEventId>, payload: Payload) -> bep::BuildEvent {
        self.posted.insert(key(&id));
        self.announce(&children);
        bep::BuildEvent { id: Some(id), children, last_message: false, payload: Some(payload) }
    }

    /// The next link of the progress chain, announcing `extra` too.
    fn progress_event(&mut self, stdout: &str, stderr: &str, extra: Vec<bep::BuildEventId>) -> bep::BuildEvent {
        let n = self.next_progress;
        self.next_progress += 1;
        let mut children = vec![id::progress(n + 1)];
        children.extend(extra);
        self.event(id::progress(n), children, Payload::Progress(bep::Progress { stdout: stdout.into(), stderr: stderr.into() }))
    }

    /// The first event. The progress chain starts here.
    pub fn started(&mut self, payload: bep::BuildStarted, mut children: Vec<bep::BuildEventId>) -> Vec<bep::BuildEvent> {
        assert!(!self.started, "BuildStarted posted twice");
        self.started = true;
        children.insert(0, id::progress(0));
        vec![self.event(id::started(), children, Payload::Started(payload))]
    }

    pub fn is_posted(&self, id: &bep::BuildEventId) -> bool {
        self.posted.contains(&key(id))
    }

    /// Whether the stream ended (`finish`): nothing can be posted after.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Posts an event, announcing it through a progress event first if no
    /// earlier event did. Posting an id again is a no-op.
    pub fn post(&mut self, id: bep::BuildEventId, children: Vec<bep::BuildEventId>, payload: Payload) -> Vec<bep::BuildEvent> {
        assert!(self.started && !self.finished, "event outside the stream");
        if self.is_posted(&id) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.announced.contains(&key(&id)) {
            out.push(self.progress_event("", "", vec![id.clone()]));
        }
        out.push(self.event(id, children, payload));
        out
    }

    /// Console output.
    pub fn progress(&mut self, stdout: &str, stderr: &str) -> Vec<bep::BuildEvent> {
        assert!(self.started && !self.finished, "event outside the stream");
        vec![self.progress_event(stdout, stderr, Vec::new())]
    }

    /// Ends the stream with `tail` (typically build metrics and tool logs,
    /// announced by `BuildFinished`). The progress chain is closed, every
    /// announced event not posted becomes `Aborted` (`INCOMPLETE`), and the
    /// last event gets `last_message`.
    pub fn finish(&mut self, tail: Vec<(bep::BuildEventId, Payload)>) -> Vec<bep::BuildEvent> {
        assert!(self.started && !self.finished, "event outside the stream");
        let mut out = Vec::new();
        let unannounced: Vec<_> = tail.iter().map(|(i, _)| i.clone()).filter(|i| !self.announced.contains(&key(i))).collect();
        // Close the chain: the last progress event announces no successor.
        let n = self.next_progress;
        out.push(self.event(id::progress(n), unannounced, Payload::Progress(bep::Progress::default())));
        let tail_keys: HashSet<Vec<u8>> = tail.iter().map(|(i, _)| key(i)).collect();
        let missing: Vec<_> = self
            .announced_order
            .iter()
            .filter(|i| !self.posted.contains(&key(i)) && !tail_keys.contains(&key(i)))
            .cloned()
            .collect();
        for i in missing {
            out.push(self.event(
                i,
                Vec::new(),
                Payload::Aborted(bep::Aborted {
                    reason: bep::aborted::AbortReason::Incomplete as i32,
                    description: "not reported before the build ended".into(),
                }),
            ));
        }
        for (i, p) in tail {
            if !self.is_posted(&i) {
                out.push(self.event(i, Vec::new(), p));
            }
        }
        if let Some(last) = out.last_mut() {
            last.last_message = true;
        }
        self.finished = true;
        out
    }
}

/// Checks the stream invariants listed in the module docs.
pub fn validate(events: &[bep::BuildEvent]) -> Result<(), String> {
    let first = events.first().ok_or("empty stream")?;
    if !matches!(first.payload, Some(Payload::Started(_))) {
        return Err("the first event isn't BuildStarted".into());
    }
    let mut announced = HashSet::new();
    let mut posted = HashSet::new();
    for (i, e) in events.iter().enumerate() {
        let id = e.id.as_ref().ok_or_else(|| format!("event {i} has no id"))?;
        let k = key(id);
        if i > 0 && !announced.contains(&k) {
            return Err(format!("event {i} ({id:?}) wasn't announced"));
        }
        if !posted.insert(k) {
            return Err(format!("event {i} ({id:?}) posted twice"));
        }
        if e.last_message != (i + 1 == events.len()) {
            return Err(format!("last_message on event {i} of {}", events.len()));
        }
        for c in &e.children {
            announced.insert(key(c));
        }
    }
    if let Some(k) = announced.iter().find(|k| !posted.contains(*k)) {
        return Err(format!("announced but never posted: {:?}", bep::BuildEventId::decode(k.as_slice())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(s: &mut EventStream, children: Vec<bep::BuildEventId>) -> Vec<bep::BuildEvent> {
        s.started(bep::BuildStarted { uuid: "inv".into(), command: "build".into(), ..Default::default() }, children)
    }

    fn finished() -> Payload {
        Payload::Finished(bep::BuildFinished::default())
    }

    #[test]
    fn a_complete_stream_is_valid() {
        let mut s = EventStream::new();
        let mut out = started(&mut s, vec![id::options_parsed(), id::build_finished()]);
        out.extend(s.post(id::options_parsed(), vec![], Payload::OptionsParsed(Default::default())));
        out.extend(s.progress("", "Compiling foo\n"));
        out.extend(s.post(id::build_finished(), vec![id::build_metrics(), id::build_tool_logs()], finished()));
        out.extend(s.finish(vec![
            (id::build_metrics(), Payload::BuildMetrics(Default::default())),
            (id::build_tool_logs(), Payload::BuildToolLogs(Default::default())),
        ]));
        validate(&out).unwrap();
        assert!(matches!(out.last().unwrap().payload, Some(Payload::BuildToolLogs(_))));
    }

    #[test]
    fn unannounced_events_are_announced_by_progress() {
        let mut s = EventStream::new();
        let mut out = started(&mut s, vec![]);
        let conf = s.post(id::configuration("release-x"), vec![], Payload::Configuration(Default::default()));
        assert_eq!(conf.len(), 2);
        assert!(matches!(conf[0].payload, Some(Payload::Progress(_))));
        assert!(conf[0].children.contains(&id::configuration("release-x")));
        out.extend(conf);
        // Posting the same configuration again does nothing.
        assert!(s.post(id::configuration("release-x"), vec![], Payload::Configuration(Default::default())).is_empty());
        out.extend(s.finish(vec![]));
        validate(&out).unwrap();
    }

    #[test]
    fn events_never_posted_are_aborted() {
        let mut s = EventStream::new();
        let mut out = started(&mut s, vec![id::target_configured("//a:bin/a")]);
        out.extend(s.finish(vec![]));
        validate(&out).unwrap();
        let aborted: Vec<_> = out.iter().filter(|e| matches!(e.payload, Some(Payload::Aborted(_)))).collect();
        assert_eq!(aborted.len(), 1);
        assert_eq!(aborted[0].id, Some(id::target_configured("//a:bin/a")));
    }

    #[test]
    fn an_unannounced_tail_is_announced_by_the_closing_progress() {
        let mut s = EventStream::new();
        let mut out = started(&mut s, vec![]);
        out.extend(s.finish(vec![(id::build_finished(), finished())]));
        validate(&out).unwrap();
        assert!(out.last().unwrap().last_message);
    }

    #[test]
    fn validate_rejects_broken_streams() {
        let mut s = EventStream::new();
        let ev = started(&mut s, vec![id::build_finished()]);
        // Announced, never posted.
        let mut bad = ev.clone();
        bad[0].last_message = true;
        assert!(validate(&bad).unwrap_err().contains("never posted"));
        // Not announced.
        let mut bad = ev.clone();
        bad.push(bep::BuildEvent { id: Some(id::workspace()), last_message: true, ..Default::default() });
        assert!(validate(&bad).unwrap_err().contains("wasn't announced"));
    }
}
