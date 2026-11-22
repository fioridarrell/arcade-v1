//! A mutex that says who held it too long (2026-11-22: on the GX10 every link's ARCADE step waited up to 44 s for the
//! ARCADE chain's lock, and nothing said who held it). A hold longer than HELD_LONG, and a wait longer than WAIT_LONG,
//! are written to the log with the code that took the lock.

use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const HELD_LONG: Duration = Duration::from_secs(1);
const WAIT_LONG: Duration = Duration::from_secs(2);

pub struct HeldLock<T> {
    name: &'static str,
    m: Mutex<T>,
}

pub struct Held<'a, T> {
    g: MutexGuard<'a, T>,
    name: &'static str,
    at: &'static Location<'static>,
    since: Instant,
}

impl<T> HeldLock<T> {
    pub const fn new(name: &'static str, v: T) -> Self {
        HeldLock { name, m: Mutex::new(v) }
    }

    #[track_caller]
    pub fn lock(&self) -> LockResult<Held<'_, T>> {
        let at = Location::caller();
        let asked = Instant::now();
        let (g, poisoned) = match self.m.lock() {
            Ok(g) => (g, false),
            Err(e) => (e.into_inner(), true),
        };
        let waited = asked.elapsed();
        if waited > WAIT_LONG {
            eprintln!("lock {}: {}:{} waited {:.1} s", self.name, at.file(), at.line(), waited.as_secs_f64());
        }
        let h = Held { g, name: self.name, at, since: Instant::now() };
        if poisoned { Err(PoisonError::new(h)) } else { Ok(h) }
    }
}

impl<T> Deref for Held<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

impl<T> DerefMut for Held<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.g
    }
}

impl<T> Drop for Held<'_, T> {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        if held > HELD_LONG {
            eprintln!("lock {}: {}:{} held {:.1} s", self.name, self.at.file(), self.at.line(), held.as_secs_f64());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_and_poison_pass_through() {
        let l = HeldLock::new("test", 1u32);
        *l.lock().unwrap() += 1;
        assert_eq!(*l.lock().unwrap(), 2);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = l.lock().unwrap();
            panic!("poison it");
        }));
        assert_eq!(*l.lock().unwrap_or_else(|e| e.into_inner()), 2);
    }
}
