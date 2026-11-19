//! The stand-in backend a Deep website build starts by itself: when the capture kept a recording of
//! the original's traffic, the run builds a stand-in from it and serves it before the builder's
//! first turn, so the builder's front end has the recorded answers to talk to from the start. It
//! is built and served through the same tools a builder would call, and stopped when the run ends.

use super::*;
use craft_anything_engine::tools::standin_tools::StandIns as Servers;

/// Where the run writes the stand-in it builds, inside `capture/`: what it holds came from the
/// original, and a new study replaces it along with the rest of the capture.
pub(super) const STANDIN_DIR: &str = "standin";

/// A stand-in this run serves.
#[derive(Clone)]
pub(super) struct RunningStandIn {
    pub id: String,
    pub url: String,
    /// The session's stand-ins, to stop this one with as the run ends, without the session (which
    /// another tool may hold for hours) or its mode (Clean room refuses the stand-in tools).
    /// `None` when it was already running before this run asked for it: it isn't this run's to stop.
    servers: Option<Arc<Servers>>,
}

impl std::fmt::Debug for RunningStandIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningStandIn").field("id", &self.id).field("url", &self.url).field("ours", &self.servers.is_some()).finish()
    }
}

/// The recording of the original's traffic the capture kept: the study's own file, or else the
/// first `.har` in `capture/` by name, so the choice doesn't depend on the order the folder lists.
pub(super) fn recording(ws: &Workspace) -> Option<PathBuf> {
    let capture = ws.capture_dir();
    let own = capture.join(study::RECORDING);
    if own.is_file() {
        return Some(own);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(&capture)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("har")))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// What the builder is told about a stand-in the run serves, in the brief and in each later
/// message (its address can change from one run to the next).
pub(super) fn running_words(url: &str) -> String {
    format!(
        "Craft Anything is already serving a stand-in backend built from the original's recorded traffic in capture/, so you don't need to start one: {url} answers your front end with what the original's backend answered (writes get the recorded answer and change nothing). While you build and check, point the front end's API calls there instead of the original's servers, through one development setting whose default stays the original's API: the stand-in runs only while Craft Anything builds, not when the app is opened later."
    )
}

impl Worker {
    /// Build a stand-in from the capture's recording and serve it, on the port the project's last
    /// one used when that's free. Only a Deep website build has one: Clean room reads nothing
    /// inside the original. A stand-in that can't be built or started is said in the feed and the
    /// build goes on without it; the builder can still make one with the tools.
    pub(super) fn start_standin(&self, ws: &mut Workspace, deep: bool) -> Option<RunningStandIn> {
        if !deep || !matches!(ws.manifest.target, TargetRef::Website { .. }) {
            return None;
        }
        let har = recording(ws)?;
        let saved = ws.manifest.standin_port.filter(|p| *p != 0);
        match self.serve_recording(ws, &har, saved) {
            Ok(running) => {
                let moved = saved
                    .filter(|p| port_of(&running.url) != Some(*p))
                    .map(|p| format!(" (port {p}, the one it used last time, is taken)"))
                    .unwrap_or_default();
                self.emit(RunEvent::Agent(AgentEvent::Status {
                    text: format!("A stand-in backend answers from the original's recording at {}{moved}.", running.url),
                }));
                Some(running)
            }
            Err(e) if e.code == ErrorCode::Cancelled => None,
            Err(e) => {
                self.emit(RunEvent::Agent(AgentEvent::Status {
                    text: format!("Couldn't start a stand-in backend from the original's recording ({}), so the builder isn't given one.", e.message),
                }));
                None
            }
        }
    }

    fn serve_recording(&self, ws: &mut Workspace, har: &Path, saved: Option<u16>) -> Result<RunningStandIn, ToolError> {
        let dir = ws.capture_dir().join(STANDIN_DIR);
        self.call("web_standin_build", json!({"path": har.display().to_string(), "output_dir": dir.display().to_string(), "limit": 1}))?;
        let dir = dir.display().to_string();
        let started = match saved {
            Some(port) => self.call("web_standin_start", json!({"path": dir, "port": port})).or_else(|e| {
                // Only a port that couldn't be listened on gives way to a free one; anything else
                // would fail the same way again.
                if e.code != ErrorCode::FileError {
                    return Err(e);
                }
                self.call("web_standin_start", json!({"path": dir}))
            })?,
            None => self.call("web_standin_start", json!({"path": dir}))?,
        };
        let (id, url, ours) = started_standin(&started).ok_or_else(|| ToolError::internal("web_standin_start didn't say where it listens"))?;
        // Read once the tool has made them; no other guard is held here.
        let servers = if ours { self.engine.session().ext.get::<Servers>() } else { None };
        if let Some(port) = port_of(&url)
            && ws.manifest.standin_port != Some(port)
        {
            // It's serving already: a port that couldn't be kept costs only a new one next time,
            // and an error here would leave it running with nothing to stop it.
            ws.manifest.standin_port = Some(port);
            if let Err(e) = ws.save() {
                log::warn!("Couldn't keep the stand-in backend's port in {}: {}", ws.root.display(), e.message);
            }
        }
        Ok(RunningStandIn { id, url, servers })
    }

    /// Stop the stand-in this run started. Called as the run ends, also when it was paused or
    /// failed: it waits on neither the session nor the stop that ended the run.
    pub(super) fn stop_standin(&self, running: &RunningStandIn) {
        if let Some(servers) = &running.servers
            && !servers.stop(&running.id)
        {
            log::info!("The build's stand-in backend {} had already stopped.", running.id);
        }
    }
}

/// The id and address `web_standin_start` answered with, and whether it started that server: one
/// already serving the same file is named in its description instead.
fn started_standin(out: &ToolOutput) -> Option<(String, String, bool)> {
    let ToolOutput::Value(v) = out else { return None };
    let server = v.get("server");
    let field = |name: &str| v.get(name).or_else(|| server.and_then(|s| s.get(name))).and_then(Value::as_str).map(str::to_string);
    let ours = !v.get("already_running").and_then(Value::as_bool).unwrap_or(false);
    Some((field("id")?, field("url")?, ours))
}

/// The port in a loopback URL such as `http://127.0.0.1:41234`.
fn port_of(url: &str) -> Option<u16> {
    url.trim_end_matches('/').rsplit_once(':').and_then(|(_, p)| p.parse().ok()).filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild::tests::check_worker;
    use std::io::{Read, Write};

    fn site(dir: &Path) -> Workspace {
        Workspace::create(dir, "Site", "Rebuild this", TargetRef::Website { url: "https://example.com".into() }).unwrap()
    }

    /// The study's recording of one API answer.
    fn record(ws: &Workspace) {
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        let requests = [json!({"method": "GET", "url": "https://api.example.test/v1/items/42", "status": 200, "mime_type": "application/json",
            "response_body": {"state": "captured", "bytes": 21, "text": "{\"title\":\"Recorded\"}"}})];
        craft_anything_engine::tools::browser_tools::write_recording(&requests, &ws.capture_dir().join(study::RECORDING)).unwrap();
    }

    /// What a front end on this computer gets for `path` from the server at `url`.
    fn get(url: &str, path: &str) -> std::io::Result<String> {
        let host = url.trim_start_matches("http://");
        let mut s = std::net::TcpStream::connect(host)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    }

    fn notes(w: &Worker) -> Vec<String> {
        w.feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .items()
            .iter()
            .flat_map(|i| match i {
                feed::FeedItem::Work { notes, .. } => notes.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn running(w: &Worker) -> Vec<Value> {
        let out = w.engine.call("web_standin_show", json!({}), CallOptions::new(Caller::Ui)).unwrap();
        let ToolOutput::Value(v) = out else { panic!("{out:?}") };
        v["servers"].as_array().cloned().unwrap_or_default()
    }

    #[test]
    fn a_deep_website_build_serves_the_recording_until_it_stops_it() {
        let d = tempfile::tempdir().unwrap();
        let mut ws = site(d.path());
        record(&ws);
        let w = check_worker(&ws, true);
        let s = w.start_standin(&mut ws, true).expect("a stand-in for the recording");
        assert!(s.url.starts_with("http://127.0.0.1:"), "{s:?}");
        let answer = get(&s.url, "/v1/items/42").unwrap();
        assert!(answer.starts_with("HTTP/1.1 200") && answer.contains("{\"title\":\"Recorded\"}"), "{answer}");
        assert!(ws.capture_dir().join(STANDIN_DIR).join(craft_anything_engine::tools::standin_tools::STANDIN_FILE).is_file());
        // The port is kept with the project, so the next build asks for it again.
        let port = port_of(&s.url);
        assert!(port.is_some());
        assert_eq!(Workspace::open(&ws.root).unwrap().manifest.standin_port, port);
        assert!(notes(&w).iter().any(|n| n.contains(&s.url)), "the feed says where it answers: {:?}", notes(&w));
        assert_eq!(running(&w).len(), 1);
        w.stop_standin(&s);
        assert!(running(&w).is_empty(), "stopped as the run ends");
    }

    /// Deep mode turned off while it built: Clean room refuses the stand-in tools, but the one the
    /// run started still stops with it.
    #[test]
    fn it_stops_after_the_session_moved_to_clean_room() {
        let d = tempfile::tempdir().unwrap();
        let mut ws = site(d.path());
        record(&ws);
        let w = check_worker(&ws, true);
        let s = w.start_standin(&mut ws, true).unwrap();
        w.engine.session().settings.deep_mode = false;
        assert!(w.engine.call("web_standin_stop", json!({"id": s.id}), CallOptions::new(Caller::Ui)).is_err(), "Clean room refuses the tool");
        w.stop_standin(&s);
        w.engine.session().settings.deep_mode = true;
        assert!(running(&w).is_empty());
    }

    /// One already serving the same file in this session (a builder's own) is used, not stopped.
    #[test]
    fn one_already_serving_the_recording_is_left_running() {
        let d = tempfile::tempdir().unwrap();
        let mut ws = site(d.path());
        record(&ws);
        let w = check_worker(&ws, true);
        let theirs = w.start_standin(&mut ws, true).unwrap();
        let again = w.start_standin(&mut ws, true).unwrap();
        assert_eq!((again.id.as_str(), again.url.as_str()), (theirs.id.as_str(), theirs.url.as_str()));
        w.stop_standin(&again);
        assert_eq!(running(&w).len(), 1, "it wasn't this run's to stop");
        w.stop_standin(&theirs);
        assert!(running(&w).is_empty());
    }

    #[test]
    fn only_a_deep_website_build_with_a_recording_gets_one() {
        let d = tempfile::tempdir().unwrap();
        // Clean room reads nothing inside the original, the recording included.
        let mut ws = site(&d.path().join("clean"));
        record(&ws);
        let w = check_worker(&ws, false);
        assert!(w.start_standin(&mut ws, false).is_none());
        // A program or a request alone has no recorded backend.
        for target in [TargetRef::None, TargetRef::Local { path: d.path().join("App") }] {
            let mut ws = Workspace::create(&d.path().join("other"), "App", "Rebuild this", target).unwrap();
            record(&ws);
            assert!(check_worker(&ws, true).start_standin(&mut ws, true).is_none());
            assert!(!ws.capture_dir().join(STANDIN_DIR).exists());
        }
        // A website whose capture kept no recording.
        let mut ws = site(&d.path().join("unrecorded"));
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        let w = check_worker(&ws, true);
        assert!(w.start_standin(&mut ws, true).is_none());
        assert!(notes(&w).is_empty(), "nothing to say: {:?}", notes(&w));
        assert!(running(&w).is_empty());
        assert!(!ws.capture_dir().join(STANDIN_DIR).exists());
        assert_eq!(Workspace::open(&ws.root).unwrap().manifest.standin_port, None);
    }

    /// The saved port is asked for first; when another program has it, a free one is taken, kept
    /// and said.
    #[test]
    fn a_busy_saved_port_gives_way_to_a_free_one() {
        let d = tempfile::tempdir().unwrap();
        let mut ws = site(d.path());
        record(&ws);
        let first = check_worker(&ws, true);
        let held = first.start_standin(&mut ws, true).unwrap();
        let saved = port_of(&held.url).unwrap();
        // Another session (the next build, while this one's still serves) asks for the saved port.
        let mut again = Workspace::open(&ws.root).unwrap();
        assert_eq!(again.manifest.standin_port, Some(saved));
        let second = check_worker(&again, true);
        let moved = second.start_standin(&mut again, true).expect("a free port instead");
        assert_ne!(port_of(&moved.url), Some(saved));
        assert!(notes(&second).iter().any(|n| n.contains(&format!("port {saved}, the one it used last time, is taken"))), "{:?}", notes(&second));
        assert!(get(&moved.url, "/v1/items/42").unwrap().contains("Recorded"));
        assert_eq!(Workspace::open(&ws.root).unwrap().manifest.standin_port, port_of(&moved.url), "the port it got is the one kept");
        second.stop_standin(&moved);
        first.stop_standin(&held);
    }

    #[test]
    fn a_recording_that_cant_be_read_is_said_and_the_build_goes_on() {
        let d = tempfile::tempdir().unwrap();
        let mut ws = site(d.path());
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        std::fs::write(ws.capture_dir().join(study::RECORDING), "not a recording").unwrap();
        let w = check_worker(&ws, true);
        assert!(w.start_standin(&mut ws, true).is_none());
        let said = notes(&w);
        assert!(said.iter().any(|n| n.starts_with("Couldn't start a stand-in backend")), "{said:?}");
        assert!(running(&w).is_empty());
        assert_eq!(Workspace::open(&ws.root).unwrap().manifest.standin_port, None);
    }

    #[test]
    fn the_studys_own_recording_comes_before_any_other() {
        let d = tempfile::tempdir().unwrap();
        let ws = site(d.path());
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        assert_eq!(recording(&ws), None);
        std::fs::write(ws.capture_dir().join("b.HAR"), "{}").unwrap();
        std::fs::write(ws.capture_dir().join("a.har"), "{}").unwrap();
        std::fs::create_dir_all(ws.capture_dir().join("0.har")).unwrap();
        assert_eq!(recording(&ws), Some(ws.capture_dir().join("a.har")), "the first file by name; a folder isn't one");
        record(&ws);
        assert_eq!(recording(&ws), Some(ws.capture_dir().join(study::RECORDING)));
    }

    #[test]
    fn the_port_comes_from_a_loopback_url() {
        assert_eq!(port_of("http://127.0.0.1:41234"), Some(41234));
        assert_eq!(port_of("http://127.0.0.1:41234/"), Some(41234));
        assert_eq!(port_of("http://127.0.0.1:0"), None);
        assert_eq!(port_of("http://127.0.0.1"), None);
        assert_eq!(port_of("http://127.0.0.1:99999"), None);
        assert_eq!(port_of(""), None);
    }

    #[test]
    fn a_start_names_its_server_whether_new_or_already_running() {
        let new = ToolOutput::Value(json!({"already_running": false, "id": "standin-2", "url": "http://127.0.0.1:5000"}));
        assert_eq!(started_standin(&new), Some(("standin-2".into(), "http://127.0.0.1:5000".into(), true)));
        let same =
            ToolOutput::Value(json!({"already_running": true, "server": {"id": "standin-1", "url": "http://127.0.0.1:5001"}, "url": "http://127.0.0.1:5001"}));
        assert_eq!(started_standin(&same), Some(("standin-1".into(), "http://127.0.0.1:5001".into(), false)));
        assert_eq!(started_standin(&ToolOutput::Value(json!({"url": "http://127.0.0.1:5001"}))), None, "no id, nothing to stop");
        assert_eq!(started_standin(&ToolOutput::Value(json!({"id": "standin-1"}))), None, "no address, nothing to tell");
    }
}
