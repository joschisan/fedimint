use std::sync::{Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::sys::wait::waitpid;
use nix::unistd::Pid;
use tokio::process::Child;

/// How long to wait after SIGTERM before escalating to SIGKILL.
///
/// Long enough for iroh-quinn to flush its close frames (avoids the
/// `untracked_bytes <= segment_size` debug assertion in
/// iroh-quinn-proto 0.13), short enough that test teardown stays snappy.
const GRACE_PERIOD: Duration = Duration::from_millis(250);

/// Deferred process reaper.
///
/// [`kill_process`] sends SIGTERM and enqueues the pid. A dedicated
/// background thread owns the escalation to SIGKILL after [`GRACE_PERIOD`]
/// and the subsequent `waitpid`. [`reap_killed_processes`] blocks until
/// every enqueued pid has been reaped, so ports and file locks are
/// guaranteed released before we spawn a replacement.
///
/// Doing the graceful SIGTERM→wait→SIGKILL dance off the tokio runtime
/// lets `Drop` stay fully synchronous while still giving peers a chance
/// to close QUIC connections cleanly.
struct Reaper {
    state: Mutex<Vec<PendingKill>>,
    cv: Condvar,
}

struct PendingKill {
    pid: Pid,
    enqueued_at: Instant,
}

static REAPER: LazyLock<&'static Reaper> = LazyLock::new(|| {
    let reaper: &'static Reaper = Box::leak(Box::new(Reaper {
        state: Mutex::new(Vec::new()),
        cv: Condvar::new(),
    }));
    thread::Builder::new()
        .name("devimint-reaper".into())
        .spawn(move || reaper_loop(reaper))
        .expect("failed to spawn devimint reaper thread");
    reaper
});

fn reaper_loop(reaper: &'static Reaper) -> ! {
    let mut state = reaper.state.lock().expect("reaper lock");
    loop {
        if state.is_empty() {
            state = reaper.cv.wait(state).expect("reaper cv");
            continue;
        }

        let now = Instant::now();
        let next_deadline = state
            .iter()
            .map(|e| e.enqueued_at + GRACE_PERIOD)
            .min()
            .expect("non-empty checked above");

        if now < next_deadline {
            let (s, _) = reaper
                .cv
                .wait_timeout(state, next_deadline - now)
                .expect("reaper cv");
            state = s;
            continue;
        }

        // SIGKILL + reap any entries past their grace deadline.
        // waitpid after SIGKILL returns in microseconds, so holding
        // the lock across it is fine.
        //
        // Errors are ignored on purpose: SIGTERM may have already killed
        // the process (ESRCH), or tokio's internal pidfd/SIGCHLD machinery
        // may have raced us to reap (ECHILD). Both outcomes are fine — we
        // only care that the process is gone and its zombie is cleared.
        state.retain(|entry| {
            if entry.enqueued_at + GRACE_PERIOD <= now {
                let _ = signal::kill(entry.pid, Signal::SIGKILL);
                let _ = waitpid(entry.pid, None);
                false
            } else {
                true
            }
        });

        if state.is_empty() {
            reaper.cv.notify_all();
        }
    }
}

pub fn kill_process(child: &Child) {
    let Some(id) = child.id() else {
        return;
    };
    let pid = Pid::from_raw(id as _);
    // Send SIGTERM now so the grace period starts immediately, without
    // waiting for the reaper thread to wake up. `kill()` is non-blocking
    // (the kernel just queues the signal), so this is safe in Drop.
    let _ = signal::kill(pid, Signal::SIGTERM);

    let reaper = *REAPER;
    reaper.state.lock().expect("reaper lock").push(PendingKill {
        pid,
        enqueued_at: Instant::now(),
    });
    reaper.cv.notify_all();
}

/// Block until the reaper queue is fully drained.
///
/// Note: this waits for *all* currently-enqueued kills, not just the
/// caller's. That's fine in devimint, where process lifecycle is
/// single-threaded per test — callers don't enqueue concurrently.
pub fn reap_killed_processes() {
    let reaper = *REAPER;
    let wait = || {
        let mut state = reaper.state.lock().expect("reaper lock");
        while !state.is_empty() {
            state = reaper.cv.wait(state).expect("reaper cv");
        }
    };

    // Use `block_in_place` only when we're inside a multi-thread tokio
    // runtime — it panics on `current_thread` runtimes and is pointless
    // outside any runtime (Drop at program exit, sync tests).
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
            fedimint_core::runtime::block_in_place(wait);
        }
        _ => wait(),
    }
}
