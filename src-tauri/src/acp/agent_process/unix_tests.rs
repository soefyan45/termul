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
use agent_client_protocol::{Channel, Client, ConnectTo};

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
