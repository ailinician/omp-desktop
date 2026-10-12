//! `<app_config_dir>/widgets.json`: the only source of plugin commands.
//! A file-level problem (unreadable, not JSON, unknown version) is an
//! `Err`; a bad entry is skipped and reported in `WidgetConfig::errors`.

use std::path::Path;

pub const TIMEOUT_DEFAULT_MS: u64 = 10_000;
pub const TIMEOUT_MIN_MS: u64 = 1_000;
pub const TIMEOUT_MAX_MS: u64 = 60_000;
pub const TRIGGERS: [&str; 4] = ["activate", "turn_end", "branch", "manual"];
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
    pub enabled: bool,
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
    enabled: Option<bool>,
}

#[derive(serde::Deserialize)]
struct RawMatch {
    path: Option<String>,
    remote: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawFile {
    version: Option<u64>,
    plugins: Option<Vec<RawEntry>>,
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
    for (i, raw) in file.plugins.unwrap_or_default().into_iter().enumerate() {
        match validate(raw, i, &out.plugins) {
            Ok(entry) => out.plugins.push(entry),
            Err(e) => out.errors.push(e),
        }
    }
    Ok(out)
}

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
    let command = raw.command.unwrap_or_default();
    if command.first().is_none_or(String::is_empty) {
        return Err(format!(
            "plugin \"{id}\": command must be a non-empty argv array"
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
    let (match_path, match_remote) = raw.matcher.map_or((None, None), |m| (m.path, m.remote));
    // `title` falls back to the id and the entry keeps both, so one copy is unavoidable.
    let title = match raw.title {
        Some(t) if !t.is_empty() => t,
        _ => id.clone(),
    };
    Ok(PluginEntry {
        id,
        title,
        command,
        match_path,
        match_remote,
        triggers,
        timeout_ms,
        enabled: raw.enabled.unwrap_or(true),
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
            {"id":"slow","title":"Slow","command":["sleep"],"timeoutMs":999999,"triggers":["branch"],"enabled":false}
        ]}"#);
        let c = load(&p).expect("ok");
        assert_eq!(c.plugins.len(), 2);
        let git = &c.plugins[0];
        assert_eq!(git.timeout_ms, TIMEOUT_DEFAULT_MS);
        assert_eq!(git.triggers, TRIGGERS.map(String::from).to_vec());
        assert!(git.enabled);
        let slow = &c.plugins[1];
        assert_eq!(slow.timeout_ms, TIMEOUT_MAX_MS);
        assert_eq!(
            slow.triggers,
            vec!["branch".to_string(), "manual".to_string()]
        );
        assert!(!slow.enabled);
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
}
