//! MOV to CR0/CR4 (a control-register access exit): the pure decision.
//!
//! With the guest/host masks bring-up writes (`crate::x86::cr::CrControls`), the CPU
//! exits only on a MOV to CR0 or CR4 that changes a bit root mode owns,
//! away from the read shadow. Every such write takes one of three decisions:
//!
//! - **emulate** ([`ExitEvent::ControlRegisterWrite`]): the new read shadow is
//!   the requested value, and the real register is that value forced into
//!   the range root mode allows (`cr::legal`). Clearing CR0.NE is the example: the guest reads
//!   NE = 0, the real NE stays 1;
//! - **#GP(0)** ([`ExitEvent::ControlRegisterGeneralProtection`]): a value the
//!   guest could not have loaded on a CPU without the virtualization
//!   extension, for instance the backend's enable bit in CR4
//!   (`HardwareVirtualization::GUEST_FORBIDDEN_CR4`; its CPUID hides the
//!   extension) or a bit root mode forbids;
//! - **stop** ([`ExitEvent::ControlRegisterUnhandled`]): a write clearing
//!   CR0.PE or CR0.PG (leaving protected mode or paging needs an unrestricted
//!   guest, which Aleph0 does not enable), or an access that Aleph0's masks
//!   should never have let exit (MOV from CR, CLTS, LMSW, CR3/CR8).
//!
//! Nothing here touches the CPU, so every case is a host unit test. See
//! docs/CONTROL_REGISTERS.md.

use crate::hal::types::{CrAccessKind, CrNumber, CrWrite};
use crate::hal::{HardwareVirtualization as _, backend};
use crate::x86::cr;

use super::{ExitEvent, ExitInfo};

/// What a reason-28 exit is. Only a MOV to CR0 or CR4 can exit with the masks
/// Aleph0 sets; any other access should not have, so it stops.
pub(super) fn control_register(exit: &ExitInfo) -> ExitEvent {
    let Some(access) = exit.cr_access else {
        return ExitEvent::ControlRegisterUnhandled;
    };
    if access.kind != CrAccessKind::MovTo {
        return ExitEvent::ControlRegisterUnhandled;
    }
    let fixed = exit.cr_constraints;
    match access.cr {
        0 => decide_mov(
            CrNumber::Cr0,
            exit.cr_source,
            fixed.cr0_must_be_one,
            fixed.cr0_may_be_one,
        ),
        4 => decide_mov(
            CrNumber::Cr4,
            exit.cr_source,
            fixed.cr4_must_be_one,
            fixed.cr4_may_be_one,
        ),
        _ => ExitEvent::ControlRegisterUnhandled,
    }
}

/// The decision for a MOV that loads `requested` into `cr`, given the bits
/// that register must have (`fixed0`) and may have (`fixed1`) in root mode.
fn decide_mov(cr: CrNumber, requested: u64, fixed0: u64, fixed1: u64) -> ExitEvent {
    // The backend's enable bit (CR4.VMXE): the guest's CPUID hides the
    // extension (`emulation::cpuid_policy`), so on a CPU without it this
    // write would #GP. Checked before the `fixed1` test, which allows the
    // bit in root mode.
    if cr == CrNumber::Cr4 && requested & backend::HardwareVirtualization::GUEST_FORBIDDEN_CR4 != 0 {
        return ExitEvent::ControlRegisterGeneralProtection;
    }
    // A bit `fixed1` forbids (must be 0 in root mode): not a value the guest
    // could load on any CPU.
    if requested & !fixed1 != 0 {
        return ExitEvent::ControlRegisterGeneralProtection;
    }
    // Leaving protected mode or paging needs an unrestricted guest, which
    // Aleph0 does not enable: stop with a diagnostic.
    if cr == CrNumber::Cr0 && requested & (cr::CR0_PE | cr::CR0_PG) != cr::CR0_PE | cr::CR0_PG {
        return ExitEvent::ControlRegisterUnhandled;
    }
    // Emulate: the guest reads `requested` back, while the real register
    // keeps the fixed bits (CR0.NE stays 1 even if the guest cleared it).
    ExitEvent::ControlRegisterWrite(CrWrite {
        cr,
        requested,
        real: cr::legal(requested, fixed0, fixed1),
    })
}
