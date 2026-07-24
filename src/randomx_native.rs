//! Narrow, reviewed ownership wrapper around the RandomX C API.
//!
//! `randomx-rs` supplies the pinned, cross-platform native RandomX build. Its
//! high-level `RandomXVM::new` wrapper does not reject a null VM pointer, so the
//! miner binds only the small C surface it needs and checks every allocation
//! before use. Dataset initialization is deliberately synchronous: this avoids
//! detached native-pointer worker threads if an operating-system thread spawn
//! fails under memory pressure.

use randomx_rs::RandomXFlag;
use std::ffi::{c_ulong, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

pub(crate) const FLAG_DEFAULT: u32 = 0;
pub(crate) const FLAG_FULL_MEM: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RuntimeMode {
    force_light: bool,
}

static RUNTIME_MODE: OnceLock<RuntimeMode> = OnceLock::new();

#[repr(C)]
struct RandomxCacheOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct RandomxDatasetOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct RandomxVmOpaque {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn randomx_alloc_cache(flags: u32) -> *mut RandomxCacheOpaque;
    fn randomx_init_cache(cache: *mut RandomxCacheOpaque, key: *const c_void, key_size: usize);
    fn randomx_release_cache(cache: *mut RandomxCacheOpaque);

    fn randomx_alloc_dataset(flags: u32) -> *mut RandomxDatasetOpaque;
    fn randomx_dataset_item_count() -> c_ulong;
    fn randomx_init_dataset(
        dataset: *mut RandomxDatasetOpaque,
        cache: *mut RandomxCacheOpaque,
        start_item: c_ulong,
        item_count: c_ulong,
    );
    fn randomx_release_dataset(dataset: *mut RandomxDatasetOpaque);

    fn randomx_create_vm(
        flags: u32,
        cache: *mut RandomxCacheOpaque,
        dataset: *mut RandomxDatasetOpaque,
    ) -> *mut RandomxVmOpaque;
    fn randomx_destroy_vm(machine: *mut RandomxVmOpaque);
    fn randomx_calculate_hash(
        machine: *mut RandomxVmOpaque,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
    );
}

pub(crate) fn configure_runtime_mode(force_light: bool) -> Result<(), String> {
    let requested = RuntimeMode { force_light };
    match RUNTIME_MODE.get() {
        Some(existing) if *existing == requested => Ok(()),
        Some(_) => Err("RandomX execution mode was already configured for this process".into()),
        None => RUNTIME_MODE
            .set(requested)
            .map_err(|_| "RandomX execution mode could not be configured".to_owned()),
    }
}

pub(crate) fn recommended_flags() -> u32 {
    if force_light_mining() {
        FLAG_DEFAULT
    } else {
        RandomXFlag::get_recommended_flags().bits()
    }
}

pub(crate) fn force_light_mining() -> bool {
    RUNTIME_MODE.get().is_some_and(|mode| mode.force_light)
}

pub(crate) struct Cache {
    pointer: NonNull<RandomxCacheOpaque>,
}

impl Cache {
    pub(crate) fn new(flags: u32, key: &[u8]) -> Result<Self, String> {
        if key.is_empty() {
            return Err("RandomX cache key cannot be empty".into());
        }

        // SAFETY: The function has no Rust-side preconditions. Its nullable
        // allocation result is checked before it is stored or dereferenced.
        let pointer = NonNull::new(unsafe { randomx_alloc_cache(flags) })
            .ok_or_else(|| "RandomX cache allocation failed".to_string())?;
        // SAFETY: `pointer` is non-null and owned by this value. `key` remains
        // valid for the duration of the call, and RandomX copies its contents.
        unsafe {
            randomx_init_cache(pointer.as_ptr(), key.as_ptr().cast::<c_void>(), key.len());
        }
        Ok(Self { pointer })
    }

    fn as_ptr(&self) -> *mut RandomxCacheOpaque {
        self.pointer.as_ptr()
    }
}

// SAFETY: After construction, a cache is immutable until release. RandomX
// documents concurrent cache reads by independent light VMs as supported.
unsafe impl Send for Cache {}
// SAFETY: See the `Send` rationale above; no method mutates an initialized
// cache, and its lifetime is reference-counted by each VM that reads it.
unsafe impl Sync for Cache {}

impl Drop for Cache {
    fn drop(&mut self) {
        // SAFETY: The pointer is non-null, uniquely released here, and every VM
        // that uses it retains an `Arc<Cache>`.
        unsafe { randomx_release_cache(self.pointer.as_ptr()) };
    }
}

pub(crate) struct Dataset {
    pointer: NonNull<RandomxDatasetOpaque>,
}

impl Dataset {
    pub(crate) fn new(cache_flags: u32, key: &[u8]) -> Result<Self, String> {
        let cache = Cache::new(cache_flags, key)?;
        // XUS Miner does not request large pages. RandomX documents no other
        // flag as valid for dataset allocation, so keep this call at DEFAULT
        // even when the temporary initialization cache uses JIT/Argon flags.
        // SAFETY: The nullable allocation result is checked before use.
        let pointer = NonNull::new(unsafe { randomx_alloc_dataset(FLAG_DEFAULT) })
            .ok_or_else(|| "RandomX full dataset allocation failed".to_string())?;
        let dataset = Self { pointer };
        // SAFETY: This function has no pointer arguments and returns the exact
        // item count required by `randomx_init_dataset`.
        let item_count = unsafe { randomx_dataset_item_count() };
        if item_count == 0 {
            return Err("RandomX reported a zero-sized dataset".into());
        }
        // SAFETY: Both allocations are live and non-null. The single call
        // initializes the complete, non-overlapping range before the dataset
        // can be shared with any VM.
        unsafe {
            randomx_init_dataset(dataset.pointer.as_ptr(), cache.as_ptr(), 0, item_count);
        }
        Ok(dataset)
    }

    fn as_ptr(&self) -> *mut RandomxDatasetOpaque {
        self.pointer.as_ptr()
    }
}

// SAFETY: Dataset construction initializes the entire range before this value
// is returned. RandomX datasets are read-only during hashing and are explicitly
// designed to be shared by independent VMs.
unsafe impl Send for Dataset {}
// SAFETY: See the `Send` rationale above. The only exposed operation after
// construction is an immutable pointer read used to create a VM.
unsafe impl Sync for Dataset {}

impl Drop for Dataset {
    fn drop(&mut self) {
        // SAFETY: The allocation is non-null and released exactly once after
        // the final `Arc<Dataset>` held by a VM is gone.
        unsafe { randomx_release_dataset(self.pointer.as_ptr()) };
    }
}

enum VmBacking {
    Light { _cache: Arc<Cache> },
    Fast { _dataset: Arc<Dataset> },
}

pub(crate) struct Vm {
    pointer: NonNull<RandomxVmOpaque>,
    _backing: VmBacking,
}

impl Vm {
    pub(crate) fn light(flags: u32, cache: Arc<Cache>) -> Result<Self, String> {
        let flags = flags & !FLAG_FULL_MEM;
        // SAFETY: `cache` is initialized and remains alive in `_backing`.
        // The nullable VM allocation result is checked before use.
        let pointer =
            NonNull::new(unsafe { randomx_create_vm(flags, cache.as_ptr(), std::ptr::null_mut()) })
                .ok_or_else(|| {
                    format!("RandomX light VM creation failed for flags 0x{flags:02x}")
                })?;
        Ok(Self {
            pointer,
            _backing: VmBacking::Light { _cache: cache },
        })
    }

    pub(crate) fn fast(flags: u32, dataset: Arc<Dataset>) -> Result<Self, String> {
        let flags = flags | FLAG_FULL_MEM;
        // SAFETY: `dataset` is completely initialized and remains alive in
        // `_backing`. The nullable VM allocation result is checked before use.
        let pointer = NonNull::new(unsafe {
            randomx_create_vm(flags, std::ptr::null_mut(), dataset.as_ptr())
        })
        .ok_or_else(|| format!("RandomX fast VM creation failed for flags 0x{flags:02x}"))?;
        Ok(Self {
            pointer,
            _backing: VmBacking::Fast { _dataset: dataset },
        })
    }

    pub(crate) fn hash(&mut self, input: &[u8]) -> Result<[u8; 32], String> {
        if input.is_empty() {
            return Err("RandomX input cannot be empty".into());
        }
        let mut output = [0_u8; 32];
        // SAFETY: The VM pointer is non-null and its backing cache/dataset is
        // retained by this value. Both slices are valid for the duration of the
        // call, and `output` provides the required writable 32 bytes.
        unsafe {
            randomx_calculate_hash(
                self.pointer.as_ptr(),
                input.as_ptr().cast::<c_void>(),
                input.len(),
                output.as_mut_ptr().cast::<c_void>(),
            );
        }
        Ok(output)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: Only successfully created, non-null VMs reach this type; the
        // pointer is released exactly once while its backing memory is live.
        unsafe { randomx_destroy_vm(self.pointer.as_ptr()) };
    }
}
