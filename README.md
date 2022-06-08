# spsc-rs

This crate provides a single producer, single consumer channel. It's a lot like std::sync::mpsc, except limited to a single producer. On the other hand, it's `no_std` and `async` compatible.
