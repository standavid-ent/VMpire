//! Test support for the `exit_decision` tests: an exit written as the Intel
//! backend reads it (VMCS values), decoded into `ExitInfo` with the
//! backend's own decoders, as the exit handler's `read_exit` does, so each
//! test keeps the values a real exit has.

use crate::hal::intel::vm_exit::{
    decode_cr_access, decode_exception_or_nmi, decode_fault, decode_faulted_nmi_unblocking_iret, decode_interrupted,
    decode_interruptibility, decode_io_access, decode_reason,
};
use crate::hal::intel::vmexit::decode::{ExitReason, InterruptionInfo};
use crate::hal::types::{CrConstraints, ExitReason as Reason, InjectedEvent};
use crate::hal::{VmExit as _, backend};
use crate::hv::exit_decision::{ExitInfo, ResumePlan};
use crate::x86::mtrr::MtrrCapabilities;

/// An exit as the Intel backend reads it: the VMCS values `ExitInfo` is
/// decoded from.
#[derive(Debug, Clone, Copy)]
pub struct RawExit {
    pub reason: ExitReason,
    pub interruption_info: InterruptionInfo,
    pub guest_rax: u64,
    pub guest_rcx: u64,
    pub guest_rdx: u64,
    pub interruptibility: u32,
    pub idt_vectoring: InterruptionInfo,
    pub idt_vectoring_error_code: u32,
    pub qualification: u64,
    pub guest_physical: u64,
    pub protected_page: u64,
    pub stack_page: u64,
    pub mtrr: MtrrCapabilities,
    pub hyperv: bool,
    pub guest_cpl: u8,
    pub cr_source: u64,
    pub cr_fixed: CrConstraints,
    pub guest_cr4: u64,
    pub xcr0_supported: u64,
}

impl RawExit {
    /// VM_EXIT_REASON.
    pub fn raw_reason(&self) -> u32 {
        u32::from(self.reason.basic) | (u32::from(self.reason.entry_failure) << 31)
    }

    /// Whether this exit came from a fault in an IRET that had ended NMI
    /// blocking, as the backend reads it.
    pub fn faulted_nmi_unblocking_iret(&self) -> bool {
        decode_faulted_nmi_unblocking_iret::<()>(
            self.raw_reason(),
            self.idt_vectoring.0,
            || Ok(self.qualification),
            || Ok(self.interruption_info.0),
        )
        .unwrap()
    }

    /// What `decide` gets: this exit through the backend's readers, each
    /// detail read for the reason that has it.
    pub fn info(&self) -> ExitInfo {
        let reason = decode_reason(self.raw_reason());
        ExitInfo {
            reason,
            event: decode_exception_or_nmi(self.interruption_info.0),
            guest_rax: self.guest_rax,
            guest_rcx: self.guest_rcx,
            guest_rdx: self.guest_rdx,
            interruptibility: decode_interruptibility(self.interruptibility),
            interrupted: decode_interrupted(self.idt_vectoring.0, || self.idt_vectoring_error_code),
            cr_access: (reason == Reason::ControlRegisterAccess).then(|| decode_cr_access(self.qualification)),
            io_access: (reason == Reason::IoInstruction).then(|| decode_io_access(self.qualification)),
            fault: matches!(reason, Reason::NestedPageFault | Reason::NestedPagingMisconfig)
                .then(|| decode_fault(self.qualification, self.guest_physical)),
            faulted_nmi_unblocking_iret: self.faulted_nmi_unblocking_iret(),
            protected_page: self.protected_page,
            stack_page: self.stack_page,
            mtrr: self.mtrr,
            hyperv: self.hyperv,
            guest_cpl: self.guest_cpl,
            cr_source: self.cr_source,
            cr_constraints: self.cr_fixed,
            guest_cr4: self.guest_cr4,
            xcr0_supported: self.xcr0_supported,
        }
    }

    /// The event this exit interrupted, to re-deliver.
    pub fn redelivery(&self, instruction_len: Option<u32>) -> Option<InjectedEvent> {
        self.info().redelivery(instruction_len)
    }
}

/// `event` as VM_ENTRY_INTERRUPTION_INFO, as the backend writes it.
pub fn entry_info(event: InjectedEvent) -> InterruptionInfo {
    InterruptionInfo(backend::VmExit::encode_event(event))
}

/// GUEST_INTERRUPTIBILITY_STATE after `plan`, from the one the exit found.
pub fn resume_interruptibility(plan: ResumePlan, found: u32) -> u32 {
    backend::VmExit::encode_interruptibility(plan.interruptibility(decode_interruptibility(found)))
}

/// GUEST_INTERRUPTIBILITY_STATE after Aleph0 skips the exiting instruction.
pub fn after_skipped_instruction(found: u32) -> u32 {
    backend::VmExit::encode_interruptibility(decode_interruptibility(found).after_skipped_instruction())
}

/// GUEST_INTERRUPTIBILITY_STATE for an NMI re-delivery.
pub fn after_nmi_redelivery(found: u32) -> u32 {
    backend::VmExit::encode_interruptibility(decode_interruptibility(found).after_nmi_redelivery())
}
