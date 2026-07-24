use std::sync::{Mutex, MutexGuard};

/// Locks `mutex`, recovering from poisoning instead of panicking. These
/// mutexes guard in-memory coordination state only; anything durable and
/// uncertain already flows to `NeedsReconciliation`, so continuing with the
/// recovered guard is safe. The poison flag is cleared after recovery, so one
/// error event is emitted per poisoning incident, not per acquisition.
pub fn lock_recovering<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!(lock = name, "mutex.poison_recovered");
            drop(poisoned.into_inner());
            mutex.clear_poison();
            // A concurrent panic could have re-poisoned the mutex between the
            // clear and this acquisition; recover without a second event.
            mutex.lock().unwrap_or_else(|re| re.into_inner())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn poisoned_mutex_recovers_and_logs_once() {
        let sink = crate::test_support::CaptureSink::install();
        let mutex = Arc::new(Mutex::new(42u32));
        let clone = Arc::clone(&mutex);
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock().unwrap();
            panic!("deliberate poisoning");
        })
        .join();
        {
            let mut guard = lock_recovering(&mutex, "test.lock");
            *guard += 1;
        }
        assert_eq!(*lock_recovering(&mutex, "test.lock"), 43);
        let recoveries = sink
            .events()
            .into_iter()
            .filter(|event| event["fields"]["message"] == "mutex.poison_recovered")
            .count();
        assert_eq!(recoveries, 1);
    }
}
