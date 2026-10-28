//! A restore must honor the Postgres URL from every source it names:
//! `--postgres-url`, `--postgres-url-file` and `SCION_POSTGRES_URL`. The
//! bug this pins: `restore` took the URL from the argv flag only, so a
//! URL from the file or the environment silently restored into SQLite,
//! and `--postgres-ca` beside a file or env URL was refused by clap.
//! These tests need the test database; they skip themselves when
//! `SCION_TEST_POSTGRES_URL` is not set.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_scion-server"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("the scion-server command runs")
}

fn must_run(args: &[&str], what: &str) {
    let out = run(args);
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The test database URL with `data`'s schema as the search path, the
/// shape the test harness gives a server, written to `file`.
fn write_url_file(data: &Path, file: &Path) {
    let url = std::env::var("SCION_TEST_POSTGRES_URL")
        .expect("SCION_TEST_POSTGRES_URL must name the test database");
    let schema = scion_server::Backend::test_schema(data)
        .expect("postgres mode names a schema for the data directory");
    let mut client =
        postgres::Client::connect(&url, postgres::NoTls).expect("the test database connects");
    client
        .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    fs::write(file, format!("{url}{sep}options=--search_path%3D{schema}")).unwrap();
}

/// A store with one repo, made by an import whose URL came from a file:
/// the same source the restore under test reads.
fn import_from_url_file(dir: &Path) -> (PathBuf, PathBuf) {
    let git = dir.join("git");
    fs::create_dir_all(git.join("src")).unwrap();
    fs::write(git.join("src/lib.rs"), "fn main() {}\n").unwrap();
    let data = dir.join("source");
    let url_file = dir.join("pg-url");
    write_url_file(&data, &url_file);
    must_run(
        &[
            "--data",
            data.to_str().unwrap(),
            "--postgres-url-file",
            url_file.to_str().unwrap(),
            "import",
            "--repo",
            "main",
            git.to_str().unwrap(),
        ],
        "the import",
    );
    (data, url_file)
}

fn backend_marker(data: &Path) -> String {
    fs::read_to_string(data.join("backend"))
        .unwrap_or_else(|e| panic!("the target must name its backend: {e}"))
}

fn skip_without_postgres() -> bool {
    std::env::var("SCION_TEST_POSTGRES_URL").is_err()
        || std::env::var("SCION_TEST_BACKEND").as_deref() != Ok("postgres")
}

/// A restore whose URL comes from `--postgres-url-file` lands in
/// Postgres: the restored directory names its backend, and it is
/// Postgres.
#[test]
fn a_restore_with_the_url_from_a_file_lands_in_postgres() {
    if skip_without_postgres() {
        println!("skipping: SCION_TEST_POSTGRES_URL is not set");
        return;
    }
    let dir = scion_server::test_support::private_tempdir();
    let (_data, url_file) = import_from_url_file(dir.path());
    let out = dir.path().join("archive");
    must_run(
        &[
            "--data",
            dir.path().join("source").to_str().unwrap(),
            "export",
            "--all",
            "--out",
            out.to_str().unwrap(),
        ],
        "the export",
    );

    let target = dir.path().join("restored");
    must_run(
        &[
            "--data",
            target.to_str().unwrap(),
            "--postgres-url-file",
            url_file.to_str().unwrap(),
            "restore",
            "--archive",
            out.to_str().unwrap(),
        ],
        "the restore",
    );
    let marker = backend_marker(&target);
    assert!(
        marker.contains("postgres"),
        "a restore with a URL file must land in Postgres, not {marker:?}"
    );
}

/// A restore whose URL comes from `SCION_POSTGRES_URL` lands in
/// Postgres too: the environment form.
#[test]
fn a_restore_with_the_url_from_the_env_lands_in_postgres() {
    if skip_without_postgres() {
        println!("skipping: SCION_TEST_POSTGRES_URL is not set");
        return;
    }
    let dir = scion_server::test_support::private_tempdir();
    let (data, url_file) = import_from_url_file(dir.path());
    let env_url = fs::read_to_string(&url_file).unwrap();
    let out = dir.path().join("archive");
    must_run(
        &[
            "--data",
            data.to_str().unwrap(),
            "export",
            "--all",
            "--out",
            out.to_str().unwrap(),
        ],
        "the export",
    );

    let target = dir.path().join("restored");
    let out = Command::new(env!("CARGO_BIN_EXE_scion-server"))
        .args([
            "--data",
            target.to_str().unwrap(),
            "restore",
            "--archive",
            out.to_str().unwrap(),
        ])
        .env("NO_COLOR", "1")
        .env("SCION_POSTGRES_URL", env_url.trim())
        .output()
        .expect("the restore runs");
    assert!(
        out.status.success(),
        "the restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let marker = backend_marker(&target);
    assert!(
        marker.contains("postgres"),
        "a restore with an env URL must land in Postgres, not {marker:?}"
    );
}

/// `--postgres-ca` with a URL from a file is accepted: the refusal of a
/// CA without a URL belongs after the resolution, not in clap.
#[test]
fn a_postgres_ca_with_a_url_from_a_file_is_accepted() {
    if skip_without_postgres() {
        println!("skipping: SCION_TEST_POSTGRES_URL is not set");
        return;
    }
    let dir = scion_server::test_support::private_tempdir();
    let (_data, url_file) = import_from_url_file(dir.path());
    // Any readable CA file passes the flag's existence check; the CA
    // itself is judged when the connection opens, which a localhost
    // test URL without TLS never does.
    let ca = dir.path().join("ca.pem");
    fs::write(&ca, "not a real CA - never opened without TLS\n").unwrap();
    must_run(
        &[
            "--data",
            dir.path().join("probe").to_str().unwrap(),
            "--postgres-url-file",
            url_file.to_str().unwrap(),
            "--postgres-ca",
            ca.to_str().unwrap(),
            "verify",
            dir.path().join("nonexistent-archive").to_str().unwrap(),
        ],
        "verify with --postgres-ca beside a URL file",
    );
}

/// `--postgres-ca` with NO URL from any source is refused, with the fix
/// named: the URL can come from three places, and clap saw only one.
#[test]
fn a_postgres_ca_without_any_url_is_refused() {
    let dir = scion_server::test_support::private_tempdir();
    let ca = dir.path().join("ca.pem");
    fs::write(&ca, "unopened\n").unwrap();
    let out = run(&[
        "--postgres-ca",
        ca.to_str().unwrap(),
        "verify",
        dir.path().join("nonexistent-archive").to_str().unwrap(),
    ]);
    assert!(
        !out.status.success(),
        "a CA without a URL must be refused"
    );
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(
        why.contains("--postgres-ca needs a Postgres URL"),
        "{why}"
    );
}
