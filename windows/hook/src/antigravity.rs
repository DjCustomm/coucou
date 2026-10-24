//! Antigravity's permission settings (`~/.gemini/config/config.json` and
//! `~/.gemini/config/projects/*.json`) so the relay only promotes `PreToolUse`
//! to `PermissionRequest` when Antigravity itself would ask the user.
//!
//! When Antigravity is in Turbo mode (`AGENT_PERMISSION_PRESET_TURBO` or
//! `CASCADE_COMMANDS_AUTO_EXECUTION_EAGER`), or when a `run_command` matches a
//! saved `command(...)` grant, the relay leaves the event as a non-blocking
//! `PreToolUse` and immediately prints `{"decision":"ask"}` — which Antigravity
//! evaluates against its own Turbo / Always Allow rules and runs without
//! prompting.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Tools that write or edit files on disk.
const FILE_EDIT_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "write_to_file",
    "replace_file_content",
    "multi_replace_file_content",
];

/// True when a `PreToolUse` event from Antigravity would prompt the user and
/// should therefore be shown as an Allow/Deny card in the island.
pub fn needs_approval(map: &Map<String, Value>, home: Option<&Path>) -> bool {
    let Some(tool) = map.get("tool_name").and_then(Value::as_str) else {
        return false;
    };
    let input = map.get("tool_input").and_then(Value::as_object);
    let workspaces = workspace_paths(map);
    let settings = load_settings(home, &workspaces);

    // Turbo mode auto-executes commands and standard tools without asking.
    if settings.is_turbo() {
        return false;
    }

    if tool == "run_command" {
        let cmd = input
            .and_then(|i| i.get("command").or_else(|| i.get("CommandLine")))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if cmd.is_empty() {
            return false;
        }
        return !settings.allows_command(cmd);
    }

    if FILE_EDIT_TOOLS.contains(&tool) {
        if settings.allows_non_workspace_files() || workspaces.is_empty() {
            return false;
        }
        let target = input
            .and_then(|i| {
                i.get("file_path")
                    .or_else(|| i.get("path"))
                    .or_else(|| i.get("Path"))
                    .or_else(|| i.get("FilePath"))
                    .or_else(|| i.get("TargetFile"))
            })
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if target.is_empty() {
            return false;
        }
        return !is_inside_workspaces(target, &workspaces);
    }

    false
}

#[derive(Default)]
struct Settings {
    preset: String,
    auto_execution: String,
    non_workspace_policy: String,
    allow_grants: Vec<String>,
}

impl Settings {
    fn is_turbo(&self) -> bool {
        self.preset == "AGENT_PERMISSION_PRESET_TURBO"
            || self.auto_execution == "CASCADE_COMMANDS_AUTO_EXECUTION_EAGER"
    }

    fn allows_non_workspace_files(&self) -> bool {
        self.non_workspace_policy == "AGENT_SETTING_POLICY_ALLOW"
    }

    fn allows_command(&self, cmd: &str) -> bool {
        let cmd_norm = cmd.trim();
        self.allow_grants.iter().any(|grant| {
            let Some(pattern) = grant.strip_prefix("command(").and_then(|s| s.strip_suffix(')')) else {
                return false;
            };
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return false;
            }
            if pattern == "*" {
                return true;
            }
            if let Some(prefix) = pattern.strip_suffix('*') {
                let prefix = prefix.trim_end();
                return cmd_norm == prefix || cmd_norm.starts_with(prefix);
            }
            // Antigravity `command(git status)` grants exact match or subcommand match (`git status -s`).
            cmd_norm == pattern
                || cmd_norm
                    .strip_prefix(pattern)
                    .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        })
    }
}

fn load_settings(home: Option<&Path>, workspaces: &[String]) -> Settings {
    let mut out = Settings::default();
    let Some(home) = home else {
        return out;
    };
    let config_dir = home.join(".gemini").join("config");

    // 1. Global userSettings in ~/.gemini/config/config.json
    if let Some(root) = read_json(&config_dir.join("config.json")) {
        if let Some(user) = root.get("userSettings").and_then(Value::as_object) {
            apply_settings_block(&mut out, user, "globalPermissionGrants");
        }
    }

    // 2. Per-project overrides in ~/.gemini/config/projects/*.json
    if !workspaces.is_empty() {
        if let Ok(entries) = std::fs::read_dir(config_dir.join("projects")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Some(proj) = read_json(&path) else {
                    continue;
                };
                if project_matches_workspaces(&proj, workspaces) {
                    if let Some(agent) = proj.get("agentSettings").and_then(Value::as_object) {
                        apply_settings_block(&mut out, agent, "permissionGrants");
                    }
                }
            }
        }
    }

    out
}

fn apply_settings_block(out: &mut Settings, block: &Map<String, Value>, grants_key: &str) {
    if let Some(s) = block.get("permissionPreset").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        out.preset = s.to_string();
    }
    if let Some(s) = block.get("autoExecutionPolicy").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        out.auto_execution = s.to_string();
    }
    if let Some(s) = block
        .get("nonWorkspaceFileAccessPolicy")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        out.non_workspace_policy = s.to_string();
    }
    if let Some(allow) = block
        .get(grants_key)
        .and_then(Value::as_object)
        .and_then(|g| g.get("allow"))
        .and_then(Value::as_array)
    {
        for item in allow.iter().filter_map(Value::as_str) {
            out.allow_grants.push(item.to_string());
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn workspace_paths(map: &Map<String, Value>) -> Vec<String> {
    let mut paths: Vec<String> = ["workspacePaths", "workspace_roots"]
        .iter()
        .filter_map(|k| map.get(*k).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if paths.is_empty() {
        if let Some(cwd) = map.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            paths.push(cwd.to_string());
        }
    }
    paths
}

fn project_matches_workspaces(proj: &Value, workspaces: &[String]) -> bool {
    let mut uris = Vec::new();
    if let Some(resources) = proj
        .get("projectResources")
        .and_then(|r| r.get("resources"))
        .and_then(Value::as_array)
    {
        for res in resources {
            if let Some(u) = res.get("folderUri").and_then(Value::as_str) {
                uris.push(u);
            }
        }
    }
    if let Some(u) = proj.get("gitFolder").and_then(|g| g.get("folderUri")).and_then(Value::as_str) {
        uris.push(u);
    }
    uris.into_iter().any(|uri| {
        let Some(folder) = uri_to_path(uri) else {
            return false;
        };
        let norm_folder = normalize_path(&folder);
        workspaces.iter().any(|w| normalize_path(w) == norm_folder)
    })
}

fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    // Windows file URI: file:///c:/Users/... -> /c:/Users/... -> c:/Users/...
    let trimmed = if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        &decoded[1..]
    } else {
        &decoded
    };
    Some(trimmed.to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalizes a filesystem path for prefix comparisons across `/` and `\`.
pub fn normalize_path(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) || (s.len() >= 2 && s.as_bytes()[1] == b':') {
        s.make_ascii_lowercase();
    }
    s
}

/// True when `target` is inside any of the workspace roots (or is a relative path
/// without `..` traversal).
pub fn is_inside_workspaces(target: &str, workspaces: &[String]) -> bool {
    let norm = normalize_path(target);
    if norm.is_empty() {
        return true;
    }
    let is_abs = norm.starts_with('/')
        || (norm.len() >= 2 && norm.as_bytes()[0].is_ascii_alphabetic() && norm.as_bytes()[1] == b':');
    if !is_abs {
        return !norm.split('/').any(|seg| seg == "..");
    }
    workspaces.iter().any(|w| {
        let root = normalize_path(w);
        !root.is_empty()
            && (norm == root
                || norm.strip_prefix(&root).is_some_and(|rest| rest.starts_with('/')))
    })
}

/// User's home directory from environment variables (`USERPROFILE` or `HOME`).
pub fn home_from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("USERPROFILE")
        .or_else(|| env("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_home(name: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("coucou-agy-test-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gemini").join("config").join("projects")).unwrap();
        dir
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn turbo_mode_never_prompts_in_the_island() {
        let home = temp_home("turbo");
        std::fs::write(
            home.join(".gemini/config/config.json"),
            r#"{"userSettings":{"permissionPreset":"AGENT_PERMISSION_PRESET_TURBO"}}"#,
        )
        .unwrap();
        let payload = map(json!({
            "tool_name": "run_command",
            "tool_input": { "command": "cargo test" },
            "workspacePaths": ["C:\\Users\\me\\proj"]
        }));
        assert!(!needs_approval(&payload, Some(&home)));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn non_turbo_prompts_for_run_command_unless_allowlisted() {
        let home = temp_home("allowlist");
        std::fs::write(
            home.join(".gemini/config/config.json"),
            r#"{
                "userSettings": {
                    "permissionPreset": "AGENT_PERMISSION_PRESET_DEFAULT",
                    "globalPermissionGrants": {
                        "allow": ["command(git status)", "command(npm test*)"]
                    }
                }
            }"#,
        )
        .unwrap();

        let ask = map(json!({
            "tool_name": "run_command",
            "tool_input": { "command": "rm -rf build" },
            "workspacePaths": ["C:\\Users\\me\\proj"]
        }));
        assert!(needs_approval(&ask, Some(&home)));

        let allowed_exact = map(json!({
            "tool_name": "run_command",
            "tool_input": { "command": "git status -s" },
            "workspacePaths": ["C:\\Users\\me\\proj"]
        }));
        assert!(!needs_approval(&allowed_exact, Some(&home)));

        let allowed_glob = map(json!({
            "tool_name": "run_command",
            "tool_input": { "command": "npm test -- --watch" },
            "workspacePaths": ["C:\\Users\\me\\proj"]
        }));
        assert!(!needs_approval(&allowed_glob, Some(&home)));

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn project_settings_override_global_preset_and_add_grants() {
        let home = temp_home("project");
        std::fs::write(
            home.join(".gemini/config/config.json"),
            r#"{"userSettings":{"permissionPreset":"AGENT_PERMISSION_PRESET_TURBO"}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".gemini/config/projects/proj1.json"),
            r#"{
                "projectId": "proj1",
                "projectResources": {
                    "resources": [{ "folderUri": "file:///c%3A/Users/me/strict-proj" }]
                },
                "agentSettings": {
                    "permissionPreset": "AGENT_PERMISSION_PRESET_DEFAULT",
                    "permissionGrants": { "allow": ["command(cargo check)"] }
                }
            }"#,
        )
        .unwrap();

        let in_strict = map(json!({
            "tool_name": "run_command",
            "tool_input": { "command": "cargo build" },
            "workspacePaths": ["C:\\Users\\me\\strict-proj"]
        }));
        assert!(needs_approval(&in_strict, Some(&home)));

        let granted_in_strict = map(json!({
            "tool_name": "run_command",
            "tool_input": { "command": "cargo check" },
            "workspacePaths": ["C:\\Users\\me\\strict-proj"]
        }));
        assert!(!needs_approval(&granted_in_strict, Some(&home)));

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn workspace_file_edits_pass_through_and_outside_edits_prompt() {
        let home = temp_home("files");
        std::fs::write(
            home.join(".gemini/config/config.json"),
            r#"{"userSettings":{"permissionPreset":"AGENT_PERMISSION_PRESET_DEFAULT"}}"#,
        )
        .unwrap();

        let inside = map(json!({
            "tool_name": "write_file",
            "tool_input": { "path": "C:\\Users\\me\\proj\\src\\lib.rs" },
            "workspacePaths": ["C:\\Users\\me\\proj"]
        }));
        assert!(!needs_approval(&inside, Some(&home)));

        let outside = map(json!({
            "tool_name": "edit_file",
            "tool_input": { "path": "C:\\Users\\me\\other\\secret.txt" },
            "workspacePaths": ["C:\\Users\\me\\proj"]
        }));
        assert!(needs_approval(&outside, Some(&home)));

        let _ = std::fs::remove_dir_all(home);
    }
}
