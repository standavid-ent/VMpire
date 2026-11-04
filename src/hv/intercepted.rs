//! Which MSRs and ports exit for each guest kind besides the MTRRs: the
//! hypervisor's policy, which every backend's intercept maps
//! (`hal::Intercepts::allocate`) follow.

use crate::hal::types::GuestKind;
use crate::hv::guest::GUEST_SCRATCH_PORT;
use crate::x86::msr::IA32_KERNEL_GS_BASE;

/// MSRs whose RDMSR and WRMSR exit to Aleph0 in the test guest, which passes
/// them through: it runs a real RDMSR/WRMSR in root operation, which reaches
/// the CPU's own MSR, so none may be in the VMCS-held MSRs (checked at compile
/// time in `hal::intel::exit_bitmaps`). They test the bitmap and the passthrough (`crate::hv::guest`); for
/// the firmware or an OS each exit would only repeat the access, so they do
/// not exit there (`Intercepts::allocate`).
///
/// The MTRR MSRs exit too (`Intercepts::allocate`), but are never
/// passed through: the guest reads and writes a copy of them
/// (`vmexit::guest_mtrrs`), since the CPU's still set the memory types of
/// Aleph0's own accesses. Every address an MTRR can have exits
/// (`mtrr::every_msr_address`), not only those the CPU has, so the guest
/// gets #GP for the others from Aleph0 rather than from whatever the CPU
/// holds there.
pub const INTERCEPTED_MSRS: [u32; 1] = [IA32_KERNEL_GS_BASE];

/// Ports whose IN and OUT exit to Aleph0 in the test guest, like
/// [`INTERCEPTED_MSRS`].
pub const INTERCEPTED_PORTS: [u16; 1] = [GUEST_SCRATCH_PORT];

/// The MSRs and ports a `kind` guest's RDMSR, WRMSR, IN and OUT exit for
/// besides the MTRRs: [`INTERCEPTED_MSRS`] and [`INTERCEPTED_PORTS`] for the
/// test guest, none for the firmware.
pub fn intercepted(kind: GuestKind) -> (&'static [u32], &'static [u16]) {
    match kind {
        GuestKind::BuiltInTest => (&INTERCEPTED_MSRS, &INTERCEPTED_PORTS),
        GuestKind::Firmware => (&[], &[]),
    }
}
