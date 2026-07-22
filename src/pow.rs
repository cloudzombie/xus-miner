//! Client-side SOV proof-of-work sealing.
//!
//! This module intentionally depends only on the audited upstream primitives:
//! `randomx-rs` for RandomX and `sha2` for SHA-256d. It has no chain repository
//! dependency and no capability to modify node consensus code.

use randomx_rs::{RandomXCache, RandomXDataset, RandomXFlag, RandomXVM};
use sha2::{Digest, Sha256};
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowAlgo {
    Sha256d,
    RandomX,
}

thread_local! {
    static RANDOMX_VM_LIGHT: RefCell<Option<(Vec<u8>, RandomXVM)>> = const { RefCell::new(None) };
    static RANDOMX_VM_FAST: RefCell<Option<(Vec<u8>, RandomXVM)>> = const { RefCell::new(None) };
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub(crate) fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

fn build_randomx_vm(key: &[u8], fast: bool) -> RandomXVM {
    if fast {
        let flags = RandomXFlag::get_recommended_flags() | RandomXFlag::FLAG_FULL_MEM;
        if let Ok(cache) = RandomXCache::new(flags, key) {
            if let Ok(dataset) = RandomXDataset::new(flags, cache, 0) {
                if let Ok(vm) = RandomXVM::new(flags, None, Some(dataset)) {
                    return vm;
                }
            }
        }
    }
    let flags = RandomXFlag::get_recommended_flags();
    let cache = RandomXCache::new(flags, key).expect("RandomX cache initialization");
    RandomXVM::new(flags, Some(cache), None).expect("RandomX VM initialization")
}

fn randomx_hash(key: &[u8], input: &[u8], fast: bool) -> [u8; 32] {
    let tls = if fast {
        &RANDOMX_VM_FAST
    } else {
        &RANDOMX_VM_LIGHT
    };
    tls.with(|cell| {
        let mut slot = cell.borrow_mut();
        let needs_build = slot
            .as_ref()
            .is_none_or(|(existing_key, _)| existing_key.as_slice() != key);
        if needs_build {
            *slot = Some((key.to_vec(), build_randomx_vm(key, fast)));
        }
        let (_, vm) = slot
            .as_ref()
            .expect("RandomX VM exists after initialization");
        let digest = vm.calculate_hash(input).expect("RandomX hash calculation");
        let mut output = [0_u8; 32];
        output.copy_from_slice(&digest);
        output
    })
}

#[cfg(test)]
pub(crate) fn pow_seal(algo: PowAlgo, key: &[u8], input: &[u8]) -> [u8; 32] {
    match algo {
        PowAlgo::Sha256d => sha256d(input),
        PowAlgo::RandomX => randomx_hash(key, input, false),
    }
}

pub(crate) fn pow_seal_mining(algo: PowAlgo, key: &[u8], input: &[u8]) -> [u8; 32] {
    match algo {
        PowAlgo::Sha256d => sha256d(input),
        PowAlgo::RandomX => randomx_hash(key, input, true),
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
}
