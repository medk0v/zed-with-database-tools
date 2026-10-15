#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
use database_connection::{
    CellInput, CellValue, ConnectionConfig, DatabaseConnection, DatabaseKind, RowChange, Secrets,
    SshConfig,
};
use std::{
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            if error.kind() != std::io::ErrorKind::InvalidInput {
                eprintln!("Could not stop test sshd: {error}");
            }
        }
        if let Err(error) = self.0.wait() {
            eprintln!("Could not reap test sshd: {error}");
        }
    }
}

#[test]
#[ignore = "requires system sshd, ssh-keygen, Python 3 and permission to bind localhost"]
#[allow(
    clippy::disallowed_methods,
    reason = "This synchronous integration fixture runs outside GPUI and waits for its child processes"
)]
fn sqlite_over_local_ssh() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let host_key = root.join("host_ed25519");
    let client_key = root.join("client_ed25519");
    for path in [&host_key, &client_key] {
        ensure!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(path)
                .status()?
                .success(),
            "Could not generate test SSH key"
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let server_config = root.join("sshd_config");
    std::fs::write(
        &server_config,
        format!(
            "Port {}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\n\
             AuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nUsePAM no\n",
            address.port(),
            host_key.display(),
            root.join("sshd.pid").display(),
            client_key.with_extension("pub").display(),
        ),
    )?;
    let mut daemon = Sshd(
        Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(server_config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = daemon.0.try_wait()? {
            bail!("Test SSH server exited: {status}");
        }
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(_) => break,
            Err(error) if Instant::now() >= deadline => return Err(error.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let temporary_home = root.join("home");
    std::fs::create_dir_all(temporary_home.join(".ssh"))?;
    let public_host_key = std::fs::read_to_string(host_key.with_extension("pub"))?;
    std::fs::write(
        temporary_home.join(".ssh/known_hosts"),
        format!("[127.0.0.1]:{} {public_host_key}", address.port()),
    )?;
    let user = Command::new("id").arg("-un").output()?;
    ensure!(user.status.success(), "Could not identify SSH test user");
    let config = ConnectionConfig {
        kind: DatabaseKind::Sqlite,
        path: Some(root.join("remote 'database' ?#.sqlite")),
        ssh: Some(SshConfig {
            host: "127.0.0.1".into(),
            port: address.port(),
            user: String::from_utf8(user.stdout)?.trim().to_owned(),
            key_path: Some(client_key),
            ..Default::default()
        }),
        ..Default::default()
    };
    let database = rusqlite::Connection::open(config.path.as_ref().context("Missing test path")?)?;
    database.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB);\
         INSERT INTO items VALUES(1,'one',X'00FF'),(2,'two',NULL);\
         CREATE INDEX names ON items(name);\
         CREATE VIEW first_item AS SELECT * FROM items WHERE id=1;",
    )?;
    drop(database);
    // Run with a private known_hosts file without changing this process's environment.
    let status = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "sqlite_over_ssh_child",
            "--ignored",
            "--nocapture",
        ])
        .env("HOME", temporary_home)
        .env(
            "ZED_TEST_SQLITE_SSH_CONFIG",
            serde_json::to_string(&config)?,
        )
        .status()?;
    ensure!(status.success(), "SQLite SSH integration subprocess failed");
    Ok(())
}

#[test]
#[ignore = "launched by sqlite_over_local_ssh with an isolated known_hosts fixture"]
fn sqlite_over_ssh_child() -> Result<()> {
    let Some(config_json) = std::env::var_os("ZED_TEST_SQLITE_SSH_CONFIG") else {
        return Ok(());
    };
    let mut config: ConnectionConfig = serde_json::from_str(&config_json.to_string_lossy())?;
    let known_hosts = PathBuf::from(std::env::var_os("HOME").context("Missing test HOME")?)
        .join(".ssh/known_hosts");
    let trusted_key = std::fs::read(&known_hosts)?;
    std::fs::write(&known_hosts, "")?;
    let error = DatabaseConnection::connect(&config, Secrets::default())
        .err()
        .context("Unknown SSH host key was accepted")?;
    ensure!(format!("{error:#}").contains("Unknown SSH host key"));
    let ssh = config.ssh.as_ref().context("Missing test SSH settings")?;
    let wrong_key = std::fs::read_to_string(
        ssh.key_path
            .as_ref()
            .context("Missing client key")?
            .with_extension("pub"),
    )?;
    std::fs::write(
        &known_hosts,
        format!("[127.0.0.1]:{} {wrong_key}", ssh.port),
    )?;
    let error = DatabaseConnection::connect(&config, Secrets::default())
        .err()
        .context("Changed SSH host key was accepted")?;
    ensure!(format!("{error:#}").contains("SSH host key mismatch"));
    std::fs::write(&known_hosts, trusted_key)?;
    let connection = DatabaseConnection::connect(&config, Secrets::default())?;
    let tables = connection.list_tables()?;
    assert_eq!(tables.len(), 2);
    let table = tables
        .iter()
        .find(|table| table.name == "items")
        .context("Missing table")?;
    assert!(
        tables
            .iter()
            .any(|table| table.name == "first_item" && table.is_view)
    );
    let snapshot = connection.start_browse(table, 10, 0)?.execute()?;
    assert_eq!(snapshot.primary_key, [0]);
    assert!(snapshot.editability_error.is_none());
    assert_eq!(
        snapshot.result.values.first().and_then(|row| row.get(2)),
        Some(&CellValue::Bytes(vec![0, 255]))
    );
    let original = snapshot
        .result
        .values
        .first()
        .context("Missing first row")?
        .clone();
    assert_eq!(
        connection
            .start_changes(
                &snapshot,
                vec![RowChange::Update {
                    original,
                    values: vec![(1, CellInput::Text("updated".into()))],
                }]
            )?
            .execute()?
            .affected_rows,
        1
    );
    let structure = connection.start_table_structure(table)?.execute()?;
    assert!(structure.indexes.iter().any(|index| index.name == "names"));
    assert_eq!(
        connection
            .query("SELECT name FROM items WHERE id=1", 1)?
            .rows,
        [vec!["updated"]]
    );
    connection.query("BEGIN", 1)?;
    connection.query("UPDATE items SET name='rollback' WHERE id=1", 1)?;
    connection.query("ROLLBACK", 1)?;
    assert_eq!(
        connection
            .query("SELECT name FROM items WHERE id=1", 1)?
            .rows,
        [vec!["updated"]]
    );
    let operation = connection.start_query(
        "WITH RECURSIVE numbers(value) AS (VALUES(1) UNION ALL SELECT value+1 FROM numbers WHERE value<1000000000) SELECT sum(value) FROM numbers", 1,
    )?;
    let cancel = operation.cancel_handle();
    let worker = std::thread::spawn(move || operation.execute());
    std::thread::sleep(Duration::from_millis(200));
    cancel.cancel()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        worker.is_finished(),
        "SSH query cancellation did not finish promptly"
    );
    assert!(
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("Query thread panicked"))?
            .is_err()
    );
    assert_eq!(connection.query("SELECT 1", 1)?.rows, [vec!["1"]]);
    connection.disconnect()?;
    config.read_only = true;
    let read_only = DatabaseConnection::connect(&config, Secrets::default())?;
    assert!(
        read_only
            .query("UPDATE items SET name='forbidden'", 1)
            .is_err()
    );
    assert!(read_only.query("PRAGMA query_only=OFF", 1).is_err());
    assert_eq!(
        read_only
            .query("SELECT name FROM items WHERE id=1", 1)?
            .rows,
        [vec!["updated"]]
    );
    read_only.disconnect()?;
    config.path = Some(PathBuf::from(format!(
        "{}.missing",
        config.path.context("Missing test path")?.display()
    )));
    assert!(DatabaseConnection::connect(&config, Secrets::default()).is_err());
    ensure!(
        !config.path.context("Missing test path")?.exists(),
        "Missing database was created"
    );
    Ok(())
}
