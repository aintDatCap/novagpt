//! Human-paced browser interaction policy.
//!
//! The ChatGPT adapter used to insert an entire prompt and click Send as
//! soon as a composer rendered. A page that receives trusted input from a
//! hidden, unfocused tab, with no keystroke sequence, no pointer path and no
//! dwell time, is behaving in a way a person cannot. This module centralizes
//! the bounded delays, the keystroke budget and the pointer path so tests can
//! use [`InteractionPacing::instant`] while production uses
//! [`InteractionPacing::human`].
//!
//! The policy is deliberately conservative and bounded: a prompt longer than
//! `max_typed_chars` is still inserted as one trusted input event because
//! Chrome exposes no user-permitted clipboard paste to an automation client.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Delays and limits for composer interaction.
#[derive(Clone, Copy, Debug)]
pub struct InteractionPacing {
    /// How long to wait for the tab to become visible and focused after
    /// Chrome is asked to raise it.
    pub reveal_timeout: Duration,
    /// Dwell after the composer is usable, before any input is dispatched.
    pub settle_after_ready: Duration,
    /// Upper bound of the random addition to `settle_after_ready`.
    pub settle_jitter: Duration,
    /// Pause between the last typed character and the submit action.
    pub submit_delay: Duration,
    /// Upper bound of the random addition to `submit_delay`.
    pub submit_jitter: Duration,
    /// Average pause between keystrokes.
    pub key_delay: Duration,
    /// Upper bound of the random addition to `key_delay`.
    pub key_jitter: Duration,
    /// Prompts longer than this many characters are inserted in one event.
    pub max_typed_chars: usize,
    /// Number of `mouseMoved` events emitted before a click; 0 disables the
    /// pointer path.
    pub pointer_steps: u8,
    /// Pause between pointer moves.
    pub pointer_delay: Duration,
    /// Upper bound of the random addition to `pointer_delay`.
    pub pointer_jitter: Duration,
    /// Minimum gap between two worker-tab creations.
    pub tab_gap: Duration,
    /// Upper bound of the random addition to `tab_gap`.
    pub tab_gap_jitter: Duration,
}

impl InteractionPacing {
    /// Production policy: a short, human-sized settle, paced keystrokes and a
    /// short pointer path before the submit click.
    pub fn human() -> Self {
        Self {
            reveal_timeout: Duration::from_secs(3),
            settle_after_ready: Duration::from_millis(700),
            settle_jitter: Duration::from_millis(500),
            submit_delay: Duration::from_millis(350),
            submit_jitter: Duration::from_millis(250),
            key_delay: Duration::from_millis(3),
            key_jitter: Duration::from_millis(7),
            max_typed_chars: 1_200,
            pointer_steps: 5,
            pointer_delay: Duration::from_millis(18),
            pointer_jitter: Duration::from_millis(14),
            tab_gap: Duration::from_millis(700),
            tab_gap_jitter: Duration::from_millis(500),
        }
    }

    /// Deterministic policy for tests and embedders that drive their own page.
    pub fn instant() -> Self {
        Self {
            reveal_timeout: Duration::from_millis(500),
            settle_after_ready: Duration::ZERO,
            settle_jitter: Duration::ZERO,
            submit_delay: Duration::ZERO,
            submit_jitter: Duration::ZERO,
            key_delay: Duration::ZERO,
            key_jitter: Duration::ZERO,
            max_typed_chars: usize::MAX,
            pointer_steps: 0,
            pointer_delay: Duration::ZERO,
            pointer_jitter: Duration::ZERO,
            tab_gap: Duration::ZERO,
            tab_gap_jitter: Duration::ZERO,
        }
    }
}

impl Default for InteractionPacing {
    fn default() -> Self {
        Self::human()
    }
}

static JITTER: AtomicU64 = AtomicU64::new(0x243f_6a88_85a3_08d3);

/// SplitMix64 over the current time and a process counter. The delays only
/// need to be uneven, not unpredictable enough for security purposes.
fn random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut value = nanos ^ JITTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// `base` plus a bounded random addition. A zero spread is exactly `base`,
/// which keeps `instant` deterministic.
pub(crate) fn jittered(base: Duration, spread: Duration) -> Duration {
    let spread = u64::try_from(spread.as_nanos()).unwrap_or(u64::MAX);
    if spread == 0 {
        return base;
    }
    base + Duration::from_nanos(random_u64() % spread)
}

/// A small coordinate wobble in `[-amplitude, amplitude]`.
pub(crate) fn wobble(amplitude: f64) -> f64 {
    if amplitude <= 0.0 {
        return 0.0;
    }
    let span = (amplitude * 2_000.0) as u64;
    let offset = (random_u64() % span.max(1)) as f64 / 1_000.0;
    offset - amplitude
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_pacing_is_deterministic_and_immediate() {
        let pacing = InteractionPacing::instant();
        assert_eq!(
            jittered(pacing.key_delay, pacing.key_jitter),
            Duration::ZERO
        );
        assert_eq!(wobble(0.0), 0.0);
        assert_eq!(pacing.max_typed_chars, usize::MAX);
        assert_eq!(pacing.pointer_steps, 0);
    }

    #[test]
    fn jitter_stays_within_the_bounded_spread() {
        for _ in 0..200 {
            let value = jittered(Duration::from_millis(10), Duration::from_millis(5));
            assert!(value >= Duration::from_millis(10));
            assert!(value < Duration::from_millis(15));
            let offset = wobble(2.0);
            assert!((-2.0..2.0).contains(&offset), "{offset}");
        }
    }

    #[test]
    fn human_pacing_is_bounded_and_positive() {
        let pacing = InteractionPacing::human();
        assert!(pacing.max_typed_chars >= 512 && pacing.max_typed_chars <= 4_000);
        assert!(pacing.pointer_steps >= 2);
        assert!(pacing.reveal_timeout >= Duration::from_secs(1));
        assert!(pacing.tab_gap >= Duration::from_millis(300));
    }
}
