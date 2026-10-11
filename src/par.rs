//! Parallelism helpers: a multi-threaded walk over local trees, blocking
//! work that stops when its future is dropped, and a limiter that caps how
//! many GIO operations one job keeps in flight.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use futures_channel::oneshot;
use gtk::gio;

/// Threads for CPU / syscall-bound work: every core.
pub fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

/// GIO operations one job keeps in flight. GIO runs local file work on its
/// own thread pool; remote backends pipeline requests. Past ~16 neither
/// gets faster, they just queue.
pub fn in_flight() -> usize {
    threads().clamp(4, 16)
}

/// Runs `work` on a GIO worker thread. Dropping the returned future (a
/// cancelled job) raises the flag `work` is given, so it can stop early.
/// None only if `work` panicked, which `panic = "abort"` rules out.
pub async fn blocking<T, F>(work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> T + Send + 'static,
{
    struct Raise(Arc<AtomicBool>);
    impl Drop for Raise {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let _raise = Raise(stop.clone());
    gio::spawn_blocking(move || work(&stop)).await.ok()
}

/// Lock that shrugs off poisoning; with `panic = "abort"` it can't happen.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Cancelled")
}

/// Visits `roots` and everything under them on all cores, without following
/// symlinks. `visit` sees each entry once, parents before their children
/// are listed (a directory's own entries may still be visited in any order),
/// and returns whether to go into it if it's a folder.
///
/// With `strict`, the first error (from `visit` or from listing a folder)
/// stops the walk and is returned; otherwise unreadable entries are skipped.
pub fn walk<F>(roots: Vec<PathBuf>, strict: bool, stop: &AtomicBool, visit: F) -> io::Result<()>
where
    F: Fn(&Path, &Metadata) -> io::Result<bool> + Sync,
{
    struct Queue {
        dirs: Vec<PathBuf>,
        busy: usize,
    }
    let queue = Mutex::new(Queue {
        dirs: Vec::new(),
        busy: 0,
    });
    let ready = Condvar::new();
    let failed: Mutex<Option<io::Error>> = Mutex::new(None);
    let halt = AtomicBool::new(false);
    let fail = |e: io::Error| {
        if strict {
            lock(&failed).get_or_insert(e);
            halt.store(true, Ordering::Relaxed);
            ready.notify_all();
        }
    };
    let halted = || halt.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed);

    for root in roots {
        match std::fs::symlink_metadata(&root) {
            Ok(md) => match visit(&root, &md) {
                Ok(true) if md.is_dir() => lock(&queue).dirs.push(root),
                Ok(_) => {}
                Err(e) => fail(e),
            },
            Err(e) => fail(e),
        }
    }

    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| loop {
                let dir = {
                    let mut q = lock(&queue);
                    loop {
                        if halted() {
                            ready.notify_all();
                            return;
                        }
                        if let Some(d) = q.dirs.pop() {
                            q.busy += 1;
                            break d;
                        }
                        if q.busy == 0 {
                            ready.notify_all();
                            return;
                        }
                        q = ready.wait(q).unwrap_or_else(PoisonError::into_inner);
                    }
                };
                let mut found = Vec::new();
                match std::fs::read_dir(&dir) {
                    Ok(entries) => {
                        for entry in entries {
                            if halted() {
                                break;
                            }
                            // DirEntry::metadata doesn't follow symlinks.
                            let r = entry.and_then(|e| {
                                let md = e.metadata()?;
                                let path = e.path();
                                if visit(&path, &md)? && md.is_dir() {
                                    found.push(path);
                                }
                                Ok(())
                            });
                            if let Err(e) = r {
                                fail(e);
                            }
                        }
                    }
                    Err(e) => fail(e),
                }
                let mut q = lock(&queue);
                q.dirs.append(&mut found);
                q.busy -= 1;
                drop(q);
                ready.notify_all();
            });
        }
    });

    if let Some(e) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(e);
    }
    if stop.load(Ordering::Relaxed) {
        return Err(stopped());
    }
    Ok(())
}

/// Runs `f` over `items` on all cores; the first error stops the rest.
pub fn each<T, F>(items: &[T], stop: &AtomicBool, f: F) -> io::Result<()>
where
    T: Sync,
    F: Fn(&T) -> io::Result<()> + Sync,
{
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<io::Error>> = Mutex::new(None);
    let halt = AtomicBool::new(false);
    let workers = threads().min(items.len());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                while !halt.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                    let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        return;
                    };
                    if let Err(e) = f(item) {
                        lock(&failed).get_or_insert(e);
                        halt.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(e);
    }
    if stop.load(Ordering::Relaxed) {
        return Err(stopped());
    }
    Ok(())
}

/// A counting semaphore for futures on one thread. Permits are weighted so
/// a big file can take them all and run alone; waiters are served in order.
pub struct Limiter {
    size: usize,
    free: Cell<usize>,
    waiting: RefCell<VecDeque<(usize, oneshot::Sender<()>)>>,
}

pub struct Permit {
    limiter: Rc<Limiter>,
    n: usize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.free.set(self.limiter.free.get() + self.n);
        self.limiter.wake();
    }
}

impl Limiter {
    pub fn new(size: usize) -> Rc<Self> {
        Rc::new(Self {
            size: size.max(1),
            free: Cell::new(size.max(1)),
            waiting: RefCell::default(),
        })
    }

    /// Waits for `n` permits (at most all of them).
    pub async fn take(self: &Rc<Self>, n: usize) -> Permit {
        let n = n.clamp(1, self.size);
        if self.waiting.borrow().is_empty() && self.free.get() >= n {
            self.free.set(self.free.get() - n);
        } else {
            let (tx, rx) = oneshot::channel();
            self.waiting.borrow_mut().push_back((n, tx));
            // `wake` hands the permits over before it answers.
            let _ = rx.await;
        }
        Permit {
            limiter: self.clone(),
            n,
        }
    }

    /// Takes them all, for work that should run alone.
    pub async fn take_all(self: &Rc<Self>) -> Permit {
        self.take(self.size).await
    }

    fn wake(&self) {
        loop {
            let mut waiting = self.waiting.borrow_mut();
            let Some(&(n, _)) = waiting.front() else {
                return;
            };
            if self.free.get() < n {
                return;
            }
            let Some((_, tx)) = waiting.pop_front() else {
                return;
            };
            drop(waiting);
            self.free.set(self.free.get() - n);
            if tx.send(()).is_err() {
                // The waiter was dropped (job cancelled); give them back.
                self.free.set(self.free.get() + n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use gtk::glib;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn walk_sees_everything_once() {
        let dir = std::env::temp_dir().join(format!("abyss-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..20 {
            let sub = dir.join(format!("d{i}/e/f"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("x"), "abc").unwrap();
            std::fs::write(dir.join(format!("d{i}/y")), "z").unwrap();
        }
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
        let (n, bytes) = (AtomicU64::new(0), AtomicU64::new(0));
        let stop = AtomicBool::new(false);
        walk(vec![dir.clone()], true, &stop, |_, md| {
            n.fetch_add(1, Ordering::Relaxed);
            if md.is_file() {
                bytes.fetch_add(md.len(), Ordering::Relaxed);
            }
            Ok(true)
        })
        .unwrap();
        // root + loop link + 20 × (d, e, f, x, y)
        assert_eq!(n.load(Ordering::Relaxed), 2 + 20 * 5);
        assert_eq!(bytes.load(Ordering::Relaxed), 20 * 4);
        // A strict walk stops on the first error.
        let e = walk(vec![dir.clone()], true, &stop, |p, _| {
            if p.ends_with("y") {
                Err(io::Error::other("boom"))
            } else {
                Ok(true)
            }
        })
        .unwrap_err();
        assert_eq!(e.to_string(), "boom");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn limiter_caps_and_orders() {
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            ctx.block_on(async {
                let lim = Limiter::new(2);
                let a = lim.take(1).await;
                let b = lim.take(1).await;
                assert_eq!(lim.free.get(), 0);
                let all = lim.take_all();
                futures_util::pin_mut!(all);
                assert!(all.as_mut().now_or_never().is_none());
                drop(a);
                assert!(all.as_mut().now_or_never().is_none());
                drop(b);
                let all = all.await;
                assert_eq!(lim.free.get(), 0);
                drop(all);
                assert_eq!(lim.free.get(), 2);
            });
        })
        .unwrap();
    }
}
