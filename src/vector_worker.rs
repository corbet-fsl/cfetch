//! Change-driven vector upkeep owned by the daemon.
//!
//! The Markdown watcher advances the catalog generation. This worker reacts
//! to that generation, hydrates shared/peer artifacts first, and derives only
//! content hashes that remain missing. The clock only retries failures; it
//! never causes repeated inference against an unchanged generation. Inputs
//! the profile refuses are remembered by payload hash for the worker's life,
//! and a failure holds off the next attempt even when new generations arrive.

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::{embed, index, paths, runtime_status, vectors};

const GENERATION_POLL: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const FAILURE_RETRY: Duration = Duration::from_secs(5 * 60);
const BATCH: usize = 1;

fn generation() -> Option<u64> {
    index::open_ro(&paths::state_dir())
        .ok()
        .map(|connection| index::generation(&connection))
        .filter(|generation| *generation > 0)
}

pub(crate) fn sources_available(cfg: &Config, _state_dir: &std::path::Path) -> bool {
    cfg.embeddings.enabled
        || vectors::VectorStore::open(&cfg.brain_root, &cfg.embeddings.spec())
            .is_ok_and(|store| !store.is_empty())
}

/// When the next sync is due. A new generation is worth a prompt attempt, but
/// never one before `not_before`: a failing endpoint or a busy index must not
/// turn every brain edit into another full attempt.
fn schedule(
    due: Option<(u64, Instant)>,
    completed: Option<u64>,
    current: Option<u64>,
    not_before: Instant,
    now: Instant,
) -> Option<(u64, Instant)> {
    match current {
        Some(current)
            if completed != Some(current)
                && due.map(|(scheduled, _)| scheduled) != Some(current) =>
        {
            Some((current, (now + SETTLE_DELAY).max(not_before)))
        }
        _ => due,
    }
}

pub fn run(cfg: Config, stopping: impl Fn() -> bool) {
    if cfg.embeddings.available().is_err() {
        return;
    }
    let mut completed_generation = None;
    let mut due: Option<(u64, Instant)> = None;
    let mut not_before = Instant::now();
    let mut refusals = embed::Refusals::default();

    while !stopping() {
        // A peer may be joined, or a compatible shared artifact may appear,
        // after daemon startup. Keep the cheap worker resident so those
        // events do not require a restart; without a source it performs no
        // index write or inference call.
        if !sources_available(&cfg, &paths::state_dir()) {
            due = None;
            std::thread::sleep(GENERATION_POLL);
            continue;
        }
        due = schedule(
            due,
            completed_generation,
            generation(),
            not_before,
            Instant::now(),
        );

        if let Some((attempted_generation, deadline)) = due
            && Instant::now() >= deadline
        {
            match embed::sync_configured(&cfg, BATCH, &mut refusals) {
                Ok((report, _)) => {
                    eprintln!(
                        "cfetch vectors: generation {attempted_generation}, {} embedded, {} imported, {} block(s) covered, {} refused over the token limit",
                        report.embedded, report.imported, report.total_blocks, report.refused,
                    );
                    completed_generation = Some(attempted_generation);
                    due = None;
                    let _ = runtime_status::refresh_static();
                }
                Err(error) => {
                    eprintln!(
                        "cfetch vectors degraded at generation {attempted_generation}: {error:#}"
                    );
                    not_before = Instant::now() + FAILURE_RETRY;
                    due = Some((attempted_generation, not_before));
                    let _ = runtime_status::refresh_static();
                }
            }
        }
        std::thread::sleep(GENERATION_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_generation_waits_out_the_failure_backoff() {
        let now = Instant::now();
        // Healthy: a new generation is attempted after the settle delay.
        assert_eq!(
            schedule(None, Some(1), Some(2), now, now),
            Some((2, now + SETTLE_DELAY))
        );
        // After a failure, a newer generation does not cut the backoff short.
        let not_before = now + FAILURE_RETRY;
        assert_eq!(
            schedule(Some((2, not_before)), None, Some(3), not_before, now),
            Some((3, not_before))
        );
        // A completed or already scheduled generation keeps what is due.
        assert_eq!(schedule(None, Some(3), Some(3), now, now), None);
        let pending = Some((3, now + SETTLE_DELAY));
        assert_eq!(schedule(pending, None, Some(3), now, now), pending);
        assert_eq!(schedule(pending, None, None, now, now), pending);
    }

    #[test]
    fn inactive_profile_worker_returns_without_retrying() {
        let started = Instant::now();
        run(Config::default(), || false);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn compatible_shared_artifacts_enable_hydration_without_an_endpoint() {
        let brain = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let cfg = Config {
            brain_root: brain.path().to_path_buf(),
            ..Config::default()
        };
        assert!(!sources_available(&cfg, state.path()));

        let spec = cfg.embeddings.spec();
        let mut store = vectors::VectorStore::open(&cfg.brain_root, &spec).unwrap();
        let mut writer = store.begin_write().unwrap();
        writer.put("content-hash", &vec![1.0; spec.dim]).unwrap();
        writer.flush().unwrap();
        drop(writer);
        assert!(sources_available(&cfg, state.path()));
    }
}
