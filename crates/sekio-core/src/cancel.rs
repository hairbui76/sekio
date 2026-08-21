use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::PreviewError;

/// Cloneable cancellation flag. The frontend keeps one clone per in-flight
/// preview and cancels it the moment the user moves to another file; the
/// render pipeline polls it at work boundaries and bails out early.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn check(&self) -> Result<(), PreviewError> {
        if self.is_cancelled() {
            Err(PreviewError::Cancelled)
        } else {
            Ok(())
        }
    }
}
