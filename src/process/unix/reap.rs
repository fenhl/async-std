use std::ops::Deref;
use std::pin::Pin;

use super::orphan::{OrphanQueue, Wait};
use crate::io;
use crate::prelude::*;
use crate::process::{kill::Kill, ExitStatus};
use crate::stream::Stream;
use crate::task::{Context, Poll};

/// Orchestrates between registering interest for receiving signals when a
/// child process has exited, and attempting to poll for process completion.
#[derive(Debug)]
pub(crate) struct Reaper<W, Q, S>
where
    W: Wait,
    Q: OrphanQueue<W>,
{
    inner: Option<W>,
    orphan_queue: Q,
    signal: S,
}

impl<W, Q, S> Deref for Reaper<W, Q, S>
where
    W: Wait,
    Q: OrphanQueue<W>,
{
    type Target = W;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().unwrap()
    }
}

impl<W, Q, S> Reaper<W, Q, S>
where
    W: Wait,
    Q: OrphanQueue<W>,
{
    pin_utils::unsafe_pinned!(signal: S);
    pin_utils::unsafe_unpinned!(inner: Option<W>);

    pub(crate) fn new(inner: W, orphan_queue: Q, signal: S) -> Self {
        Self {
            inner: Some(inner),
            orphan_queue,
            signal,
        }
    }
}

impl<W, Q, S> Future for Reaper<W, Q, S>
where
    W: Wait,
    Q: OrphanQueue<W>,
    S: Stream<Item = io::Result<libc::c_int>> + Unpin + Sized,
{
    type Output = io::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<<Self as Future>::Output> {
        loop {
            // If the child hasn't exited yet, then it's our responsibility to
            // ensure the current task gets notified when it might be able to
            // make progress.
            //
            // As described in `spawn` above, we just indicate that we can
            // next make progress once a SIGCHLD is received.
            //
            // However, we will register for a notification on the next signal
            // BEFORE we poll the child. Otherwise it is possible that the child
            // can exit and the signal can arrive after we last polled the child,
            // but before we've registered for a notification on the next signal
            // (this can cause a deadlock if there are no more spawned children
            // which can generate a different signal for us). A side effect of
            // pre-registering for signal notifications is that when the child
            // exits, we will have already registered for an additional
            // notification we don't need to consume. If another signal arrives,
            // this future's task will be notified/woken up again. Since the
            // futures model allows for spurious wake ups this extra wakeup
            // should not cause significant issues with parent futures.
            let registered_interest = !self.as_mut().signal().poll_next(cx)?.is_ready();

            self.orphan_queue.reap_orphans();
            if let Some(status) = self.as_mut().inner().as_mut().unwrap().try_wait()? {
                return Poll::Ready(Ok(status));
            }

            // If our attempt to poll for the next signal was not ready, then
            // we've arranged for our task to get notified and we can bail out.
            if registered_interest {
                return Poll::Pending;
            } else {
                // Otherwise, if the signal stream delivered a signal to us, we
                // won't get notified at the next signal, so we'll loop and try
                // again.
                continue;
            }
        }
    }
}

impl<W, Q, S> Kill for Reaper<W, Q, S>
where
    W: Kill + Wait,
    Q: OrphanQueue<W>,
{
    fn kill(&mut self) -> io::Result<()> {
        self.inner.as_mut().unwrap().kill()
    }
}

impl<W, Q, S> Drop for Reaper<W, Q, S>
where
    W: Wait,
    Q: OrphanQueue<W>,
{
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.inner.as_mut().unwrap().try_wait() {
            return;
        }

        let orphan = self.inner.take().unwrap();
        self.orphan_queue.push_orphan(orphan);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::process::ExitStatus;
    use std::cell::{Cell, RefCell};
    use std::os::unix::process::ExitStatusExt;

    #[derive(Debug)]
    struct MockWait {
        total_kills: usize,
        total_waits: usize,
        num_wait_until_status: usize,
        status: ExitStatus,
    }

    impl MockWait {
        fn new(status: ExitStatus, num_wait_until_status: usize) -> Self {
            Self {
                total_kills: 0,
                total_waits: 0,
                num_wait_until_status,
                status,
            }
        }
    }

    impl Wait for MockWait {
        fn id(&self) -> u32 {
            0
        }

        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            let ret = if self.num_wait_until_status == self.total_waits {
                Some(self.status)
            } else {
                None
            };

            self.total_waits += 1;
            Ok(ret)
        }
    }

    impl Kill for MockWait {
        fn kill(&mut self) -> io::Result<()> {
            self.total_kills += 1;
            Ok(())
        }
    }

    struct MockStream {
        total_polls: usize,
        values: Vec<Option<()>>,
    }

    impl MockStream {
        fn new(values: Vec<Option<()>>) -> Self {
            Self {
                total_polls: 0,
                values,
            }
        }
    }

    impl Stream for MockStream {
        type Item = io::Result<()>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.total_polls += 1;
            match self.values.remove(0) {
                Some(()) => Poll::Ready(Some(Ok(()))),
                None => Poll::Pending,
            }
        }
    }

    struct MockQueue<W> {
        all_enqueued: RefCell<Vec<W>>,
        total_reaps: Cell<usize>,
    }

    impl<W> MockQueue<W> {
        fn new() -> Self {
            Self {
                all_enqueued: RefCell::new(Vec::new()),
                total_reaps: Cell::new(0),
            }
        }
    }

    impl<W: Wait> OrphanQueue<W> for MockQueue<W> {
        fn push_orphan(&self, orphan: W) {
            self.all_enqueued.borrow_mut().push(orphan);
        }

        fn reap_orphans(&self) {
            self.total_reaps.set(self.total_reaps.get() + 1);
        }
    }

    // #[test]
    // fn reaper() {
    //     let exit = ExitStatus::from_raw(0);
    //     let mock = MockWait::new(exit, 3);
    //     let mut grim = Reaper::new(
    //         mock,
    //         MockQueue::new(),
    //         MockStream::new(vec![None, Some(()), None, None, None]),
    //     );

    //     // Not yet exited, interest registered
    //     assert_eq!(Poll::Pending, grim.poll().expect("failed to wait"));
    //     assert_eq!(1, grim.signal.total_polls);
    //     assert_eq!(1, grim.total_waits);
    //     assert_eq!(1, grim.orphan_queue.total_reaps.get());
    //     assert!(grim.orphan_queue.all_enqueued.borrow().is_empty());

    //     // Not yet exited, couldn't register interest the first time
    //     // but managed to register interest the second time around
    //     assert_eq!(Poll::Pending, grim.poll().expect("failed to wait"));
    //     assert_eq!(3, grim.signal.total_polls);
    //     assert_eq!(3, grim.total_waits);
    //     assert_eq!(3, grim.orphan_queue.total_reaps.get());
    //     assert!(grim.orphan_queue.all_enqueued.borrow().is_empty());

    //     // Exited
    //     assert_eq!(Poll::Ready(exit), grim.poll().expect("failed to wait"));
    //     assert_eq!(4, grim.signal.total_polls);
    //     assert_eq!(4, grim.total_waits);
    //     assert_eq!(4, grim.orphan_queue.total_reaps.get());
    //     assert!(grim.orphan_queue.all_enqueued.borrow().is_empty());
    // }

    #[test]
    fn kill() {
        let exit = ExitStatus::from_raw(0);
        let mut grim = Reaper::new(
            MockWait::new(exit, 0),
            MockQueue::new(),
            MockStream::new(vec![None]),
        );

        grim.kill().unwrap();
        assert_eq!(1, grim.total_kills);
        assert_eq!(0, grim.orphan_queue.total_reaps.get());
        assert!(grim.orphan_queue.all_enqueued.borrow().is_empty());
    }

    #[test]
    fn drop_reaps_if_possible() {
        let exit = ExitStatus::from_raw(0);
        let mut mock = MockWait::new(exit, 0);

        {
            let queue = MockQueue::new();

            let grim = Reaper::new(&mut mock, &queue, MockStream::new(vec![]));

            drop(grim);

            assert_eq!(0, queue.total_reaps.get());
            assert!(queue.all_enqueued.borrow().is_empty());
        }

        assert_eq!(1, mock.total_waits);
        assert_eq!(0, mock.total_kills);
    }

    #[test]
    fn drop_enqueues_orphan_if_wait_fails() {
        let exit = ExitStatus::from_raw(0);
        let mut mock = MockWait::new(exit, 2);

        {
            let queue = MockQueue::<&mut MockWait>::new();
            let grim = Reaper::new(&mut mock, &queue, MockStream::new(vec![]));
            drop(grim);

            assert_eq!(0, queue.total_reaps.get());
            assert_eq!(1, queue.all_enqueued.borrow().len());
        }

        assert_eq!(1, mock.total_waits);
        assert_eq!(0, mock.total_kills);
    }
}
