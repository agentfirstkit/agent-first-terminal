//! Concurrent session creation is a product guarantee, not an accident.
//!
//! The lib suite used to fail about half of its runs with `openpty: Os { code:
//! -6 }`, which reads like PTY exhaustion under parallelism. It was not — a
//! sibling test signalling the shared process group was killing other tests'
//! children. These two tests pin the real behaviour so that diagnosis cannot be
//! re-litigated: a caller may open many sessions in sequence, and many callers
//! may open them at the same instant.
use agent_first_terminal::{TerminalOpenSpec, TerminalSessionManager};

fn spec() -> TerminalOpenSpec {
    TerminalOpenSpec {
        program: Some("/bin/sh".to_string()),
        ..TerminalOpenSpec::default()
    }
}

/// One manager, many sessions, sequentially — the ordinary product path.
#[test]
fn one_manager_opens_many_sessions_sequentially() {
    let mut manager = TerminalSessionManager::new();
    for index in 0..24 {
        manager
            .open(format!("seq_{index}"), spec())
            .unwrap_or_else(|error| panic!("sequential open {index} failed: {error}"));
    }
}

/// Many threads, each its own manager, opening at the same instant — what the
/// test harness does, and what several agents driving one host would do.
#[test]
fn many_threads_open_sessions_at_once() {
    let threads = 24;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let handles: Vec<_> = (0..threads)
        .map(|index| {
            let barrier = std::sync::Arc::clone(&barrier);
            let failures = std::sync::Arc::clone(&failures);
            std::thread::spawn(move || {
                let mut manager = TerminalSessionManager::new();
                barrier.wait();
                if let Err(error) = manager.open(format!("par_{index}"), spec()) {
                    #[allow(clippy::unwrap_used)]
                    failures.lock().unwrap().push(format!("{index}: {error}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            })
        })
        .collect();
    for handle in handles {
        let _joined = handle.join();
    }

    #[allow(clippy::unwrap_used)]
    let failures = failures.lock().unwrap();
    assert!(
        failures.is_empty(),
        "{} of {threads} concurrent opens failed: {:?}",
        failures.len(),
        *failures
    );
}
