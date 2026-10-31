//! Startup against a real database (§5.3, NFR-MNT-06): the real binary, a real config file and
//! a throwaway PostgreSQL, so these need Docker.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use tempfile::TempDir;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

const BIN: &str = env!("CARGO_BIN_EXE_icon-market");

/// How long the app may take to start or to stop.
const PATIENCE: Duration = Duration::from_secs(60);

/// A throwaway PostgreSQL, and the host and port it listens on.
async fn postgres() -> (ContainerAsync<Postgres>, String, u16) {
    let container = Postgres::default().with_tag("18").start().await.expect(
        "cannot start PostgreSQL; is Docker running at DOCKER_HOST or /var/run/docker.sock?",
    );
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (container, host, port)
}

/// The database as its superuser sees it.
async fn connect(host: &str, port: u16) -> PgPool {
    let options = PgConnectOptions::new_without_pgpass()
        .host(host)
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres")
        .ssl_mode(PgSslMode::Disable);
    PgPool::connect_with(options).await.unwrap()
}

/// An admin's LINE user ID.
const ADMIN: &str = "U0123456789abcdef0123456789abcdef";

/// A valid file for the app on `listen_port`, connecting to the database at `host`:`port` as
/// `user`, whose password is their name, with `admins` as the admins.
fn config(listen_port: u16, host: &str, port: u16, user: &str, admins: &[&str]) -> String {
    let admins = admins
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
[app]
baseUrl = "https://market.example.com"
listenPort = {listen_port}

[village]
name = "Test village"

[line]
channelSecret = "line-secret-value"
channelAccessToken = "line-token-value"

[ai]
enabled = false

[database]
host = "{host}"
port = {port}
name = "postgres"
user = "{user}"
password = "{user}"
sslMode = "disable"

[catalog]
defaultCategories = ["อาหาร", "เครื่องดื่ม"]

[privacy]
noticeVersion = "1"
noticeText = "Notice"

[admin]
lineUserIds = [{admins}]
"#
    )
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// The status `GET /health` answers with, or `None` while nothing listens.
fn health(port: u16) -> Option<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response.split(' ').nth(1)?.parse().ok()
}

/// The app, running from a fresh directory on a config file. Killed if dropped while running.
struct App {
    child: Child,
    port: u16,
    stdout: Option<JoinHandle<String>>,
    _dir: TempDir,
}

impl App {
    fn start(port: u16, config: &str) -> App {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, config).unwrap();
        let mut child = Command::new(BIN)
            .arg(&path)
            .current_dir(dir.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // Read as it comes, so a full pipe never blocks the app
        let mut stdout = child.stdout.take().unwrap();
        let stdout = thread::spawn(move || {
            let mut output = String::new();
            stdout.read_to_string(&mut output).unwrap();
            output
        });
        App {
            child,
            port,
            stdout: Some(stdout),
            _dir: dir,
        }
    }

    /// Waits until `GET /health` answers 200, failing if the app exits first.
    fn wait_until_healthy(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        while health(self.port) != Some(200) {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("the app exited with {status}: {:#?}", self.log());
            }
            assert!(Instant::now() < deadline, "the app did not become healthy");
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Stops the app as a service manager would, with SIGTERM, and returns its log.
    fn stop(&mut self) -> Vec<Value> {
        let pid = self.child.id().to_string();
        let kill = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(kill.success());
        let status = self.wait();
        assert!(status.success(), "the app stopped with {status}");
        self.log()
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the app did not exit");
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// The log lines written to standard output, once the app has exited.
    fn log(&mut self) -> Vec<Value> {
        let output = self.stdout.take().unwrap().join().unwrap();
        output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The position of the log line with `message`.
fn position(log: &[Value], message: &str) -> usize {
    log.iter()
        .position(|line| line["message"] == message)
        .unwrap_or_else(|| panic!("no {message:?} in {log:#?}"))
}

/// AC-07, AC-08, MIG-02, MIG-04
#[tokio::test]
async fn an_empty_database_is_migrated_and_seeded_before_requests_are_accepted() {
    let (_postgres, host, db_port) = postgres().await;
    let db = connect(&host, db_port).await;
    let port = free_port();
    let config = config(port, &host, db_port, "postgres", &[ADMIN]);

    let mut app = App::start(port, &config);
    app.wait_until_healthy();
    let log = app.stop();
    let migrated = &log[position(&log, "database schema is up to date")];
    let seeded = position(&log, "seed data applied");
    let jobs_started = position(&log, "background jobs started");
    let accepting = position(&log, "accepting requests");
    assert!(
        seeded < jobs_started && jobs_started < accepting,
        "{log:#?}"
    );
    assert_eq!(log[seeded]["categories"], 2);

    // FR-JOB-03: the jobs are registered before requests are accepted
    let jobs: Vec<(String, String)> =
        sqlx::query_as("SELECT name, schedule FROM jobs ORDER BY name")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        jobs,
        [
            (
                "apply-retention".to_owned(),
                "daily 03:00 Asia/Bangkok".to_owned()
            ),
            ("delete-expired".to_owned(), "every 900s".to_owned()),
            ("expire-orders".to_owned(), "every 60s".to_owned()),
            ("retry-pushes".to_owned(), "every 60s".to_owned()),
        ]
    );

    let categories: Vec<String> =
        sqlx::query_scalar("SELECT name FROM categories ORDER BY sort_order")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(categories, ["อาหาร", "เครื่องดื่ม"]);
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(migrated["added"], tables, "{migrated}");

    // AC-08: starting again changes nothing, and SeaORM sees no difference to warn about
    let mut app = App::start(port, &config);
    app.wait_until_healthy();
    let log = app.stop();
    let again = &log[position(&log, "database schema is up to date")];
    assert_eq!(again["added"], 0, "{again}");
    assert_eq!(again["relaxed"], 0, "{again}");
    assert_eq!(again["snapshot"], migrated["snapshot"]);
    assert_eq!(log[position(&log, "seed data applied")]["categories"], 0);
    let from_sea_orm: Vec<_> = log
        .iter()
        .filter(|line| {
            line["target"]
                .as_str()
                .is_some_and(|t| t.starts_with("sea_orm"))
        })
        .collect();
    assert!(from_sea_orm.is_empty(), "{from_sea_orm:#?}");
}

/// FR-AIP-04, FR-AI-15: a model that takes no audio can't hear voice messages, so they are turned
/// off with a warning, and the app starts all the same. Checked on the real binary: its one
/// global subscriber sees every line, where a test's own can miss them.
#[tokio::test]
async fn a_model_without_audio_input_turns_voice_messages_off_with_a_warning() {
    let (_postgres, host, db_port) = postgres().await;
    let port = free_port();
    let config = config(port, &host, db_port, "postgres", &[ADMIN]).replace(
        "[ai]\nenabled = false\n",
        r#"[ai]
enabled = true
provider = "claude"
model = "claude-opus-5"
apiKey = "ai-key-value"

[ai.features]
voice = true
"#,
    );
    assert!(config.contains("voice = true"), "{config}");

    let mut app = App::start(port, &config);
    app.wait_until_healthy();
    let log = app.stop();
    let warned: Vec<&Value> = log
        .iter()
        .filter(|line| line["message"] == "AI feature turned off: the model lacks what it needs")
        .collect();
    assert_eq!(warned.len(), 1, "{log:#?}");
    assert_eq!(warned[0]["level"], "WARN");
    assert_eq!(warned[0]["feature"], "voice");
    assert_eq!(warned[0]["missing"], "audio input");
    assert_eq!(warned[0]["provider"], "claude");
    position(&log, "accepting requests");
}

/// MIG-06, MIG-08
#[tokio::test]
async fn a_database_user_who_cannot_create_tables_stops_startup() {
    let (_postgres, host, db_port) = postgres().await;
    // Since PostgreSQL 15, only the database owner may create in `public`
    sqlx::query("CREATE ROLE app LOGIN PASSWORD 'app'")
        .execute(&connect(&host, db_port).await)
        .await
        .unwrap();
    let port = free_port();

    let mut app = App::start(port, &config(port, &host, db_port, "app", &[ADMIN]));
    let status = app.wait();
    assert!(!status.success());
    let log = app.log();
    let fatal = &log[position(&log, "exiting after a fatal error")];
    let error = fatal["error"].as_str().unwrap();
    assert!(
        error.starts_with(
            "auto-migration failed: the database user needs rights to create and alter \
             objects in its schema (MIG-08): "
        ),
        "{error}"
    );
    assert!(
        error.contains("permission denied for schema public"),
        "{error}"
    );
    assert!(
        !log.iter()
            .any(|line| line["message"] == "accepting requests"),
        "{log:#?}"
    );
}

/// The LINE user IDs of the admins, in ID order.
async fn admins(db: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT line_user_id FROM users WHERE is_admin ORDER BY id")
        .fetch_all(db)
        .await
        .unwrap()
}

/// FR-ADM-01, FR-ADM-08: the admin role follows `admin.lineUserIds` at every start.
#[tokio::test]
async fn removing_an_admin_from_the_config_revokes_the_role_at_restart() {
    let (_postgres, host, db_port) = postgres().await;
    let db = connect(&host, db_port).await;
    let port = free_port();
    let other = "Ufedcba9876543210fedcba9876543210";
    let both = config(port, &host, db_port, "postgres", &[ADMIN, other]);

    // The first start creates the schema; then both admins follow the OA
    let mut app = App::start(port, &both);
    app.wait_until_healthy();
    app.stop();
    for line_user_id in [ADMIN, other] {
        sqlx::query(
            "INSERT INTO users (line_user_id, is_admin, is_following, status, created_at, \
             updated_at) VALUES ($1, false, true, 'ACTIVE', now(), now())",
        )
        .bind(line_user_id)
        .execute(&db)
        .await
        .unwrap();
    }

    let mut app = App::start(port, &both);
    app.wait_until_healthy();
    let log = app.stop();
    assert_eq!(
        log[position(&log, "seed data applied")]["admins_granted"],
        2
    );
    assert_eq!(admins(&db).await, [ADMIN, other]);

    // The operator removes one and restarts
    let mut app = App::start(port, &config(port, &host, db_port, "postgres", &[ADMIN]));
    app.wait_until_healthy();
    let log = app.stop();
    let seeded = &log[position(&log, "seed data applied")];
    assert_eq!(seeded["admins_granted"], 0, "{seeded}");
    assert_eq!(seeded["admins_revoked"], 1, "{seeded}");
    assert_eq!(admins(&db).await, [ADMIN]);

    let audited: Vec<(Option<i64>, String, String)> = sqlx::query_as(
        "SELECT a.actor_id, a.action, u.line_user_id FROM audit_logs a \
         JOIN users u ON u.id = a.target_id ORDER BY a.id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    let audited: Vec<_> = audited
        .iter()
        .map(|(actor, action, user)| (*actor, action.as_str(), user.as_str()))
        .collect();
    assert_eq!(
        audited,
        [
            (None, "admin.granted", ADMIN),
            (None, "admin.granted", other),
            (None, "admin.revoked", other),
        ]
    );
}
