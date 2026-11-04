//! The VM-exit handler the backend's exit path calls: read the exit, judge
//! it, carry it out, report it.
//!
//! [`handle_exit`] runs four steps:
//!
//! 1. [`read_exit`] reads the exit through the backend's readers
//!    (`hal::VmExit`) and the guest registers. It prints nothing.
//! 2. [`judge`] folds in an NMI that arrived while Aleph0 ran (`crate::host`)
//!    and passes the exit to `exit_decision::decide` with the state kept
//!    between exits.
//! 3. [`carry_out`] executes an emulated instruction and prepares the guest
//!    as the event's `ResumePlan` says, or stops the guest. It prints nothing
//!    either: a step that fails says why in its [`ResumeFailure`].
//! 4. `report.rs` prints the exit and its [`Outcome`], once everything about
//!    the exit is known, in as much detail as the print policy gives it
//!    (`policy.rs`): whole for a stop and for the first exits of each reason,
//!    rate-limited after that.

use core::arch::x86_64::{__cpuid_count, CpuidResult};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use core::fmt;

use super::guest_mtrrs::{self, MtrrAccessFailure, MtrrWrite};
use super::history::{self, ExitRecord};
use super::policy::{self, COUNTERS, MAX_LEVEL, OTHER_REASONS, exit_detail};
use super::report::{ExitReport, report_exit, report_stop};
use super::stats::{self, VcpuStats};
use crate::hal::types::{
    CrConstraints, ExitReason, Flush, GuestKind, GuestRegisters as RegisterFrame, InjectedEvent, Interruptibility,
};
use crate::hal::{HardwareVirtualization as _, NestedPaging as _, VmExit as _, backend};
use crate::host;
use crate::hv::exit_decision::{
    Emulation, ExitAction, ExitEvent, ExitInfo, ExitState, IoSize, ResumePlan, cpuid_policy, decide, is_canonical_48,
    merge_in,
};
use crate::hv::features;
use crate::hv::hyperv::{self, HypercallRegisters};
use crate::output::Phase;
use crate::port;
use crate::x86::cr;
use crate::x86::msr;
use crate::x86::mtrr::MtrrMsr;
use crate::x86::page;

/// The backend's error for a field it could not read or write.
type FieldError = <backend::VmExit as crate::hal::VmExit>::Error;
/// The backend's protected page.
type ProtectedPage = <backend::NestedPaging as crate::hal::NestedPaging>::ProtectedPage;
/// The backend's error for a flush that failed.
type FlushError = <backend::NestedPaging as crate::hal::NestedPaging>::FlushError;

/// Exits so far per basic reason ([`policy::counter`]), including the
/// current one: for the print policy and the histogram every stop prints.
/// Only the exit handler touches them, with interrupts off, on the one CPU
/// that runs the guest; the atomics just avoid `static mut`.
static EXITS_BY_REASON: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

/// Counts an exit of basic reason `basic`, and returns how many there have
/// been, this one included.
fn count_exit(slot: usize) -> u64 {
    EXITS_BY_REASON[slot].fetch_add(1, Ordering::Relaxed) + 1
}

/// The counts of [`EXITS_BY_REASON`] as they stand.
fn exits_by_reason() -> [u64; COUNTERS] {
    core::array::from_fn(|slot| EXITS_BY_REASON[slot].load(Ordering::Relaxed))
}

// The `ExitState` that `decide` carries from one exit to the next, one
// atomic per field, for the same reason as `EXITS_BY_REASON`.
static MAIN_VMCALLS: AtomicU32 = AtomicU32::new(0);
static NMI_PENDING: AtomicBool = AtomicBool::new(false);
static NMIS_HANDLED: AtomicU32 = AtomicU32::new(0);
static EPT_WRITES_GRANTED: AtomicU32 = AtomicU32::new(0);
static NMI_STACK_GRANTS: AtomicU32 = AtomicU32::new(0);

fn load_state() -> ExitState {
    ExitState {
        main_vmcalls: MAIN_VMCALLS.load(Ordering::Relaxed),
        nmi_pending: NMI_PENDING.load(Ordering::Relaxed),
        nmis_handled: NMIS_HANDLED.load(Ordering::Relaxed),
        ept_writes_granted: EPT_WRITES_GRANTED.load(Ordering::Relaxed),
        nmi_stack_grants: NMI_STACK_GRANTS.load(Ordering::Relaxed),
    }
}

fn store_state(state: ExitState) {
    MAIN_VMCALLS.store(state.main_vmcalls, Ordering::Relaxed);
    NMI_PENDING.store(state.nmi_pending, Ordering::Relaxed);
    NMIS_HANDLED.store(state.nmis_handled, Ordering::Relaxed);
    EPT_WRITES_GRANTED.store(state.ept_writes_granted, Ordering::Relaxed);
    NMI_STACK_GRANTS.store(state.nmi_stack_grants, Ordering::Relaxed);
}

/// The guest RIP after an instruction of `length` bytes, or `None` when that would wrap past the top of the
/// address space or land outside the canonical range.
///
/// `rip` is the guest's: a guest kernel can map code in the last bytes of the address space, where the sum
/// overflows (a panic in debug builds, a silent wrap in release). An instruction ending at the last canonical
/// byte of the lower half does not overflow, but gives `0x0000_8000_0000_0000`, and VM entry would then fail its
/// guest-state checks on RIP. The caller stops the guest instead, in both cases.
fn next_rip(rip: u64, length: u64) -> Option<u64> {
    rip.checked_add(length).filter(|&next| is_canonical_48(next))
}

/// The value a MOV to CR0/CR4 loads, from the general-purpose register
/// `number` names (`CrAccess::gpr`). RSP (4) is not in `frame`; it is the
/// guest's RSP, read only for a control-register exit (`rsp`). Returns
/// `None` when the source is RSP but the guest's RSP was unreadable.
fn cr_source_value(number: u8, frame: &RegisterFrame, rsp: Option<Result<u64, FieldError>>) -> Option<u64> {
    Some(match number {
        0 => frame.rax,
        1 => frame.rcx,
        2 => frame.rdx,
        3 => frame.rbx,
        4 => rsp?.ok()?,
        5 => frame.rbp,
        6 => frame.rsi,
        7 => frame.rdi,
        8 => frame.r8,
        9 => frame.r9,
        10 => frame.r10,
        11 => frame.r11,
        12 => frame.r12,
        13 => frame.r13,
        14 => frame.r14,
        // `number` is 4 bits (`CrAccess::gpr`), so the last is R15.
        _ => frame.r15,
    })
}

/// The VM-exit handler: decides what the exit is (`exit_decision::decide`),
/// carries that out, prints the exit over serial, and returns what the
/// backend's exit path does next with the exit's number: resume the guest
/// (its state prepared to go on), or, for the test guest, stop it. A stop of
/// the firmware halts here ([`halt_unless_returnable`]).
///
/// Runs in exit context with interrupts off. It may only use things that
/// need no firmware: the backend's exit (`exit`) and the serial port.
/// `uefi::println!` must not be called here; `log!` and the panic handler
/// skip the console once the phase below is set. For an emulated
/// instruction it also writes the result into the guest's registers in
/// `frame`.
pub fn handle_exit(exit: &mut backend::VmExit, frame: &mut RegisterFrame) -> (ExitAction, u64) {
    // First, so every line below (and any panic) skips the firmware console.
    // Put back when the guest resumes ([`resume_phase`]). After a stop that
    // returns, bring-up switches it back once it has left root mode.
    let phase = crate::output::set_phase(Phase::VmExit);
    #[cfg(feature = "host-fault-test")]
    host::fault_on_purpose();
    #[cfg(feature = "forward-refusal-test")]
    hyperv::refuse_on_purpose();

    let guest = GuestKind::SELECTED;
    let number = stats::update(VcpuStats::count_exit);
    let read = read_exit(exit, frame);
    let host_nmi = host::take_pending_nmi();
    let (event, next) = judge(guest, read.exit, host_nmi);
    let outcome = carry_out(event, &read, next, frame, exit);
    if let Outcome::Resumed { plan, .. } = outcome {
        stats::update(|stats| stats.count_resumed(event, plan));
        stats::update_forwarded(|forwarded| forwarded.count(plan.emulate));
    }
    // Before anything prints, so a panic while printing still shows this exit.
    history::record(ExitRecord {
        number,
        raw: exit.record(),
        registers: read.registers,
        event,
    });
    let slot = read.reason_code.map_or(OTHER_REASONS, |code| {
        policy::counter(backend::VmExit::describe_reason(code).number)
    });
    let count = count_exit(slot);
    report_exit(&ExitReport {
        guest,
        number,
        count,
        detail: exit_detail(
            count,
            outcome.stops(),
            exit.unreadable(),
            event == ExitEvent::BootServicesEnded,
            MAX_LEVEL,
        ),
        read: &read,
        exit,
        host_nmi,
        event,
        next,
        outcome,
    });
    if outcome.stops() {
        report_stop(number, frame, &exits_by_reason(), exit);
        halt_unless_returnable(guest);
    } else {
        #[cfg(feature = "host-nmi-test")]
        if event == ExitEvent::MainVmcall {
            wait_for_host_nmi(number);
        }
        // Last, after every change of this exit.
        keep_nmi_window_for_host_nmi(number, exit);
        crate::output::set_phase(resume_phase(event, phase));
    }
    (outcome.action(), number)
}

/// The output phase the guest resumes in after `event`: the one the exit
/// found (`found`), except after the `ExitBootServices` VMCALL, from which
/// on it is [`Phase::Runtime`] for good: the firmware console is gone.
fn resume_phase(event: ExitEvent, found: Phase) -> Phase {
    if event == ExitEvent::BootServicesEnded {
        Phase::Runtime
    } else {
        found
    }
}

/// Halts after a stop of a `guest` whose stop cannot return to bring-up
/// (`GuestKind::stop_returns_to_bring_up`): the firmware, launched in place,
/// has reused the stack `Vcpu::run_in_place` ran on, and no frame was saved
/// to jump back to. The stop's report is already on serial.
fn halt_unless_returnable(guest: GuestKind) {
    if !guest.stop_returns_to_bring_up() {
        crate::output::halt_forever();
    }
}

/// What [`read_exit`] read about one exit, through the backend's readers;
/// the backend keeps every field it read for its own report lines.
pub(super) struct ExitRead {
    /// The exit's reason.
    pub(super) reason: Result<ExitReason, FieldError>,
    /// The backend's raw reason code, for the counters and the stop value.
    pub(super) reason_code: Result<u32, FieldError>,
    /// Where the guest stopped.
    pub(super) rip: Result<u64, FieldError>,
    /// The exiting instruction's length.
    pub(super) length: Result<u64, FieldError>,
    /// The guest's RAX, RBX, RCX and RDX as the exit path saved them, before
    /// an emulated instruction changes them.
    ///
    /// After a VM-entry failure the guest never ran, so they are whatever was
    /// live when entry was attempted: the host's registers for the first
    /// entry, or the restored guest registers (the previous exit's) for a
    /// resume. Either way, not this exit's guest.
    pub(super) registers: [u64; 4],
    /// The page the nested page tables protect, for granting write.
    protected: Option<ProtectedPage>,
    /// Everything `decide` needs, `None` if a field it needs was unreadable.
    pub(super) exit: Option<ExitInfo>,
}

/// Reads the exit through the backend's readers and the guest registers,
/// and collects what [`judge`] and [`carry_out`] need. Prints nothing.
fn read_exit(exit: &backend::VmExit, frame: &RegisterFrame) -> ExitRead {
    let reason = exit.reason();
    let reason_code = exit.reason_code();
    let decoded = reason.ok();
    let event = exit.exception_or_nmi();
    let nested_exit = matches!(
        decoded,
        Some(ExitReason::NestedPageFault | ExitReason::NestedPagingMisconfig)
    );
    let fault = nested_exit.then(|| exit.nested_page_fault());
    // Only a hypercall is judged by the CPL; 0 otherwise, which `decide`
    // never looks at, and an unreadable one leaves the hypercall unjudged.
    let guest_cpl = if decoded == Some(ExitReason::Hypercall) {
        exit.guest_cpl().ok()
    } else {
        Some(0)
    };
    // Only a MOV to CR0/CR4 needs the CR constraints, the guest's RSP (for a
    // write sourced from RSP) and the source value; `decide` ignores them for
    // any other exit.
    let cr_access_exit = decoded == Some(ExitReason::ControlRegisterAccess);
    let cr_constraints = if cr_access_exit {
        // SAFETY: exit context implies root mode, so the extension is
        // supported.
        unsafe { backend::HardwareVirtualization::cr_constraints() }
    } else {
        CrConstraints::NONE
    };
    let cr_access = cr_access_exit.then(|| exit.cr_access());
    let guest_rsp = cr_access_exit.then(|| exit.guest_rsp());
    // The value the MOV loads; `None` only if its source is RSP and the
    // guest's RSP was unreadable, which leaves the exit unjudged. `Some(0)`
    // for any other exit, which `decide` never looks at.
    let cr_source = match cr_access {
        Some(Ok(access)) => cr_source_value(access.gpr, frame, guest_rsp),
        Some(Err(_)) => None,
        None => Some(0),
    };
    // Only CPUID copies the guest's CR4 (OSXSAVE, PKE); an unreadable one
    // leaves the CPUID unjudged, and 0 stands in for any other exit.
    let guest_cr4 = if decoded == Some(ExitReason::Cpuid) {
        exit.guest_cr4().ok()
    } else {
        Some(0)
    };
    let io_access = (decoded == Some(ExitReason::IoInstruction)).then(|| exit.io_access());
    let rip = exit.guest_rip();
    let length = exit.instruction_length();
    let interruptibility = exit.interruptibility();
    let interrupted = exit.interrupted_event();
    let faulted_nmi_unblocking_iret = exit.faulted_nmi_unblocking_iret();
    // The read-only page this exit's access fell in, if any: a nested page
    // fault whose GPA is one of the published protected pages (the data
    // page, or the NMI-delivery stack page). The grant path makes exactly
    // this page writable.
    let protected = fault
        .and_then(Result::ok)
        .and_then(|fault| backend::NestedPaging::protected_page_for(fault.gpa));
    let mtrr = guest_mtrrs::capabilities();

    // An optional detail is `Some(None)` when the exit does not have it,
    // `None` when it was unreadable: then the exit cannot be judged.
    fn detail<T, E>(value: Option<Result<T, E>>) -> Option<Option<T>> {
        match value {
            Some(Ok(value)) => Some(Some(value)),
            Some(Err(_)) => None,
            None => Some(None),
        }
    }
    let exit_info = match (
        decoded,
        event.ok(),
        interruptibility.ok(),
        interrupted.ok(),
        faulted_nmi_unblocking_iret.ok(),
        detail(cr_access),
        detail(io_access),
        detail(fault),
        guest_cpl,
        cr_source,
        guest_cr4,
    ) {
        (
            Some(reason),
            Some(event),
            Some(interruptibility),
            Some(interrupted),
            Some(faulted_nmi_unblocking_iret),
            Some(cr_access),
            Some(io_access),
            Some(fault),
            Some(guest_cpl),
            Some(cr_source),
            Some(guest_cr4),
        ) => Some(ExitInfo {
            reason,
            event,
            guest_rax: frame.rax,
            guest_rcx: frame.rcx,
            guest_rdx: frame.rdx,
            interruptibility,
            interrupted,
            cr_access,
            io_access,
            fault,
            faulted_nmi_unblocking_iret,
            protected_page: protected.map_or(0, backend::NestedPaging::protected_page_gpa),
            stack_page: backend::NestedPaging::nmi_stack_page(),
            mtrr,
            hyperv: hyperv::offered(),
            guest_cpl,
            cr_source,
            cr_constraints,
            guest_cr4,
            xcr0_supported: features::published().xcr0,
        }),
        // An unreadable field is reported later; without it the exit cannot
        // be judged.
        _ => None,
    };

    ExitRead {
        reason,
        reason_code,
        rip,
        length,
        registers: [frame.rax, frame.rbx, frame.rcx, frame.rdx],
        protected,
        exit: exit_info,
    }
}

/// Decides what `exit` of `guest` is with the state kept between exits, and
/// keeps the state for the next one. An NMI that arrived in root operation
/// since the last exit (`host_nmi`) is pending for the guest first. An exit
/// that could not be read is unexpected.
fn judge(guest: GuestKind, exit: Option<ExitInfo>, host_nmi: bool) -> (ExitEvent, ExitState) {
    let state = load_state().with_host_nmi(host_nmi);
    let (event, next) = match exit {
        Some(exit) => decide(guest, state, exit),
        None => (ExitEvent::Unexpected, state),
    };
    store_state(next);
    (event, next)
}

/// What carrying out an exit's event came to.
#[derive(Debug, Clone, Copy)]
pub(super) enum Outcome {
    /// The guest goes on: the VMCS is prepared as `plan` says.
    Resumed { plan: ResumePlan, resumed: Resumed },
    /// The plan could not be carried out, so the guest stops.
    ResumeFailed(ResumeFailure),
    /// The event itself stops the guest (`Finished` or `Unexpected`).
    Stopped(ExitAction),
}

impl Outcome {
    /// What the entry stub does next.
    pub(super) fn action(self) -> ExitAction {
        match self {
            Self::Resumed { plan, .. } => ExitAction::Resume(plan),
            Self::ResumeFailed(_) => ExitAction::Unexpected,
            Self::Stopped(action) => action,
        }
    }

    /// Whether the guest stops at this exit.
    pub(super) fn stops(self) -> bool {
        !matches!(self, Self::Resumed { .. })
    }
}

/// Carries out `event`: prepares the guest to resume as its plan says, or
/// stops it. `next` is the state after the exit. Prints nothing.
fn carry_out(
    event: ExitEvent,
    read: &ExitRead,
    next: ExitState,
    frame: &mut RegisterFrame,
    exit: &mut backend::VmExit,
) -> Outcome {
    match event.action() {
        ExitAction::Resume(plan) => {
            let plan = plan.keeping_pending_nmi(next);
            // A fault in an IRET that had ended NMI blocking: resume with the
            // blocking set again. `exit` is always read for a resumed exit.
            let plan = match read.exit {
                Some(exit) => plan.restoring_nmi_blocking(&exit),
                None => plan,
            };
            // Re-deliver an event this exit interrupted mid-delivery. The plan
            // refuses when it would also advance RIP or inject its own event,
            // since only one event is injected per VM entry.
            let plan = match read
                .exit
                .and_then(|exit| exit.redelivery(read.length.ok().map(|length| length as u32)))
            {
                Some(redelivery) => plan.redelivering(redelivery),
                None => Some(plan),
            };
            match plan {
                Some(plan) => match prepare_resume(read, plan, frame, exit) {
                    Ok((plan, resumed)) => Outcome::Resumed { plan, resumed },
                    Err(failure) => Outcome::ResumeFailed(failure),
                },
                None => Outcome::ResumeFailed(ResumeFailure::RedeliveryConflict),
            }
        }
        stop => Outcome::Stopped(stop),
    }
}

/// Turns NMI-window exiting on if an NMI arrived in root operation after
/// [`judge`] looked. The host NMI handler turns it on itself, but a write
/// of the primary controls in [`prepare_resume`] may have replaced its
/// write; nothing writes them after this, so the guest exits to take the
/// NMI as soon as it can.
fn keep_nmi_window_for_host_nmi(number: u64, exit: &mut backend::VmExit) {
    if !host::nmi_pending() {
        return;
    }
    crate::serial_println!(
        "{} VM exit #{number}: an NMI arrived in root operation during this exit; NMI-window exiting on.",
        crate::TAG
    );
    if let Err(error) = exit.set_nmi_window(true) {
        crate::serial_println!(
            "{} VM exit #{number}: could not turn NMI-window exiting on ({error:?}); the NMI waits for the next exit.",
            crate::TAG
        );
    }
}

/// Waits in root operation, after main VMCALL #1, until an NMI arrives or
/// the guest's own wait would have given up. `run-hyperv.ps1 -InjectNmi`
/// sends its NMI once serial shows the resume line printed just before, so
/// the NMI reaches the host NMI handler instead of the guest.
#[cfg(feature = "host-nmi-test")]
fn wait_for_host_nmi(number: u64) {
    let tag = crate::TAG;
    crate::serial_println!("{tag} VM exit #{number}: host-nmi-test: waiting in VMX root operation for an NMI.");
    let mut rounds = crate::hv::guest::GUEST_WAIT_ITERATIONS;
    while !host::nmi_pending() && rounds > 0 {
        core::hint::spin_loop();
        rounds -= 1;
    }
    if host::nmi_pending() {
        crate::serial_println!(
            "{tag} VM exit #{number}: host-nmi-test: the host NMI handler ran ({} NMI(s) in root operation so far).",
            host::nmis_taken()
        );
    } else {
        crate::serial_println!("{tag} VM exit #{number}: host-nmi-test: no NMI arrived; going on.");
    }
}

/// What an emulated instruction did, for the report.
#[derive(Debug, Clone, Copy)]
pub(super) enum EmulationOutcome {
    /// CPUID `leaf`/`subleaf`: what the processor returned (`raw`) and what
    /// the guest got after `cpuid_policy` (`guest`).
    Cpuid {
        leaf: u32,
        subleaf: u32,
        raw: CpuidResult,
        guest: CpuidResult,
    },
    /// XSETBV wrote `value` to XCR0.
    Xsetbv { value: u64 },
    /// RDMSR of `msr` returned `value`.
    Rdmsr { msr: u32, value: u64 },
    /// WRMSR wrote `value` to `msr`.
    Wrmsr { msr: u32, value: u64 },
    /// RDMSR of the guest's MTRR `msr` returned `value`, from its copy.
    MtrrRead { msr: MtrrMsr, value: u64 },
    /// WRMSR wrote `value` to the guest's MTRR `msr`, in its copy, with
    /// what EPT made of it (`write`). The CPU's own MSR holds `cpu_value`,
    /// read after the write, which it leaves alone.
    MtrrWrite {
        msr: MtrrMsr,
        value: u64,
        cpu_value: u64,
        write: MtrrWrite,
    },
    /// IN of `size` from `port` read `value`.
    In { port: u16, size: IoSize, value: u32 },
    /// OUT of `value` (`size` wide) to `port`.
    Out { port: u16, size: IoSize, value: u32 },
    /// The hypercall with input value `input` went to Hyper-V, which
    /// returned `result` in RAX: the status in bits 15:0, the reps done in
    /// bits 43:32.
    Hypercall { input: u64, result: u64 },
    /// RDMSR of the synthetic MSR `msr` returned `value`, from Hyper-V.
    SyntheticRead { msr: u32, value: u64 },
    /// WRMSR wrote `value` to the synthetic MSR `msr`, at Hyper-V.
    SyntheticWrite { msr: u32, value: u64 },
    /// Hyper-V refused the forwarded `emulation` with `exception`, which the
    /// guest gets at its own instruction instead.
    Refused {
        emulation: Emulation,
        exception: InjectedEvent,
    },
}

impl EmulationOutcome {
    /// The exception the guest gets when Hyper-V refused the access.
    fn refused(self) -> Option<InjectedEvent> {
        match self {
            Self::Refused { exception, .. } => Some(exception),
            _ => None,
        }
    }
}

/// Whether the hypercall with input value `input` names a page of Aleph0's
/// own memory as its input or output ([`hyperv::hypercall_gpas`]), which
/// Hyper-V would read or write past EPT, so the forwarder must refuse it.
fn hypercall_names_aleph0(input: u64, frame: &RegisterFrame) -> bool {
    hyperv::hypercall_gpas(input, frame.rdx, frame.r8)
        .into_iter()
        .flatten()
        .any(page::is_resident)
}

/// Executes `emulation` in root operation and writes its result into the
/// guest's registers in `frame`. Only an access to the guest's MTRRs can
/// fail; one Hyper-V refuses comes back as [`EmulationOutcome::Refused`],
/// with `frame` unchanged.
fn execute(emulation: Emulation, frame: &mut RegisterFrame) -> Result<EmulationOutcome, ResumeFailure> {
    Ok(match emulation {
        Emulation::Cpuid {
            leaf,
            subleaf,
            guest_cr4,
        } => {
            // In root operation CPUID returns the processor's values (under
            // Hyper-V: what Hyper-V shows Aleph0).
            let raw = __cpuid_count(leaf, subleaf);
            let guest = cpuid_policy(leaf, subleaf, raw, guest_cr4, features::published());
            // CPUID writes 32-bit registers, which zero-extends them.
            frame.rax = guest.eax.into();
            frame.rbx = guest.ebx.into();
            frame.rcx = guest.ecx.into();
            frame.rdx = guest.edx.into();
            EmulationOutcome::Cpuid {
                leaf,
                subleaf,
                raw,
                guest,
            }
        }
        Emulation::Xsetbv { value } => {
            // SAFETY: `decide` lets an XSETBV through only with supported XCR0
            // bits published, which bring-up does only with CR4.OSXSAVE in
            // HOST_CR4 (`GuestFeatures::host_cr4`), and only for a value XCR0
            // takes, so it cannot fault. VM entry and exit do not switch XCR0
            // and Aleph0 never touches vector state (`vmexit` entry stub), so
            // the guest's XCR0 is simply the CPU's.
            unsafe { cr::xsetbv(value) };
            EmulationOutcome::Xsetbv { value }
        }
        Emulation::Rdmsr { msr } => {
            // SAFETY: `decide` emulates only INTERCEPTED_MSRS, which exist on
            // every CPU Aleph0 runs on (IA32_KERNEL_GS_BASE: every 64-bit
            // CPU) and have no read side effects.
            let value = unsafe { msr::rdmsr(msr) };
            // RDMSR writes EAX and EDX, which zero-extends them.
            frame.rax = value & 0xFFFF_FFFF;
            frame.rdx = value >> 32;
            EmulationOutcome::Rdmsr { msr, value }
        }
        Emulation::Wrmsr { msr, value } => {
            // SAFETY: an intercepted MSR (see Rdmsr), and `decide` checked
            // the value is canonical, so WRMSR cannot fault. VM entry and
            // exit do not switch IA32_KERNEL_GS_BASE and Aleph0 never runs
            // SWAPGS, so the guest's value is simply the CPU's.
            unsafe { msr::wrmsr(msr, value) };
            EmulationOutcome::Wrmsr { msr, value }
        }
        Emulation::MtrrRead { msr } => {
            let value = guest_mtrrs::read(msr).map_err(ResumeFailure::GuestMtrrs)?;
            // RDMSR writes EAX and EDX, which zero-extends them.
            frame.rax = value & 0xFFFF_FFFF;
            frame.rdx = value >> 32;
            EmulationOutcome::MtrrRead { msr, value }
        }
        Emulation::MtrrWrite { msr, value } => {
            // SAFETY: exit context: VMX root operation, the guest stopped at
            // this exit.
            let write = unsafe { guest_mtrrs::write(msr, value) }.map_err(ResumeFailure::GuestMtrrs)?;
            // SAFETY: `decide` routes only the MTRR MSRs the CPU has to the
            // copy, and reading one has no side effects. Read for the report,
            // which shows the CPU's untouched.
            let cpu_value = unsafe { msr::rdmsr(msr.address()) };
            EmulationOutcome::MtrrWrite {
                msr,
                value,
                cpu_value,
                write,
            }
        }
        Emulation::In { port, size } => {
            // SAFETY: the guest asked for exactly this access, which it could
            // have made without Aleph0, and IN/OUT at CPL 0 in root operation
            // cannot fault. A 2- or 4-byte access at 0x3FC-0x3FE also touches
            // 0x3FF, so it exits and passes through to COM1's registers at
            // 0x3FC-0x3FE, the guest's own UART (docs/EMULATED_EXITS.md
            // section 5).
            let value = unsafe {
                match size {
                    IoSize::Byte => u32::from(port::inb(port)),
                    IoSize::Word => u32::from(port::inw(port)),
                    IoSize::Dword => port::inl(port),
                }
            };
            frame.rax = merge_in(frame.rax, size, value);
            EmulationOutcome::In { port, size, value }
        }
        Emulation::Out { port, size, value } => {
            // SAFETY: as for In: the guest's own OUT.
            unsafe {
                match size {
                    IoSize::Byte => port::outb(port, value as u8),
                    IoSize::Word => port::outw(port, value as u16),
                    IoSize::Dword => port::outl(port, value),
                }
            }
            EmulationOutcome::Out { port, size, value }
        }
        Emulation::Hypercall { input } if hypercall_names_aleph0(input, frame) => {
            // The hypercall names an Aleph0 page as its input or output: Hyper-V
            // would read or write Aleph0's own memory past EPT (HYPERV_INTERFACE.md
            // section 8). Refuse it as Hyper-V refuses a bad hypercall, with #GP
            // at the guest's VMCALL, instead of forwarding.
            EmulationOutcome::Refused {
                emulation,
                exception: InjectedEvent::GENERAL_PROTECTION,
            }
        }
        Emulation::Hypercall { input } => {
            let mut registers = HypercallRegisters {
                rcx: input,
                rdx: frame.rdx,
                r8: frame.r8,
                rax: 0,
            };
            // SAFETY: exit context: VMX root operation at CPL 0 with the host
            // IDT loaded, the guest stopped at its VMCALL. `decide` forwards
            // only with Hyper-V's interface present and from the guest's CPL
            // 0, so this is a hypercall the guest could have made itself.
            match unsafe { hyperv::forward_hypercall(&mut registers) } {
                Ok(()) => {
                    frame.rax = registers.rax;
                    frame.rcx = registers.rcx;
                    frame.rdx = registers.rdx;
                    frame.r8 = registers.r8;
                    EmulationOutcome::Hypercall {
                        input,
                        result: registers.rax,
                    }
                }
                Err(_) => EmulationOutcome::Refused {
                    emulation,
                    exception: InjectedEvent::INVALID_OPCODE,
                },
            }
        }
        Emulation::SyntheticRead { msr } => {
            // SAFETY: as for Hypercall, at the guest's RDMSR, which needs CPL
            // 0 to exit at all.
            match unsafe { hyperv::forward_rdmsr(msr) } {
                Ok(value) => {
                    // RDMSR writes EAX and EDX, which zero-extends them.
                    frame.rax = value & 0xFFFF_FFFF;
                    frame.rdx = value >> 32;
                    EmulationOutcome::SyntheticRead { msr, value }
                }
                Err(_) => EmulationOutcome::Refused {
                    emulation,
                    exception: InjectedEvent::GENERAL_PROTECTION,
                },
            }
        }
        Emulation::SyntheticWrite { msr, value } if hyperv::overlay_gpa(msr, value).is_some_and(page::is_resident) => {
            // The guest asks Hyper-V to overlay one of its pages at a GPA that
            // falls in Aleph0's own memory (HYPERV_INTERFACE.md section 8):
            // refuse with #GP at the guest's WRMSR instead of forwarding.
            EmulationOutcome::Refused {
                emulation,
                exception: InjectedEvent::GENERAL_PROTECTION,
            }
        }
        Emulation::SyntheticWrite { msr, value } => {
            // SAFETY: as for SyntheticRead, at the guest's WRMSR.
            match unsafe { hyperv::forward_wrmsr(msr, value) } {
                Ok(()) => EmulationOutcome::SyntheticWrite { msr, value },
                Err(_) => EmulationOutcome::Refused {
                    emulation,
                    exception: InjectedEvent::GENERAL_PROTECTION,
                },
            }
        }
    })
}

/// How the guest goes on after [`prepare_resume`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Resumed {
    /// Where the guest resumes.
    pub(super) rip: u64,
    /// The flush used, if the protected page was made writable.
    pub(super) invept: Option<Flush>,
    /// What the emulated instruction did, if the plan emulated one.
    pub(super) emulated: Option<EmulationOutcome>,
}

/// Why the guest cannot resume from an exit it was meant to resume from.
/// Its `Display` is the report's line, before "; not resuming.".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResumeFailure {
    /// GUEST_RIP could not be read.
    RipUnreadable,
    /// RIP must move past the instruction, but VM_EXIT_INSTRUCTION_LEN could
    /// not be read.
    LengthUnreadable,
    /// The next RIP would wrap past the top of the address space or be
    /// non-canonical ([`next_rip`]).
    NextRipInvalid,
    /// A VMWRITE the plan needs failed; `what` names the step.
    Write { what: &'static str, error: FieldError },
    /// The plan makes the protected page writable, but there is none.
    NoProtectedPage,
    /// The flush failed after the protected page was made writable.
    GrantWrite(FlushError),
    /// RDMSR or WRMSR of the guest's MTRRs failed.
    GuestMtrrs(MtrrAccessFailure),
    /// The exit interrupted an event's delivery, but this exit's plan already
    /// advances RIP or injects its own event, so the event cannot be
    /// re-delivered: only one event is injected per VM entry. (A software
    /// event whose instruction length is unreadable fails the same way.)
    RedeliveryConflict,
}

impl fmt::Display for ResumeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::RipUnreadable => f.write_str("guest RIP unreadable"),
            Self::LengthUnreadable => f.write_str("instruction length unreadable"),
            Self::NextRipInvalid => f.write_str("next guest RIP would wrap or be non-canonical"),
            Self::Write { what, error } => write!(f, "could not {what} ({error:?})"),
            Self::NoProtectedPage => f.write_str("no protected page to make writable"),
            Self::GrantWrite(error) => write!(
                f,
                "could not make the protected page writable ({} {error:?})",
                backend::NestedPaging::FLUSH
            ),
            Self::GuestMtrrs(failure) => write!(f, "{failure}"),
            Self::RedeliveryConflict => f.write_str(
                "an interrupted event cannot be re-delivered while the exit advances RIP or injects its own event",
            ),
        }
    }
}

/// Prepares the guest (and the nested page tables) as `plan` says, through
/// the backend's `exit`: executes the emulated
/// instruction into `frame`, moves GUEST_RIP past the exiting instruction,
/// updates the guest's interruptibility state (an STI or MOV SS shadow ended,
/// blocking by NMI cleared or set again), queues an NMI, an exception or a
/// re-delivered event for injection at the next VM entry, turns NMI-window
/// and interrupt-window exiting on or off, and makes the protected page
/// writable. Returns the plan carried out, which injects an exception instead
/// of advancing RIP when Hyper-V refused a forwarded access, and how the guest
/// resumes; or why it cannot.
fn prepare_resume(
    read: &ExitRead,
    plan: ResumePlan,
    frame: &mut RegisterFrame,
    exit: &mut backend::VmExit,
) -> Result<(ResumePlan, Resumed), ResumeFailure> {
    let write = |what| move |error| ResumeFailure::Write { what, error };
    let rip = read.rip.map_err(|_| ResumeFailure::RipUnreadable)?;
    // Checked before the emulation: with the length unreadable or the new RIP
    // wrapping or non-canonical, a WRMSR or OUT would already have happened
    // when the guest stops, and serial would never show it.
    let advance_to = if plan.advance_rip {
        let length = read.length.map_err(|_| ResumeFailure::LengthUnreadable)?;
        Some(next_rip(rip, length).ok_or(ResumeFailure::NextRipInvalid)?)
    } else {
        None
    };
    // Before RIP moves: the guest resumes past the instruction with its
    // result already in its registers.
    let emulated = plan.emulate.map(|emulation| execute(emulation, frame)).transpose()?;
    // Hyper-V refused a forwarded access: the guest gets the same exception,
    // at its own instruction, as it would without Aleph0.
    let (plan, advance_to) = match emulated.and_then(EmulationOutcome::refused) {
        Some(exception) => (plan.refused_with(exception), None),
        None => (plan, advance_to),
    };
    let ResumePlan {
        inject_nmi,
        nmi_window,
        interrupt_window,
        grant_write,
        inject_exception,
        redeliver,
        ..
    } = plan;

    let resume_rip = if let Some(next) = advance_to {
        exit.set_rip(next).map_err(write("advance guest RIP"))?;
        next
    } else {
        // An NMI exit: the guest goes on at the instruction it was about to
        // run, and the injected NMI is delivered before it. An EPT
        // violation: the faulting write runs again. A #GP or #UD: delivered
        // at the RDMSR, WRMSR or VMCALL, which faults.
        rip
    };

    // Ends an STI or MOV SS shadow when RIP moved, clears blocking by NMI for
    // an NMI re-delivery, or sets it again after a fault in an NMI-unblocking
    // IRET (`ResumePlan::interruptibility`). `exit` is always read here:
    // `decide` only plans a resume for an exit it could judge. Written only
    // when something changes, so most exits pay nothing.
    let found = read.exit.map_or(Interruptibility::NONE, |exit| exit.interruptibility);
    let interruptibility = plan.interruptibility(found);
    if interruptibility != found {
        exit.set_interruptibility(interruptibility)
            .map_err(write("update the guest's interruptibility state"))?;
    }
    if inject_nmi {
        exit.inject_event(InjectedEvent::NMI)
            .map_err(write("queue the NMI for injection"))?;
    }
    if let Some(exception) = inject_exception {
        exit.inject_event(exception)
            .map_err(write("queue the exception for injection"))?;
    }
    if let Some(redelivery) = redeliver {
        exit.inject_event(redelivery)
            .map_err(write("queue the interrupted event for re-delivery"))?;
    }
    if let Some(on) = nmi_window {
        exit.set_nmi_window(on).map_err(write(if on {
            "turn NMI-window exiting on"
        } else {
            "turn NMI-window exiting off"
        }))?;
    }
    if let Some(on) = interrupt_window {
        exit.set_interrupt_window(on).map_err(write(if on {
            "turn interrupt-window exiting on"
        } else {
            "turn interrupt-window exiting off"
        }))?;
    }
    if let Some(cr_write) = plan.write_cr {
        // Emulate a MOV to CR0/CR4: the guest reads `requested` back, while
        // the real register keeps the fixed bits (`real`).
        exit.write_cr(cr_write)
            .map_err(|(what, error)| ResumeFailure::Write { what, error })?;
    }
    let invept = if grant_write {
        let page = read.protected.ok_or(ResumeFailure::NoProtectedPage)?;
        // SAFETY: exit context, guest stopped. `page` came from
        // `NestedPaging::protected_page_for`, so `build` made the tables,
        // which are never freed.
        Some(unsafe { backend::NestedPaging::grant_write(page) }.map_err(ResumeFailure::GrantWrite)?)
    } else {
        None
    };
    Ok((
        plan,
        Resumed {
            rip: resume_rip,
            invept,
            emulated,
        },
    ))
}

/// After the backend's exit path found that resuming the guest failed (and
/// printed why): the same stop report as an exit that stops the guest, then
/// a halt for the firmware. `frame` holds the guest's registers.
pub fn stop_after_failed_resume(frame: &RegisterFrame) {
    let guest = GuestKind::SELECTED;
    // SAFETY: the resume just failed in exit context: still root mode, with
    // the guest's vCPU current.
    let exit = unsafe { backend::VmExit::current() };
    // The resume belongs to the last exit, so its number heads the report.
    report_stop(stats::snapshot().exits, frame, &exits_by_reason(), &exit);
    halt_unless_returnable(guest);
}

#[cfg(test)]
#[path = "../../../tests/unit/hv/vmexit/handler.rs"]
mod tests;
