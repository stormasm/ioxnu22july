//! Backoff functionality.
#![deny(rustdoc::broken_intra_doc_links, rustdoc::bare_urls, rust_2018_idioms)]
#![warn(
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    clippy::explicit_iter_loop,
    clippy::future_not_send,
    clippy::use_self,
    clippy::clone_on_ref_ptr
)]
use observability_deps::tracing::info;
use rand::prelude::*;
use std::ops::ControlFlow;
use std::time::Duration;

/// Exponential backoff with jitter
///
/// See <https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/>
#[derive(Debug, Clone)]
#[allow(missing_copy_implementations)]
pub struct BackoffConfig {
    /// Initial backoff.
    pub init_backoff: Duration,

    /// Maximum backoff.
    pub max_backoff: Duration,

    /// Multiplier for each backoff round.
    pub base: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            init_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(500),
            base: 3.,
        }
    }
}

// TODO: Currently, retrying can't fail, but there should be a global maximum timeout that
// causes an error if the total time retrying exceeds that amount.

/// Error after giving up retrying.
pub type BackoffError = std::convert::Infallible;

/// Backoff result.
pub type BackoffResult<T> = Result<T, BackoffError>;

/// [`Backoff`] can be created from a [`BackoffConfig`]
///
/// Consecutive calls to [`Backoff::next`] will return the next backoff interval
///
pub struct Backoff {
    init_backoff: f64,
    next_backoff_secs: f64,
    max_backoff_secs: f64,
    base: f64,
    rng: Option<Box<dyn RngCore + Sync + Send>>,
}

impl std::fmt::Debug for Backoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backoff")
            .field("init_backoff", &self.init_backoff)
            .field("next_backoff_secs", &self.next_backoff_secs)
            .field("max_backoff_secs", &self.max_backoff_secs)
            .field("base", &self.base)
            .finish()
    }
}

impl Backoff {
    /// Create a new [`Backoff`] from the provided [`BackoffConfig`]
    pub fn new(config: &BackoffConfig) -> Self {
        Self::new_with_rng(config, None)
    }

    /// Creates a new `Backoff` with the optional `rng`
    ///
    /// Used [`rand::thread_rng()`] if no rng provided
    pub fn new_with_rng(
        config: &BackoffConfig,
        rng: Option<Box<dyn RngCore + Sync + Send>>,
    ) -> Self {
        let init_backoff = config.init_backoff.as_secs_f64();
        Self {
            init_backoff,
            next_backoff_secs: init_backoff,
            max_backoff_secs: config.max_backoff.as_secs_f64(),
            base: config.base,
            rng,
        }
    }

    /// Returns the next backoff duration to wait for
    fn next(&mut self) -> Duration {
        let range = self.init_backoff..(self.next_backoff_secs * self.base);

        let rand_backoff = match self.rng.as_mut() {
            Some(rng) => rng.gen_range(range),
            None => thread_rng().gen_range(range),
        };

        let next_backoff = self.max_backoff_secs.min(rand_backoff);
        Duration::from_secs_f64(std::mem::replace(&mut self.next_backoff_secs, next_backoff))
    }

    /// Perform an async operation that retries with a backoff
    // TODO: Currently, this can't fail, but there should be a global maximum timeout that
    // causes an error if the total time retrying exceeds that amount.
    pub async fn retry_with_backoff<F, F1, B, E>(
        &mut self,
        task_name: &str,
        mut do_stuff: F,
    ) -> BackoffResult<B>
    where
        F: (FnMut() -> F1) + Send,
        F1: std::future::Future<Output = ControlFlow<B, E>> + Send,
        E: std::error::Error + Send,
    {
        loop {
            // first execute `F` and then use it, so we can avoid `F: Sync`.
            let do_stuff = do_stuff();

            let e = match do_stuff.await {
                ControlFlow::Break(r) => break Ok(r),
                ControlFlow::Continue(e) => e,
            };

            let backoff = self.next();
            info!(
                e=%e,
                task_name,
                backoff_secs = backoff.as_secs(),
                "request encountered non-fatal error - backing off",
            );
            tokio::time::sleep(backoff).await;
        }
    }

    /// Retry all errors.
    pub async fn retry_all_errors<F, F1, B, E>(
        &mut self,
        task_name: &str,
        mut do_stuff: F,
    ) -> BackoffResult<B>
    where
        F: (FnMut() -> F1) + Send,
        F1: std::future::Future<Output = Result<B, E>> + Send,
        E: std::error::Error + Send,
    {
        self.retry_with_backoff(task_name, move || {
            // first execute `F` and then use it, so we can avoid `F: Sync`.
            let do_stuff = do_stuff();

            async {
                match do_stuff.await {
                    Ok(b) => ControlFlow::Break(b),
                    Err(e) => ControlFlow::Continue(e),
                }
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::mock::StepRng;

    #[test]
    fn test_backoff() {
        let init_backoff_secs = 1.;
        let max_backoff_secs = 500.;
        let base = 3.;

        let config = BackoffConfig {
            init_backoff: Duration::from_secs_f64(init_backoff_secs),
            max_backoff: Duration::from_secs_f64(max_backoff_secs),
            base,
        };

        let assert_fuzzy_eq = |a: f64, b: f64| assert!((b - a).abs() < 0.0001, "{} != {}", a, b);

        // Create a static rng that takes the minimum of the range
        let rng = Box::new(StepRng::new(0, 0));
        let mut backoff = Backoff::new_with_rng(&config, Some(rng));

        for _ in 0..20 {
            assert_eq!(backoff.next().as_secs_f64(), init_backoff_secs);
        }

        // Create a static rng that takes the maximum of the range
        let rng = Box::new(StepRng::new(u64::MAX, 0));
        let mut backoff = Backoff::new_with_rng(&config, Some(rng));

        for i in 0..20 {
            let value = (base.powi(i) * init_backoff_secs).min(max_backoff_secs);
            assert_fuzzy_eq(backoff.next().as_secs_f64(), value);
        }

        // Create a static rng that takes the mid point of the range
        let rng = Box::new(StepRng::new(u64::MAX / 2, 0));
        let mut backoff = Backoff::new_with_rng(&config, Some(rng));

        let mut value = init_backoff_secs;
        for _ in 0..20 {
            assert_fuzzy_eq(backoff.next().as_secs_f64(), value);
            value =
                (init_backoff_secs + (value * base - init_backoff_secs) / 2.).min(max_backoff_secs);
        }
    }
}
