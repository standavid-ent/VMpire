//! VMX (Intel VT-x) support detection.
//!
//! Checking whether the CPU can enter VMX operation takes two checks,
//! which [`detect`] combines into the single entry point the rest of the
//! hypervisor calls:
//!
//! 1. [`is_vmx_supported`] — CPUID.1:ECX.VMX\[bit 5\] is set.
//! 2. [`msr::feature_control_vmx_enabled`] — IA32_FEATURE_CONTROL MSR
//!    (0x3A) has the lock bit set and the "VMX outside SMX" bit set.
//!
//! The CPUID check must run before the MSR check: IA32_FEATURE_CONTROL is
//! only guaranteed to exist on CPUs that support VMX or SMX, so reading it
//! on a CPU that doesn't (per CPUID) risks a #GP with no exception handler
//! installed yet.

pub mod ept;
pub mod exit_bitmaps;
pub mod guest_state;
pub mod instruction;
pub mod msr;
pub mod region;
pub mod vmcs;
pub mod vmexit;
pub mod vmxon;

mod hypercall;
mod intercepts;
mod nested_paging;
mod vcpu;
mod virtualization;
pub(crate) mod vm_exit;

// One type per `hal` trait (HAL_STRUCTURE.md section 1).
pub use hypercall::Hypercall;
pub use intercepts::Intercepts;
pub use nested_paging::NestedPaging;
pub use vcpu::Vcpu;
pub use virtualization::HardwareVirtualization;
pub use vm_exit::VmExit;

use core::arch::x86_64::__cpuid;

/// Checks CPUID.1:ECX.VMX\[bit 5\] to determine whether the CPU implements
/// VMX (Intel VT-x).
fn is_vmx_supported() -> bool {
    __cpuid(1).ecx >> 5 & 1 == 1
}

/// Overall VMX support/readiness state for the current CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxCapabilities {
    /// CPUID reports VMX and IA32_FEATURE_CONTROL allows entering VMX operation.
    Supported,
    /// CPUID does not report the VMX feature bit.
    NotSupportedByCpu,
    /// CPUID reports VMX, but IA32_FEATURE_CONTROL disables it (e.g. locked
    /// by BIOS/firmware with the VMX-outside-SMX bit clear).
    DisabledByFirmware,
}

/// Runs the full VMX support/enablement check.
///
/// Checks CPUID first; the MSR check only runs if CPUID confirms VMX
/// support, since IA32_FEATURE_CONTROL isn't guaranteed to exist otherwise.
/// That order is what makes this safe to call on any CPU.
pub fn detect() -> VmxCapabilities {
    if !is_vmx_supported() {
        return VmxCapabilities::NotSupportedByCpu;
    }

    // SAFETY: CPUID reports VMX, so IA32_FEATURE_CONTROL exists.
    if unsafe { msr::feature_control_vmx_enabled() } {
        VmxCapabilities::Supported
    } else {
        VmxCapabilities::DisabledByFirmware
    }
}
