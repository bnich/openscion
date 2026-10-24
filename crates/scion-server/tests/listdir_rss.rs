//! The server's RSS stays bounded while many clients hold unread
//! `ListDir` replies at once: the reply budget counts the whole body
//! before it is built, so a spent budget refuses instead of building.
//! Red before the rule: the old order built the body first and took
//! its share after, so every concurrent build sat in RAM uncounted
//! (the `whole_share` shortcut this suite retires).
//!
//! Needs `/proc` (Linux): the measurement is the child server's
//! `VmRSS`.

#![cfg(target_os = "linux")]

use prost::Message as _;
use scion_core::{Changeset, ChangesetId, FileState, IdentityTime, RepoPath};
use scion_proto::runtime::auth::Bearer;
use scion_proto::v1::ListDirRequest;
use scion_proto::v1::scion_client::ScionClient;
use scion_server::test_support::store::Store;

mod common;

const REPO: &str = "main";
const TOKEN: &str = "secret";

/// One directory wide enough that one `ListDir` reply's tree body is
/// about `names * 160` bytes; 13,000 names of 120 characters is about
/// 2 MB.
const NAMES: usize = 13_000;
/// Held unread streams: 120 bodies of 2 MB is 240 MB of builds under
/// the old order, well over the 128 MiB budget.
const STREAMS: usize = 120;
/// The slack over baseline plus the budget: allocator churn, the
/// worker's own buffers, tonic's frames.
const SLACK_KB: u64 = 48 * 1024;

fn author() -> IdentityTime {
    IdentityTime::new(b"t".to_vec(), b"t@x".to_vec(), 1_700_000_000, 0)
        .expect("identity fits the format")
}

fn wide_name(i: usize) -> RepoPath {
    let mut name = format!("entry-{i:06}.").into_bytes();
    name.resize(120, b'a');
    RepoPath::new(name.as_slice()).unwrap()
}

/// Seeds an on-disk store whose root directory holds `NAMES` entries.
fn seed(dir: &std::path::Path) -> ChangesetId {
    let store = Store::open(dir).unwrap();
    let blob = store.blobs().put(b"payload\n").unwrap();
    let changeset = Changeset::new(
        vec![],
        author(),
        None,
        b"wide\n".to_vec(),
        (0..NAMES)
            .map(|i| {
                (
                    wide_name(i),
                    Some(FileState::Regular {
                        content: blob,
                        executable: false,
                    }),
                )
            })
            .collect(),
    )
    .expect("changeset fits the format");
    store
        .write(|c| {
            let repo = c.create_repo(REPO)?;
            c.insert_changeset(repo, changeset)
        })
        .unwrap()
}

/// The child server's `VmRSS` in kB; 0 when it is gone.
fn rss_kb(pid: u32) -> u64 {
    let status = format!("/proc/{pid}/status");
    std::fs::read_to_string(status).ok().and_then(|text| {
        text.lines().find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|kb| kb.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
    })
}

/// A running real `scion-server` on the seeded store.
struct Server {
    child: std::process::Child,
    addr: std::net::SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &std::path::Path) -> Server {
    let log = std::fs::File::create(dir.join("server.log")).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_scion-server"))
        .arg("--data")
        .arg(dir)
        .args(["serve", "--token", TOKEN, "--listen", "127.0.0.1:0"])
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        // The mutation lever rides through to the child, for the red.
        .env(
            "SCION_LISTDIR_MUTATION",
            std::env::var("SCION_LISTDIR_MUTATION").unwrap_or_default(),
        )
        .stdout(std::process::Stdio::from(log.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .unwrap();
    let log_path = dir.join("server.log");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let addr = loop {
        if let Some(addr) = bound_addr(&log_path) {
            break addr;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the server never announced its address:\n{}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Server { child, addr }
}

/// The `listen=` address of the newest `serving` line in the log.
fn bound_addr(log: &std::path::Path) -> Option<std::net::SocketAddr> {
    let text = std::fs::read_to_string(log).ok()?;
    text.lines()
        .filter(|line| line.contains("serving"))
        .filter_map(|line| line.split("listen=").next_back())
        .next_back()
        .and_then(|addr| addr.split_whitespace().next())
        .and_then(|addr| addr.parse().ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn held_list_dir_replies_stay_under_the_budget_in_rss() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let server = serve(dir.path());

    let mut client = ScionClient::with_interceptor(
        common::channel(&format!("http://{}", server.addr))
            .await
            .unwrap(),
        Bearer::new(TOKEN).unwrap(),
    );
    let baseline_kb = rss_kb(server.child.id());
    assert!(
        baseline_kb > 0,
        "the sampler saw no RSS; this test needs /proc"
    );

    let mut held = Vec::new();
    for _ in 0..STREAMS {
        let request = ListDirRequest {
            repo: REPO.into(),
            changeset_id: Vec::new(),
            path: Vec::new(),
        };
        held.push(client.list_dir(request).await.expect("list_dir opens"));
        // Hold the stream: its reply is never read.
    }

    // Every opened call either built its body (budget-paid) or was
    // refused; give the builds a moment to land, then measure.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let after_kb = rss_kb(server.child.id()).expect("the server stays up");
    let budget_kb = (scion_server::REPLY_MEMORY_BUDGET / 1024) as u64;
    let ceiling = baseline_kb + budget_kb + SLACK_KB;
    println!("baseline {baseline_kb} kB, after {after_kb} kB, ceiling {ceiling} kB");
    assert!(
        after_kb <= ceiling,
        "the server's RSS ({after_kb} kB) passed the ceiling ({ceiling} kB); \
         replies are built outside the budget"
    );
}
