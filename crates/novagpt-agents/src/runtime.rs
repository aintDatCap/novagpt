//! Provider-independent, in-process agent execution.
//!
//! The caller supplies the backend and explicitly starts each task. This module
//! does not discover providers, make network requests, access workspaces, or
//! start background workers. Backend outputs are untrusted.

use async_trait::async_trait;
use std::{num::NonZeroU16, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, watch};

/// A shared, one-way cancellation signal. Clones refer to the same signal.
#[derive(Clone)]
pub struct CancellationToken {
    signal: watch::Sender<bool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        let (signal, _) = watch::channel(false);
        Self { signal }
    }

    /// Idempotently cancel all runs that share this token.
    pub fn cancel(&self) {
        self.signal.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.signal.borrow()
    }

    /// Wait for cancellation, including cancellation before subscription.
    pub async fn cancelled(&self) {
        let mut receiver = self.signal.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

/// A caller-defined task payload and optional execution deadline.
pub struct AgentTask<T> {
    pub input: T,
    pub timeout: Option<Duration>,
}

/// An explicitly supplied task executor. Implementations own provider
/// authorization, payload limits, and any external resource cleanup.
///
/// The execution future may be dropped at any await point on cancellation,
/// timeout, or caller abandonment. Do not detach work or rely on a future
/// being polled again for cleanup. Dropping it does not undo external effects.
#[async_trait]
pub trait RuntimeBackend: Send + Sync {
    type Input: Send;
    type Output: Send;
    type Error: Send;

    async fn execute(
        &self,
        input: Self::Input,
        cancellation: CancellationToken,
    ) -> Result<Self::Output, Self::Error>;
}

/// Runtime failures are distinct from the backend's typed failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentRunError<E> {
    #[error("Agent task cancelled")]
    Cancelled,
    #[error("Agent task timed out")]
    TimedOut,
    #[error("Agent runtime at capacity")]
    Capacity,
    #[error("Agent backend failed")]
    Backend(E),
}

pub type AgentRunResult<T, E> = Result<T, AgentRunError<E>>;

/// Bounded execution over an injected backend, without a hidden task queue.
///
/// Concurrent runs share a semaphore. Excess calls fail immediately instead
/// of accumulating queued payloads. No tasks are spawned by the runtime.
pub struct AgentRuntime<B: ?Sized> {
    backend: Arc<B>,
    permits: Semaphore,
}

impl<B: RuntimeBackend + ?Sized> AgentRuntime<B> {
    pub fn new(backend: Arc<B>, max_concurrent: NonZeroU16) -> Self {
        Self {
            backend,
            permits: Semaphore::new(usize::from(max_concurrent.get())),
        }
    }

    /// Execute once on the caller's Tokio runtime. The optional timeout
    /// requires Tokio's time driver. Cancellation wins when both it and the
    /// execution are ready in the same poll. A timeout does not cancel other
    /// runs sharing the token; no failed or uncertain task is retried.
    pub async fn run(
        &self,
        task: AgentTask<B::Input>,
        cancellation: CancellationToken,
    ) -> AgentRunResult<B::Output, B::Error> {
        if cancellation.is_cancelled() {
            return Err(AgentRunError::Cancelled);
        }
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| AgentRunError::Capacity)?;
        let AgentTask { input, timeout } = task;
        if timeout == Some(Duration::ZERO) {
            return Err(AgentRunError::TimedOut);
        }
        let execute = async {
            self.backend
                .execute(input, cancellation.clone())
                .await
                .map_err(AgentRunError::Backend)
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AgentRunError::Cancelled),
            result = async {
                match timeout {
                    Some(duration) => tokio::time::timeout(duration, execute)
                        .await
                        .map_err(|_| AgentRunError::TimedOut)?,
                    None => execute.await,
                }
            } => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::pending,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Fixture {
        entered: Notify,
        calls: AtomicUsize,
        active: AtomicUsize,
    }

    struct Active<'a>(&'a AtomicUsize);

    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl RuntimeBackend for Fixture {
        type Input = u32;
        type Output = u32;
        type Error = &'static str;

        async fn execute(
            &self,
            input: u32,
            _cancellation: CancellationToken,
        ) -> Result<u32, &'static str> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(&self.active);
            self.entered.notify_one();
            match input {
                0 => pending().await,
                1 => Err("fixture failure"),
                value => Ok(value),
            }
        }
    }

    fn fixture() -> (Arc<Fixture>, AgentRuntime<Fixture>) {
        let backend = Arc::new(Fixture::default());
        let runtime = AgentRuntime::new(backend.clone(), NonZeroU16::MIN);
        (backend, runtime)
    }

    fn task(input: u32) -> AgentTask<u32> {
        AgentTask {
            input,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn typed_success_and_failure() {
        let (_, runtime) = fixture();
        assert_eq!(
            runtime.run(task(42), CancellationToken::new()).await,
            Ok(42)
        );
        assert_eq!(
            runtime.run(task(1), CancellationToken::new()).await,
            Err(AgentRunError::Backend("fixture failure"))
        );
    }

    #[tokio::test]
    async fn pre_cancelled_task_never_calls_backend() {
        let (backend, runtime) = fixture();
        let cancellation = CancellationToken::new();
        cancellation.clone().cancel();
        assert_eq!(
            runtime.run(task(42), cancellation).await,
            Err(AgentRunError::Cancelled)
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_drops_execution_and_releases_capacity() {
        let (backend, runtime) = fixture();
        let cancellation = CancellationToken::new();
        let run = runtime.run(task(0), cancellation.clone());
        let controller = async {
            backend.entered.notified().await;
            assert_eq!(
                runtime.run(task(42), CancellationToken::new()).await,
                Err(AgentRunError::Capacity)
            );
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(run, controller);
        assert_eq!(result, Err(AgentRunError::Cancelled));
        assert_eq!(backend.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime.run(task(42), CancellationToken::new()).await,
            Ok(42)
        );
    }

    #[tokio::test]
    async fn timeout_drops_execution_without_cancelling_shared_token() {
        let (backend, runtime) = fixture();
        let cancellation = CancellationToken::new();
        let result = runtime
            .run(
                AgentTask {
                    input: 0,
                    timeout: Some(Duration::from_millis(10)),
                },
                cancellation.clone(),
            )
            .await;
        assert_eq!(result, Err(AgentRunError::TimedOut));
        assert!(!cancellation.is_cancelled());
        assert_eq!(backend.active.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.run(task(42), cancellation).await, Ok(42));
    }
}
