use std::{collections::VecDeque, sync::Mutex, thread};

/// Applies `operation` with at most `limit` worker threads and restores input order.
pub(crate) fn bounded_map<T, U, F>(items: Vec<T>, limit: usize, operation: &F) -> Vec<U>
where
    T: Send,
    U: Send,
    F: Fn(T) -> U + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let worker_count = limit.max(1).min(items.len());
    let queue = Mutex::new(items.into_iter().enumerate().collect::<VecDeque<_>>());
    let completed = Mutex::new(Vec::new());

    thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let work = queue
                        .lock()
                        .expect("review work queue poisoned")
                        .pop_front();
                    let Some((index, item)) = work else { break };
                    let output = operation(item);
                    completed
                        .lock()
                        .expect("review result queue poisoned")
                        .push((index, output));
                }
            });
        }
    });

    let mut completed = completed
        .into_inner()
        .expect("review result queue poisoned");
    completed.sort_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, output)| output).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::bounded_map;

    #[test]
    fn concurrency_is_real_bounded_and_output_order_is_stable() {
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let first_wave = Arc::new(Barrier::new(3));

        let output = bounded_map((0_u64..6).collect(), 3, &|item| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(now, Ordering::SeqCst);
            if item < 3 {
                first_wave.wait();
            }
            std::thread::sleep(Duration::from_millis((6 - item) * 2));
            active.fetch_sub(1, Ordering::SeqCst);
            item * item
        });

        assert_eq!(output, vec![0, 1, 4, 9, 16, 25]);
        assert_eq!(maximum.load(Ordering::SeqCst), 3);
    }
}
