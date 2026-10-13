//! What the webview sees: the shapes the `terminal_*` commands return and the
//! `terminal://` events ([`TauriEmit`]).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::pump::{Emit, TerminalEmit};

/// [`Emit`] over Tauri events.
pub struct TauriEmit(pub AppHandle);

/// One shell's `terminal://data/{id}` and `terminal://exit/{id}` events: the
/// names are built once per shell and one base64 buffer is reused per event.
pub struct TauriTerminal {
    app: AppHandle,
    data_event: String,
    exit_event: String,
    instance: u64,
    b64: String,
}

#[derive(Serialize, Clone)]
struct DataPayload<'a> {
    instance: u64,
    seq: u64,
    b64: &'a str,
}

#[derive(Serialize, Clone)]
struct ExitPayload {
    instance: u64,
    code: Option<i32>,
}

impl Emit for TauriEmit {
    type Terminal = TauriTerminal;

    fn terminal(self, id: &str, instance: u64) -> TauriTerminal {
        TauriTerminal {
            app: self.0,
            data_event: format!("terminal://data/{id}"),
            exit_event: format!("terminal://exit/{id}"),
            instance,
            b64: String::new(),
        }
    }
}

impl TerminalEmit for TauriTerminal {
    fn data(&mut self, seq: u64, bytes: &[u8]) {
        self.b64.clear();
        STANDARD.encode_string(bytes, &mut self.b64);
        let _ = self.app.emit(
            &self.data_event,
            DataPayload {
                instance: self.instance,
                seq,
                b64: &self.b64,
            },
        );
    }

    fn exit(&mut self, code: Option<i32>) {
        let _ = self.app.emit(
            &self.exit_event,
            ExitPayload {
                instance: self.instance,
                code,
            },
        );
    }
}

/// A tab's shell as the webview sees it.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStatus {
    pub instance: u64,
    /// The program, for display.
    pub shell: String,
    pub state: TerminalState,
    /// Set once exited; `None` there = killed by a signal.
    pub exit_code: Option<i32>,
    /// The pseudo console xterm.js has to adapt to (Windows only, `null`
    /// elsewhere): see [`WindowsPty`].
    pub windows_pty: Option<WindowsPty>,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TerminalState {
    Starting,
    Running,
    Exited,
}

/// The output a shell still holds, for a webview that missed events.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReplayWire {
    /// The shell the bytes are from (the current one).
    pub instance: u64,
    pub b64: String,
    /// `seq` of the last event included (0 when none is held).
    pub head_seq: u64,
    /// Some requested output was already evicted.
    pub dropped: bool,
}

/// What xterm.js's `windowsPty` option wants to know: the pseudo console
/// rewrites the screen itself and its behaviour (reflow, wrapped-line
/// handling) changed across Windows builds, so the renderer adjusts by build
/// number.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WindowsPty {
    /// Always `"conpty"` (`portable-pty` has no winpty backend).
    pub backend: &'static str,
    /// The OS build (`CurrentBuildNumber`), e.g. 22631.
    pub build_number: u32,
}

/// The first Windows build with a pseudo console (Windows 10 1809).
#[cfg(any(windows, test))]
const CONPTY_MIN_BUILD: u32 = 17763;

/// The pseudo console of this system: read once from the registry
/// (`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\CurrentBuildNumber`)
/// and cached. `None` when the build number cannot be read.
#[cfg(windows)]
static WINDOWS_PTY: std::sync::LazyLock<Option<WindowsPty>> = std::sync::LazyLock::new(|| {
    let build = windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()?
        .get_string("CurrentBuildNumber")
        .ok()?;
    Some(WindowsPty {
        backend: "conpty",
        build_number: build.trim().parse().ok()?,
    })
});

#[cfg(windows)]
pub fn windows_pty() -> Option<WindowsPty> {
    *WINDOWS_PTY
}

/// No pseudo console to describe off Windows.
#[cfg(not(windows))]
pub const fn windows_pty() -> Option<WindowsPty> {
    None
}

/// Refuse to spawn on a Windows without a pseudo console (before build 17763):
/// `portable-pty` panics there, which aborts the app. An unreadable build
/// number is let through — there is nothing to judge by.
#[cfg(any(windows, test))]
pub fn require_conpty(pty: Option<WindowsPty>) -> Result<(), String> {
    match pty {
        Some(pty) if pty.build_number < CONPTY_MIN_BUILD => Err(format!(
            "the terminal needs Windows 10 version 1809 (build {CONPTY_MIN_BUILD}) or later; \
             this is build {}",
            pty.build_number
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_without_conpty_is_refused() {
        let build = |build_number| {
            Some(WindowsPty {
                backend: "conpty",
                build_number,
            })
        };
        let refused = require_conpty(build(17134)).unwrap_err();
        assert!(
            refused.contains("17763") && refused.contains("17134"),
            "{refused}"
        );
        assert_eq!(require_conpty(build(17763)), Ok(()));
        assert_eq!(require_conpty(build(22631)), Ok(()));
        assert_eq!(require_conpty(None), Ok(()), "unknown build");
    }
}
