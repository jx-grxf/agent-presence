//! Two daemons racing for the lock, run as real processes.
//!
//! The bug this covers was not reproducible in-process: the lock was the *existence* of
//! the pid file and the owner was its *contents*, so the loser had to catch the winner in
//! the window between creating the file and writing to it. That window is microseconds
//! wide, which is exactly how far apart two hooks from the same turn start their daemons.
//!
//! Both survivors bound the same control socket, and `Listener::bind` unlinks whatever it
//! finds — so the second daemon silently stole every event from the first, and the two of
//! them cleared each other's card.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Somewhere for this test's config, pid file and control socket to live, so it cannot
/// collide with the developer's own running daemon.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn spawn_daemon(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_agent-presence"))
            .arg("daemon")
            .env("AGENT_PRESENCE_HOME", &self.0)
            // No Discord on CI, and a failed connect is a debug-level non-event.
            .env("AGENT_PRESENCE_LOG", "agent_presence=warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Wait for the population to settle, then report how many are still running.
fn survivors(children: &mut [Child], settle: Duration) -> usize {
    std::thread::sleep(settle);
    let mut alive = 0;
    for child in children.iter_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            alive += 1;
        }
    }
    alive
}

fn reap(children: &mut [Child]) {
    for child in children.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn only_one_daemon_survives_a_racing_start() {
    let sandbox = Sandbox::new("race");

    // Started back to back with no synchronisation, which is how a burst of hook events
    // starts them. Eight, so a rare interleaving still shows up.
    let mut children: Vec<Child> = (0..8).map(|_| sandbox.spawn_daemon()).collect();
    let alive = survivors(&mut children, Duration::from_millis(1500));
    reap(&mut children);

    assert_eq!(
        alive, 1,
        "exactly one daemon may hold the lock; {alive} were still running"
    );
}

/// The exact window the old lock lost, reproduced without having to win a race.
///
/// It made the file's *existence* the lock and its *contents* the owner, so between
/// `create_new` returning and the pid being written there was a moment where the file
/// existed but named nobody. A second daemon reading it there concluded the lock was
/// stale, deleted it, and claimed one of its own.
///
/// Emptying the file by hand puts a live holder into exactly that state. `flock` does not
/// care what the file says — the kernel holds the lock as long as the fd is open — so the
/// newcomer must still be turned away.
#[cfg(unix)]
#[test]
fn an_empty_pid_file_does_not_hand_the_lock_to_a_newcomer() {
    let sandbox = Sandbox::new("window");

    let mut holder = sandbox.spawn_daemon();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        survivors(std::slice::from_mut(&mut holder), Duration::from_millis(0)),
        1,
        "the holder should be running"
    );

    std::fs::write(sandbox.0.join("daemon.pid"), b"").expect("empty the pid file");

    let mut newcomer = sandbox.spawn_daemon();
    let status = newcomer.wait().expect("newcomer exits");
    let holder_alive = survivors(std::slice::from_mut(&mut holder), Duration::from_millis(0));
    reap(std::slice::from_mut(&mut holder));

    assert!(
        !status.success(),
        "a live holder with an unwritten pid file still owns the lock"
    );
    assert_eq!(holder_alive, 1, "the holder must not have been displaced");
}

#[test]
fn the_lock_is_released_when_the_holder_is_killed() {
    let sandbox = Sandbox::new("kill");

    let mut first = sandbox.spawn_daemon();
    assert_eq!(
        survivors(std::slice::from_mut(&mut first), Duration::from_millis(600)),
        1,
        "the first daemon should be running"
    );

    // SIGKILL, so no Drop runs and the pid file is left behind. A successor must still be
    // able to start — this is the case the old stale-file heuristic existed to handle,
    // and the one `flock` gets right for free.
    first.kill().unwrap();
    first.wait().unwrap();

    let mut second = sandbox.spawn_daemon();
    let alive = survivors(
        std::slice::from_mut(&mut second),
        Duration::from_millis(800),
    );
    reap(std::slice::from_mut(&mut second));

    assert_eq!(alive, 1, "a killed holder must not lock the daemon out");
}

#[test]
fn a_second_daemon_gives_up_promptly() {
    let sandbox = Sandbox::new("loser");

    let mut holder = sandbox.spawn_daemon();
    std::thread::sleep(Duration::from_millis(600));

    let started = Instant::now();
    let mut loser = sandbox.spawn_daemon();
    let status = loser.wait().expect("second daemon exits");

    reap(std::slice::from_mut(&mut holder));

    assert!(!status.success(), "the loser must exit with an error");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the loser must fail fast rather than queue behind the holder"
    );
}
