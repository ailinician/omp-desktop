//! `<app_config_dir>/widgets.json`: the only source of plugin commands.
//! A file-level problem (unreadable, not JSON, unknown version) is an
//! `Err`; a bad entry is skipped and reported in `WidgetConfig::errors`.

use std::path::Path;

pub const TIMEOUT_DEFAULT_MS: u64 = 10_000;
pub const TIMEOUT_MIN_MS: u64 = 1_000;
pub const TIMEOUT_MAX_MS: u64 = 60_000;
pub const INTERVAL_MIN_MS: u64 = 15_000;
pub const INTERVAL_MAX_MS: u64 = 3_600_000;
pub const TRIGGERS: [&str; 5] = ["activate", "turn_end", "branch", "manual", "interval"];
/// Entries past this many in `widgets.json` are not validated; one error reports them.
pub const MAX_PLUGINS: usize = 32;
const SUPPORTED_VERSION: u64 = 1;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    pub id: String,
    pub title: String,
    pub command: Vec<String>,
    pub match_path: Option<String>,
    pub match_remote: Option<String>,
    pub triggers: Vec<String>,
    pub timeout_ms: u64,
    pub interval_ms: Option<u64>,
    pub enabled: bool,
    pub scope: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct WidgetConfig {
    pub plugins: Vec<PluginEntry>,
    pub errors: Vec<String>,
}

/// Raw on-disk shape; validated into [`PluginEntry`] by [`validate`].
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    id: Option<String>,
    title: Option<String>,
    command: Option<Vec<String>>,
    #[serde(rename = "match")]
    matcher: Option<RawMatch>,
    triggers: Option<Vec<String>>,
    timeout_ms: Option<u64>,
    interval_ms: Option<u64>,
    enabled: Option<bool>,
    scope: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawMatch {
    path: Option<String>,
    remote: Option<String>,
}

/// Plugin entries stay raw JSON so one malformed entry cannot fail the file.
#[derive(serde::Deserialize)]
struct RawFile {
    version: Option<u64>,
    plugins: Option<Vec<serde_json::Value>>,
}

pub fn load(path: &Path) -> Result<WidgetConfig, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WidgetConfig::default()),
        Err(e) => return Err(format!("widgets.json: {e}")),
    };
    let file: RawFile = serde_json::from_slice(&bytes).map_err(|e| format!("widgets.json: {e}"))?;
    match file.version {
        Some(SUPPORTED_VERSION) => {}
        Some(v) => {
            return Err(format!(
                "widgets.json: version {v} is not supported (this desktop supports {SUPPORTED_VERSION})"
            ))
        }
        None => return Err("widgets.json: missing \"version\"".to_string()),
    }
    let mut out = WidgetConfig::default();
    let entries = file.plugins.unwrap_or_default();
    if entries.len() > MAX_PLUGINS {
        out.errors.push(format!(
            "widgets.json: {} plugin entries ignored (at most {MAX_PLUGINS} are read)",
            entries.len() - MAX_PLUGINS
        ));
    }
    for (i, value) in entries.into_iter().take(MAX_PLUGINS).enumerate() {
        let result = serde_json::from_value::<RawEntry>(value)
            .map_err(|e| format!("plugin #{i}: {e}"))
            .and_then(|raw| validate(raw, i, &out.plugins));
        match result {
            Ok(entry) => out.plugins.push(entry),
            Err(e) => out.errors.push(e),
        }
    }
    Ok(out)
}

/// Checks the IPC arguments of `widget_context` / `run_widget_plugin`: a known
/// trigger, and a project path that is empty (tab without project) or an
/// absolute existing directory.
pub fn validate_widget_args(path: &str, trigger: &str) -> Result<(), String> {
    if !TRIGGERS.contains(&trigger) {
        return Err("invalid trigger".to_string());
    }
    if !path.is_empty() {
        let p = Path::new(path);
        if !p.is_absolute() || !p.is_dir() {
            return Err("project path must be an absolute directory".to_string());
        }
    }
    Ok(())
}

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn home_dir() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
}

/// Replaces a leading `~/` in every argv element with `home`. Only the exact
/// `~/` prefix counts: `~user` and a bare `~` stay untouched. Fails when an
/// element needs expanding but there is no home directory.
fn expand_home(command: &mut [String], home: Option<&str>) -> Result<(), ()> {
    for arg in command {
        if let Some(rest) = arg.strip_prefix("~/") {
            let home = home.ok_or(())?;
            *arg = Path::new(home).join(rest).to_string_lossy().into_owned();
        }
    }
    Ok(())
}

fn validate(raw: RawEntry, index: usize, seen: &[PluginEntry]) -> Result<PluginEntry, String> {
    let id = raw.id.unwrap_or_default();
    if !id_ok(&id) {
        return Err(format!(
            "plugin #{index}: id \"{id}\" must match [a-z0-9-]{{1,32}}"
        ));
    }
    if seen.iter().any(|p| p.id == id) {
        return Err(format!("plugin \"{id}\": duplicate id"));
    }
    let mut command = raw.command.unwrap_or_default();
    if command.first().is_none_or(String::is_empty) {
        return Err(format!(
            "plugin \"{id}\": command must be a non-empty argv array"
        ));
    }
    expand_home(&mut command, home_dir().as_deref())
        .map_err(|()| format!("plugin \"{id}\": cannot expand ~: no home directory"))?;
    // The working directory is the untrusted project: a relative program path
    // (`./x`, `bin/x`) would resolve inside it.
    let argv0 = command.first().map_or("", String::as_str);
    if argv0.contains(['/', '\\']) && !Path::new(argv0).is_absolute() {
        return Err(format!(
            "plugin \"{id}\": command path must be absolute or a bare program name"
        ));
    }
    let mut triggers: Vec<String> = raw.triggers.map_or_else(
        || TRIGGERS.map(String::from).to_vec(),
        |list| {
            list.into_iter()
                .filter(|t| TRIGGERS.contains(&t.as_str()))
                .collect()
        },
    );
    if !triggers.iter().any(|t| t == "manual") {
        triggers.push("manual".to_string());
    }
    let timeout_ms = raw
        .timeout_ms
        .unwrap_or(TIMEOUT_DEFAULT_MS)
        .clamp(TIMEOUT_MIN_MS, TIMEOUT_MAX_MS);
    let interval_ms = raw
        .interval_ms
        .filter(|&v| v > 0)
        .map(|v| v.clamp(INTERVAL_MIN_MS, INTERVAL_MAX_MS));
    let (match_path, match_remote) = raw.matcher.map_or((None, None), |m| (m.path, m.remote));
    // `title` falls back to the id and the entry keeps both, so one copy is unavoidable.
    let title = match raw.title {
        Some(t) if !t.is_empty() => t,
        _ => id.clone(),
    };
    let scope = raw.scope.unwrap_or_else(|| "tab".to_string());
    if scope != "tab" && scope != "global" {
        return Err(format!(
            "plugin \"{id}\": scope must be \"tab\" or \"global\""
        ));
    }
    Ok(PluginEntry {
        id,
        title,
        command,
        match_path,
        match_remote,
        triggers,
        timeout_ms,
        interval_ms,
        enabled: raw.enabled.unwrap_or(true),
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("omp-widgets-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let p = dir.join("widgets.json");
        std::fs::write(&p, body).expect("write");
        p
    }

    #[test]
    fn missing_file_is_empty_config() {
        let p = std::env::temp_dir().join("omp-widgets-does-not-exist/widgets.json");
        let c = load(&p).expect("missing file is ok");
        assert!(c.plugins.is_empty());
        assert_eq!(c.errors, Vec::<String>::new());
    }

    #[test]
    fn config_rejects_invalid_json() {
        let p = tmp("bom", b"\xEF\xBB\xBF{\"version\":1,\"plugins\":[],}");
        let err = load(&p).expect_err("trailing comma / BOM must fail");
        assert!(err.contains("widgets.json"), "{err}");
    }

    #[test]
    fn config_rejects_unknown_version() {
        let p = tmp("v2", br#"{"version":2,"plugins":[]}"#);
        let err = load(&p).expect_err("version 2 unsupported");
        assert!(err.contains("version 2"), "{err}");
    }

    #[test]
    fn entry_defaults_and_clamps() {
        let p = tmp("defaults", br#"{"version":1,"plugins":[
            {"id":"git","title":"Git","command":["bash","x.sh"]},
            {"id":"slow","title":"Slow","command":["sleep"],"timeoutMs":999999,"triggers":["branch"],"enabled":false},
            {"id":"int","title":"Interval","command":["date"],"intervalMs":1000},
            {"id":"big","title":"Big","command":["date"],"intervalMs":1000000000}
        ]}"#);
        let c = load(&p).expect("ok");
        assert_eq!(c.plugins.len(), 4);
        let git = &c.plugins[0];
        assert_eq!(git.timeout_ms, TIMEOUT_DEFAULT_MS);
        assert_eq!(git.triggers, TRIGGERS.map(String::from).to_vec());
        assert!(git.enabled);
        assert_eq!(git.interval_ms, None);
        let slow = &c.plugins[1];
        assert_eq!(slow.timeout_ms, TIMEOUT_MAX_MS);
        assert_eq!(
            slow.triggers,
            vec!["branch".to_string(), "manual".to_string()]
        );
        assert!(!slow.enabled);
        let int = &c.plugins[2];
        assert_eq!(int.interval_ms, Some(INTERVAL_MIN_MS));
        let big = &c.plugins[3];
        assert_eq!(big.interval_ms, Some(INTERVAL_MAX_MS));
    }

    #[test]
    fn bad_entries_are_reported_not_fatal() {
        let p = tmp(
            "bad",
            br#"{"version":1,"plugins":[
            {"id":"Bad Id","title":"x","command":["a"]},
            {"id":"dup","title":"x","command":["a"]},
            {"id":"dup","title":"y","command":["b"]},
            {"id":"nocmd","title":"x","command":[]},
            {"id":"ok","title":"Ok","command":["true"]}
        ]}"#,
        );
        let c = load(&p).expect("ok");
        assert_eq!(
            c.plugins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["dup", "ok"]
        );
        assert_eq!(c.errors.len(), 3, "{:?}", c.errors);
        assert!(c.errors.iter().any(|e| e.contains("Bad Id")));
        assert!(c.errors.iter().any(|e| e.contains("duplicate")));
        assert!(c.errors.iter().any(|e| e.contains("nocmd")));
    }

    #[test]
    fn relative_command_paths_are_rejected() {
        let abs = if cfg!(windows) {
            "C:\\x\\run"
        } else {
            "/abs/x"
        };
        let body = format!(
            r#"{{"version":1,"plugins":[
            {{"id":"dot","command":["./x"]}},
            {{"id":"sub","command":["bin/x"]}},
            {{"id":"back","command":["bin\\x"]}},
            {{"id":"abs","command":[{abs:?}]}},
            {{"id":"bare","command":["bash"]}}
        ]}}"#
        );
        let c = load(&tmp("argv0", body.as_bytes())).expect("ok");
        assert_eq!(
            c.plugins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["abs", "bare"]
        );
        assert_eq!(c.errors.len(), 3, "{:?}", c.errors);
        assert!(
            c.errors
                .iter()
                .all(|e| e.contains("command path must be absolute or a bare program name")),
            "{:?}",
            c.errors
        );
    }

    #[test]
    fn leading_tilde_is_expanded_in_every_argv_element() {
        let mut cmd = ["~/bin/run", "~/conf.json", "~x", "~", "a~/b"].map(String::from);
        expand_home(&mut cmd, Some("/home/u")).expect("home set");
        assert_eq!(
            cmd.to_vec(),
            [
                Path::new("/home/u").join("bin/run"),
                Path::new("/home/u").join("conf.json"),
                "~x".into(),
                "~".into(),
                "a~/b".into(),
            ]
            .map(|p| p.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn tilde_without_home_is_an_error_but_plain_commands_pass() {
        let mut cmd = vec!["~/x".to_string()];
        assert!(expand_home(&mut cmd, None).is_err());
        let mut plain = vec!["bash".to_string(), "~x".to_string()];
        assert!(expand_home(&mut plain, None).is_ok());
    }

    #[test]
    fn tilde_command_passes_the_absolute_check() {
        let Some(home) = home_dir() else { return };
        let body = br#"{"version":1,"plugins":[
            {"id":"t","command":["~/.omp-desktop/widgets/x","~/arg"]},
            {"id":"u","command":["~x"]}
        ]}"#;
        let c = load(&tmp("tilde", body)).expect("ok");
        let expected = |rest: &str| Path::new(&home).join(rest).to_string_lossy().into_owned();
        assert_eq!(c.plugins.len(), 2, "{:?}", c.errors);
        assert_eq!(
            c.plugins[0].command,
            [expected(".omp-desktop/widgets/x"), expected("arg")]
        );
        // `~x` is left alone: a bare program name.
        assert_eq!(c.plugins[1].command, ["~x"]);
    }

    #[test]
    fn plugin_count_is_capped() {
        let entries: Vec<String> = (0..=MAX_PLUGINS)
            .map(|i| format!(r#"{{"id":"p{i}","command":["true"]}}"#))
            .collect();
        let body = format!(r#"{{"version":1,"plugins":[{}]}}"#, entries.join(","));
        let c = load(&tmp("cap", body.as_bytes())).expect("ok");
        assert_eq!(c.plugins.len(), MAX_PLUGINS);
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert!(
            c.errors[0].contains("1 plugin entries ignored"),
            "{:?}",
            c.errors
        );
    }

    #[test]
    fn widget_args_are_validated() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().expect("utf-8 temp dir");
        assert_eq!(validate_widget_args(dir, "manual"), Ok(()));
        assert_eq!(validate_widget_args("", "turn_end"), Ok(()), "no project");
        assert_eq!(
            validate_widget_args(dir, "rm -rf"),
            Err("invalid trigger".to_string())
        );
        let msg = Err("project path must be an absolute directory".to_string());
        assert_eq!(validate_widget_args("relative/dir", "manual"), msg);
        // "." exists, so only the absoluteness check rejects it.
        assert_eq!(validate_widget_args(".", "manual"), msg);
        assert_eq!(
            validate_widget_args(&format!("{dir}/omp-widgets-no-such-dir"), "manual"),
            msg
        );
    }

    #[test]
    fn bad_entry_types_are_isolated() {
        let p = tmp(
            "types",
            br#"{"version":1,"plugins":[
            {"id":"bad","command":"true"},
            {"id":"ok","command":["true"]},
            {"id":"x","title":5,"command":["a"]}
        ]}"#,
        );
        let c = load(&p).expect("entry type errors are not fatal");
        assert_eq!(
            c.plugins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["ok"]
        );
        assert_eq!(c.errors.len(), 2, "{:?}", c.errors);
        assert!(c.errors.iter().any(|e| e.contains("#0")), "{:?}", c.errors);
        assert!(c.errors.iter().any(|e| e.contains("#2")), "{:?}", c.errors);
    }

    #[test]
    fn scope_defaults_to_tab_and_rejects_unknown_values() {
        let p = tmp(
            "scope",
            br#"{"version":1,"plugins":[
            {"id":"a","command":["true"]},
            {"id":"b","command":["true"],"scope":"global"},
            {"id":"c","command":["true"],"scope":"tab"},
            {"id":"d","command":["true"],"scope":"window"}
        ]}"#,
        );
        let c = load(&p).expect("ok");
        let scopes: Vec<_> = c
            .plugins
            .iter()
            .map(|p| (p.id.as_str(), p.scope.as_str()))
            .collect();
        assert_eq!(scopes, vec![("a", "tab"), ("b", "global"), ("c", "tab")]);
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert!(
            c.errors[0].contains("\"d\"") && c.errors[0].contains("scope"),
            "{:?}",
            c.errors
        );
    }
}
