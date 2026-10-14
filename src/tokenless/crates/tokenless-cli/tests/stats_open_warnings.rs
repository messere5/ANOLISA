//! A locked stats database must not become a silent stats loss.
//!
//! The 0.8.5 fail-soft contract (CHANGELOG, PR #3587) promises that failed
//! stats and retrieval-event writes emit a `[tokenless-stats]` warning while
//! compression and retrieval continue. The recorder-open failure path broke
//! that promise: every CLI entry point degraded to `None` (or skipped the
//! `let Ok(recorder)` chain) without a word on stderr, so a database held by
//! another process (a backup, a migration, `BEGIN EXCLUSIVE`) silently
//! dropped rows. These tests hold the SQLite write lock and require the
//! warning while the command itself keeps succeeding (issue #7285).

use std::process::Command;

use tokenless_ccr::extract_hash;
use tokenless_stats::get_home_dir;

fn tokenless_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tokenless"))
}

struct TempDataDir {
    _temp_dir: tempfile::TempDir,
    data_dir: std::path::PathBuf,
}

impl TempDataDir {
    fn new() -> Option<Self> {
        let home = get_home_dir();
        let temp_dir = tempfile::Builder::new()
            .prefix("tokenless-stats-open-warnings-")
            .tempdir()
            .ok()?;
        let root = temp_dir.path().to_path_buf();
        // The data directory must sit outside the passwd-backed home so the
        // file-level override validation cannot quietly re-route the stats
        // database to `~/.tokenless/stats.db` and break the assertions.
        if !home.is_empty()
            && root
                .canonicalize()
                .ok()?
                .starts_with(std::path::Path::new(&home).canonicalize().ok()?)
        {
            return None;
        }
        let data_dir = root.join("databases");
        Some(Self {
            _temp_dir: temp_dir,
            data_dir,
        })
    }

    fn command(&self) -> Command {
        let mut command = tokenless_bin();
        command
            .env("TOKENLESS_DATA_DIR", &self.data_dir)
            .env_remove("TOKENLESS_STATS_DB")
            .env_remove("TOKENLESS_STASH_DB");
        command
    }
}

/// Holds the SQLite write lock (`BEGIN EXCLUSIVE`) for as long as the guard
/// lives, standing in for any external process that pins the stats database.
struct ExclusiveLock {
    _conn: rusqlite::Connection,
}

impl ExclusiveLock {
    fn new(path: &std::path::Path) -> Self {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        Self { _conn: conn }
    }
}

fn stats_rows(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM stats", [], |row| row.get(0))
        .unwrap()
}

fn spawn_with_stdin(
    command: &mut Command,
    args: &[&str],
    stdin_text: &str,
) -> std::process::Output {
    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin_text.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap()
}

fn post_tool_request_json(content: &str, recovery_available: bool, session_id: &str) -> String {
    serde_json::json!({
        "protocol_version": 2,
        "operation": "post_tool",
        "attribution": {
            "agent_id": "integration-agent",
            "session_id": session_id,
            "tool_use_id": "call-1"
        },
        "input": {
            "result_kind": "tool",
            "tool_name": "WebFetch",
            "content": content,
            "status": "success",
            "content_origin": "api_response",
            "output_optimization": "none",
            "capabilities": {
                "replace_output": true,
                "recovery": {"kind": if recovery_available { "shell" } else { "none" }},
                "replace_with_text": false
            }
        }
    })
    .to_string()
}

fn records_content() -> String {
    format!(
        r#"{{"records":[{}]}}"#,
        (0..200)
            .map(|i| format!(r#"{{"id":{i},"name":"row-{i}"}}"#))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn compression_env(command: &mut Command) -> &mut Command {
    command
        .env("TOKENLESS_COMPRESSION_ENABLED", "1")
        .env("TOKENLESS_STATS_ENABLED", "1")
        .env("TOKENLESS_SLS_ENABLED", "0")
}

/// The `compress-response` subcommand: the stats row of a compression with
/// real savings must be either written or announced as lost, never dropped
/// silently because the recorder could not be opened.
#[test]
fn locked_stats_db_warns_on_the_compression_subcommand() {
    let fixture = match TempDataDir::new() {
        Some(fixture) => fixture,
        None => return,
    };
    let stats_db = fixture.data_dir.join("stats.db");
    let response = serde_json::to_string(&serde_json::json!({"tail": "世界".repeat(300)})).unwrap();
    let args = [
        "compress-response",
        "--truncate-strings-at",
        "120",
        "--no-stash",
        "--session-id",
        "locked-stats",
    ];

    let seed = spawn_with_stdin(compression_env(&mut fixture.command()), &args, &response);
    assert!(
        seed.status.success(),
        "seed compression failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    assert_eq!(stats_rows(&stats_db), 1);

    let lock = ExclusiveLock::new(&stats_db);
    let output = spawn_with_stdin(compression_env(&mut fixture.command()), &args, &response);
    assert!(
        output.status.success(),
        "compression must stay fail-soft while the stats database is locked: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[tokenless-stats] WARNING: failed to open stats recorder"),
        "a locked stats database must surface the promised warning, stderr: {stderr}"
    );
    drop(lock);
    assert_eq!(
        stats_rows(&stats_db),
        1,
        "the locked run cannot write a row; it must only be audible about the loss"
    );
}

/// The hook dispatch path (`tokenless compress` on stdin): the recorder is
/// opened before dispatch, and an open failure must warn while the response
/// is still produced.
#[test]
fn locked_stats_db_warns_on_the_hook_dispatch() {
    let fixture = match TempDataDir::new() {
        Some(fixture) => fixture,
        None => return,
    };
    let stats_db = fixture.data_dir.join("stats.db");
    let request = post_tool_request_json(&records_content(), true, "locked-dispatch");

    let seed = spawn_with_stdin(
        compression_env(&mut fixture.command()),
        &["compress"],
        &request,
    );
    assert!(
        seed.status.success(),
        "seed dispatch failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    assert_eq!(stats_rows(&stats_db), 1);

    let lock = ExclusiveLock::new(&stats_db);
    let output = spawn_with_stdin(
        compression_env(&mut fixture.command()),
        &["compress"],
        &request,
    );
    assert!(
        output.status.success(),
        "dispatch must stay fail-soft while the stats database is locked: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("dispatch response must stay valid JSON");
    assert!(
        response["result"]["output"].as_str().is_some(),
        "dispatch must still replace the tool output: {response}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[tokenless-stats] WARNING: failed to open stats recorder"),
        "a locked stats database must surface the promised warning, stderr: {stderr}"
    );
    drop(lock);
    assert_eq!(stats_rows(&stats_db), 1);
}

/// The retrieve path: retrieval events are attribution, never a gate, but
/// losing them to a locked database must still be audible (issue #7285's
/// retrieval leg).
#[test]
fn locked_stats_db_warns_on_retrieve() {
    let fixture = match TempDataDir::new() {
        Some(fixture) => fixture,
        None => return,
    };
    let stats_db = fixture.data_dir.join("stats.db");
    let request = post_tool_request_json(&records_content(), true, "locked-retrieve");

    let seed = spawn_with_stdin(
        compression_env(&mut fixture.command()),
        &["compress"],
        &request,
    );
    assert!(
        seed.status.success(),
        "seed dispatch failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let seed_response: serde_json::Value = serde_json::from_slice(&seed.stdout).unwrap();
    let marker = extract_hash(seed_response["result"]["output"].as_str().unwrap())
        .expect("array truncation should emit a recovery instruction");
    assert_eq!(stats_rows(&stats_db), 1);

    let lock = ExclusiveLock::new(&stats_db);
    let hit = fixture
        .command()
        .env("TOKENLESS_STATS_ENABLED", "1")
        .args(["retrieve", marker])
        .output()
        .unwrap();
    assert!(
        hit.status.success(),
        "retrieval must stay fail-soft while the stats database is locked: {}",
        String::from_utf8_lossy(&hit.stderr)
    );
    let stderr = String::from_utf8_lossy(&hit.stderr);
    assert!(
        stderr.contains("[tokenless-stats] WARNING: failed to open stats recorder"),
        "a locked stats database must surface the promised warning, stderr: {stderr}"
    );
    drop(lock);
    assert_eq!(
        stats_rows(&stats_db),
        1,
        "the locked retrieve cannot record its event; it must only be audible about the loss"
    );
}
