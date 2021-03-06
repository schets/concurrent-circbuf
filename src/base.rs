//! A concurrent circular buffer.
//!
//! The data structure can be thought of as a dynamically growable and shrinkable buffer that has
//! two ends: tx and rx. A [`CircBuf`] can [`send`] elements into the tx end and
//! [`try_recv`][CircBuf::try_recv] elements from the rx end. A [`CircBuf`] doesn't implement `Sync`
//! so it cannot be shared among multiple threads. However, it can create [`Receiver`]s, and those
//! can be easily cloned, shared, and sent to other threads. [`Receiver`]s can only
//! [`try_recv`][Receiver::try_recv] elements from the rx end.
//!
//! Here's a visualization of the data structure:
//!
//! ```text
//!                         rx
//!                          _
//!    CircBuf::try_recv -> | | <- Receiver::try_recv
//!                         | |
//!                         | |
//!                         | |
//!        CircBuf::send -> |_|
//!
//!                         tx
//! ```
//!
//! # Fair work-stealing schedulers
//!
//! Usually, the data structure is used in fair work-stealing schedulers as follows.
//!
//! There are a number of threads. Each thread owns a [`CircBuf`] and creates a [`Receiver`] that is
//! shared among all other threads. Alternatively, it creates multiple [`Receiver`]s - one for each
//! of the other threads.
//!
//! Then, all threads are executing in a loop. In the loop, each one attempts to
//! [`try_recv`][CircBuf::try_recv] some work from its own [`CircBuf`]. But if it is empty, it
//! attempts to [`try_recv`][Receiver::try_recv] work from some other thread instead. When executing
//! work (or being idle), a thread may produce more work, which gets [`send`]ed into its
//! [`CircBuf`].
//!
//! It is worth noting that it is discouraged to use work-stealing deque for fair schedulers,
//! because its `pop()` may return the work that is just `push()`ed, effectively scheduling the same
//! work repeatedly.
//!
//! [`CircBuf`]: struct.CircBuf.html
//! [`Receiver`]: struct.Receiver.html
//! [`send`]: struct.CircBuf.html#method.send
//! [CircBuf::try_recv]: struct.CircBuf.html#method.try_recv
//! [Receiver::try_recv]: struct.Receiver.html#method.try_recv

use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::thread;
use std::sync::Arc;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;

use epoch::{self, Atomic, Owned};
use utils::cache_padded::CachePadded;

/// Minimum capacity for a circular buffer.
const DEFAULT_MIN_CAP: usize = 16;

/// If an array of at least this size is retired, thread-local garbage is flushed so that it gets
/// deallocated as soon as possible.
const FLUSH_THRESHOLD_BYTES: usize = 1 << 10;

/// An array that holds elements in a circular buffer.
struct Array<T> {
    /// Pointer to the allocated memory.
    ptr: *mut T,

    /// Capacity of the array. Always a power of two.
    cap: usize,
}

unsafe impl<T> Send for Array<T> {}

impl<T> Array<T> {
    /// Returns a new array with the specified capacity.
    fn new(cap: usize) -> Self {
        debug_assert_eq!(cap, cap.next_power_of_two());

        let mut v = Vec::with_capacity(cap);
        let ptr = v.as_mut_ptr();
        mem::forget(v);

        Array { ptr, cap }
    }

    /// Returns a pointer to the element at the specified `index`.
    unsafe fn at(&self, index: isize) -> *mut T {
        // `self.cap` is always a power of two.
        self.ptr.offset(index & (self.cap - 1) as isize)
    }

    /// Writes `value` into the specified `index`.
    unsafe fn write(&self, index: isize, value: T) {
        ptr::write(self.at(index), value)
    }

    /// Reads a value from the specified `index`.
    unsafe fn read(&self, index: isize) -> T {
        ptr::read(self.at(index))
    }
}

impl<T> Drop for Array<T> {
    fn drop(&mut self) {
        unsafe {
            drop(Vec::from_raw_parts(self.ptr, 0, self.cap));
        }
    }
}

/// Errors in receiving data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// Lost the race for receiving data to another concurrent operation. Try again.
    Retry,
}

/// Internal data that is shared among a circular buffer and its receivers.
struct Inner<T> {
    /// The rx index.
    rx: AtomicIsize,

    /// The tx index.
    tx: AtomicIsize,

    /// The underlying array.
    array: Atomic<Array<T>>,

    /// Minimum capacity of the array. Always a power of two.
    min_cap: usize,
}

impl<T> Inner<T> {
    /// Returns a new `Inner` with default minimum capacity.
    fn new() -> Self {
        Self::with_min_capacity(DEFAULT_MIN_CAP)
    }

    /// Returns a new `Inner` with minimum capacity of `min_cap` rounded to the next power of two.
    fn with_min_capacity(min_cap: usize) -> Self {
        let power = min_cap.next_power_of_two();
        assert!(power >= min_cap, "capacity too large: {}", min_cap);
        Inner {
            rx: AtomicIsize::new(0),
            tx: AtomicIsize::new(0),
            array: Atomic::new(Array::new(power)),
            min_cap: power,
        }
    }

    /// Resizes the internal array to the new capacity of `new_cap`.
    #[cold]
    unsafe fn resize(&self, new_cap: usize) {
        // Load rx, tx, and array.
        let rx = self.rx.load(Ordering::Relaxed);
        let tx = self.tx.load(Ordering::Relaxed);
        let array = self.array.load(Ordering::Relaxed, epoch::unprotected());

        // Allocate a new array.
        let new = Array::new(new_cap);

        // Copy data from the old array to the new one.
        let mut i = rx;
        while i != tx {
            ptr::copy_nonoverlapping(array.deref().at(i), new.at(i), 1);
            i = i.wrapping_add(1);
        }

        let guard = &epoch::pin();
        let new = Owned::new(new).into_shared(guard);

        // Store the new array.
        self.array.store(new, Ordering::Release);

        // Destroy the old array later.
        guard.defer(move || array.into_owned());

        // If the array is very large, then flush the thread-local garbage in order to
        // deallocate it as soon as possible.
        if mem::size_of::<T>() * new_cap >= FLUSH_THRESHOLD_BYTES {
            guard.flush();
        }
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        // Load rx, tx, and array.
        let rx = self.rx.load(Ordering::Relaxed);
        let tx = self.tx.load(Ordering::Relaxed);

        unsafe {
            let array = self.array.load(Ordering::Relaxed, epoch::unprotected());

            // Go through the array from rx to tx and drop all elements in the circular buffer.
            let mut i = rx;
            while i != tx {
                ptr::drop_in_place(array.deref().at(i));
                i = i.wrapping_add(1);
            }

            // Free the memory allocated by the array.
            drop(array.into_owned());
        }
    }
}

/// A concurrent circular buffer.
///
/// A circular buffer has two ends: rx and tx. Elements can be [`send`]ed into the tx end and
/// [`try_recv`][CircBuf::try_recv]ed from the rx end. The rx end is special in that receivers can
/// also receive from the rx end using [`try_recv`][Receiver::try_recv] method.
///
/// # Receivers
///
/// While [`CircBuf`] doesn't implement `Sync`, it can create [`Receiver`]s using the method
/// [`receiver`][receiver], and those can be easily shared among multiple threads. [`Receiver`]s can
/// only [`try_recv`][Receiver::try_recv] elements from the rx end of the circular buffer.
///
/// # Capacity
///
/// The data structure dynamically grows and shrinks as elements are inserted and removed from
/// it. If the internal array gets full, a new one twice the size of the original is
/// allocated. Similarly, if it is less than a quarter full, a new array half the size of the
/// original is allocated.
///
/// In order to prevent frequent resizing (reallocations may be costly), it is possible to specify a
/// large minimum capacity for the circular buffer by calling [`CircBuf::with_min_capacity`]. This
/// constructor will make sure that the internal array never shrinks below that size.
///
/// [`CircBuf`]: struct.CircBuf.html
/// [`Receiver`]: struct.Receiver.html
/// [`send`]: struct.CircBuf.html#method.send
/// [receiver]: struct.CircBuf.html#method.receiver
/// [`CircBuf::with_min_capacity`]: struct.CircBuf.html#method.with_min_capacity
/// [CircBuf::try_recv]: struct.CircBuf.html#method.try_recv
/// [Receiver::try_recv]: struct.Receiver.html#method.try_recv
pub struct CircBuf<T> {
    inner: Arc<CachePadded<Inner<T>>>,
    _marker: PhantomData<*mut ()>, // !Send + !Sync
}

unsafe impl<T: Send> Send for CircBuf<T> {}

impl<T> CircBuf<T> {
    /// Returns a new circular buffer.
    ///
    /// The internal array is destructed as soon as the circular buffer and all its receivers get dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::CircBuf;
    ///
    /// let cb = CircBuf::<i32>::new();
    /// ```
    pub fn new() -> CircBuf<T> {
        CircBuf {
            inner: Arc::new(CachePadded::new(Inner::new())),
            _marker: PhantomData,
        }
    }

    /// Returns a new circular buffer with the specified minimum capacity.
    ///
    /// If the capacity is not a power of two, it will be rounded up to the next one.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::CircBuf;
    ///
    /// // The minimum capacity will be rounded up to 1024.
    /// let cb = CircBuf::<i32>::with_min_capacity(1000);
    /// ```
    pub fn with_min_capacity(min_cap: usize) -> CircBuf<T> {
        CircBuf {
            inner: Arc::new(CachePadded::new(Inner::with_min_capacity(min_cap))),
            _marker: PhantomData,
        }
    }

    /// Sends an element into the tx end of the circular buffer.
    ///
    /// If the internal array is full, a new one twice the capacity of the current one will be
    /// allocated.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::CircBuf;
    ///
    /// let cb = CircBuf::new();
    /// cb.send(1);
    /// cb.send(2);
    /// ```
    pub fn send(&self, value: T) {
        unsafe {
            // Load rx, tx, and array. The array doesn't have to be epoch-protected because the
            // current thread (the worker) is the only one that grows and shrinks it.
            let mut array = self.inner.array.load(Ordering::Relaxed, epoch::unprotected());
            let tx = self.inner.tx.load(Ordering::Relaxed);
            let rx = self.inner.rx.load(Ordering::Acquire);

            // Calculate the length and the capacity of the circular buffer.
            let len = tx.wrapping_sub(rx);
            let cap = array.deref().cap;

            // If the circular buffer is full, grow the underlying array.
            if len >= cap as isize {
                self.inner.resize(2 * cap);
                array = self.inner.array.load(Ordering::Relaxed, epoch::unprotected());
            }

            // Write `value` into the right slot and increment `tx`.
            array.deref().write(tx, value);
            self.inner.tx.store(tx.wrapping_add(1), Ordering::Release);
        }
    }

    /// Receives an element from the rx end of the circular buffer.
    ///
    /// It returns `Ok(Some(v))` if a value `v` is received, and `Ok(None)` if the circular buffer
    /// is empty. Unlike most methods in concurrent data structures, if another operation gets in
    /// the way while attempting to receive data, this method will bail out immediately with
    /// [`RecvError::Retry`] instead of retrying.
    ///
    /// If the internal array is less than a quarter full, a new array half the capacity of the
    /// current one will be allocated.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::{CircBuf, RecvError};
    ///
    /// let cb = CircBuf::new();
    /// cb.send(1);
    /// cb.send(2);
    ///
    /// // Attempt to receive an element.
    /// //
    /// // It should return `Ok(Some(v))` for a value `v`, or `Err(RecvError::Retry)`.
    /// assert_ne!(cb.try_recv(), Ok(None));
    /// ```
    ///
    /// [`RecvError::Retry`]: enum.RecvError.html#variant.Retry
    pub fn try_recv(&self) -> Result<Option<T>, RecvError> {
        let tx = self.inner.tx.load(Ordering::Relaxed);
        let rx = self.inner.rx.load(Ordering::Relaxed);
        let len = tx.wrapping_sub(rx);

        // Is the circular buffer empty?
        if len <= 0 {
            return Ok(None);
        }

        // Try incrementing rx to receive a value.
        self.inner.rx
            .compare_exchange_weak(rx, rx.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed)
            .map(|_| unsafe {
                let buf = self.inner.array.load(Ordering::Relaxed, epoch::unprotected());
                let value = buf.deref().read(rx);

                // Shrink the array if `len - 1` is less than one fourth of `self.min_cap`.
                let cap = buf.deref().cap;
                if cap > self.inner.min_cap && len <= cap as isize / 4 {
                    self.inner.resize(cap / 2);
                }

                Some(value)
            })
            .map_err(|_| RecvError::Retry)
    }

    /// Receives an element from the rx end of the circular buffer.
    ///
    /// It returns `Some(v)` if a value `v` is received, and `None` if the circular buffer is empty.
    ///
    /// If the internal array is less than a quarter full, a new array half the capacity of the
    /// current one will be allocated.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::{CircBuf, RecvError};
    ///
    /// let cb = CircBuf::new();
    /// cb.send(1);
    /// cb.send(2);
    ///
    /// // Receive an element.
    /// assert_eq!(cb.recv(), Some(1));
    /// assert_eq!(cb.recv(), Some(2));
    /// ```
    pub fn recv(&self) -> Option<T> {
        let tx = self.inner.tx.load(Ordering::Relaxed);
        let mut rx = self.inner.rx.load(Ordering::Relaxed);

        loop {
            let len = tx.wrapping_sub(rx);

            // Is the circular buffer empty?
            if len <= 0 {
                return None;
            }

            // Try incrementing rx to receive a value.
            match self.inner.rx.compare_exchange_weak(rx, rx.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed) {
                Err(rx_cur) => {
                    rx = rx_cur;
                    thread::yield_now();
                }
                Ok(_) => unsafe {
                    let buf = self.inner.array.load(Ordering::Relaxed, epoch::unprotected());
                    let value = buf.deref().read(rx);

                    // Shrink the array if `len - 1` is less than one fourth of `self.min_cap`.
                    let cap = buf.deref().cap;
                    if cap > self.inner.min_cap && len <= cap as isize / 4 {
                        self.inner.resize(cap / 2);
                    }

                    return Some(value);
                }
            }
        }
    }

    /// Creates a receiver that can be shared with other threads.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::CircBuf;
    /// use std::thread;
    ///
    /// let cb = CircBuf::new();
    /// cb.send(1);
    /// cb.send(2);
    ///
    /// let r = cb.receiver();
    ///
    /// thread::spawn(move || {
    ///     assert_eq!(r.try_recv(), Ok(Some(1)));
    /// }).join().unwrap();
    /// ```
    pub fn receiver(&self) -> Receiver<T> {
        Receiver {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> fmt::Debug for CircBuf<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CircBuf {{ ... }}")
    }
}

impl<T> Default for CircBuf<T> {
    fn default() -> CircBuf<T> {
        CircBuf::new()
    }
}

/// A receiver that receives elements from the rx end of a circular buffer.
///
/// You can receive an element from the rx end using [`try_recv`]. If there are no concurrent
/// receive operation, you can receive an element from the rx end using [`recv_exclusive`].
///
/// Receivers can be cloned in order to create more of them. They also implement `Send` and `Sync`
/// so they can be easily shared among multiple threads.
///
/// [`try_recv`]: struct.Receiver.html#method.try_recv
/// [`recv_exclusive`]: struct.Receiver.html#method.recv_exclusive
pub struct Receiver<T> {
    inner: Arc<CachePadded<Inner<T>>>,
    _marker: PhantomData<*mut ()>, // !Send + !Sync
}

unsafe impl<T: Send> Send for Receiver<T> {}
unsafe impl<T: Send> Sync for Receiver<T> {}

impl<T> Receiver<T> {
    /// Receives an element from the rx end of its circular buffer.
    ///
    /// It returns `Ok(Some(v))` if a value `v` is received, and `Ok(None)` if the circular buffer
    /// is empty. Unlike most methods in concurrent data structures, if another operation gets in
    /// the way while attempting to receive data, this method will bail out immediately with
    /// [`RecvError::Retry`] instead of retrying.
    ///
    /// This method will not attempt to resize the internal array.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::{CircBuf, RecvError};
    ///
    /// let cb = CircBuf::new();
    /// let r = cb.receiver();
    /// cb.send(1);
    /// cb.send(2);
    ///
    /// // Attempt to receive an element, but keep retrying if we get `Retry`.
    /// let stolen = loop {
    ///     match r.try_recv() {
    ///         Ok(r) => break r,
    ///         Err(RecvError::Retry) => {}
    ///     }
    /// };
    /// assert_eq!(stolen, Some(1));
    /// ```
    ///
    /// [`RecvError::Retry`]: enum.RecvError.html#variant.Retry
    pub fn try_recv(&self) -> Result<Option<T>, RecvError> {
        // Load rx and tx.
        let rx = self.inner.rx.load(Ordering::Relaxed);
        let tx = self.inner.tx.load(Ordering::Acquire);

        // Is the circular buffer empty?
        if tx.wrapping_sub(rx) <= 0 {
            return Ok(None);
        }

        // Load array and read the data at the rx end.
        let value = {
            let guard = &epoch::pin();
            let array = self.inner.array.load(Ordering::Acquire, guard);
            unsafe { array.deref().read(rx) }
        };

        // Try incrementing rx to receive the value.
        if self.inner
            .rx
            .compare_exchange_weak(rx, rx.wrapping_add(1), Ordering::Release, Ordering::Relaxed)
            .is_ok() {
            Ok(Some(value))
        } else {
            // We didn't receive this value, forget it.
            mem::forget(value);
            Err(RecvError::Retry)
        }
    }

    /// Receives an element from the rx end of its circular buffer.
    ///
    /// It returns `Some(v)` if a value `v` is received, and `None` if the circular buffer is empty.
    ///
    /// This method will not attempt to resize the internal array.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::{CircBuf, RecvError};
    ///
    /// let cb = CircBuf::new();
    /// let r = cb.receiver();
    /// cb.send(1);
    /// cb.send(2);
    ///
    /// assert_eq!(r.recv(), Some(1));
    /// assert_eq!(r.recv(), Some(2));
    /// ```
    pub fn recv(&self) -> Option<T> {
        // Load rx and tx.
        let mut rx = self.inner.rx.load(Ordering::Relaxed);
        let tx = self.inner.tx.load(Ordering::Acquire);

        let guard = &epoch::pin();

        loop {
            // Is the circular buffer empty?
            if tx.wrapping_sub(rx) <= 0 {
                return None;
            }

            // Load array and read the data at the rx end.
            let array = self.inner.array.load(Ordering::Acquire, guard);
            let value = unsafe { array.deref().read(rx) };

            // Try incrementing rx to receive the value.
            match self.inner.rx.compare_exchange_weak(rx, rx.wrapping_add(1), Ordering::Release, Ordering::Relaxed) {
                Err(rx_cur) => {
                    // We didn't receive this value, forget it.
                    mem::forget(value);

                    rx = rx_cur;
                    thread::yield_now();
                }
                Ok(_) => {
                    return Some(value);
                }
            }
        }
    }

    /// Receives an element from the rx end of its circular buffer.
    ///
    /// It returns `Some(v)` if a value `v` is received, and `None` if the circular buffer is empty.
    ///
    /// This method will not attempt to resize the internal array.
    ///
    /// # Safety
    ///
    /// You have to guarantee that there are no concurrent receive operations, such as
    /// [`CircBuf::try_recv`], [`Receiver::try_recv`], and [`recv_exclusive`]. In other words, other
    /// receive operations should happen either before or after it.
    ///
    /// This method will not attempt to resize the internal array.
    ///
    /// # Examples
    ///
    /// ```
    /// use concurrent_circbuf::base::{CircBuf, RecvError};
    ///
    /// let cb = CircBuf::new();
    /// let r = cb.receiver();
    /// cb.send(1);
    /// cb.send(2);
    ///
    /// // Attempt to receive an element.
    /// let stolen = unsafe { r.recv_exclusive() };
    /// assert_eq!(stolen, Some(1));
    /// ```
    ///
    /// [`RecvError::Retry`]: enum.RecvError.html#variant.Retry
    /// [`CircBuf::try_recv`]: struct.CircBuf.html#method.try_recv
    /// [`Receiver::try_recv`]: struct.Receiver.html#method.try_recv
    /// [`recv_exclusive`]: struct.Receiver.html#method.recv_exclusive
    pub unsafe fn recv_exclusive(&self) -> Option<T> {
        // Load rx and tx.
        let rx = self.inner.rx.load(Ordering::Relaxed);
        let tx = self.inner.tx.load(Ordering::Acquire);

        // Is the circular buffer empty?
        if tx.wrapping_sub(rx) <= 0 {
            return None;
        }

        // Load array and read the data at the rx end.
        let value = {
            let guard = &epoch::pin();
            let buf = self.inner.array.load(Ordering::Acquire, guard);
            buf.deref().read(rx)
        };

        // Increment rx to receive the value.
        self.inner.rx.store(rx.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

impl<T> Clone for Receiver<T> {
    /// Creates another receiver.
    fn clone(&self) -> Receiver<T> {
        Receiver {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Receiver {{ ... }}")
    }
}

#[cfg(test)]
mod tests {
    extern crate rand;

    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::atomic::Ordering::SeqCst;
    use std::thread;

    use epoch;
    use self::rand::Rng;

    use super::{CircBuf, RecvError};

    fn retry<T, F: Fn() -> Result<T, RecvError>>(f: F) -> T {
        loop {
            match f() {
                Ok(r) => break r,
                Err(RecvError::Retry) => {}
            }
        }
    }

    #[test]
    fn smoke() {
        let cb = CircBuf::new();
        let r = cb.receiver();

        assert_eq!(retry(|| cb.try_recv()), None);
        assert_eq!(retry(|| r.try_recv()), None);

        cb.send(1);
        assert_eq!(retry(|| cb.try_recv()), Some(1));
        assert_eq!(retry(|| cb.try_recv()), None);
        assert_eq!(retry(|| r.try_recv()), None);

        cb.send(2);
        assert_eq!(r.recv(), Some(2));
        assert_eq!(r.recv(), None);
        assert_eq!(cb.recv(), None);

        cb.send(3);
        cb.send(4);
        cb.send(5);
        assert_eq!(cb.recv(), Some(3));
        assert_eq!(r.recv(), Some(4));
        assert_eq!(cb.recv(), Some(5));
        assert_eq!(cb.recv(), None);
    }

    #[test]
    fn try_recv_send() {
        const STEPS: usize = 50_000;

        let cb = CircBuf::new();
        let r = cb.receiver();
        let t = thread::spawn(move || {
            for i in 0..STEPS {
                loop {
                    if let Ok(Some(v)) = r.try_recv() {
                        assert_eq!(i, v);
                        break;
                    }
                }
            }
        });

        for i in 0..STEPS {
            cb.send(i);
        }
        t.join().unwrap();
    }

    #[test]
    fn stampede() {
        const COUNT: usize = 50_000;

        let cb = CircBuf::new();

        for i in 0..COUNT {
            cb.send(Box::new(i + 1));
        }
        let remaining = Arc::new(AtomicUsize::new(COUNT));

        let threads = (0..8)
            .map(|_| {
                let r = cb.receiver();
                let remaining = remaining.clone();

                thread::spawn(move || {
                    let mut last = 0;
                    while remaining.load(SeqCst) > 0 {
                        if let Ok(Some(x)) = r.try_recv() {
                            assert!(last < *x);
                            last = *x;
                            remaining.fetch_sub(1, SeqCst);
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        while remaining.load(SeqCst) > 0 {
            if let Ok(Some(_)) = cb.try_recv() {
                remaining.fetch_sub(1, SeqCst);
            }
        }

        for t in threads {
            t.join().unwrap();
        }
    }

    fn run_stress() {
        const COUNT: usize = 50_000;

        let cb = CircBuf::new();
        let done = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));

        let threads = (0..8)
            .map(|_| {
                let r = cb.receiver();
                let done = done.clone();
                let hits = hits.clone();

                thread::spawn(move || {
                    while !done.load(SeqCst) {
                        if r.recv().is_some() {
                            hits.fetch_add(1, SeqCst);
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        let mut rng = rand::thread_rng();
        let mut expected = 0;
        while expected < COUNT {
            if rng.gen_range(0, 3) == 0 {
                if cb.recv().is_some() {
                    hits.fetch_add(1, SeqCst);
                }
            } else {
                cb.send(expected);
                expected += 1;
            }
        }

        while hits.load(SeqCst) < COUNT {
            if cb.recv().is_some() {
                hits.fetch_add(1, SeqCst);
            }
        }
        done.store(true, SeqCst);

        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn stress() {
        run_stress();
    }

    #[test]
    fn stress_pinned() {
        let _guard = epoch::pin();
        run_stress();
    }

    #[test]
    fn no_starvation() {
        const COUNT: usize = 50_000;

        let cb = CircBuf::new();
        let done = Arc::new(AtomicBool::new(false));

        let (threads, hits): (Vec<_>, Vec<_>) = (0..8)
            .map(|_| {
                let r = cb.receiver();
                let done = done.clone();
                let hits = Arc::new(AtomicUsize::new(0));

                let t = {
                    let hits = hits.clone();
                    thread::spawn(move || {
                        while !done.load(SeqCst) {
                            if let Ok(Some(_)) = r.try_recv() {
                                hits.fetch_add(1, SeqCst);
                            }
                        }
                    })
                };

                (t, hits)
            })
            .unzip();

        let mut rng = rand::thread_rng();
        let mut my_hits = 0;
        loop {
            for i in 0..rng.gen_range(0, COUNT) {
                if rng.gen_range(0, 3) == 0 && my_hits == 0 {
                    if let Ok(Some(_)) = cb.try_recv() {
                        my_hits += 1;
                    }
                } else {
                    cb.send(i);
                }
            }

            if my_hits > 0 && hits.iter().all(|h| h.load(SeqCst) > 0) {
                break;
            }
        }
        done.store(true, SeqCst);

        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn destructors() {
        const COUNT: usize = 50_000;

        struct Elem(usize, Arc<Mutex<Vec<usize>>>);

        impl Drop for Elem {
            fn drop(&mut self) {
                self.1.lock().unwrap().push(self.0);
            }
        }

        let cb = CircBuf::new();

        let dropped = Arc::new(Mutex::new(Vec::new()));
        let remaining = Arc::new(AtomicUsize::new(COUNT));
        for i in 0..COUNT {
            cb.send(Elem(i, dropped.clone()));
        }

        let threads = (0..8)
            .map(|_| {
                let r = cb.receiver();
                let remaining = remaining.clone();

                thread::spawn(move || {
                    for _ in 0..1000 {
                        if let Ok(Some(_)) = r.try_recv() {
                            remaining.fetch_sub(1, SeqCst);
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        for _ in 0..1000 {
            if let Ok(Some(_)) = cb.try_recv() {
                remaining.fetch_sub(1, SeqCst);
            }
        }

        for t in threads {
            t.join().unwrap();
        }

        let rem = remaining.load(SeqCst);
        assert!(rem > 0);

        {
            let mut v = dropped.lock().unwrap();
            assert_eq!(v.len(), COUNT - rem);
            v.clear();
        }

        drop(cb);

        {
            let mut v = dropped.lock().unwrap();
            assert_eq!(v.len(), rem);
            v.sort();
            for pair in v.windows(2) {
                assert_eq!(pair[0] + 1, pair[1]);
            }
        }
    }
}
