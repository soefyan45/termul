//! Unix transport tests for [`TreeKillAgent`].
//!
//! The regression test polls the transport the way an optimized build does:
//! with a waker whose clone does not compare equal to the original. An idle
//! agent must leave the transport parked under such a waker (#1391).

use std::future::Future as _;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use agent_client_protocol::schema::v1::{McpServer, McpServerStdio};
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, ConnectionTo, LineDirection};

use super::TreeKillAgent;

/// Shared state behind the relinked waker: the test task's waker and how
/// often the transport woke it.
struct Relay {
    parent: Mutex<Option<Waker>>,
    wakes: AtomicUsize,
}

// Two vtables with identical behaviour. A clone made through one carries the
// other, so `Waker::will_wake` (a pointer comparison) reports every clone as
// a different task. Optimized builds do the same by duplicating the vtable.
static VTABLE_A: RawWakerVTable = RawWakerVTable::new(clone_to_b, wake, wake_by_ref, drop_relay);
static VTABLE_B: RawWakerVTable = RawWakerVTable::new(clone_to_a, wake, wake_by_ref, drop_relay);

unsafe fn clone_to_b(data: *const ()) -> RawWaker {
    unsafe { Arc::increment_strong_count(data.cast::<Relay>()) };
    RawWaker::new(data, &VTABLE_B)
}

unsafe fn clone_to_a(data: *const ()) -> RawWaker {
    unsafe { Arc::increment_strong_count(data.cast::<Relay>()) };
    RawWaker::new(data, &VTABLE_A)
}

unsafe fn wake(data: *const ()) {
    unsafe {
        wake_by_ref(data);
        drop_relay(data);
    }
}

unsafe fn wake_by_ref(data: *const ()) {
    let relay = unsafe { &*data.cast::<Relay>() };
    relay.wakes.fetch_add(1, Ordering::Relaxed);
    if let Some(parent) = relay.parent.lock().unwrap().as_ref() {
        parent.wake_by_ref();
    }
}

unsafe fn drop_relay(data: *const ()) {
    unsafe { Arc::decrement_strong_count(data.cast::<Relay>()) };
}

fn relinked_waker(relay: &Arc<Relay>) -> Waker {
    let data = Arc::into_raw(relay.clone()).cast::<()>();
    unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE_A)) }
}

/// An agent that prints one startup line to stderr, then idles with its
/// stdio open. The line wakes the transport once; nothing follows.
fn idle_agent() -> TreeKillAgent {
    TreeKillAgent::new(McpServer::Stdio(
        McpServerStdio::new("idle-agent", PathBuf::from("sh")).args(vec![
            "-c".to_string(),
            "echo ready >&2; exec cat".to_string(),
        ]),
    ))
}

#[tokio::test]
async fn idle_transport_stays_parked_under_a_relinked_waker() {
    let (channel, _peer) = Channel::duplex();
    let mut transport = pin!(ConnectTo::<Client>::connect_to(idle_agent(), channel));
    let relay = Arc::new(Relay {
        parent: Mutex::new(None),
        wakes: AtomicUsize::new(0),
    });
    let mut polls = 0_usize;
    let outcome = tokio::time::timeout(
        Duration::from_millis(500),
        std::future::poll_fn(|cx| {
            *relay.parent.lock().unwrap() = Some(cx.waker().clone());
            polls += 1;
            let waker = relinked_waker(&relay);
            let mut relinked = Context::from_waker(&waker);
            transport.as_mut().poll(&mut relinked)
        }),
    )
    .await;
    assert!(
        outcome.is_err(),
        "the idle agent transport finished instead of staying open: {outcome:?}"
    );
    assert!(
        polls <= 16,
        "an idle agent transport was polled {polls} times in 500 ms and woke itself {} times",
        relay.wakes.load(Ordering::Relaxed)
    );
}

const PID_PREFIX: &str = "CHILD_PID=";
const PID_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A launcher that starts a long-lived `sleep` grandchild (a stand-in for a
/// wrapper's self-spawned worker), prints its pid to stderr, then runs `tail`.
fn launcher_agent(tail: &str) -> (TreeKillAgent, tokio::sync::mpsc::UnboundedReceiver<u32>) {
    let script = format!("sleep 600 & echo {PID_PREFIX}$! >&2; {tail}");
    let server = McpServer::Stdio(
        McpServerStdio::new("fake-launcher", PathBuf::from("sh"))
            .args(vec!["-c".to_string(), script]),
    );
    let (pid_tx, pid_rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = TreeKillAgent::new(server).with_debug(move |line, direction| {
        if direction == LineDirection::Stderr {
            if let Some(pid) = line.trim().strip_prefix(PID_PREFIX) {
                if let Ok(pid) = pid.trim().parse() {
                    let _ = pid_tx.send(pid);
                }
            }
        }
    });
    (agent, pid_rx)
}

/// A reaped or zombie process is not alive.
fn is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let state = stat.rfind(')').map(|end| &stat[end + 2..]);
                !state.is_some_and(|state| state.starts_with('Z'))
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while is_alive(pid) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

struct KillOnFail(u32);

impl Drop for KillOnFail {
    fn drop(&mut self) {
        if is_alive(self.0) {
            // SAFETY: `kill` has no memory-safety preconditions.
            unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }
}

#[tokio::test]
async fn dropping_the_connection_kills_the_agent_grandchild() {
    let (agent, mut pid_rx) = launcher_agent("exec cat");
    let connection = tokio::spawn(async move {
        Client
            .builder()
            .connect_with(agent, async move |_cx: ConnectionTo<Agent>| {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await
    });

    let grandchild = tokio::time::timeout(PID_TIMEOUT, pid_rx.recv())
        .await
        .expect("fake launcher reported its grandchild in time")
        .expect("pid channel open");
    let _cleanup = KillOnFail(grandchild);
    assert!(
        is_alive(grandchild),
        "grandchild should run while connected"
    );

    connection.abort();
    let _ = connection.await;

    assert!(
        wait_for_exit(grandchild, EXIT_TIMEOUT),
        "grandchild {grandchild} survived the dropped connection"
    );
}

#[tokio::test]
async fn launcher_exit_kills_the_grandchild_and_reports_stderr() {
    let (agent, mut pid_rx) = launcher_agent("exit 3");
    let result = tokio::time::timeout(
        PID_TIMEOUT,
        Client
            .builder()
            .connect_with(agent, async move |_cx: ConnectionTo<Agent>| {
                futures::future::pending::<()>().await;
                Ok(())
            }),
    )
    .await
    .expect("connection settles after the launcher exits");

    let grandchild = pid_rx
        .try_recv()
        .expect("fake launcher reported its grandchild");
    let _cleanup = KillOnFail(grandchild);

    let error = format!("{:?}", result.expect_err("early non-zero exit is an error"));
    assert!(
        error.contains("Process exited with exit status: 3"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains(PID_PREFIX),
        "stderr tail missing from: {error}"
    );
    assert!(
        wait_for_exit(grandchild, EXIT_TIMEOUT),
        "grandchild {grandchild} survived the launcher exit"
    );
}

#[tokio::test]
async fn protocol_shutdown_kills_a_launcher_that_ignores_stdin_eof() {
    // The `AcpManager::kill` path: the client's main closure returns `Ok(())`
    // and the protocol shuts down cleanly, but the launcher never exits on
    // stdin EOF. After the 1s grace the whole tree must be terminated.
    let (agent, mut pid_rx) = launcher_agent("exec sleep 600");
    let (grandchild_tx, grandchild_rx) = tokio::sync::oneshot::channel();
    let result = tokio::time::timeout(
        PID_TIMEOUT,
        Client
            .builder()
            .connect_with(agent, async move |_cx: ConnectionTo<Agent>| {
                let pid = pid_rx
                    .recv()
                    .await
                    .expect("fake launcher reported its grandchild");
                let _ = grandchild_tx.send(pid);
                Ok(())
            }),
    )
    .await
    .expect("connection settles after protocol shutdown");
    let grandchild = grandchild_rx.await.expect("grandchild pid forwarded");
    let _cleanup = KillOnFail(grandchild);

    assert!(
        result.is_ok(),
        "clean protocol shutdown is not an error: {result:?}"
    );
    assert!(
        wait_for_exit(grandchild, EXIT_TIMEOUT),
        "grandchild {grandchild} survived protocol shutdown"
    );
}
