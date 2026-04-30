//! Simple cooperative cancellation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn never() -> Self {
        Self::default()
    }

    pub fn trigger(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn triggered(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn child(&self) -> Self {
        self.clone()
    }
}
