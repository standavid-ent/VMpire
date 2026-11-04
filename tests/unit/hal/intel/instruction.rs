//! Unit tests for `src/vmx/instruction.rs`.
//!
//! Compiled only by `cargo test-host`. That file includes this one with
//! `#[path]` as its child module `tests`, so `use super::*` reaches its
//! private items.

use super::*;

const CF: u64 = 1 << 0;
const ZF: u64 = 1 << 6;
/// RFLAGS bit 1 always reads as 1.
const RESERVED: u64 = 1 << 1;

#[test]
fn clear_flags_are_success() {
    assert_eq!(check_rflags(RESERVED), Ok(()));
    // IF and other unrelated flags do not change the outcome.
    assert_eq!(check_rflags(RESERVED | 1 << 9 | 1 << 2), Ok(()));
}

#[test]
fn cf_is_fail_invalid() {
    assert_eq!(check_rflags(RESERVED | CF), Err(VmxFail::Invalid));
}

#[test]
fn zf_is_fail_valid() {
    assert_eq!(check_rflags(RESERVED | ZF), Err(VmxFail::Valid));
}

#[test]
fn cf_wins_over_zf() {
    assert_eq!(check_rflags(CF | ZF), Err(VmxFail::Invalid));
}
