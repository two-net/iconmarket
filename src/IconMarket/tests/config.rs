//! The config file seen from outside: the real binary, a real file and a real environment.

use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_icon-market");

const SECRETS: [&str; 4] = [
    "line-secret-value",
    "line-token-value",
    "ai-key-value",
    "db-password-value",
];

/// A valid file whose database is at 127.0.0.1:`database_port`.
fn config(database_port: u16) -> String {
    format!(
        r#"
[app]
baseUrl = "https://market.example.com"

[village]
name = "Test village"

[line]
channelSecret = "line-secret-value"
channelAccessToken = "line-token-value"

[ai]
provider = "claude"
model = "a-model"
apiKey = "ai-key-value"

[database]
host = "127.0.0.1"
port = {database_port}
name = "from-file"
user = "from-file"
password = "db-password-value"
sslMode = "disable"

[privacy]
noticeVersion = "1"
noticeText = "Notice"

[admin]
lineUserIds = ["U0123456789abcdef0123456789abcdef"]
"#
    )
}

/// Writes `config` to a fresh directory and builds the command that runs the app on it,
/// from that directory and with `env` added to the environment.
fn command(config: &str, env: &[(&str, &str)]) -> (TempDir, Command) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, config).unwrap();
    let mut command = Command::new(BIN);
    command
        .arg(&path)
        .current_dir(dir.path())
        .envs(env.iter().copied());
    (dir, command)
}

fn run(config: &str, env: &[(&str, &str)]) -> (Output, String) {
    let (_dir, mut command) = command(config, env);
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, stderr)
}

fn assert_no_secrets(output: &Output) {
    let all = [&output.stdout[..], &output.stderr[..]].concat();
    let all = String::from_utf8_lossy(&all);
    for secret in SECRETS {
        assert!(!all.contains(secret), "{all}");
    }
}

/// AC-09
#[test]
fn missing_password_stops_startup_naming_the_key() {
    let config = config(1).replace(r#"password = "db-password-value""#, "");
    let (output, stderr) = run(
        &config,
        &[
            ("DATABASE_URL", "postgres://env:env-password@127.0.0.1/env"),
            ("PGPASSWORD", "env-password"),
        ],
    );
    assert!(!output.status.success());
    assert!(stderr.contains("database.password: required"), "{stderr}");
    assert!(!stderr.contains("env-password"), "{stderr}");
    assert_no_secrets(&output);
}

#[test]
fn several_mistakes_are_reported_together() {
    let config = config(1)
        .replace("https://market.example.com", "http://market.example.com")
        .replace(r#"name = "Test village""#, "")
        .replace(r#"password = "db-password-value""#, "password = 98765432")
        .replace("[ai]", "[ai]\ntemperature = 5");
    let (output, stderr) = run(&config, &[]);
    assert!(!output.status.success());
    for key in [
        "app.baseUrl",
        "village.name",
        "ai.temperature",
        "database.password",
    ] {
        assert!(stderr.contains(&format!("  - {key}: ")), "{stderr}");
    }
    assert!(!stderr.contains("98765432"), "{stderr}");
    // The logger never started
    assert!(output.stdout.is_empty());
}

/// AC-10: database settings in the environment are ignored; the app connects where the
/// file says. Two local listeners stand in for the database, so no PostgreSQL is needed.
#[test]
fn database_settings_come_only_from_the_file() {
    let from_file = listener();
    let from_env = listener();
    let file_port = from_file.local_addr().unwrap().port();
    let env_port = from_env.local_addr().unwrap().port().to_string();
    let env_url = format!("postgres://env:env-password@127.0.0.1:{env_port}/env");

    let (_dir, mut command) = command(
        &config(file_port),
        &[
            ("DATABASE_URL", env_url.as_str()),
            ("PGHOST", "127.0.0.1"),
            ("PGPORT", env_port.as_str()),
            ("PGUSER", "env"),
            ("PGPASSWORD", "env-password"),
            ("PGDATABASE", "env"),
        ],
    );
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Hang up on every connection, so the app fails to connect and exits
    let (mut file_hits, mut env_hits) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        file_hits += hang_up_all(&from_file);
        env_hits += hang_up_all(&from_env);
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the app did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    file_hits += hang_up_all(&from_file);
    env_hits += hang_up_all(&from_env);

    assert!(file_hits > 0, "the app never connected where the file says");
    assert_eq!(env_hits, 0, "the app connected where the environment says");
}

fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

/// Accepts and drops every pending connection, returning how many there were.
fn hang_up_all(listener: &TcpListener) -> usize {
    let mut count = 0;
    loop {
        match listener.accept() {
            Ok(_) => count += 1,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return count,
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}
