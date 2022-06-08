#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(feature = "async")]
use core::task::{Context, Poll};

#[cfg(feature = "async")]
use futures::task::AtomicWaker;

pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    let state = Arc::new(State::new(buffer));
    (
        Sender {
            state: state.clone(),
        },
        Receiver { state },
    )
}

const RING_STATE_CURSOR_BITS: usize = 28;
const RING_STATE_FRONT_CURSOR_MASK: u64 = (1 << RING_STATE_CURSOR_BITS) - 1;
const RING_STATE_BACK_CURSOR_MASK: u64 = RING_STATE_FRONT_CURSOR_MASK << RING_STATE_CURSOR_BITS;
const RING_STATE_RECEIVER_CLOSED_FLAG: u64 = 1 << 63;
const RING_STATE_SENDER_CLOSED_FLAG: u64 = 1 << 62;

struct State<T> {
    capacity: usize,
    ring_buffer: UnsafeCell<Vec<MaybeUninit<T>>>,
    ring_state: AtomicU64,
    #[cfg(feature = "async")]
    recv_waker: AtomicWaker,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

impl<T> State<T> {
    fn new(capacity: usize) -> Self {
        if capacity * 2 > (RING_STATE_CURSOR_BITS << 1) - 1 {
            panic!("capacity too large: {}", capacity);
        }
        let mut ring_buffer = Vec::with_capacity(capacity);
        ring_buffer.resize_with(capacity, MaybeUninit::uninit);
        Self {
            capacity,
            ring_buffer: UnsafeCell::new(ring_buffer),
            ring_state: AtomicU64::new(0),
            recv_waker: AtomicWaker::new(),
        }
    }

    fn receiver_closed(&self) {
        self.ring_state
            .fetch_or(RING_STATE_RECEIVER_CLOSED_FLAG, Ordering::SeqCst);
    }

    fn sender_closed(&self) {
        self.ring_state
            .fetch_or(RING_STATE_SENDER_CLOSED_FLAG, Ordering::SeqCst);
    }

    /// # Safety
    /// This function must not be called by more than one thread at a time.
    unsafe fn try_recv(&self) -> Result<T, TryRecvError> {
        let ring_state = self.ring_state.load(Ordering::SeqCst);
        let front = (ring_state & RING_STATE_FRONT_CURSOR_MASK) as usize;
        let back = ((ring_state & RING_STATE_BACK_CURSOR_MASK) >> RING_STATE_CURSOR_BITS) as usize;
        if front != back {
            let value = (*self.ring_buffer.get())[front % self.capacity].assume_init_read();
            self.ring_state
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |ring_state| {
                    let front = (ring_state & RING_STATE_FRONT_CURSOR_MASK) as usize + 1;
                    Some((ring_state & !RING_STATE_FRONT_CURSOR_MASK) | front as u64)
                })
                .expect("we always return Some, so fetch_update should always succeed");
            Ok(value)
        } else if (ring_state & RING_STATE_SENDER_CLOSED_FLAG) != 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    #[cfg(feature = "async")]
    unsafe fn poll_recv(&self, ctx: &mut Context) -> Poll<Option<T>> {
        self.recv_waker.register(ctx.waker());
        match self.try_recv() {
            Ok(v) => Poll::Ready(Some(v)),
            Err(TryRecvError::Disconnected) => Poll::Ready(None),
            Err(TryRecvError::Empty) => Poll::Pending,
        }
    }

    /// # Safety
    /// This function must not be called by more than one thread at a time.
    unsafe fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let ring_state = self.ring_state.load(Ordering::SeqCst);
        if (ring_state & RING_STATE_RECEIVER_CLOSED_FLAG) != 0 {
            Err(TrySendError::Closed(value))
        } else {
            let front = (ring_state & RING_STATE_FRONT_CURSOR_MASK) as usize;
            let back =
                ((ring_state & RING_STATE_BACK_CURSOR_MASK) >> RING_STATE_CURSOR_BITS) as usize;
            if back - front == self.capacity {
                Err(TrySendError::Full(value))
            } else {
                (*self.ring_buffer.get())[back % self.capacity].write(value);
                self.ring_state
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |ring_state| {
                        let mut front = (ring_state & RING_STATE_FRONT_CURSOR_MASK) as usize;
                        let mut back = ((ring_state & RING_STATE_BACK_CURSOR_MASK)
                            >> RING_STATE_CURSOR_BITS)
                            as usize
                            + 1;
                        if back >= self.capacity * 2 {
                            front -= self.capacity;
                            back -= self.capacity;
                        }
                        Some(
                            (ring_state
                                & !(RING_STATE_FRONT_CURSOR_MASK | RING_STATE_BACK_CURSOR_MASK))
                                | front as u64
                                | ((back as u64) << RING_STATE_CURSOR_BITS),
                        )
                    })
                    .expect("we always return Some, so fetch_update should always succeed");
                self.recv_waker.wake();
                Ok(())
            }
        }
    }
}

impl<T> Drop for State<T> {
    fn drop(&mut self) {
        // drain the channel to make sure everything gets dropped
        unsafe { while self.try_recv().is_ok() {} }
    }
}

pub struct Sender<T> {
    state: Arc<State<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        unsafe { self.state.try_send(value) }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.state.sender_closed()
    }
}

pub struct Receiver<T> {
    state: Arc<State<T>>,
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        unsafe { self.state.try_recv() }
    }

    #[cfg(feature = "async")]
    pub async fn recv(&self) -> Option<T> {
        unsafe { futures::future::poll_fn(|ctx| self.state.poll_recv(ctx)).await }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.state.receiver_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel() {
        let (tx, rx) = channel::<i32>(4);

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(tx.try_send(1), Ok(()));
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        for _ in 0..5 {
            assert_eq!(tx.try_send(1), Ok(()));
            assert_eq!(tx.try_send(2), Ok(()));
            assert_eq!(tx.try_send(3), Ok(()));
            assert_eq!(tx.try_send(4), Ok(()));
            assert_eq!(tx.try_send(5), Err(TrySendError::Full(5)));
            assert_eq!(rx.try_recv(), Ok(1));
            assert_eq!(rx.try_recv(), Ok(2));
            assert_eq!(rx.try_recv(), Ok(3));
            assert_eq!(rx.try_recv(), Ok(4));
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        core::mem::drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[tokio::test]
    async fn test_async() {
        let (tx, rx) = channel::<i32>(4);

        assert_eq!(tx.try_send(1), Ok(()));
        assert_eq!(tx.try_send(2), Ok(()));
        assert_eq!(tx.try_send(3), Ok(()));
        assert_eq!(tx.try_send(4), Ok(()));
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
        assert_eq!(rx.recv().await, Some(4));

        core::mem::drop(tx);
        assert_eq!(rx.recv().await, None);
    }
}
