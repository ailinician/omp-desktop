//! Terminal manager tests on Windows: the default shell (`%ComSpec%`, i.e.
//! `cmd.exe`) on a pseudo console, observed through a collecting [`Emit`].
//! The `sh` tests in `tests.rs` are unix-only; these run on the Windows CI
//! leg. Every wait polls with a generous timeout: no timing is asserted.

use super::pump::TerminalEmit;
use super::test_support::{open_with, stdout_of, wait_until};
use super::{Emit, TerminalManager, TerminalState};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(30);

/// Everything the manager emitted, for every shell.
#[derive(Default)]
struct Collector {
    data: Mutex<Vec<u8>>,
    exits: Mutex<Vec<Option<i32>>>,
}

/// One shell's emitter, recording into the shared [`Collector`].
struct Collecting(Arc<Collector>);

impl Emit for Arc<Collector> {
    type Terminal = Collecting;

    fn terminal(self, _id: &str, _instance: u64) -> Collecting {
        Collecting(self)
    }
}

impl TerminalEmit for Collecting {
    fn data(&mut self, _seq: u64, bytes: &[u8]) {
        self.0.data.lock().unwrap().extend_from_slice(bytes);
    }

    fn exit(&mut self, code: Option<i32>) {
        self.0.exits.lock().unwrap().push(code);
    }
}

impl Collector {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data.lock().unwrap()).into_owned()
    }

    fn exited(&self) -> bool {
        !self.exits.lock().unwrap().is_empty()
    }
}

/// `id` registered with the temp dir as its folder, its default shell
/// opened, and the pseudo console's start-up cursor query answered.
/// `portable-pty` creates the console with `PSEUDOCONSOLE_INHERIT_CURSOR`,
/// so conhost first asks the terminal where the cursor is (`ESC[6n`) and
/// runs nothing until a reply comes back; in the app xterm.js answers it.
fn open(id: &str) -> (TerminalManager, Arc<Collector>) {
    let (manager, out) = open_with(TerminalManager::new(None), id);
    assert!(
        wait_until(SLOW, || out.text().contains("\x1b[6n")),
        "no cursor query; output: {:?}",
        out.text()
    );
    manager.write(id, "\x1b[1;1R").unwrap();
    (manager, out)
}

/// Whether a `ping` whose command line contains `marker` is running.
fn ping_running(marker: &str) -> bool {
    stdout_of(
        "powershell",
        [
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name='PING.EXE'\" \
             | ForEach-Object { $_.CommandLine }",
        ],
    )
    .contains(marker)
}

#[test]
fn the_shells_exit_code_is_reported() {
    let (manager, out) = open("win-exit");
    manager.write("win-exit", "exit 3\r\n").unwrap();

    assert!(
        wait_until(SLOW, || out.exited()),
        "no exit event; output: {:?}",
        out.text()
    );
    assert_eq!(*out.exits.lock().unwrap(), [Some(3)]);
    let status = manager.status("win-exit").unwrap();
    assert_eq!(
        (status.state, status.exit_code),
        (TerminalState::Exited, Some(3))
    );
}

#[test]
fn replay_returns_the_shells_output() {
    let (manager, out) = open("win-replay");
    // The caret keeps the echoed input from containing the output.
    manager.write("win-replay", "echo RE^PLAYED\r\n").unwrap();
    assert!(
        wait_until(SLOW, || out.text().contains("REPLAYED")),
        "output: {:?}",
        out.text()
    );

    let status = manager.status("win-replay").unwrap();
    let replay = manager.replay("win-replay", status.instance, 0).unwrap();
    assert_eq!(replay.instance, status.instance);
    assert!(replay.head_seq >= 1, "no event held");
    let bytes = STANDARD.decode(&replay.b64).unwrap();
    assert!(
        String::from_utf8_lossy(&bytes).contains("REPLAYED"),
        "replay: {:?}",
        String::from_utf8_lossy(&bytes)
    );
    manager.close_tab("win-replay");
}

/// A job the shell started in the background belongs to the terminal's job
/// object: closing the tab ends it too.
#[test]
fn close_tab_ends_a_background_job() {
    // A count no other `ping` on the machine is likely to use.
    let count = 1000 + std::process::id() % 1000;
    let marker = format!("-n {count} 127.0.0.1");
    let (manager, _out) = open("win-job");
    manager
        .write("win-job", format!("start /b ping {marker} > nul\r\n"))
        .unwrap();
    assert!(
        wait_until(SLOW, || ping_running(&marker)),
        "the background ping never started"
    );

    manager.close_tab("win-job");

    assert!(
        wait_until(SLOW, || !ping_running(&marker)),
        "the background ping survived close_tab"
    );
    assert!(manager.tab_ids().is_empty(), "tab kept");
}

/// The status tells the renderer which pseudo console it talks to.
#[test]
fn the_status_describes_the_pseudo_console() {
    let (manager, _out) = open("win-pty");
    let status = manager.status("win-pty").unwrap();
    let pty = status.windows_pty.expect("windowsPty on Windows");
    assert_eq!(pty.backend, "conpty");
    // ConPTY exists since Windows 10 1809 (build 17763).
    assert!(pty.build_number >= 17763, "build {}", pty.build_number);

    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["windowsPty"]["backend"], "conpty");
    assert_eq!(wire["windowsPty"]["buildNumber"], pty.build_number);
    manager.close_tab("win-pty");
}
