//! What the VM-exit handler does with each exit: pure decisions, one
//! handler per exit reason.
//!
//! The handler reads the exit through the backend's readers into an
//! [`ExitInfo`], calls
//! [`decide`] with the [`ExitState`] kept between exits, and applies the
//! [`ExitAction`] of the returned [`ExitEvent`]. Nothing here touches the
//! CPU, so every case is a host unit test.
//!
//! [`decide`] first checks what holds for every exit (a failed VM entry, an
//! interrupted event delivery), then dispatches on the exit reason to a
//! handler, the way KVM's `kvm_vmx_exit_handlers[]` does, but as a `match`
//! instead of a table of function pointers, so the handled reasons are listed
//! in one place:
//!
//! - `emulation.rs`: instructions Aleph0 executes for any guest (CPUID,
//!   XSETBV, RDMSR, WRMSR, IN, OUT), RDMSR and WRMSR of the guest's MTRRs, which
//!   reach a copy of them instead, and of Hyper-V's synthetic MSRs, which
//!   are forwarded to Hyper-V, and HLT, which it skips. Each handler reads
//!   the exit alone: `fn(&ExitInfo) -> ExitEvent`. MWAIT and MONITOR, which
//!   get #UD, need no handler of their own.
//! - `scenario.rs`: exits that depend on what came before:
//!   `fn(ExitState, &ExitInfo) -> (ExitEvent, ExitState)`. NMI reinjection,
//!   with its NMI and interrupt windows, for every guest, and the test
//!   guest's own exits: its EPT write and its VMCALLs. Any other EPT
//!   violation stops the guest as [`ExitEvent::EptViolation`].
//! - An EPT misconfiguration stops the guest as
//!   [`ExitEvent::EptMisconfig`]. Aleph0 makes no misconfigured entry on
//!   purpose (KVM does, for MMIO, and emulates the access in
//!   `handle_ept_misconfig`), so one means the map is broken.
//!
//! The guest's kind (`GuestKind`) decides the VMCALLs and the test guest's
//! own exits. The firmware has no scripted end: its VMCALLs are Aleph0's
//! own (`crate::hv::vmcall`) or else Hyper-V hypercalls, forwarded to Hyper-V
//! (`firmware_vmcall`), and every EPT violation stops it, so it never
//! finishes and runs until an exit Aleph0 does not handle.
//!
//! An exit reason without a handler stops the guest. Nothing here counts
//! exits: `vmexit::stats` does, for every guest, from what the handler
//! carried out.

mod control_register;
mod emulation;
mod scenario;

use crate::hal::types::{CrAccess, CrConstraints, GuestKind, IoAccess, NestedPageFault};
use crate::hv::vmcall;
use crate::x86::mtrr::MtrrCapabilities;

pub use crate::hal::types::{CrWrite, ExitReason, InjectedEvent, Interruptibility, IoSize, NMI_VECTOR};
pub use emulation::{Emulation, cpuid_policy, is_canonical_48, merge_in, wrmsr_value};
pub use scenario::{PLANNED_MAIN_VMCALLS, expected_main_rax};

/// What the handler read about the current exit, through the backend's
/// readers (`hal::VmExit`): nothing here is a backend's encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    pub reason: ExitReason,
    /// The exception or NMI of an [`ExitReason::ExceptionOrNmi`] exit; `None`
    /// for any other exit.
    pub event: Option<InjectedEvent>,
    /// The guest's RAX, RCX and RDX. CPUID reads its leaf from EAX and its
    /// subleaf from ECX. RDMSR and WRMSR read the MSR from ECX, and WRMSR its
    /// value from EDX:EAX. OUT reads its value from AL, AX or EAX. At main
    /// hypercall #1, ECX and EDX hold what the guest read from its data page
    /// before and after its write.
    pub guest_rax: u64,
    pub guest_rcx: u64,
    pub guest_rdx: u64,
    /// The STI and MOV SS shadows and NMI blocking the exit found.
    pub interruptibility: Interruptibility,
    /// An event whose delivery through the guest IDT the exit interrupted,
    /// which is re-delivered; `None` when the exit interrupted none.
    pub interrupted: Option<InjectedEvent>,
    /// The access of a control-register exit; `None` for any other.
    pub cr_access: Option<CrAccess>,
    /// The access of an I/O exit; `None` for any other.
    pub io_access: Option<IoAccess>,
    /// The fault of a nested-paging exit; `None` for any other.
    pub fault: Option<NestedPageFault>,
    /// The exit came from a fault in an IRET that had already ended NMI
    /// blocking. The CPU leaves the guest unblocked then, and resuming at the
    /// IRET would let an NMI in before the handler's IRET has run: the resume
    /// must set NMI blocking again ([`ResumePlan::restoring_nmi_blocking`]).
    pub faulted_nmi_unblocking_iret: bool,
    /// GPA of the page the nested page tables map read-only
    /// (`NestedPaging::protected_page_for`), 0 if none. For a nested page
    /// fault it is the protected page the access fell in: the guest's data
    /// page, or the reinjected NMI's stack page.
    pub protected_page: u64,
    /// GPA of the page mapped read-only for the reinjected NMI's stack
    /// (`NestedPaging::nmi_stack_page`), 0 if none. The NMI-delivery fault
    /// lands in this page and the guest's own data write in
    /// [`Self::protected_page`]'s; the scenario tells the two faults apart by
    /// which page the GPA is in, not by the interrupted event (a hypervisor
    /// may report the interrupted NMI as an NMI or a hardware exception).
    pub stack_page: u64,
    /// The guest's MTRR MSRs: which exist, and which values WRMSR refuses
    /// (`vmexit::guest_mtrrs`). [`MtrrCapabilities::NONE`] until bring-up
    /// publishes them, and on a CPU without MTRRs.
    pub mtrr: MtrrCapabilities,
    /// Whether bring-up found Hyper-V's interface (`hyperv::offered`): its
    /// hypercalls and synthetic MSRs are forwarded to it.
    pub hyperv: bool,
    /// The guest's CPL, read for hypercall exits only; 0 otherwise, which
    /// [`decide`] never looks at.
    pub guest_cpl: u8,
    /// The value a MOV to CR0/CR4 loads, from the register the access names
    /// (the guest's RSP for RSP). Read for control-register exits only; 0
    /// otherwise.
    pub cr_source: u64,
    /// The CR0/CR4 bits root mode fixes, for the control-register decision.
    /// Read for control-register exits only; [`CrConstraints::NONE`]
    /// otherwise.
    pub cr_constraints: CrConstraints,
    /// The guest's CR4, which CPUID's OSXSAVE and OSPKE bits copy. Read for
    /// CPUID exits only; 0 otherwise.
    pub guest_cr4: u64,
    /// The XCR0 bits the guest's XSETBV may set (`features::GuestFeatures`),
    /// 0 when Aleph0 cannot run XSETBV.
    pub xcr0_supported: u64,
}

impl ExitInfo {
    /// The event this exit interrupted mid-delivery, to re-deliver at the
    /// next VM entry, or `None` when the exit interrupted no delivery.
    /// `instruction_len` is the exiting instruction's length, needed only for
    /// a software interrupt or exception; `None` when it was unreadable.
    pub fn redelivery(&self, instruction_len: Option<u32>) -> Option<InjectedEvent> {
        let event = self.interrupted?;
        Some(InjectedEvent {
            instruction_len: event.needs_instruction_len().then_some(instruction_len).flatten(),
            ..event
        })
    }
}

/// What the handler remembers between exits: the NMI waiting for the guest,
/// and how far the test guest's scenario has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitState {
    /// Main VMCALLs handled so far.
    pub main_vmcalls: u32,
    /// An NMI arrived while the guest blocked NMIs and is not injected yet.
    pub nmi_pending: bool,
    /// VMCALLs from the guest's NMI handler: reinjected NMIs that arrived.
    pub nmis_handled: u32,
    /// EPT violations on the data page after which write was granted.
    pub ept_writes_granted: u32,
    /// EPT violations while an NMI was being delivered (its push faulting on
    /// the read-only stack page) after which write was granted and the NMI
    /// re-delivered.
    pub nmi_stack_grants: u32,
}

impl ExitState {
    /// The state once an NMI that arrived while Aleph0 ran in root operation
    /// (`arrived`) is folded in: pending for the guest, like an NMI the guest
    /// could not take yet. The exit's own handler then decides with it, so
    /// an NMI-window exit injects it, and every other exit keeps it waiting
    /// for the window ([`ResumePlan::keeping_pending_nmi`]).
    ///
    /// At most one NMI waits, as on real hardware: an NMI arriving while one
    /// is pending is the same pending NMI.
    pub fn with_host_nmi(self, arrived: bool) -> Self {
        Self {
            nmi_pending: self.nmi_pending || arrived,
            ..self
        }
    }
}

/// What an exit turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitEvent {
    /// A main VMCALL before the last one.
    MainVmcall,
    /// The last planned main VMCALL.
    LastMainVmcall,
    /// The guest's NMI handler reporting that a reinjected NMI arrived.
    GuestNmiHandler,
    /// An NMI the guest can take now: inject it.
    NmiInjected,
    /// An NMI while the guest cannot take an NMI yet (MOV SS or NMI
    /// blocking, with or without STI blocking): wait for the NMI window.
    NmiDeferred,
    /// An NMI held back by blocking by STI alone: wait for the interrupt
    /// window, which opens once the STI shadow ends. A CPU may report an
    /// NMI-window exit inside the shadow, so the NMI window could not wait it
    /// out ([`ResumePlan::WAIT_FOR_STI_SHADOW`]).
    NmiDeferredPastSti,
    /// The NMI window opened: inject the pending NMI.
    NmiWindowInjected,
    /// The interrupt window opened, so the STI shadow has ended: inject the
    /// pending NMI.
    InterruptWindowInjected,
    /// The guest wrote its protected data page: grant write and retry the
    /// write.
    EptWriteGranted,
    /// A main VMCALL arrived before any EPT violation: the write to the data
    /// page went through, so EPT did not protect it.
    EptWriteMissing,
    /// Main VMCALL #1 after the granted write, but ECX/EDX do not hold what
    /// the guest should have read from its data page: the seed before its
    /// write, its own value after it.
    DataPageMismatch,
    /// An EPT violation other than the guest's first write to its
    /// protected data page. The map allows every other access to every GPA
    /// up to MAXPHYADDR, so the map itself is wrong: stop, and show the walk
    /// of the GPA.
    EptViolation,
    /// An EPT misconfiguration: an entry on the GPA's path is one the CPU
    /// refuses. `Ept::build` makes none, so stop, and show the walk.
    EptMisconfig,
    /// An instruction Aleph0 executes for the guest, then resumes past it.
    Emulated(Emulation),
    /// RDMSR, WRMSR or XSETBV the guest gets a #GP for: an MSR outside the
    /// MSR bitmap that is not forwarded to Hyper-V, an MTRR address the CPU
    /// has no MSR at, a non-canonical value for an intercepted MSR, a value
    /// its MTRRs would refuse, or an XSETBV of another XCR than XCR0 or of a
    /// value XCR0 does not take.
    GuestGeneralProtection,
    /// A MOV to CR0/CR4 Aleph0 emulates: write what the guest reads back and
    /// the real register,
    /// then resume past it (`control_register`).
    ControlRegisterWrite(CrWrite),
    /// A MOV to CR0/CR4 the guest could not have made on a CPU without VMX
    /// (CR4.VMXE set, or a bit FIXED1 forbids): inject #GP(0).
    ControlRegisterGeneralProtection,
    /// A control-register access Aleph0's masks should never have let exit, or
    /// a write leaving protected mode or paging: stop with the qualification.
    ControlRegisterUnhandled,
    /// HLT: resume past it at once. The guest's next instruction runs as if
    /// an interrupt had woken it.
    Halted,
    /// An instruction the guest gets #UD for, as on hardware without what
    /// it asks for: MWAIT or MONITOR (the guest's CPUID.1:ECX bit 3 is clear
    /// under Hyper-V, and `cpuid_policy` passes it through), or a VMCALL
    /// that is neither Aleph0's own nor one Hyper-V takes: without Hyper-V's
    /// interface, or from CPL > 0, which Hyper-V refuses with #UD too.
    GuestInvalidOpcode,
    /// Aleph0's `ExitBootServices` event ran in the guest
    /// ([`vmcall::BOOT_SERVICES_ENDED`]): resume past its VMCALL, from now
    /// on in the runtime output phase.
    BootServicesEnded,
    /// A VMCALL that only tests the exit ([`vmcall::TEST`]): resume past it.
    TestVmcall,
    /// Anything else.
    Unexpected,
}

/// How the handler prepares the guest (and the nested page tables) before
/// resuming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumePlan {
    /// Move the guest's RIP past the exiting instruction, which also ends
    /// the STI and MOV SS shadows (`Interruptibility::after_skipped_instruction`).
    pub advance_rip: bool,
    /// Inject [`InjectedEvent::NMI`] at the next VM entry.
    pub inject_nmi: bool,
    /// Turn NMI-window exiting on (`Some(true)`) or off (`Some(false)`), or
    /// leave it (`None`).
    pub nmi_window: Option<bool>,
    /// Turn interrupt-window exiting on (`Some(true)`) or off
    /// (`Some(false)`), or leave it (`None`). Only on while an STI shadow
    /// holds back a pending NMI.
    pub interrupt_window: Option<bool>,
    /// Make the protected page writable in EPT
    /// (`ept::ProtectedPage::grant_write`).
    pub grant_write: bool,
    /// Execute this instruction for the guest and put its result in the
    /// guest's registers, before RIP moves past it.
    pub emulate: Option<Emulation>,
    /// Inject this exception at the next VM entry, with error code 0
    /// when it delivers one. RIP stays on the faulting instruction.
    pub inject_exception: Option<InjectedEvent>,
    /// Re-deliver an event the exit interrupted mid-delivery, by writing its
    /// VM-entry fields. Never set together with `advance_rip`, `inject_nmi`
    /// or `inject_exception`: only one event is injected per entry
    /// ([`ResumePlan::redelivering`]).
    pub redeliver: Option<InjectedEvent>,
    /// Clear NMI blocking before
    /// resuming. Set only when re-delivering an NMI whose delivery faulted:
    /// that faulted delivery left bit 3 set, and VM entry refuses to inject
    /// an NMI while it is ([`ResumePlan::redelivering`]).
    pub clear_nmi_blocking: bool,
    /// Set NMI blocking again before
    /// resuming. Set only after a fault in an IRET that had ended NMI
    /// blocking ([`ResumePlan::restoring_nmi_blocking`]); never together with
    /// `clear_nmi_blocking`, which needs a valid IDT-vectoring field, where
    /// bit 12 is undefined.
    pub restore_nmi_blocking: bool,
    /// Write a guest CR0/CR4 value and its read shadow before resuming, for a
    /// MOV to CR0/CR4 Aleph0 emulates ([`ExitEvent::ControlRegisterWrite`]).
    pub write_cr: Option<CrWrite>,
}

impl ResumePlan {
    /// Go on where the guest was, with nothing changed: the base the plans
    /// below change.
    const AS_IS: Self = Self {
        advance_rip: false,
        inject_nmi: false,
        nmi_window: None,
        interrupt_window: None,
        grant_write: false,
        emulate: None,
        inject_exception: None,
        redeliver: None,
        clear_nmi_blocking: false,
        restore_nmi_blocking: false,
        write_cr: None,
    };
    /// Go on after the exiting instruction (a VMCALL).
    pub const PAST_INSTRUCTION: Self = Self {
        advance_rip: true,
        ..Self::AS_IS
    };
    /// Inject an NMI, delivered before the instruction the guest was about
    /// to run. Injecting always turns NMI-window and interrupt-window exiting
    /// off: either may have been on for the NMI now injected, and turning one
    /// off when it was already off is harmless.
    pub const INJECT_NMI: Self = Self {
        inject_nmi: true,
        nmi_window: Some(false),
        interrupt_window: Some(false),
        ..Self::AS_IS
    };
    /// Go on where the guest was, with NMI-window exiting on, so the next
    /// exit comes as soon as the guest can take an NMI. Interrupt-window
    /// exiting goes off: an interrupt-window exit with NMI blocking still set
    /// ends up here, and the NMI window alone waits for that blocking's IRET.
    pub const WAIT_FOR_NMI_WINDOW: Self = Self {
        nmi_window: Some(true),
        interrupt_window: Some(false),
        ..Self::AS_IS
    };
    /// Go on where the guest was, with interrupt-window exiting on and
    /// NMI-window exiting off, while blocking by STI alone holds a pending NMI
    /// back. A CPU may report an NMI-window exit inside an STI shadow (Intel
    /// SDM Vol. 3C, "NMI-Window Exiting"), and the NMI could not be injected
    /// there either, so with the NMI window on such a CPU would exit again at
    /// the same instruction forever. The interrupt window opens only once the
    /// shadow has ended, and blocking by STI implies RFLAGS.IF = 1, so it
    /// opens right after the guest's next instruction, unless that
    /// instruction is CLI: then the NMI waits for the guest's next exit,
    /// which turns the NMI window back on ([`ResumePlan::keeping_pending_nmi`]).
    /// KVM does the same (`vmx_enable_nmi_window`).
    pub const WAIT_FOR_STI_SHADOW: Self = Self {
        nmi_window: Some(false),
        interrupt_window: Some(true),
        ..Self::AS_IS
    };
    /// Make the protected page writable; RIP stays on the faulting write, so
    /// it runs again.
    pub const RETRY_AFTER_GRANT: Self = Self {
        grant_write: true,
        ..Self::AS_IS
    };
    /// Inject #GP(0); RIP stays on the faulting instruction, as for any
    /// fault.
    pub const INJECT_GP: Self = Self {
        inject_exception: Some(InjectedEvent::GENERAL_PROTECTION),
        ..Self::AS_IS
    };
    /// Inject #UD; RIP stays on the faulting instruction.
    pub const INJECT_UD: Self = Self {
        inject_exception: Some(InjectedEvent::INVALID_OPCODE),
        ..Self::INJECT_GP
    };
}

impl ResumePlan {
    /// This plan, with NMI-window exiting turned on if an NMI is still
    /// pending after the exit (`next`). Without that, an NMI folded in by
    /// [`ExitState::with_host_nmi`] at an exit that does not handle NMIs
    /// would wait until some unrelated exit came along. Plans that inject an
    /// NMI leave nothing pending, so they keep turning the window off. A plan
    /// that waits out an STI shadow ([`ResumePlan::WAIT_FOR_STI_SHADOW`])
    /// keeps the NMI window off: inside the shadow it could exit at the same
    /// instruction forever.
    pub fn keeping_pending_nmi(self, next: ExitState) -> Self {
        if next.nmi_pending && self.interrupt_window != Some(true) {
            Self {
                nmi_window: Some(true),
                ..self
            }
        } else {
            self
        }
    }

    /// This plan once Hyper-V refused its forwarded access with `exception`:
    /// RIP stays on the guest's instruction, which gets the exception, as it
    /// would without Aleph0. The rest of the plan (NMI window, the emulation
    /// it tried, for the counts) is kept.
    pub fn refused_with(self, exception: InjectedEvent) -> Self {
        Self {
            advance_rip: false,
            inject_exception: Some(exception),
            ..self
        }
    }

    /// This plan, setting blocking by NMI again when `exit` was a fault in an
    /// IRET that had ended it ([`ExitInfo::faulted_nmi_unblocking_iret`]).
    /// The guest then resumes at the IRET (the faulting access runs again, or
    /// the fault is delivered there) with NMIs blocked, as they were before
    /// the IRET ran; its next IRET ends the blocking. KVM does the same
    /// (`vmx_recover_nmi_blocking`, `handle_ept_violation`).
    pub fn restoring_nmi_blocking(self, exit: &ExitInfo) -> Self {
        Self {
            restore_nmi_blocking: exit.faulted_nmi_unblocking_iret,
            ..self
        }
    }

    /// The interruptibility to resume with, from the one the exit found
    /// (`found`): the STI and MOV SS shadows ended when RIP moves past the
    /// instruction (`Interruptibility::after_skipped_instruction`), NMI
    /// blocking cleared for an NMI re-delivery
    /// (`Interruptibility::after_nmi_redelivery`) or set again after a fault
    /// in an NMI-unblocking IRET. The handler writes it only when this
    /// differs from `found`.
    pub fn interruptibility(self, found: Interruptibility) -> Interruptibility {
        let mut state = found;
        if self.advance_rip {
            state = state.after_skipped_instruction();
        }
        if self.clear_nmi_blocking {
            state = state.after_nmi_redelivery();
        }
        if self.restore_nmi_blocking {
            state.nmi_blocked = true;
        }
        state
    }

    /// This plan with `redelivery` added, or `None` when it cannot be added
    /// and the guest must stop.
    ///
    /// Only one event is injected per VM entry, so a plan that advances RIP
    /// or injects an NMI or an exception of its own cannot also re-deliver
    /// one: stop (no double-fault synthesis yet). A software interrupt or
    /// exception also needs its instruction length, so an unreadable length
    /// stops too.
    pub fn redelivering(self, redelivery: InjectedEvent) -> Option<Self> {
        if self.advance_rip || self.inject_nmi || self.inject_exception.is_some() {
            return None;
        }
        if redelivery.needs_instruction_len() && redelivery.instruction_len.is_none() {
            return None;
        }
        Some(Self {
            redeliver: Some(redelivery),
            // The faulted NMI delivery left blocking by NMI set; clear it so
            // VM entry can inject the re-delivered NMI. `redelivery.info` is
            // already re-armed to a type-2 NMI for a vector-2 vectoring.
            clear_nmi_blocking: redelivery.is_nmi(),
            ..self
        })
    }
}

/// What the handler does after an exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAction {
    /// Return to the guest, after preparing it as the plan says.
    Resume(ResumePlan),
    /// The last planned exit arrived as expected; stop.
    Finished,
    /// Not an exit this hypervisor handles; stop without resuming.
    Unexpected,
}

impl ExitEvent {
    /// The action for this event.
    pub fn action(self) -> ExitAction {
        match self {
            Self::MainVmcall | Self::GuestNmiHandler => ExitAction::Resume(ResumePlan::PAST_INSTRUCTION),
            Self::NmiInjected | Self::NmiWindowInjected | Self::InterruptWindowInjected => {
                ExitAction::Resume(ResumePlan::INJECT_NMI)
            }
            Self::NmiDeferred => ExitAction::Resume(ResumePlan::WAIT_FOR_NMI_WINDOW),
            Self::NmiDeferredPastSti => ExitAction::Resume(ResumePlan::WAIT_FOR_STI_SHADOW),
            Self::EptWriteGranted => ExitAction::Resume(ResumePlan::RETRY_AFTER_GRANT),
            Self::Emulated(emulation) => ExitAction::Resume(ResumePlan {
                emulate: Some(emulation),
                ..ResumePlan::PAST_INSTRUCTION
            }),
            Self::GuestGeneralProtection | Self::ControlRegisterGeneralProtection => {
                ExitAction::Resume(ResumePlan::INJECT_GP)
            }
            Self::ControlRegisterWrite(write) => ExitAction::Resume(ResumePlan {
                write_cr: Some(write),
                ..ResumePlan::PAST_INSTRUCTION
            }),
            Self::Halted | Self::BootServicesEnded | Self::TestVmcall => {
                ExitAction::Resume(ResumePlan::PAST_INSTRUCTION)
            }
            Self::GuestInvalidOpcode => ExitAction::Resume(ResumePlan::INJECT_UD),
            Self::LastMainVmcall => ExitAction::Finished,
            Self::EptWriteMissing
            | Self::DataPageMismatch
            | Self::EptViolation
            | Self::EptMisconfig
            | Self::ControlRegisterUnhandled
            | Self::Unexpected => ExitAction::Unexpected,
        }
    }
}

/// Decides what `exit` of a `guest` is, given what happened before
/// (`state`), and returns the event with the state for the next exit.
///
/// A VM-entry failure is never handled, whatever the reason. Every other
/// exit goes to the handler for its reason. An exit that interrupted an
/// event's delivery (`exit.idt_vectoring` valid) is still judged by its
/// reason; the handler re-delivers the event with the plan's `redeliver`
/// (built by [`ExitInfo::redelivery`] and [`ResumePlan::redelivering`]).
pub fn decide(guest: GuestKind, state: ExitState, exit: ExitInfo) -> (ExitEvent, ExitState) {
    if let ExitReason::EntryFailure(_) = exit.reason {
        return (ExitEvent::Unexpected, state);
    }
    let test_guest = guest == GuestKind::BuiltInTest;
    match exit.reason {
        ExitReason::Cpuid => (emulation::cpuid(&exit), state),
        // Test only: the OS kernel's XSETBV stops it (`os-stop-test`).
        ExitReason::Xsetbv if cfg!(feature = "os-stop-test") => (ExitEvent::Unexpected, state),
        ExitReason::Xsetbv => (emulation::xsetbv(&exit), state),
        ExitReason::Rdmsr => (emulation::rdmsr(&exit), state),
        ExitReason::Wrmsr => (emulation::wrmsr(&exit), state),
        ExitReason::IoInstruction => (emulation::io(&exit), state),
        ExitReason::Hlt => (emulation::hlt(), state),
        ExitReason::Mwait | ExitReason::Monitor => (ExitEvent::GuestInvalidOpcode, state),
        ExitReason::ExceptionOrNmi => scenario::exception_or_nmi(state, &exit),
        ExitReason::NmiWindow => scenario::nmi_window(state, &exit),
        ExitReason::InterruptWindow => scenario::interrupt_window(state, &exit),
        ExitReason::NestedPageFault if test_guest => scenario::ept_violation(state, &exit),
        ExitReason::NestedPageFault => (ExitEvent::EptViolation, state),
        ExitReason::NestedPagingMisconfig => (ExitEvent::EptMisconfig, state),
        ExitReason::ControlRegisterAccess => (control_register::control_register(&exit), state),
        ExitReason::Hypercall if test_guest => scenario::vmcall(state, &exit),
        ExitReason::Hypercall => (firmware_vmcall(&exit), state),
        _ => (ExitEvent::Unexpected, state),
    }
}

/// A VMCALL of the firmware, or of the OS after it:
///
/// | CPL | RCX | Event |
/// |---|---|---|
/// | > 0 | anything | #UD, as Hyper-V gives (and KVM's emulation of it) |
/// | 0 | one of Aleph0's own (`crate::hv::vmcall`) | handled here |
/// | 0 | anything else, with Hyper-V's interface | a hypercall, forwarded |
/// | 0 | anything else, without it | #UD, as on hardware without a hypervisor |
///
/// Aleph0's own values have reserved hypercall-input bits set, so no
/// hypercall Hyper-V would take is mistaken for one.
fn firmware_vmcall(exit: &ExitInfo) -> ExitEvent {
    if exit.guest_cpl != 0 {
        return ExitEvent::GuestInvalidOpcode;
    }
    match exit.guest_rcx {
        vmcall::BOOT_SERVICES_ENDED => ExitEvent::BootServicesEnded,
        vmcall::TEST => ExitEvent::TestVmcall,
        // shortcut: forwards the x64 register convention only; a guest
        // calling from 32-bit mode passes its input in EDX:EAX and would
        // need translating, once a 32-bit guest exists.
        input if exit.hyperv => ExitEvent::Emulated(Emulation::Hypercall { input }),
        _ => ExitEvent::GuestInvalidOpcode,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/hv/exit_decision/raw_exit.rs"]
mod raw_exit;

#[cfg(test)]
#[path = "../../../tests/unit/hv/exit_decision.rs"]
mod tests;
