//! `hal::HardwareVirtualization` for VT-x: detection, the capability MSRs,
//! VMXON and VMXOFF, over the original functions of `vmxon`, `msr` and
//! `detect`.

use super::instruction::VmxFail;
use super::vmcs::fields::{
    SECONDARY_ENABLE_EPT, SECONDARY_ENABLE_INVPCID, SECONDARY_ENABLE_RDTSCP, SECONDARY_ENABLE_USER_WAIT_PAUSE,
    SECONDARY_ENABLE_XSAVES,
};
use super::vmxon::{self, VmxOnError, VmxOperation};
use super::{VmxCapabilities, detect};
use crate::hal::intel::msr::{self, VmcsControlMsrs};
use crate::hal::types::{Capabilities, CpuidBit, CpuidRegister, CrConstraints, OptionalFeatures};
use crate::x86::cr::{self, CR4_VMXE};

/// VMX operation, entered by VMXON.
pub struct HardwareVirtualization(VmxOperation);

/// Why VMXON or VMXOFF failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualizationError {
    VmxOn(VmxOnError),
    Vmxoff(VmxFail),
}

/// The secondary controls for `features`: EPT, which the map needs, and
/// each optional control offered.
pub(super) fn secondary_controls(features: OptionalFeatures) -> u32 {
    [
        (features.rdtscp, SECONDARY_ENABLE_RDTSCP),
        (features.invpcid, SECONDARY_ENABLE_INVPCID),
        (features.xsaves, SECONDARY_ENABLE_XSAVES),
        (features.waitpkg, SECONDARY_ENABLE_USER_WAIT_PAUSE),
    ]
    .into_iter()
    .filter(|&(offered, _)| offered)
    .fold(SECONDARY_ENABLE_EPT, |controls, (_, control)| controls | control)
}

impl crate::hal::HardwareVirtualization for HardwareVirtualization {
    type ControlCapabilities = VmcsControlMsrs;
    type Error = VirtualizationError;

    const NAME: &'static str = "VMX";
    /// CPUID.1:ECX bit 5: VMX. A guest that saw it would try VMXON, which
    /// always exits (reason 27), and Aleph0 does not emulate VMX.
    const CPUID_FEATURE: CpuidBit = CpuidBit {
        leaf: 1,
        register: CpuidRegister::Ecx,
        mask: 1 << 5,
    };
    /// CR4.VMXE: the guest's CPUID hides VMX, so on a CPU without VMX this
    /// write would #GP.
    const GUEST_FORBIDDEN_CR4: u64 = CR4_VMXE;
    const ENABLE_BIT: &'static str = "CR4.VMXE";

    fn detect() -> Capabilities {
        match detect() {
            VmxCapabilities::Supported => Capabilities::Supported,
            VmxCapabilities::NotSupportedByCpu => Capabilities::NotSupportedByCpu,
            VmxCapabilities::DisabledByFirmware => Capabilities::DisabledByFirmware,
        }
    }

    unsafe fn control_capabilities() -> VmcsControlMsrs {
        // SAFETY: the caller confirmed VMX support.
        unsafe { msr::read_vmcs_control_msrs() }
    }

    /// The VMX capability MSRs: IA32_VMX_BASIC; for each of the four control
    /// fields and the secondary controls, which bits must be 1 (low half)
    /// and may be 1 (high half); and IA32_VMX_EPT_VPID_CAP.
    fn report_control_capabilities(controls: &VmcsControlMsrs) {
        log!(
            "VMX BASIC={:#018x}; true controls={}",
            controls.basic,
            controls.basic & msr::VMX_BASIC_TRUE_CONTROLS != 0,
        );
        for (name, capability) in [
            ("pin", controls.pinbased),
            ("primary", controls.primary),
            ("exit", controls.exit),
            ("entry", controls.entry),
        ] {
            log!(
                "VMX {name}: required={:#010x} allowed={:#010x}",
                capability.required(),
                capability.allowed(),
            );
        }
        log!(
            "VMX secondary: required={:#010x} allowed={:#010x}; EPT/VPID capabilities={:#018x}",
            controls.secondary.required(),
            controls.secondary.allowed(),
            controls.ept_vpid_cap,
        );
    }

    /// Each optional secondary control the CPU allows.
    fn optional_features(controls: &VmcsControlMsrs) -> OptionalFeatures {
        let allows = |control| controls.secondary.allows(control);
        OptionalFeatures {
            rdtscp: allows(SECONDARY_ENABLE_RDTSCP),
            invpcid: allows(SECONDARY_ENABLE_INVPCID),
            xsaves: allows(SECONDARY_ENABLE_XSAVES),
            waitpkg: allows(SECONDARY_ENABLE_USER_WAIT_PAUSE),
        }
    }

    /// The secondary controls requested, the XCR0 bits the guest's XSETBV
    /// may set, and HOST_CR4, which has OSXSAVE whenever Aleph0 runs XSETBV.
    fn report_guest_features(features: OptionalFeatures, xcr0: u64, host_cr4: u64) {
        log!(
            "Guest features: secondary controls {:#010x}; XCR0 supported {:#x} (XSETBV {}); HOST_CR4 {host_cr4:#x}.",
            secondary_controls(features),
            xcr0,
            if xcr0 != 0 { "run in root operation" } else { "gets #GP" },
        );
    }

    /// The IA32_VMX_CR0/CR4_FIXED0/FIXED1 MSRs.
    unsafe fn cr_constraints() -> CrConstraints {
        // SAFETY: the caller guarantees VMX support, so the FIXED MSRs exist
        // and reading them has no side effects.
        unsafe {
            CrConstraints {
                cr0_must_be_one: msr::rdmsr(msr::IA32_VMX_CR0_FIXED0),
                cr0_may_be_one: msr::rdmsr(msr::IA32_VMX_CR0_FIXED1),
                cr4_must_be_one: msr::rdmsr(msr::IA32_VMX_CR4_FIXED0),
                cr4_may_be_one: msr::rdmsr(msr::IA32_VMX_CR4_FIXED1),
            }
        }
    }

    unsafe fn enable() -> Result<Self, VirtualizationError> {
        // SAFETY: the caller's obligation, as for `enter_vmx_root_operation`.
        unsafe { vmxon::enter_vmx_root_operation() }
            .map(Self)
            .map_err(VirtualizationError::VmxOn)
    }

    unsafe fn disable(self) -> Result<(), VirtualizationError> {
        // SAFETY: the caller guarantees VMX operation with the VMCS cleared,
        // and no VMX instruction runs after this.
        unsafe { self.0.leave() }.map_err(|fail| {
            log!("Teardown: VMXOFF failed: {fail:?}; still in VMX operation, CR4.VMXE left set.");
            VirtualizationError::Vmxoff(fail)
        })
    }

    fn original_cr0(&self) -> u64 {
        self.0.original_cr0()
    }

    fn original_cr4(&self) -> u64 {
        self.0.original_cr4()
    }

    fn enabled() -> bool {
        cr::read_cr4() & CR4_VMXE != 0
    }

    fn report_error(error: VirtualizationError) {
        match error {
            VirtualizationError::VmxOn(error) => log!("VMXON failed: {error:?}"),
            VirtualizationError::Vmxoff(fail) => log!("Leaving VMX operation failed: Vmxoff({fail:?})."),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/hal/intel/virtualization.rs"]
mod tests;
