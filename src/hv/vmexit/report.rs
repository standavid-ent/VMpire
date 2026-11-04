//! The VM-exit handler's report lines: what an exit was and what Aleph0 did
//! about it. Kept apart from `handler.rs`, so that file reads as the steps.
//!
//! Like the handler, everything here runs in exit context: only the
//! backend's exit and the serial port, never firmware. Lines that print the
//! backend's own fields (the exit as read, the nested-page walk) are the
//! backend's (`hal::VmExit::report_read`, `hal::NestedPaging::report_walk`).

use core::fmt;

use super::guest_mtrrs::{self, MtrrWrite};
use super::handler::{EmulationOutcome, ExitRead, Outcome, Resumed};
use super::policy::{COUNTERS, Detail, Histogram};
use super::stats::Tally;
use super::{history, stats};
use crate::hal::types::{
    CrAccess, CrAccessKind, ExitReason, Flush, GuestKind, GuestRegisters as RegisterFrame, Interruptibility, Retyped,
};
use crate::hal::{HardwareVirtualization, NestedPaging, VmExit, backend};
use crate::hv::exit_decision::{
    CrWrite, ExitEvent, ExitInfo, ExitState, InjectedEvent, PLANNED_MAIN_VMCALLS, ResumePlan, expected_main_rax,
    wrmsr_value,
};
use crate::hv::guest::{GUEST_DATA_SEED, GUEST_EPT_VALUE};
use crate::hv::hyperv;
use crate::x86::mtrr::{self, FIXED_END, MtrrMsr, Mtrrs};
use crate::x86::page::PAGE_SIZE;

// The backend's names, for the report lines.
const EPT: &str = <backend::NestedPaging as NestedPaging>::NAME;
const INVEPT: &str = <backend::NestedPaging as NestedPaging>::FLUSH;
const VMCALL: &str = <backend::VmExit as VmExit>::HYPERCALL;
const VMX: &str = <backend::HardwareVirtualization as HardwareVirtualization>::NAME;
const VMXE: &str = <backend::HardwareVirtualization as HardwareVirtualization>::ENABLE_BIT;

/// A control-register access as its exit qualification shows it: the
/// register in bits 3:0, the access type in bits 5:4, the general-purpose
/// register in bits 11:8.
fn cr_access_bits(access: CrAccess) -> u64 {
    let kind = match access.kind {
        CrAccessKind::MovTo => 0,
        CrAccessKind::MovFrom => 1,
        CrAccessKind::Clts => 2,
        CrAccessKind::Lmsw => 3,
    };
    u64::from(access.cr) | (kind << 4) | (u64::from(access.gpr) << 8)
}

/// One exit, as its report needs it.
pub(super) struct ExitReport<'a> {
    /// The guest it came from.
    pub(super) guest: GuestKind,
    /// The exit's number, from 1.
    pub(super) number: u64,
    /// How many exits of its reason there have been, this one included.
    pub(super) count: u64,
    /// How much of it the print policy prints.
    pub(super) detail: Detail,
    /// What was read about it.
    pub(super) read: &'a ExitRead,
    /// The backend's exit, for its own report lines.
    pub(super) exit: &'a backend::VmExit,
    /// Whether an NMI arrived in root operation since the last exit.
    pub(super) host_nmi: bool,
    /// What it turned out to be.
    pub(super) event: ExitEvent,
    /// The state after it.
    pub(super) next: ExitState,
    /// What came of it.
    pub(super) outcome: Outcome,
}

/// Prints one exit in as much detail as the print policy gives it: what was
/// read about it, an NMI that arrived in root operation meanwhile, what it
/// turned out to be and what came of it. The NMI is printed whatever the
/// detail.
pub(super) fn report_exit(report: &ExitReport<'_>) {
    let ExitReport {
        guest,
        number,
        count,
        detail,
        read,
        exit,
        host_nmi,
        event,
        next,
        outcome,
    } = *report;
    let tag = crate::TAG;
    match detail {
        Detail::Full => {}
        Detail::Summary => {
            // Only exits that resume are rate-limited, and only readable
            // ones, so the reason is known here.
            let reason = backend::VmExit::describe_reason(read.reason_code.unwrap_or(0));
            crate::serial_println!(
                "{tag} VM exit #{number}: reason={} ({}), {count} exits of this reason so far.",
                reason.number,
                reason.name,
            );
            if let Ok(reason) = read.reason {
                report_forwarded(number, reason);
            }
            if host_nmi {
                report_host_nmi(number);
            }
            return;
        }
        Detail::Silent => {
            if host_nmi {
                report_host_nmi(number);
            }
            return;
        }
    }
    exit.report_read(number, read.registers);
    if host_nmi {
        report_host_nmi(number);
    }
    match outcome {
        Outcome::Resumed { plan, resumed } => {
            report_event(guest, number, event, read.exit, next, Some(resumed));
            report_interrupt_state(number, plan, read.exit);
        }
        Outcome::ResumeFailed(failure) => {
            crate::serial_println!("{tag} VM exit #{number}: {failure}; not resuming.")
        }
        Outcome::Stopped(_) => report_event(guest, number, event, read.exit, next, None),
    }
    if outcome.stops() {
        crate::serial_println!("{tag} VM exit #{number}: stopping the guest; {}.", after_stop(guest));
    }
}

/// Where a stop of `guest` goes, for its report line.
pub fn after_stop(guest: GuestKind) -> &'static str {
    if guest.stop_returns_to_bring_up() {
        "returning to bring-up"
    } else {
        "halting: the firmware has nothing to return to"
    }
}

/// Prints the walk of `gpa` through the nested page tables
/// (`NestedPaging::report_walk`).
fn report_nested_walk(number: u64, gpa: u64) {
    let walked = guest_mtrrs::inspect(|_, tables| {
        // SAFETY: exit context.
        unsafe { backend::NestedPaging::report_walk(Some(tables), number, gpa) }
    });
    if walked.is_none() {
        // SAFETY: exit context.
        unsafe { backend::NestedPaging::report_walk(None, number, gpa) }
    }
}

/// Prints that an NMI arrived while Aleph0 ran in root operation, and is
/// now pending for the guest. Not "resuming the guest": that text arms
/// `run-hyperv.ps1 -InjectNmi`.
fn report_host_nmi(number: u64) {
    crate::serial_println!(
        "{} VM exit #{number}: an NMI arrived while Aleph0 ran in root operation ({} so far); pending for the guest.",
        crate::TAG,
        crate::host::nmis_taken(),
    );
}

/// Prints, after the summary line of exit `number`, what Aleph0 forwarded
/// to Hyper-V so far at exits of its reason `reason`: hypercalls by call
/// code for a hypercall, synthetic MSRs for RDMSR and WRMSR. Nothing when
/// none was, or for another reason.
fn report_forwarded(number: u64, reason: ExitReason) {
    let forwarded = stats::forwarded();
    let (what, tally) = match reason {
        ExitReason::Hypercall => ("hypercalls by call code", forwarded.hypercalls),
        ExitReason::Rdmsr => ("synthetic MSR reads", forwarded.synthetic_reads),
        ExitReason::Wrmsr => ("synthetic MSR writes", forwarded.synthetic_writes),
        _ => return,
    };
    if tally != Tally::EMPTY {
        crate::serial_println!(
            "{} VM exit #{number}: forwarded to Hyper-V so far: {what}: {tally}.",
            crate::TAG
        );
    }
}

/// Prints what every stop adds after the lines of exit `number`: the
/// guest's state and registers (`frame`), the vCPU's exit statistics with
/// what was forwarded to Hyper-V, how many exits of each reason there were
/// (`exits_by_reason`), including those the print policy left out, and the
/// last exits, oldest first.
pub(super) fn report_stop(
    number: u64,
    frame: &RegisterFrame,
    exits_by_reason: &[u64; COUNTERS],
    exit: &backend::VmExit,
) {
    exit.dump(number, frame);
    let tag = crate::TAG;
    crate::serial_println!("{tag} VM exit #{number}: exit statistics: {}.", stats::snapshot());
    let forwarded = stats::forwarded();
    crate::serial_println!(
        "{tag} VM exit #{number}: forwarded to Hyper-V: hypercalls by call code: {}; synthetic MSR reads: {}; synthetic MSR writes: {}.",
        forwarded.hypercalls,
        forwarded.synthetic_reads,
        forwarded.synthetic_writes,
    );
    crate::serial_println!(
        "{} VM exit #{number}: exits by reason: {}.",
        crate::TAG,
        Histogram(exits_by_reason)
    );
    history::report(format_args!("VM exit #{number}"));
}

/// `, {name}={value:#x}` for a field that is there; nothing otherwise.
struct OptionalField(&'static str, Option<u32>);

impl fmt::Display for OptionalField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.1 {
            Some(value) => write!(f, ", {}={value:#x}", self.0),
            None => Ok(()),
        }
    }
}

/// Prints, after the event line of exit `number`, what its resume `plan`
/// does to the guest's events besides the event itself: the event the exit
/// interrupted, re-delivered, and blocking by NMI set again after a fault in
/// an NMI-unblocking IRET. Nothing for a plan that does neither.
fn report_interrupt_state(number: u64, plan: ResumePlan, exit: Option<ExitInfo>) {
    let tag = crate::TAG;
    if let Some(redelivery) = plan.redeliver {
        crate::serial_println!(
            "{tag} VM exit #{number}: re-delivering the interrupted event: entry interruption info={:#010x}{}{}{}.",
            redelivery,
            OptionalField("error code", redelivery.error_code),
            OptionalField("instruction length", redelivery.instruction_len),
            if plan.clear_nmi_blocking {
                ", blocking by NMI cleared"
            } else {
                ""
            },
        );
    }
    if plan.restore_nmi_blocking {
        crate::serial_println!(
            "{tag} VM exit #{number}: the exit came from a fault in an IRET that had ended NMI blocking; blocking by NMI set again (interruptibility={:#x}).",
            plan.interruptibility(exit.map_or(Interruptibility::NONE, |exit| exit.interruptibility)),
        );
    }
}

/// Prints what exit `number` of `guest` turned out to be and what Aleph0
/// does about it. `state` is the state after the exit; `resumed` is how the
/// guest goes on, for events that resume it.
fn report_event(
    guest: GuestKind,
    number: u64,
    event: ExitEvent,
    exit: Option<ExitInfo>,
    state: ExitState,
    resumed: Option<Resumed>,
) {
    let tag = crate::TAG;
    let rax = exit.map_or(0, |exit| exit.guest_rax);
    let rcx = exit.map_or(0, |exit| exit.guest_rcx);
    let rip = resumed.map_or(0, |resumed| resumed.rip);
    let nmis_injected = stats::snapshot().nmi_injections;
    match event {
        ExitEvent::EptWriteGranted => {
            // Name the page the grant was for: the guest's data page, or the
            // reinjected NMI's stack page, told apart by the faulting GPA.
            let (page, gpa) = exit.map_or(("protected data page", 0), |exit| {
                let gpa = exit.fault.map_or(0, |fault| fault.gpa);
                if exit.stack_page != 0 && gpa & !(PAGE_SIZE as u64 - 1) == exit.stack_page {
                    ("NMI stack page", gpa)
                } else {
                    ("protected data page", gpa)
                }
            });
            crate::serial_println!(
                "{tag} VM exit #{number}: write to the {page} at GPA {gpa:#018x}: write granted (W=1), {INVEPT} {}; retrying the write at rip={rip:#018x}.",
                resumed
                    .and_then(|resumed| resumed.invept)
                    .map_or("not run", Flush::name),
            )
        }
        ExitEvent::DataPageMismatch => crate::serial_println!(
            "{tag} VM exit #{number}: the data page read back rcx={:#x} rdx={:#x}, expected {GUEST_DATA_SEED:#x} and {GUEST_EPT_VALUE:#x}.",
            exit.map_or(0, |exit| exit.guest_rcx),
            exit.map_or(0, |exit| exit.guest_rdx),
        ),
        ExitEvent::EptWriteMissing => crate::serial_println!(
            "{tag} VM exit #{number}: main {VMCALL} (rax={rax:#x}) before any {EPT} violation: the guest's write to its data page was not caught, so {EPT} did not protect it."
        ),
        ExitEvent::EptViolation => {
            crate::serial_println!(
                "{tag} VM exit #{number}: an {EPT} violation Aleph0 does not handle: only the guest's first write to its protected data page is one; the map is wrong for this GPA."
            );
            report_nested_walk(number, exit.and_then(|exit| exit.fault).map_or(0, |fault| fault.gpa));
        }
        // The read's line already says an entry is invalid; the walk says
        // which.
        ExitEvent::EptMisconfig => {
            report_nested_walk(number, exit.and_then(|exit| exit.fault).map_or(0, |fault| fault.gpa))
        }
        ExitEvent::MainVmcall => crate::serial_println!(
            "{tag} VM exit #{number}: expected {VMCALL} (rax={rax:#x}); resuming the guest at rip={rip:#018x}."
        ),
        ExitEvent::GuestNmiHandler => crate::serial_println!(
            "{tag} VM exit #{number}: guest NMI handler ran (rax={rax:#x}); the reinjected NMI was delivered; resuming the guest at rip={rip:#018x}."
        ),
        ExitEvent::NmiInjected => crate::serial_println!(
            "{tag} VM exit #{number}: NMI (interruption info={:#010x}); reinjecting it into the guest at rip={rip:#018x}.",
            exit.and_then(|exit| exit.event)
                .map_or(0, backend::VmExit::encode_event),
        ),
        ExitEvent::NmiDeferred => crate::serial_println!(
            "{tag} VM exit #{number}: NMI cannot be injected yet (interruptibility={:#x}); waiting for the NMI window.",
            exit.map_or(Interruptibility::NONE, |exit| exit.interruptibility),
        ),
        ExitEvent::NmiDeferredPastSti => crate::serial_println!(
            "{tag} VM exit #{number}: NMI held back by an STI shadow only (interruptibility={:#x}); waiting for the interrupt window, NMI-window exiting off.",
            exit.map_or(Interruptibility::NONE, |exit| exit.interruptibility),
        ),
        ExitEvent::NmiWindowInjected => crate::serial_println!(
            "{tag} VM exit #{number}: NMI window open; reinjecting the pending NMI at rip={rip:#018x}."
        ),
        ExitEvent::InterruptWindowInjected => crate::serial_println!(
            "{tag} VM exit #{number}: interrupt window open, the STI shadow has ended; reinjecting the pending NMI at rip={rip:#018x}."
        ),
        ExitEvent::LastMainVmcall if nmis_injected == 0 && state.nmis_handled == 0 && !state.nmi_pending => {
            crate::serial_println!(
                "{tag} VM exit #{number}: expected {VMCALL} (rax={rax:#x}); all {PLANNED_MAIN_VMCALLS} planned {VMCALL}s handled; no NMI arrived."
            )
        }
        ExitEvent::LastMainVmcall => crate::serial_println!(
            "{tag} VM exit #{number}: expected {VMCALL} (rax={rax:#x}); all {PLANNED_MAIN_VMCALLS} planned {VMCALL}s handled; NMIs: {nmis_injected} reinjected, {} handled by the guest{}.",
            state.nmis_handled,
            // An NMI deferred but never injected before the last VMCALL.
            if state.nmi_pending {
                ", 1 still pending (dropped)"
            } else {
                ""
            },
        ),
        ExitEvent::Emulated(_) => report_emulation(number, resumed.and_then(|resumed| resumed.emulated), rip),
        ExitEvent::GuestGeneralProtection => report_general_protection(number, exit, rip),
        ExitEvent::ControlRegisterWrite(write) => report_cr_write(number, write, rip),
        ExitEvent::ControlRegisterGeneralProtection => crate::serial_println!(
            "{tag} VM exit #{number}: MOV to CR{} <- {:#x}: a value the guest could not set on a CPU without {VMX} ({VMXE} or a reserved bit); injecting #GP(0) at rip={rip:#018x}.",
            exit.and_then(|exit| exit.cr_access).map_or(0, |access| access.cr),
            exit.map_or(0, |exit| exit.cr_source),
        ),
        ExitEvent::ControlRegisterUnhandled => crate::serial_println!(
            "{tag} VM exit #{number}: control-register access (qualification={:#018x}) Aleph0 does not handle (MOV from CR, CLTS, LMSW, CR3/CR8, or leaving protected mode/paging).",
            exit.and_then(|exit| exit.cr_access).map_or(0, cr_access_bits),
        ),
        // Not "resuming the guest": `run-hyperv.ps1 -InjectNmi` waits for
        // that text after main VMCALL #1, and this exit comes before it.
        ExitEvent::Halted => crate::serial_println!("{tag} VM exit #{number}: HLT; going on at rip={rip:#018x}."),
        ExitEvent::GuestInvalidOpcode => report_invalid_opcode(number, exit, rip),
        ExitEvent::BootServicesEnded => crate::serial_println!(
            "{tag} VM exit #{number}: {VMCALL} from Aleph0's ExitBootServices event (rcx={rcx:#x}): boot services are over; output phase now runtime (serial only); going on at rip={rip:#018x}."
        ),
        ExitEvent::TestVmcall => {
            crate::serial_println!(
                "{tag} VM exit #{number}: test {VMCALL} (rcx={rcx:#x}); going on at rip={rip:#018x}."
            )
        }
        ExitEvent::Unexpected if guest == GuestKind::BuiltInTest => crate::serial_println!(
            "{tag} VM exit #{number}: NOT an expected exit (the next main {VMCALL} should have rax={:#x}).",
            expected_main_rax(state),
        ),
        // The firmware has no script: the lines above name the exit.
        ExitEvent::Unexpected => {
            crate::serial_println!("{tag} VM exit #{number}: NOT an exit Aleph0 handles for the firmware.")
        }
    }
    if event == ExitEvent::LastMainVmcall {
        crate::serial_println!(
            "{tag} VM exit #{number}: {EPT}: {} write violation(s) on the protected data page and {} on the NMI stack page, write granted.",
            state.ept_writes_granted,
            state.nmi_stack_grants,
        );
    }
}

/// Prints what an emulated instruction did and where the guest resumes.
fn report_emulation(number: u64, outcome: Option<EmulationOutcome>, rip: u64) {
    let tag = crate::TAG;
    match outcome {
        Some(EmulationOutcome::Cpuid {
            leaf,
            subleaf,
            raw,
            guest,
        }) => crate::serial_println!(
            "{tag} VM exit #{number}: CPUID leaf={leaf:#x} subleaf={subleaf:#x} -> eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x} (processor ecx={:#010x}); resuming at rip={rip:#018x}.",
            guest.eax,
            guest.ebx,
            guest.ecx,
            guest.edx,
            raw.ecx,
        ),
        Some(EmulationOutcome::Xsetbv { value }) => {
            crate::serial_println!("{tag} VM exit #{number}: XSETBV XCR0 <- {value:#x}; resuming at rip={rip:#018x}.")
        }
        Some(EmulationOutcome::Rdmsr { msr, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: RDMSR {msr:#x} -> {value:#018x}; resuming at rip={rip:#018x}."
        ),
        Some(EmulationOutcome::Wrmsr { msr, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: WRMSR {msr:#x} <- {value:#018x}; resuming at rip={rip:#018x}."
        ),
        Some(EmulationOutcome::MtrrRead { msr, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: RDMSR {:#x} ({msr}) -> {value:#018x}, from the guest's MTRRs; resuming at rip={rip:#018x}.",
            msr.address(),
        ),
        Some(EmulationOutcome::MtrrWrite {
            msr,
            value,
            cpu_value,
            write,
        }) => report_mtrr_write(number, msr, value, cpu_value, write, rip),
        Some(EmulationOutcome::In { port, size, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: IN port {port:#x} size={} -> {value:#x}; resuming at rip={rip:#018x}.",
            size.bytes(),
        ),
        Some(EmulationOutcome::Out { port, size, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: OUT port {port:#x} size={} value={value:#x}; resuming at rip={rip:#018x}.",
            size.bytes(),
        ),
        Some(EmulationOutcome::Hypercall { input, result }) => crate::serial_println!(
            "{tag} VM exit #{number}: hypercall {:#06x} (rcx={input:#x}) forwarded to Hyper-V -> status {:#x} (rax={result:#x}); resuming at rip={rip:#018x}.",
            hyperv::call_code(input),
            result as u16,
        ),
        Some(EmulationOutcome::SyntheticRead { msr, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: RDMSR {msr:#x} (synthetic) forwarded to Hyper-V -> {value:#018x}; resuming at rip={rip:#018x}."
        ),
        Some(EmulationOutcome::SyntheticWrite { msr, value }) => crate::serial_println!(
            "{tag} VM exit #{number}: WRMSR {msr:#x} (synthetic) <- {value:#018x} forwarded to Hyper-V; resuming at rip={rip:#018x}."
        ),
        Some(EmulationOutcome::Refused { emulation, exception }) => crate::serial_println!(
            "{tag} VM exit #{number}: Hyper-V refused the forwarded {emulation:?} with {}; injecting it at rip={rip:#018x}.",
            exception_mnemonic(exception),
        ),
        None => crate::serial_println!("{tag} VM exit #{number}: emulated instruction not executed."),
    }
}

/// `#UD` or `#GP(0)`, the exceptions a forwarded access is refused with.
fn exception_mnemonic(exception: InjectedEvent) -> &'static str {
    if exception == InjectedEvent::INVALID_OPCODE {
        "#UD"
    } else {
        "#GP(0)"
    }
}

/// Prints why the guest gets #UD: for a VMCALL, that it came from CPL > 0
/// or that there is no Hyper-V to take it; otherwise, that it is MWAIT or
/// MONITOR.
fn report_invalid_opcode(number: u64, exit: Option<ExitInfo>, rip: u64) {
    let tag = crate::TAG;
    match exit.filter(|exit| exit.reason == ExitReason::Hypercall) {
        Some(exit) if exit.guest_cpl != 0 => crate::serial_println!(
            "{tag} VM exit #{number}: {VMCALL} (rcx={:#x}) from CPL {}, which Hyper-V refuses too; injecting #UD at rip={rip:#018x}.",
            exit.guest_rcx,
            exit.guest_cpl,
        ),
        Some(exit) => crate::serial_println!(
            "{tag} VM exit #{number}: {VMCALL} (rcx={:#x}) is not Aleph0's, and there is no Hyper-V interface to forward it to; injecting #UD at rip={rip:#018x}.",
            exit.guest_rcx,
        ),
        None => crate::serial_println!(
            "{tag} VM exit #{number}: MWAIT or MONITOR, which the guest's CPUID does not offer; injecting #UD at rip={rip:#018x}."
        ),
    }
}

/// Prints a MOV to CR0/CR4 Aleph0 emulated: the value the guest asked for
/// (also its new read shadow), and the real GUEST_CRn, which keeps the VMX
/// fixed bits (so CR4.VMXE stays set, CR0.NE stays set).
fn report_cr_write(number: u64, write: CrWrite, rip: u64) {
    crate::serial_println!(
        "{} VM exit #{number}: MOV to {} <- {:#x}: guest reads {:#x} back (read shadow), GUEST_{} = {:#x}; resuming at rip={rip:#018x}.",
        crate::TAG,
        write.cr.name(),
        write.requested,
        write.requested,
        write.cr.name(),
        write.real,
    );
}

/// Prints why the guest gets a #GP at its XSETBV, RDMSR or WRMSR: for an
/// XSETBV, the XCR and value with the XCR0 bits supported; for a WRMSR of
/// one of its MTRRs, what the CPU refuses in the value; for an MTRR address
/// the CPU has no MSR at, that; otherwise, that the MSR is outside the MSR
/// bitmap or the value is not canonical.
fn report_general_protection(number: u64, exit: Option<ExitInfo>, rip: u64) {
    let tag = crate::TAG;
    if let Some(exit) = exit.filter(|exit| exit.reason == ExitReason::Xsetbv) {
        crate::serial_println!(
            "{tag} VM exit #{number}: XSETBV XCR{} <- {:#x}: not XCR0, or a value XCR0 does not take (supported {:#x}); injecting #GP(0) at rip={rip:#018x}.",
            exit.guest_rcx as u32,
            wrmsr_value(&exit),
            exit.xcr0_supported,
        );
        return;
    }
    let msr = exit.map_or(0, |exit| exit.guest_rcx as u32);
    let refused_mtrr = exit.filter(|exit| exit.reason == ExitReason::Wrmsr).and_then(|exit| {
        let mtrr = exit.mtrr.msr(msr)?;
        let value = wrmsr_value(&exit);
        exit.mtrr.check(mtrr, value).err().map(|error| (mtrr, value, error))
    });
    let absent_mtrr = exit.is_some_and(|exit| mtrr::is_msr_address(msr) && exit.mtrr.msr(msr).is_none());
    match refused_mtrr {
        Some((mtrr, value, error)) => crate::serial_println!(
            "{tag} VM exit #{number}: WRMSR {msr:#x} ({mtrr}) <- {value:#018x} refused: {error}; the guest's MTRRs unchanged; injecting #GP(0) at rip={rip:#018x}."
        ),
        None if absent_mtrr => crate::serial_println!(
            "{tag} VM exit #{number}: MSR {msr:#x} is an MTRR address this CPU has no MSR at (see IA32_MTRRCAP); injecting #GP(0) at rip={rip:#018x}."
        ),
        None => crate::serial_println!(
            "{tag} VM exit #{number}: MSR {msr:#x} is outside the MSR bitmap, or the WRMSR value is not canonical; injecting #GP(0) at rip={rip:#018x}."
        ),
    }
}

/// Prints a WRMSR of the guest's MTRRs: the write, beside the CPU's own
/// MSR, which keeps its value; then, if the types changed, the guest's
/// MTRRs as they now stand and the EPT regions that changed, read back from
/// the PD entries.
fn report_mtrr_write(number: u64, msr: MtrrMsr, value: u64, cpu_value: u64, write: MtrrWrite, rip: u64) {
    let tag = crate::TAG;
    crate::serial_println!(
        "{tag} VM exit #{number}: WRMSR {:#x} ({msr}) <- {value:#018x}, into the guest's MTRRs; the CPU's {msr} stays {cpu_value:#018x}.",
        msr.address(),
    );
    match write.retyped {
        None => crate::serial_println!(
            "{tag} VM exit #{number}: every address keeps its memory type; {EPT} unchanged; resuming at rip={rip:#018x}."
        ),
        Some(retyped) => {
            guest_mtrrs::inspect(|mtrrs, types| {
                report_guest_mtrrs(number, mtrrs, msr.is_fixed());
                report_retyped(
                    number,
                    &retyped,
                    write.invept,
                    backend::NestedPaging::free_tables(types),
                );
                if let Some((start, end)) = retyped.span {
                    backend::NestedPaging::report_regions(types, number, start, end);
                }
            });
            crate::serial_println!("{tag} VM exit #{number}: resuming at rip={rip:#018x}.");
        }
    }
}

/// Prints the guest's MTRRs as bring-up prints the CPU's: whether they are
/// on, the variable ranges in use, and, after a write to a fixed-range MSR
/// (`fixed_written`), the types of the first MiB.
fn report_guest_mtrrs(number: u64, mtrrs: &Mtrrs, fixed_written: bool) {
    let tag = crate::TAG;
    crate::serial_println!("{tag} VM exit #{number}: guest MTRRs: {mtrrs}.");
    let Mtrrs::Enabled(enabled) = mtrrs else {
        return;
    };
    for range in enabled.variable_ranges() {
        crate::serial_println!("{tag} VM exit #{number}: guest MTRR {range}");
    }
    if fixed_written && enabled.fixed_enabled() {
        for run in mtrrs.runs(FIXED_END, PAGE_SIZE as u64) {
            crate::serial_println!(
                "{tag} VM exit #{number}: guest MTRR fixed {:#07x}..{:#07x}: {}",
                run.start,
                run.end,
                run.type_name(),
            );
        }
    }
}

/// Prints what changed in EPT after the guest's MTRRs did, and the spare
/// PTs `free_pts` left.
fn report_retyped(number: u64, retyped: &Retyped, invept: Option<Flush>, free_pts: usize) {
    let tag = crate::TAG;
    match invept {
        Some(invept) => crate::serial_println!(
            "{tag} VM exit #{number}: {EPT}: {} region(s) retyped ({} split into a spare PT, {} merged back into one 2 MiB page); {free_pts} spare PT(s) left; {INVEPT} {}.",
            retyped.regions,
            retyped.split,
            retyped.merged,
            invept.name(),
        ),
        None => crate::serial_println!(
            "{tag} VM exit #{number}: {EPT} unchanged: the types changed only where nothing is mapped."
        ),
    }
    if retyped.uncached > 0 {
        crate::serial_println!(
            "{tag} VM exit #{number}: {EPT}: {} region(s) of mixed types mapped UC as one page: no spare PT left.",
            retyped.uncached,
        );
    }
}
