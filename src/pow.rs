//! Client-side SOV proof-of-work sealing.
//!
//! This module depends only on the pinned RandomX native implementation and
//! `sha2`. It has no chain repository dependency and no capability to modify
//! node consensus code.

use crate::diag;
use crate::randomx_native::{
    force_light_mining, recommended_flags, Cache, Dataset, Vm, FLAG_DEFAULT, FLAG_FULL_MEM,
};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const FAST_RETRY_DELAY: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowAlgo {
    Sha256d,
    RandomX,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MiningMode {
    Sha256d,
    RandomXFastShared,
    RandomXLightFallback,
    RandomXLightRecovery,
}

impl MiningMode {
    pub(crate) fn telemetry_name(self) -> &'static str {
        match self {
            Self::Sha256d => "sha256d",
            Self::RandomXFastShared => "fast-shared",
            Self::RandomXLightFallback => "light-fallback",
            Self::RandomXLightRecovery => "light-recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MiningHash {
    pub(crate) digest: [u8; 32],
    pub(crate) mode: MiningMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RandomXMode {
    Light,
    FastShared,
    LightFallback,
    LightRecovery,
}

struct RandomXEngine {
    vm: Vm,
    mode: RandomXMode,
    retry_fast_at: Option<Instant>,
}

enum SharedDatasetOutcome {
    Ready(Arc<Dataset>),
    Releasing {
        previous_key: Vec<u8>,
        previous: Arc<Dataset>,
        retry_at: Instant,
    },
    Unavailable {
        reason: String,
        retry_at: Instant,
    },
}

struct SharedDatasetState {
    key: Vec<u8>,
    outcome: SharedDatasetOutcome,
}

struct SharedLightCacheState {
    key: Vec<u8>,
    flags: u32,
    cache: Arc<Cache>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleasingAction {
    PromotePrevious,
    WaitForRelease,
    DropPreviousAndBuild,
    Retarget,
}

static SHARED_FAST_DATASET: OnceLock<Mutex<Option<SharedDatasetState>>> = OnceLock::new();
static SHARED_LIGHT_CACHE: OnceLock<Mutex<Option<SharedLightCacheState>>> = OnceLock::new();

thread_local! {
    static RANDOMX_VM_LIGHT: RefCell<Option<(Vec<u8>, RandomXEngine)>> = const { RefCell::new(None) };
    static RANDOMX_VM_MINING: RefCell<Option<(Vec<u8>, RandomXEngine)>> = const { RefCell::new(None) };
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub(crate) fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

fn build_fast_dataset(key: &[u8]) -> Result<Arc<Dataset>, String> {
    let optimized = recommended_flags();
    diag::info(&format!(
        "RandomX full-dataset initialization starting: cache-flags={} (synchronous; the dataset publishes only after this returns)",
        diag::describe_randomx_flags(optimized),
    ));
    let started = Instant::now();
    // A full dataset initialized with FLAG_DEFAULT can take many minutes
    // because RandomX loses its cache compiler. If optimized initialization is
    // unavailable, fail promptly so the caller can enter bounded light mode
    // instead of appearing hung at 0 H/s.
    let dataset = Dataset::new(optimized, key).map_err(|error| {
        diag::error(&format!(
            "RandomX full-dataset initialization failed after {} ms: {error}",
            started.elapsed().as_millis(),
        ));
        format!("optimized dataset initialization failed ({error})")
    })?;
    diag::info(&format!(
        "RandomX full-dataset initialization complete in {} ms",
        started.elapsed().as_millis(),
    ));
    Ok(Arc::new(dataset))
}

fn releasing_action(
    managed_key: &[u8],
    previous_key: &[u8],
    requested_key: &[u8],
    retry_pending: bool,
    workers_still_own_previous: bool,
) -> ReleasingAction {
    if previous_key == requested_key {
        ReleasingAction::PromotePrevious
    } else if managed_key != requested_key {
        ReleasingAction::Retarget
    } else if retry_pending || workers_still_own_previous {
        ReleasingAction::WaitForRelease
    } else {
        ReleasingAction::DropPreviousAndBuild
    }
}

fn shared_fast_dataset(key: &[u8]) -> Result<Arc<Dataset>, String> {
    let cache = SHARED_FAST_DATASET.get_or_init(|| Mutex::new(None));
    let mut state = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();

    if let Some(existing) = state.take() {
        let SharedDatasetState {
            key: managed_key,
            outcome,
        } = existing;
        match outcome {
            SharedDatasetOutcome::Ready(dataset) if managed_key.as_slice() == key => {
                let result = Arc::clone(&dataset);
                *state = Some(SharedDatasetState {
                    key: managed_key,
                    outcome: SharedDatasetOutcome::Ready(dataset),
                });
                return Ok(result);
            }
            SharedDatasetOutcome::Ready(previous) => {
                // Keep the manager's old-dataset reference until every worker
                // has left its current native hash call and released its VM.
                let reason =
                    "RandomX seed changed; releasing the previous shared dataset before rebuilding"
                        .to_owned();
                diag::info(
                    "RandomX seed changed; draining every worker off the previous shared dataset before rebuilding",
                );
                *state = Some(SharedDatasetState {
                    key: key.to_vec(),
                    outcome: SharedDatasetOutcome::Releasing {
                        previous_key: managed_key,
                        previous,
                        retry_at: now + FAST_RETRY_DELAY,
                    },
                });
                return Err(reason);
            }
            SharedDatasetOutcome::Unavailable { reason, retry_at }
                if managed_key.as_slice() == key && now < retry_at =>
            {
                *state = Some(SharedDatasetState {
                    key: managed_key,
                    outcome: SharedDatasetOutcome::Unavailable {
                        reason: reason.clone(),
                        retry_at,
                    },
                });
                return Err(reason);
            }
            SharedDatasetOutcome::Unavailable { .. } => {}
            SharedDatasetOutcome::Releasing {
                previous_key,
                previous,
                retry_at,
            } => match releasing_action(
                &managed_key,
                &previous_key,
                key,
                now < retry_at,
                Arc::strong_count(&previous) > 1,
            ) {
                ReleasingAction::PromotePrevious => {
                    // A reorg returned to the dataset that workers may still
                    // own. Reuse it immediately instead of waiting for valid
                    // VMs to release the dataset they are being asked to use.
                    let result = Arc::clone(&previous);
                    *state = Some(SharedDatasetState {
                        key: previous_key,
                        outcome: SharedDatasetOutcome::Ready(previous),
                    });
                    return Ok(result);
                }
                ReleasingAction::WaitForRelease => {
                    let reason = "RandomX seed changed; waiting for every worker to release the previous shared dataset".to_owned();
                    *state = Some(SharedDatasetState {
                        key: managed_key,
                        outcome: SharedDatasetOutcome::Releasing {
                            previous_key,
                            previous,
                            retry_at: if now < retry_at {
                                retry_at
                            } else {
                                now + FAST_RETRY_DELAY
                            },
                        },
                    });
                    return Err(reason);
                }
                ReleasingAction::DropPreviousAndBuild => {
                    // This is the manager's final Arc. Drop it before
                    // allocating the replacement so two full datasets never
                    // coexist.
                    drop(previous);
                }
                ReleasingAction::Retarget => {
                    // The desired seed changed again while the original
                    // dataset was draining. Retarget without allocating
                    // another dataset.
                    let reason = "RandomX seed changed; releasing the previous shared dataset before rebuilding".to_owned();
                    *state = Some(SharedDatasetState {
                        key: key.to_vec(),
                        outcome: SharedDatasetOutcome::Releasing {
                            previous_key,
                            previous,
                            retry_at: now + FAST_RETRY_DELAY,
                        },
                    });
                    return Err(reason);
                }
            },
        }
    }

    match build_fast_dataset(key) {
        Ok(dataset) => {
            *state = Some(SharedDatasetState {
                key: key.to_vec(),
                outcome: SharedDatasetOutcome::Ready(Arc::clone(&dataset)),
            });
            Ok(dataset)
        }
        Err(reason) => {
            *state = Some(SharedDatasetState {
                key: key.to_vec(),
                outcome: SharedDatasetOutcome::Unavailable {
                    reason: reason.clone(),
                    retry_at: now + FAST_RETRY_DELAY,
                },
            });
            Err(reason)
        }
    }
}

fn release_fast_dataset_if_unused(key: &[u8], failure: &str) {
    let cache = SHARED_FAST_DATASET.get_or_init(|| Mutex::new(None));
    let mut state = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let can_release = state.as_ref().is_some_and(|existing| {
        existing.key.as_slice() == key
            && matches!(
                &existing.outcome,
                SharedDatasetOutcome::Ready(dataset) if Arc::strong_count(dataset) == 1
            )
    });
    if can_release {
        *state = Some(SharedDatasetState {
            key: key.to_vec(),
            outcome: SharedDatasetOutcome::Unavailable {
                reason: format!(
                    "released unused full dataset after every fast VM allocation failed: {failure}"
                ),
                retry_at: Instant::now() + FAST_RETRY_DELAY,
            },
        });
    }
}

fn build_light_cache(key: &[u8]) -> Result<(u32, Arc<Cache>), String> {
    let optimized = recommended_flags();
    diag::info(&format!(
        "RandomX light cache initialization: flags={}",
        diag::describe_randomx_flags(optimized),
    ));
    match Cache::new(optimized, key) {
        Ok(cache) => Ok((optimized, Arc::new(cache))),
        Err(optimized_error) => Cache::new(FLAG_DEFAULT, key)
            .inspect(|_| {
                diag::warn(&format!(
                    "optimized RandomX light cache failed ({optimized_error}); using the portable DEFAULT cache"
                ));
            })
            .map(|cache| (FLAG_DEFAULT, Arc::new(cache)))
            .map_err(|portable_error| {
                format!(
                    "optimized light cache allocation failed ({optimized_error}); portable light cache allocation also failed ({portable_error})"
                )
            }),
    }
}

fn shared_light_cache(key: &[u8]) -> Result<(u32, Arc<Cache>), String> {
    let cache = SHARED_LIGHT_CACHE.get_or_init(|| Mutex::new(None));
    let mut state = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = state.as_ref() {
        if existing.key.as_slice() == key {
            return Ok((existing.flags, Arc::clone(&existing.cache)));
        }
    }

    // Drop the manager's previous key reference before allocating the next
    // cache. VMs keep their own Arc until each worker changes jobs.
    *state = None;
    let (flags, new_cache) = build_light_cache(key)?;
    *state = Some(SharedLightCacheState {
        key: key.to_vec(),
        flags,
        cache: Arc::clone(&new_cache),
    });
    Ok((flags, new_cache))
}

fn build_light_vm(key: &[u8], mode: RandomXMode) -> Result<RandomXEngine, String> {
    let (flags, cache) = shared_light_cache(key)?;
    let vm = Vm::light(flags, Arc::clone(&cache)).or_else(|optimized_error| {
        Vm::light(FLAG_DEFAULT, cache).map_err(|portable_error| {
            format!(
                "optimized light VM creation failed ({optimized_error}); portable light VM creation also failed ({portable_error})"
            )
        })
    })?;
    Ok(RandomXEngine {
        vm,
        mode,
        retry_fast_at: (mode == RandomXMode::LightFallback)
            .then(|| Instant::now() + FAST_RETRY_DELAY),
    })
}

fn build_mining_vm(key: &[u8]) -> Result<RandomXEngine, String> {
    if force_light_mining() {
        return build_light_vm(key, RandomXMode::LightRecovery);
    }
    let fast = shared_fast_dataset(key).and_then(|dataset| {
        let optimized = recommended_flags() | FLAG_FULL_MEM;
        Vm::fast(optimized, Arc::clone(&dataset)).or_else(|optimized_error| {
            Vm::fast(FLAG_FULL_MEM, dataset).map_err(|portable_error| {
                format!(
                    "optimized fast VM creation failed ({optimized_error}); portable fast VM creation also failed ({portable_error})"
                )
            })
        })
    });
    match fast {
        Ok(vm) => Ok(RandomXEngine {
            vm,
            mode: RandomXMode::FastShared,
            retry_fast_at: None,
        }),
        Err(fast_error) => {
            diag::warn(&format!(
                "fast shared-dataset RandomX unavailable on this worker: {fast_error}"
            ));
            // If no worker successfully attached a VM, release the manager's
            // full dataset before allocating the light cache. When another VM
            // still owns it, retaining it is required for that worker's safety.
            release_fast_dataset_if_unused(key, &fast_error);
            let fallback =
                build_light_vm(key, RandomXMode::LightFallback).map_err(|light_error| {
                    format!(
                        "fast RandomX unavailable ({fast_error}); safe light-memory fallback also failed ({light_error})"
                    )
                })?;
            eprintln!(
                "RandomX fast mode unavailable ({fast_error}); using safe light-memory fallback and retrying fast mode in {} seconds",
                FAST_RETRY_DELAY.as_secs()
            );
            Ok(fallback)
        }
    }
}

fn engine_needs_build(
    slot: Option<&(Vec<u8>, RandomXEngine)>,
    key: &[u8],
    mining: bool,
    now: Instant,
) -> bool {
    slot.is_none_or(|(existing_key, engine)| {
        existing_key.as_slice() != key
            || (mining
                && engine.mode == RandomXMode::LightFallback
                && engine.retry_fast_at.is_some_and(|retry_at| now >= retry_at))
    })
}

fn randomx_hash<F>(
    key: &[u8],
    input: &[u8],
    mining: bool,
    before_build: F,
) -> Result<([u8; 32], RandomXMode), String>
where
    F: FnOnce(),
{
    let tls = if mining {
        &RANDOMX_VM_MINING
    } else {
        &RANDOMX_VM_LIGHT
    };
    let mut before_build = Some(before_build);
    tls.with(|cell| {
        let mut slot = cell.borrow_mut();
        let now = Instant::now();
        let should_build = engine_needs_build(slot.as_ref(), key, mining, now);
        if should_build {
            if let Some(callback) = before_build.take() {
                callback();
            }
            // Release any old dataset reference before allocating for a new
            // seed or retrying after memory pressure.
            *slot = None;
            let engine = if mining {
                build_mining_vm(key)?
            } else {
                build_light_vm(key, RandomXMode::Light)?
            };
            *slot = Some((key.to_vec(), engine));
        }
        let engine = &mut slot
            .as_mut()
            .ok_or_else(|| "RandomX VM missing after successful initialization".to_string())?
            .1;
        let digest = engine.vm.hash(input)?;
        Ok((digest, engine.mode))
    })
}

#[cfg(test)]
pub(crate) fn pow_hash(algo: PowAlgo, key: &[u8], input: &[u8]) -> Result<[u8; 32], String> {
    match algo {
        PowAlgo::Sha256d => Ok(sha256d(input)),
        PowAlgo::RandomX => randomx_hash(key, input, false, || {}).map(|(digest, _)| digest),
    }
}

pub(crate) fn pow_hash_mining<F>(
    algo: PowAlgo,
    key: &[u8],
    input: &[u8],
    before_build: F,
) -> Result<MiningHash, String>
where
    F: FnOnce(),
{
    match algo {
        PowAlgo::Sha256d => Ok(MiningHash {
            digest: sha256d(input),
            mode: MiningMode::Sha256d,
        }),
        PowAlgo::RandomX => {
            let (digest, mode) = randomx_hash(key, input, true, before_build)?;
            Ok(MiningHash {
                digest,
                mode: match mode {
                    RandomXMode::FastShared => MiningMode::RandomXFastShared,
                    RandomXMode::LightFallback => MiningMode::RandomXLightFallback,
                    RandomXMode::LightRecovery => MiningMode::RandomXLightRecovery,
                    RandomXMode::Light => {
                        return Err(
                            "internal RandomX mode error: verifier VM entered mining path".into(),
                        );
                    }
                },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256d_is_double_sha256() {
        assert_ne!(sha256(b"abc"), sha256d(b"abc"));
        assert_eq!(sha256d(b"abc"), sha256(&sha256(b"abc")));
    }

    #[test]
    fn seed_reversion_promotes_the_still_live_previous_dataset() {
        assert_eq!(
            releasing_action(b"seed-2", b"seed-1", b"seed-1", true, true),
            ReleasingAction::PromotePrevious
        );
        assert_eq!(
            releasing_action(b"seed-2", b"seed-1", b"seed-2", false, true),
            ReleasingAction::WaitForRelease
        );
        assert_eq!(
            releasing_action(b"seed-2", b"seed-1", b"seed-2", false, false),
            ReleasingAction::DropPreviousAndBuild
        );
        assert_eq!(
            releasing_action(b"seed-2", b"seed-1", b"seed-3", false, false),
            ReleasingAction::Retarget
        );
    }

    #[test]
    fn light_randomx_matches_the_upstream_known_answer() {
        assert_eq!(
            hex::encode(pow_hash(PowAlgo::RandomX, b"test key 000", b"This is a test").unwrap()),
            "639183aae1bf4c9a35884cb46b09cad9175f04efd7684e7262a0ac1c2f0b4e3f"
        );
    }

    /// Multi-worker lifecycle regression: several threads share one RandomX
    /// key manager, hash concurrently, switch seeds together, and return to a
    /// previous seed. Every digest must stay correct and no thread may crash,
    /// proving the shared native allocation outlives every VM that uses it
    /// while workers churn. The cheap light engine keeps this in the ordinary
    /// suite; the full-dataset manager reuses the same Arc ownership pattern.
    #[test]
    fn concurrent_workers_share_and_churn_randomx_seeds_safely() {
        use std::sync::Barrier;
        use std::thread;

        const WORKERS: usize = 4;
        let seeds: [&[u8]; 3] = [b"churn seed A", b"churn seed B", b"churn seed A"];
        let mut expected = Vec::new();
        for seed in seeds {
            expected.push(pow_hash(PowAlgo::RandomX, seed, b"stress input").unwrap());
        }

        let barrier = Barrier::new(WORKERS);
        thread::scope(|scope| {
            for worker in 0..WORKERS {
                let barrier = &barrier;
                let expected = &expected;
                scope.spawn(move || {
                    for (phase, seed) in seeds.iter().enumerate() {
                        barrier.wait();
                        for round in 0..3 {
                            let digest = pow_hash(PowAlgo::RandomX, seed, b"stress input")
                                .unwrap_or_else(|error| {
                                    panic!(
                                        "worker {worker} phase {phase} round {round} failed: {error}"
                                    )
                                });
                            assert_eq!(digest, expected[phase]);
                        }
                    }
                });
            }
        });
    }
}
