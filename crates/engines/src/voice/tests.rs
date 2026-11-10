//! The voice client against a stand-in service ([`standin::StandInService`]) on an ephemeral
//! port, and the process side against this test binary acting as the service (a guarded, ignored
//! test that prints `READY`). The real voice service and port 8191 are never used.

use super::client::ClientOptions;
use super::*;
use serde_json::json;
use std::sync::mpsc::Receiver;
use std::time::Instant;

const WAIT: Duration = Duration::from_secs(10);

fn quick() -> ClientOptions {
    ClientOptions { reconnect_window: Duration::from_millis(800), connect_wait: Duration::from_secs(3) }
}

/// The next event that isn't a state change (states come with many replies).
fn next_non_state(rx: &Receiver<VoiceEvent>) -> VoiceEvent {
    let t0 = Instant::now();
    loop {
        let left = WAIT.saturating_sub(t0.elapsed());
        let ev = rx.recv_timeout(left).unwrap_or_else(|_| panic!("no event within {WAIT:?}"));
        // States and loudness come all the time; the rest is what a test waits for.
        if !matches!(ev, VoiceEvent::State { .. } | VoiceEvent::Level { .. }) {
            return ev;
        }
    }
}

fn wait_for(rx: &Receiver<VoiceEvent>, what: impl Fn(&VoiceEvent) -> bool) -> VoiceEvent {
    let t0 = Instant::now();
    loop {
        let left = WAIT.saturating_sub(t0.elapsed());
        let ev = rx.recv_timeout(left).unwrap_or_else(|_| panic!("the event didn't come within {WAIT:?}"));
        if what(&ev) {
            return ev;
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn events_are_read_leniently() {
    let p = VoiceEvent::parse;
    assert_eq!(
        p(r#"{"type":"state","listening":true,"capturing":false,"speaking":true,"mode":"push_to_talk","enrolled":true,"extra":{"x":1}}"#),
        Some(VoiceEvent::State {
            listening: true,
            capturing: false,
            speaking: true,
            mode: "push_to_talk".into(),
            enrolled: true,
            models_ready: true,
            loading: String::new(),
            load_step: 0,
            load_steps: 0
        })
    );
    assert_eq!(p(r#"{"type":"wake","word":"hey craft","score":0.8}"#), Some(VoiceEvent::Wake { word: "hey craft".into(), score: 0.8 }));
    assert_eq!(p(r#"{"type":"partial","text":"open the tra","t":12}"#), Some(VoiceEvent::Partial { text: "open the tra".into() }));
    assert_eq!(
        p(r#"{"type":"utterance","text":"open the trailer folder","speaker_ok":true,"speaker_score":0.71,"duration_s":2.4,"lang":"en","new_field":[1,2]}"#),
        Some(VoiceEvent::Utterance { text: "open the trailer folder".into(), speaker_ok: true, speaker_score: 0.71, duration_s: 2.4, verified: true })
    );
    assert_eq!(p(r#"{"type":"barge_in","at":3.2}"#), Some(VoiceEvent::BargeIn));
    assert_eq!(p(r#"{"type":"spoken","id":"s1","interrupted":true}"#), Some(VoiceEvent::Spoken { id: "s1".into(), interrupted: true }));
    assert_eq!(p(r#"{"type":"enroll_progress","seconds":12.5,"needed":20}"#), Some(VoiceEvent::EnrollProgress { seconds: 12.5, needed: 20.0 }));
    assert_eq!(p(r#"{"type":"enrolled","ok":true}"#), Some(VoiceEvent::Enrolled { ok: true }));
    assert_eq!(
        p(r#"{"type":"voice_designed","ref_audio":"v/a.wav","ref_text":"Hello"}"#),
        Some(VoiceEvent::VoiceDesigned { ref_audio: PathBuf::from("v/a.wav"), ref_text: "Hello".into() })
    );
    // Devices as names or as objects with a name.
    assert_eq!(
        p(r#"{"type":"devices","inputs":["Mic",{"name":"USB mic","index":3},7],"outputs":[]}"#),
        Some(VoiceEvent::Devices { inputs: vec!["Mic".into(), "USB mic".into()], outputs: vec![] })
    );
    assert_eq!(p(r#"{"type":"error","message":"no microphone","fatal":false}"#), Some(VoiceEvent::Error { message: "no microphone".into(), fatal: false }));
    assert_eq!(p(r#"{"type":"config_ok","id":"c1"}"#), Some(VoiceEvent::ConfigOk));
    assert_eq!(p(r#"{"type":"status","listening":true,"version":1}"#), Some(VoiceEvent::Status { state: json!({"listening": true, "version": 1}) }));
    // Missing fields get their empty values; wrong types too.
    assert_eq!(
        p(r#"{"type":"utterance","text":7}"#),
        Some(VoiceEvent::Utterance { text: String::new(), speaker_ok: false, speaker_score: 0.0, duration_s: 0.0, verified: true })
    );
    // Not events: skipped, never a crash.
    for bad in ["", "null", "[]", "{}", r#"{"type":"something_new","x":1}"#, r#"{"type":5}"#, "{not json", "\u{0}\u{1}"] {
        assert_eq!(p(bad), None, "{bad:?}");
    }
    // Events serialise back with their type.
    assert_eq!(
        serde_json::to_value(VoiceEvent::Spoken { id: "a".into(), interrupted: false }).unwrap(),
        json!({"type": "spoken", "id": "a", "interrupted": false})
    );
    assert_eq!(serde_json::to_value(VoiceEvent::Disconnected).unwrap(), json!({"type": "disconnected"}));
}

#[test]
fn commands_are_written_as_the_protocol_says() {
    let j = |c: VoiceCommand| serde_json::from_str::<Value>(&c.to_json().unwrap()).unwrap();
    assert_eq!(j(VoiceCommand::Ptt { down: true }), json!({"cmd": "ptt", "down": true}));
    assert_eq!(
        j(VoiceCommand::Speak { id: "s1".into(), text: "Hi.".into(), interruptible: true }),
        json!({"cmd": "speak", "id": "s1", "text": "Hi.", "interruptible": true})
    );
    assert_eq!(j(VoiceCommand::StopSpeaking), json!({"cmd": "stop_speaking"}));
    assert_eq!(j(VoiceCommand::EnrollStart), json!({"cmd": "enroll_start"}));
    assert_eq!(j(VoiceCommand::EnrollStop), json!({"cmd": "enroll_stop"}));
    assert_eq!(
        j(VoiceCommand::DesignVoice { description: "calm".into(), sample_text: "Hello".into() }),
        json!({"cmd": "design_voice", "description": "calm", "sample_text": "Hello"})
    );
    assert_eq!(j(VoiceCommand::Devices), json!({"cmd": "devices"}));
    assert_eq!(j(VoiceCommand::Status), json!({"cmd": "status"}));
    assert_eq!(j(VoiceCommand::Shutdown), json!({"cmd": "shutdown"}));
    assert_eq!(
        j(VoiceCommand::Config(VoiceConfig::default())),
        json!({"cmd": "config", "listen_mode": "push_to_talk", "wake_word": "hey craft", "input_device": null, "output_device": null,
               "speaker_threshold": 0.55, "language": "en", "tts_voice": null, "vad_silence_ms": 700, "sounds": true,
               "echo_cancellation": true})
    );
    let voice = VoiceConfig {
        listen_mode: ListenMode::WakeWord,
        input_device: Some("USB mic".into()),
        tts_voice: Some(TtsVoice { ref_audio: PathBuf::from("a.wav"), ref_text: "Hello".into() }),
        ..VoiceConfig::default()
    };
    let v = j(VoiceCommand::Config(voice.clone()));
    assert_eq!((&v["listen_mode"], &v["input_device"], &v["tts_voice"]["ref_text"]), (&json!("wake_word"), &json!("USB mic"), &json!("Hello")));
    // And read back (a stand-in or a log reader can parse them).
    assert_eq!(serde_json::from_value::<VoiceCommand>(v).unwrap(), VoiceCommand::Config(voice));
}

#[test]
fn the_ready_line_is_read_strictly() {
    assert_eq!(parse_ready(r#"READY {"port":8191,"version":1}"#), Some((8191, 1)));
    assert_eq!(parse_ready(r#"  READY {"port":40000,"version":2,"extra":true}  "#), Some((40000, 2)));
    for bad in ["READY", "READY {}", r#"READY {"port":0,"version":1}"#, r#"READY {"port":70000,"version":1}"#, r#"READY {"port":1}"#, "ready {}", "NOT READY"] {
        assert_eq!(parse_ready(bad), None, "{bad:?}");
    }
}

#[test]
fn the_client_hears_scripted_events_and_sends_commands() {
    let script = vec![
        json!({"type": "partial", "text": "open the tra"}),
        json!({"type": "utterance", "text": "open the trailer folder", "speaker_ok": true, "speaker_score": 0.71, "duration_s": 2.4, "lang": "en"}),
        json!({"type": "something_from_a_newer_service", "x": 1}),
        json!({"type": "barge_in"}),
        json!({"type": "spoken", "id": "s0", "interrupted": true}),
        json!({"type": "error", "message": "no microphone", "fatal": false}),
    ];
    let svc = standin::StandInService::start(script).unwrap();
    let client = VoiceClient::connect_with(svc.port(), quick()).unwrap();
    let rx = client.events();
    // The first state may be replayed or still arriving; then the script, in order, the unknown
    // event skipped.
    assert_eq!(next_non_state(&rx), VoiceEvent::Partial { text: "open the tra".into() });
    assert_eq!(
        next_non_state(&rx),
        VoiceEvent::Utterance { text: "open the trailer folder".into(), speaker_ok: true, speaker_score: 0.71, duration_s: 2.4, verified: true }
    );
    assert_eq!(next_non_state(&rx), VoiceEvent::BargeIn);
    assert_eq!(next_non_state(&rx), VoiceEvent::Spoken { id: "s0".into(), interrupted: true });
    assert_eq!(next_non_state(&rx), VoiceEvent::Error { message: "no microphone".into(), fatal: false });
    // A second listener hears the same from now on, starting with the last state.
    let rx2 = client.events();
    assert!(matches!(rx2.recv_timeout(WAIT).unwrap(), VoiceEvent::State { .. }), "a new listener gets the last state");

    client.config(&VoiceConfig { listen_mode: ListenMode::Always, ..VoiceConfig::default() }).unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::ConfigOk);
    client.ptt(true).unwrap();
    let st = wait_for(&rx, |e| matches!(e, VoiceEvent::State { capturing: true, .. }));
    assert!(matches!(st, VoiceEvent::State { ref mode, .. } if mode == "always"), "{st:?}");
    client.ptt(false).unwrap();
    client.speak("s1", "The trailer folder is open.").unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::Spoken { id: "s1".into(), interrupted: false });
    client.stop_speaking().unwrap();
    client.enroll_start().unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::EnrollProgress { seconds: 0.0, needed: 20.0 });
    client.enroll_stop().unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::Enrolled { ok: true });
    client.design_voice("a calm low voice", "Hello there.").unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::VoiceDesigned { ref_audio: PathBuf::from("stand-in.wav"), ref_text: "Hello there.".into() });
    client.devices().unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::Devices { inputs: vec!["Stand-in microphone".into()], outputs: vec!["Stand-in speakers".into()] });
    client.request_status().unwrap();
    assert!(matches!(next_non_state(&rx), VoiceEvent::Status { state } if state["version"] == 1));
    // Pushed events (the service "hearing" something) arrive too.
    svc.push(json!({"type": "wake", "word": "hey craft", "score": 0.9}));
    assert_eq!(next_non_state(&rx), VoiceEvent::Wake { word: "hey craft".into(), score: 0.9 });
    assert!(wait_for(&rx2, |e| matches!(e, VoiceEvent::Wake { .. })) == VoiceEvent::Wake { word: "hey craft".into(), score: 0.9 });

    // The service got exactly what was sent, in order.
    assert_eq!(svc.command_names(), ["config", "ptt", "ptt", "speak", "stop_speaking", "enroll_start", "enroll_stop", "design_voice", "devices", "status"]);
    let cmds = svc.commands();
    assert_eq!(cmds[0]["listen_mode"], "always");
    assert_eq!((&cmds[1]["down"], &cmds[2]["down"]), (&json!(true), &json!(false)));
    assert_eq!(cmds[3], json!({"cmd": "speak", "id": "s1", "text": "The trailer folder is open.", "interruptible": true}));
    assert!(client.is_alive());

    // Shutting down asks the service to end, tells every listener once, and refuses more commands.
    client.shutdown();
    assert!(svc.wait_for_command("shutdown", WAIT));
    assert_eq!(wait_for(&rx, |e| matches!(e, VoiceEvent::Disconnected)), VoiceEvent::Disconnected);
    assert!(!client.is_alive());
    assert_eq!(client.speak("s2", "again").unwrap_err().code, ErrorCode::EngineMissing);
    client.shutdown();
    let late = client.events();
    assert!(std::iter::from_fn(|| late.recv_timeout(Duration::from_secs(1)).ok()).any(|e| e == VoiceEvent::Disconnected), "a late listener hears it's gone");
    assert_eq!(svc.command_names().iter().filter(|c| *c == "shutdown").count(), 1);
}

#[test]
fn what_is_said_before_anyone_listens_goes_to_the_first_listener() {
    let svc = standin::StandInService::start(vec![json!({"type": "partial", "text": "early words"})]).unwrap();
    let client = VoiceClient::connect_with(svc.port(), quick()).unwrap();
    // Long enough for the state and the partial to have come in before anyone listens.
    std::thread::sleep(Duration::from_millis(300));
    let first = client.events();
    assert!(matches!(first.recv_timeout(WAIT).unwrap(), VoiceEvent::State { .. }));
    assert_eq!(first.recv_timeout(WAIT).unwrap(), VoiceEvent::Partial { text: "early words".into() });
    // A later listener gets only the last state, then what comes next.
    let later = client.events();
    assert!(matches!(later.recv_timeout(WAIT).unwrap(), VoiceEvent::State { .. }));
    svc.push(json!({"type": "barge_in"}));
    assert_eq!(next_non_state(&later), VoiceEvent::BargeIn);
    assert_eq!(next_non_state(&first), VoiceEvent::BargeIn);
}

#[test]
fn a_dropped_link_is_restored_and_the_configuration_sent_again() {
    let svc = standin::StandInService::start(Vec::new()).unwrap();
    let client = VoiceClient::connect_with(svc.port(), ClientOptions { reconnect_window: Duration::from_secs(5), ..quick() }).unwrap();
    let rx = client.events();
    client.config(&VoiceConfig { wake_word: "hey stand-in".into(), ..VoiceConfig::default() }).unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::ConfigOk);
    svc.drop_connection();
    // Back on a new connection: the config again, then a status request.
    let t0 = Instant::now();
    while svc.connections() < 2 && t0.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(svc.connections(), 2, "the client connected again");
    assert_eq!(next_non_state(&rx), VoiceEvent::ConfigOk);
    assert!(matches!(next_non_state(&rx), VoiceEvent::Status { .. }));
    let cmds = svc.commands();
    assert_eq!(svc.command_names(), ["config", "config", "status"]);
    assert_eq!(cmds[1]["wake_word"], "hey stand-in");
    // Still usable, and nobody was told it was gone.
    client.speak("after", "Still here.").unwrap();
    assert_eq!(next_non_state(&rx), VoiceEvent::Spoken { id: "after".into(), interrupted: false });
    assert!(client.is_alive());
    assert!(rx.try_iter().all(|e| e != VoiceEvent::Disconnected));
}

#[test]
fn a_service_that_goes_away_is_reported_once() {
    let svc = standin::StandInService::start(Vec::new()).unwrap();
    let client = VoiceClient::connect_with(svc.port(), quick()).unwrap();
    let rx = client.events();
    svc.stop();
    let t0 = Instant::now();
    assert_eq!(wait_for(&rx, |e| matches!(e, VoiceEvent::Disconnected)), VoiceEvent::Disconnected);
    assert!(t0.elapsed() < WAIT);
    assert!(!client.is_alive());
    assert_eq!(client.ptt(true).unwrap_err().code, ErrorCode::EngineMissing);
    // Once only, and the client can still be shut down and dropped.
    client.shutdown();
    std::thread::sleep(Duration::from_millis(100));
    assert!(rx.try_iter().all(|e| e != VoiceEvent::Disconnected));
    drop(client);
}

#[test]
fn connecting_to_nothing_fails_plainly() {
    let port = free_port();
    let err = VoiceClient::connect_with(port, ClientOptions { connect_wait: Duration::from_millis(300), ..quick() }).unwrap_err();
    assert_eq!(err.code, ErrorCode::EngineMissing, "{err:?}");
    assert!(err.message.contains(&port.to_string()), "{}", err.message);
}

// ---- the process side ------------------------------------------------------------------------

const STAND_IN_ENV: &str = "CRAFT_VOICE_READY_STAND_IN";

/// This test binary as the voice service: `ready:<port>` prints READY for that port and waits,
/// `silent` waits without a word, `fail` complains and exits, `old:<port>` says a newer protocol.
fn stand_in_command(mode: &str) -> ServiceCommand {
    ServiceCommand {
        program: std::env::current_exe().unwrap().into_os_string(),
        args: ["--ignored", "--exact", "voice::tests::ready_stand_in", "--nocapture", "--test-threads=1"].iter().map(Into::into).collect(),
        env: vec![(STAND_IN_ENV.into(), mode.into())],
        cwd: None,
    }
}

/// Not a test: the stand-in service process the tests above start (guarded so running ignored
/// tests by hand does nothing).
#[test]
#[ignore = "started by the voice process tests as a stand-in service"]
fn ready_stand_in() {
    let Ok(mode) = std::env::var(STAND_IN_ENV) else { return };
    let wait = || {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    if let Some(port) = mode.strip_prefix("ready:") {
        println!("\nREADY {{\"port\":{port},\"version\":1}}");
        eprintln!("stand-in service up");
        wait();
    } else if let Some(port) = mode.strip_prefix("old:") {
        println!("\nREADY {{\"port\":{port},\"version\":99}}");
        wait();
    } else if mode == "silent" {
        eprintln!("loading models forever");
        wait();
    } else {
        eprintln!("ModuleNotFoundError: No module named 'faster_whisper'");
        std::process::exit(3);
    }
}

#[test]
fn a_started_service_is_connected_after_ready_and_stopped_on_shutdown() {
    let svc = standin::StandInService::start(vec![json!({"type": "partial", "text": "hello"})]).unwrap();
    let port = svc.port();
    let client = VoiceClient::launch(stand_in_command(&format!("ready:{port}")), port, Duration::from_secs(30), &CancelToken::default(), quick()).unwrap();
    let pid = client.pid().expect("it started the service");
    assert!(pid > 0 && client.port() == port);
    let rx = client.events();
    assert_eq!(next_non_state(&rx), VoiceEvent::Partial { text: "hello".into() });
    assert!(client.is_alive());
    let t0 = Instant::now();
    while !client.log_tail(10).iter().any(|l| l.contains("stand-in service up")) && t0.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(client.log_tail(10).iter().any(|l| l.contains("stand-in service up")), "{:?}", client.log_tail(10));
    client.shutdown();
    assert!(svc.shutdown_asked(), "the service was asked to end");
    assert!(!client.is_alive());
    assert_eq!(wait_for(&rx, |e| matches!(e, VoiceEvent::Disconnected)), VoiceEvent::Disconnected);
}

#[test]
fn a_service_that_never_says_ready_times_out_and_is_stopped() {
    let port = free_port();
    let t0 = Instant::now();
    let err = VoiceClient::launch(stand_in_command("silent"), port, Duration::from_millis(1500), &CancelToken::default(), quick()).unwrap_err();
    assert_eq!(err.code, ErrorCode::TimedOut, "{err:?}");
    assert!(err.message.contains("ready"), "{}", err.message);
    assert!(t0.elapsed() < Duration::from_secs(20));
}

#[test]
fn waiting_for_ready_can_be_cancelled() {
    let cancel = CancelToken::default();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        c.cancel();
    });
    let t0 = Instant::now();
    let err = VoiceClient::launch(stand_in_command("silent"), free_port(), Duration::from_secs(60), &cancel, quick()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled, "{err:?}");
    assert!(t0.elapsed() < Duration::from_secs(15));
}

#[test]
fn a_service_that_dies_at_start_says_why() {
    let err = VoiceClient::launch(stand_in_command("fail"), free_port(), Duration::from_secs(30), &CancelToken::default(), quick()).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProgramFailed, "{err:?}");
    assert!(err.message.contains("faster_whisper"), "the service's own words are kept: {}", err.message);
}

#[test]
fn a_service_of_another_protocol_version_is_refused() {
    let port = free_port();
    let err = VoiceClient::launch(stand_in_command(&format!("old:{port}")), port, Duration::from_secs(30), &CancelToken::default(), quick()).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotSupported, "{err:?}");
}

// ---- status ----------------------------------------------------------------------------------

fn setup_in(dir: &Path) -> VoiceSetup {
    VoiceSetup {
        python: None,
        port: free_port(),
        models_dir: dir.join("models"),
        device: "cpu".into(),
        scripts_dir: dir.join("voice"),
        ..VoiceSetup::default()
    }
}

#[test]
fn status_says_what_is_missing() {
    let d = tempfile::tempdir().unwrap();
    let mut setup = setup_in(d.path());
    let st = status(&setup);
    assert!(!st.python_found && !st.script_present && !st.models_present && !st.startable && !st.serving, "{st:?}");
    assert_eq!(st.port_state, PortState::Free);
    assert!(st.detail.contains("no voice Python"), "{}", st.detail);
    assert_eq!(st.placeholder, service_is_placeholder());
    // A Python that isn't there is named.
    setup.python = Some(d.path().join("no-python.exe"));
    let st = status(&setup);
    assert!(!st.python_found && st.detail.contains("no-python.exe"), "{}", st.detail);
    let err = VoiceClient::start(&setup).unwrap_err();
    assert_eq!(err.code, ErrorCode::EngineMissing, "nothing is started without the Python");
    assert!(!setup.script_path().exists(), "nor written");
    // A Python that's there (any file will do for the check), the script written, a model fetched.
    let py = d.path().join("python.exe");
    std::fs::write(&py, b"").unwrap();
    setup.python = Some(py);
    setup.write_script().unwrap();
    std::fs::create_dir_all(setup.models_dir.join("whisper")).unwrap();
    let st = status(&setup);
    assert!(st.python_found && st.script_present && st.models_present && st.startable, "{st:?}");
    assert_eq!(std::fs::read_to_string(setup.script_path()).unwrap(), VOICE_SERVICE_PY);
    // A changed script is "not present" (it's written again on start).
    std::fs::write(setup.script_path(), "old").unwrap();
    assert!(!status(&setup).script_present);
}

#[test]
fn status_tells_a_voice_service_from_another_program_on_the_port() {
    let d = tempfile::tempdir().unwrap();
    let svc = standin::StandInService::start(Vec::new()).unwrap();
    let mut setup = VoiceSetup { port: svc.port(), ..setup_in(d.path()) };
    let st = status(&setup);
    assert!(st.serving && !st.startable, "{st:?}");
    assert!(matches!(&st.port_state, PortState::Serving { version: 1, state } if state["type"] == "state"), "{:?}", st.port_state);
    // Something that doesn't answer as a voice service.
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    setup.port = other.local_addr().unwrap().port();
    let keep = std::thread::spawn(move || {
        // Accept and close without a word.
        for s in other.incoming() {
            drop(s);
        }
    });
    let st = status(&setup);
    assert!(matches!(st.port_state, PortState::Busy { .. }), "{:?}", st.port_state);
    assert!(!st.serving && !st.startable);
    // And starting there is refused before anything runs.
    let py = d.path().join("python.exe");
    std::fs::write(&py, b"").unwrap();
    setup.python = Some(py);
    assert_eq!(VoiceClient::start(&setup).unwrap_err().code, ErrorCode::BadSettings);
    drop(keep); // It keeps accepting until the test binary ends.
}

#[test]
fn the_start_command_is_the_contracts() {
    let d = tempfile::tempdir().unwrap();
    let py = d.path().join("python.exe");
    std::fs::write(&py, b"").unwrap();
    let setup = VoiceSetup { python: Some(py.clone()), port: 8191, device: "cuda:0".into(), ..setup_in(d.path()) };
    let cmd = setup.command().unwrap();
    assert_eq!(PathBuf::from(&cmd.program), py);
    let args: Vec<String> = cmd.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let me = std::process::id().to_string();
    let script = setup.script_path().display().to_string();
    let models = setup.models_dir.display().to_string();
    assert_eq!(args, ["-I", &script, "--host", "127.0.0.1", "--port", "8191", "--models-dir", &models, "--device", "cuda:0", "--parent-pid", &me]);
    assert!(setup.script_path().is_file() && setup.models_dir.is_dir());
    assert!(cmd.env.iter().any(|(k, v)| k == "PYTHONUNBUFFERED" && v == "1"), "READY must not sit in a buffer");
    assert!(!args.iter().any(|a| a == "--allow-inject"), "never from the settings");
    let test = VoiceSetup { allow_inject: true, ..setup.clone() };
    let args: Vec<String> = test.command().unwrap().args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(args.last().map(String::as_str), Some("--allow-inject"));
}

/// Levels, loading progress and chimes are read and written.
#[test]
fn levels_loading_and_chimes_are_read_and_written() {
    assert_eq!(VoiceEvent::parse(r#"{"type":"level","in":0.42,"out":1.7}"#), Some(VoiceEvent::Level { input: 0.42, output: 1.0 }), "clamped to 0..1");
    let st = VoiceEvent::parse(r#"{"type":"state","listening":true,"models_ready":false,"loading":"speech recognition","load_step":2,"load_steps":5}"#);
    assert!(
        matches!(st, Some(VoiceEvent::State { models_ready: false, load_step: 2, load_steps: 5, ref loading, .. }) if loading == "speech recognition"),
        "{st:?}"
    );
    let j: Value = serde_json::from_str(&VoiceCommand::Chime { kind: "done".into() }.to_json().unwrap()).unwrap();
    assert_eq!(j, json!({"cmd": "chime", "kind": "done"}));
    let c: Value = serde_json::from_str(&VoiceCommand::Config(VoiceConfig::default()).to_json().unwrap()).unwrap();
    assert_eq!(c["sounds"], true);
}

/// The service's own speech coming back is reported, not heard.
#[test]
fn echo_ignored_is_read() {
    assert_eq!(
        VoiceEvent::parse(r#"{"type":"echo_ignored","text":"how can I help","reason":"own_words"}"#),
        Some(VoiceEvent::EchoIgnored { text: "how can I help".into(), reason: "own_words".into() })
    );
    let c: Value = serde_json::from_str(&VoiceCommand::Config(VoiceConfig::default()).to_json().unwrap()).unwrap();
    assert_eq!(c["echo_cancellation"], true);
}

/// The stand-in says how loud it is while push-to-talk is held, and goes quiet after.
#[test]
fn the_stand_in_reports_levels_while_capturing() {
    let svc = standin::StandInService::start(vec![]).unwrap();
    let client = VoiceClient::connect(svc.port()).unwrap();
    let rx = client.events();
    client.ptt(true).unwrap();
    let up = wait_for(&rx, |e| matches!(e, VoiceEvent::Level { input, .. } if *input > 0.3));
    assert!(matches!(up, VoiceEvent::Level { .. }));
    client.ptt(false).unwrap();
    let down = wait_for(&rx, |e| matches!(e, VoiceEvent::Level { input, .. } if *input == 0.0));
    assert!(matches!(down, VoiceEvent::Level { .. }));
}

/// Before enrolment the service says `verified: false`; test audio goes in as `inject_wav`.
#[test]
fn verification_and_test_injection_are_read_and_written() {
    let u = VoiceEvent::parse(r#"{"type":"utterance","text":"hi","speaker_ok":true,"speaker_score":0.0,"duration_s":1.0,"verified":false}"#);
    assert!(matches!(u, Some(VoiceEvent::Utterance { verified: false, speaker_ok: true, .. })), "{u:?}");
    assert_eq!(VoiceEvent::parse(r#"{"type":"injected","path":"a.wav","seconds":1.5}"#), Some(VoiceEvent::Injected { seconds: 1.5 }));
    let j: Value = serde_json::from_str(&VoiceCommand::InjectWav { path: PathBuf::from("a.wav"), ptt: None }.to_json().unwrap()).unwrap();
    assert_eq!(j, json!({"cmd": "inject_wav", "path": "a.wav"}));
    let j: Value = serde_json::from_str(&VoiceCommand::InjectWav { path: PathBuf::from("a.wav"), ptt: Some(false) }.to_json().unwrap()).unwrap();
    assert_eq!(j["ptt"], false);
}

/// The shipped service script for real, when a Python is given (`CRAFT_TEST_VOICE_PYTHON`); the
/// placeholder needs only the standard library. Skipped otherwise.
#[test]
fn the_shipped_service_starts_answers_and_stops_with_a_real_python() {
    let Some(python) = std::env::var_os("CRAFT_TEST_VOICE_PYTHON").map(PathBuf::from).filter(|p| p.is_file()) else {
        eprintln!("skipped: set CRAFT_TEST_VOICE_PYTHON to a Python to run the shipped voice service");
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let setup = VoiceSetup { python: Some(python), device: "cpu".into(), ready_timeout: Duration::from_secs(120), ..setup_in(d.path()) };
    let client = VoiceClient::start(&setup).unwrap();
    let rx = client.events();
    assert!(matches!(status(&setup).port_state, PortState::Serving { version: 1, .. }));
    client.config(&VoiceConfig::default()).unwrap();
    assert_eq!(wait_for(&rx, |e| matches!(e, VoiceEvent::ConfigOk)), VoiceEvent::ConfigOk);
    client.speak("t1", "Testing.").unwrap();
    assert!(matches!(wait_for(&rx, |e| matches!(e, VoiceEvent::Spoken { .. })), VoiceEvent::Spoken { id, .. } if id == "t1"));
    client.request_status().unwrap();
    assert!(matches!(wait_for(&rx, |e| matches!(e, VoiceEvent::Status { .. })), VoiceEvent::Status { .. }));
    let t0 = Instant::now();
    client.shutdown();
    assert!(t0.elapsed() < Duration::from_secs(8), "it ended when asked");
    assert_eq!(status(&setup).port_state, PortState::Free);
}
