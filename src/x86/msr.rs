//! Model-specific registers (MSRs): their addresses, the bits this
//! hypervisor reads in them, and RDMSR and WRMSR.
use core::arch::asm;

/// SYSENTER's host code selector, stack pointer, and entry address.
pub const IA32_SYSENTER_CS: u32 = 0x174;
pub const IA32_SYSENTER_ESP: u32 = 0x175;
pub const IA32_SYSENTER_EIP: u32 = 0x176;

/// Page attribute table and extended feature enable register.
pub const IA32_PAT: u32 = 0x277;
pub const IA32_EFER: u32 = 0xC000_0080;

/// IA32_EFER bit 11 (NXE): no-execute enable. While clear, the XD bit in a
/// page-table entry is reserved, not a permission.
pub const EFER_NXE: u64 = 1 << 11;

/// MSRs holding the FS and GS segment bases. Unlike the selectors, these are
/// live in long mode: operating systems use FS/GS for per-thread and per-CPU
/// data.
pub const IA32_FS_BASE: u32 = 0xC000_0100;
pub const IA32_GS_BASE: u32 = 0xC000_0101;
/// IA32_KERNEL_GS_BASE: the GS base SWAPGS exchanges with IA32_GS_BASE.
/// VM entry and exit do not switch it, so while the guest runs it holds the
/// guest's value, and Aleph0 (which never runs SWAPGS) does not use it.
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;
/// MSRs whose guest value is a VMCS guest-state field (loaded at VM entry,
/// saved at VM exit when the controls say so). Listed for
/// `exit_bitmaps::VMCS_HELD_MSRS`; Aleph0 does not read or write them.
pub const IA32_DEBUGCTL: u32 = 0x1D9;
pub const IA32_PERF_GLOBAL_CTRL: u32 = 0x38F;
pub const IA32_BNDCFGS: u32 = 0xD90;

/// Reads an arbitrary MSR.
///
/// # Safety
///
/// `msr` must exist on this CPU, or RDMSR raises #GP. A few MSRs change
/// state when read; reading one of those must be what the caller wants.
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);

    // SAFETY: RDMSR only reads the MSR; the caller guarantees it exists
    // and that reading it is harmless.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }

    ((high as u64) << 32) | low as u64
}

/// Writes an arbitrary MSR.
///
/// # Safety
///
/// `msr` must exist on this CPU and accept `value`, or WRMSR raises #GP.
/// The new value changes how the CPU behaves, so it must be one the code
/// running afterwards (Aleph0, firmware) can live with.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: the caller guarantees the MSR exists, accepts the value, and
    // that the change is harmless here.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}
