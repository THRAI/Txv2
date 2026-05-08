use tx_substrate::SpinMutex;

#[test]
fn spinmutex_lock_unlock_round_trip() {
    // Construct via the public `const fn new`, then take and mutate
    // through the guard. Drop the guard, re-lock, and confirm the
    // previous mutation is observed — proves both `Deref`/`DerefMut`
    // and the release path on drop.
    let mutex = SpinMutex::new(0u64);
    {
        let mut guard = mutex.lock();
        *guard = 7;
    }
    {
        let guard = mutex.lock();
        assert_eq!(*guard, 7);
    }
    // Re-lock once more to confirm the lock is releaseable across
    // multiple acquire/release cycles.
    {
        let mut guard = mutex.lock();
        *guard += 1;
    }
    assert_eq!(*mutex.lock(), 8);
}

#[test]
fn spinmutex_holds_send_payload() {
    // `T: Send` lets `SpinMutex<T>` be `Sync`. Use a thread to prove
    // the `unsafe impl<T: Send> Sync` is honoured at the type level.
    let mutex = std::sync::Arc::new(SpinMutex::new(0i32));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let m = std::sync::Arc::clone(&mutex);
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    let mut g = m.lock();
                    *g += 1;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread join");
    }
    assert_eq!(*mutex.lock(), 4_000);
}
