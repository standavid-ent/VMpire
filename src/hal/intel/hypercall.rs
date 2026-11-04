//! `hal::Hypercall` for VT-x: VMCALL. Aleph0's own call from the guest
//! (`hv::vmcall`), and the forwarder that repeats the guest's hypercall to
//! Hyper-V from root operation, moved unchanged from `hv::hyperv`.

use core::arch::asm;

use crate::hal::types::{HypercallRegisters, RecoveryPoint, Refused};

// `hyperv_forward_hypercall` reads and writes the fields at these offsets.
const _: () = {
    assert!(core::mem::offset_of!(HypercallRegisters, rcx) == 0);
    assert!(core::mem::offset_of!(HypercallRegisters, rdx) == 8);
    assert!(core::mem::offset_of!(HypercallRegisters, r8) == 16);
    assert!(core::mem::offset_of!(HypercallRegisters, rax) == 24);
};

/// IDT vector of #UD, which Hyper-V refuses a hypercall with.
const INVALID_OPCODE: u8 = 6;

// The forwarder, Microsoft x64 ABI (`efiapi`), returning 0 when the
// hypercall ran and 1 when Hyper-V raised #UD. The host fault handler
// resumes that #UD at the `_refused` label, with RSP and RFLAGS as at the
// instruction and every register the ABI lets a callee change possibly
// changed.
//
// hyperv_forward_hypercall(registers = RCX): loads RCX, RDX and R8 from
// `registers`, keeping the pointer on the stack, since Hyper-V may write
// RCX (and RDX, R8 for a fast hypercall's output), and stores all four back.
core::arch::global_asm!(
    r#"
    .globl hyperv_forward_hypercall
    .globl hyperv_hypercall_instruction
    .globl hyperv_hypercall_refused
hyperv_forward_hypercall:
    push rcx
    mov rax, rcx
    mov rcx, [rax]
    mov rdx, [rax + 8]
    mov r8, [rax + 16]
hyperv_hypercall_instruction:
    vmcall
    pop r9
    mov [r9], rcx
    mov [r9 + 8], rdx
    mov [r9 + 16], r8
    mov [r9 + 24], rax
    xor eax, eax
    ret
hyperv_hypercall_refused:
    pop r9
    mov eax, 1
    ret
"#
);

unsafe extern "efiapi" {
    fn hyperv_forward_hypercall(registers: *mut HypercallRegisters) -> u64;
}

unsafe extern "C" {
    static hyperv_hypercall_instruction: u8;
    static hyperv_hypercall_refused: u8;
}

/// VMCALL.
pub struct Hypercall;

impl crate::hal::Hypercall for Hypercall {
    unsafe fn call(rcx: u64) {
        // SAFETY: the caller guarantees Aleph0's guest, so the VMCALL exits
        // to Aleph0, which changes no guest register for it and resumes past
        // it. It touches no memory and no stack.
        unsafe {
            asm!(
                "vmcall",
                inout("rcx") rcx => _,
                options(nomem, nostack),
            );
        }
    }

    unsafe fn forward(registers: &mut HypercallRegisters) -> Result<(), Refused> {
        // SAFETY: the caller's guarantees; the forwarder reads and writes only
        // `registers` and its own stack.
        match unsafe { hyperv_forward_hypercall(registers) } {
            0 => Ok(()),
            _ => Err(Refused),
        }
    }

    fn recovery_point() -> RecoveryPoint {
        RecoveryPoint {
            instruction: &raw const hyperv_hypercall_instruction,
            vector: INVALID_OPCODE,
            resume: &raw const hyperv_hypercall_refused,
        }
    }
}
