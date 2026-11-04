//! Hardware abstraction layer: the traits and neutral types the shared code
//! (`bring_up`, `hv`, `host`, `resident`) uses, and the backends that
//! implement them. `intel/` is the original VT-x code. The Cargo feature
//! picks the backend at compile time; shared code calls it as `backend::…`
//! with the trait in scope. See HAL_STRUCTURE.md and HAL_INTERFACE.md.
//!
//! Terms the `# Safety` sections use:
//! - **root mode**: after [`HardwareVirtualization::enable`] and before
//!   [`HardwareVirtualization::disable`] (Intel: VMX root operation).
//! - **current vCPU**: after [`Vcpu::create`] on this CPU (Intel: its VMCS
//!   is current).
//! - **exit context**: the guest is stopped at a VM exit on this CPU, which
//!   runs the exit handler in root mode with the current vCPU.

use core::fmt::Debug;

use crate::x86::cr::CrControls;
use crate::x86::mtrr::{MtrrCapabilities, Mtrrs};
use crate::x86::page::Allocation;

use types::{
    Capabilities, CpuidBit, CrAccess, CrConstraints, CrWrite, Flush, GuestEntry, GuestKind, GuestRegisters, GuestStop,
    Hidden, HostTables, HypercallRegisters, InjectedEvent, Interruptibility, IoAccess, NestedPageFault,
    OptionalFeatures, ReasonDescription, RecoveryPoint, Refused, Retyped, WalkEnd,
};

pub mod intel;
pub mod types;

pub use types::ExitReason;

#[cfg(feature = "intel")]
pub use intel as backend;

#[cfg(not(feature = "intel"))]
compile_error!("no hardware backend selected: build with the `intel` feature (the default)");

/// The backend's control capabilities ([`HardwareVirtualization::ControlCapabilities`]).
pub type ControlCapabilities = <backend::HardwareVirtualization as HardwareVirtualization>::ControlCapabilities;

/// The CPU's virtualization extension (VT-x / AMD-V): whether it is there,
/// its capabilities, turning it on and off.
pub trait HardwareVirtualization: Sized {
    /// What the CPU allows, read once.
    type ControlCapabilities: Copy;
    type Error: Copy + Debug + PartialEq + Eq;

    /// The extension's name, for the reports ("VMX").
    const NAME: &'static str;
    /// The CPUID bit that advertises the extension; the guest must not see it.
    const CPUID_FEATURE: CpuidBit;
    /// CR4 bits a guest MOV to CR4 may not set: it gets #GP(0).
    const GUEST_FORBIDDEN_CR4: u64;
    /// The name of the bit that turns the extension on, for the reports
    /// ("CR4.VMXE").
    const ENABLE_BIT: &'static str;

    /// Supported, absent, or disabled by firmware. Safe on any CPU.
    fn detect() -> Capabilities;

    /// # Safety
    ///
    /// [`detect`](Self::detect) returned [`Capabilities::Supported`].
    unsafe fn control_capabilities() -> Self::ControlCapabilities;

    /// The serial lines about them.
    fn report_control_capabilities(capabilities: &Self::ControlCapabilities);

    /// Which optional guest features the CPU can offer.
    fn optional_features(capabilities: &Self::ControlCapabilities) -> OptionalFeatures;

    /// The serial line about what the guest is offered: `features`, the XCR0
    /// bits its XSETBV may set (`xcr0`, 0 for none), and the host's CR4.
    fn report_guest_features(features: OptionalFeatures, xcr0: u64, host_cr4: u64);

    /// The CR0/CR4 bits that must and may be 1 in root mode.
    ///
    /// # Safety
    ///
    /// [`detect`](Self::detect) returned [`Capabilities::Supported`].
    unsafe fn cr_constraints() -> CrConstraints;

    /// Enters root mode.
    ///
    /// # Safety
    ///
    /// Supported (as for [`control_capabilities`](Self::control_capabilities))
    /// and not in root mode yet.
    unsafe fn enable() -> Result<Self, Self::Error>;

    /// Leaves root mode and clears the enable bit. Prints its own failure.
    ///
    /// # Safety
    ///
    /// Root mode on this CPU, with no vCPU in use ([`Vcpu::release`] ran).
    unsafe fn disable(self) -> Result<(), Self::Error>;

    /// CR0 before [`enable`](Self::enable).
    fn original_cr0(&self) -> u64;

    /// CR4 before [`enable`](Self::enable).
    fn original_cr4(&self) -> u64;

    /// Whether the enable bit ([`ENABLE_BIT`](Self::ENABLE_BIT)) is set now.
    fn enabled() -> bool;

    /// Prints why enabling or disabling failed.
    fn report_error(error: Self::Error);
}

/// The virtual CPU before and while it runs: allocate, create, configure,
/// run.
pub trait Vcpu: Sized {
    type Error: Copy + Debug + PartialEq + Eq;

    /// Allocates what the vCPU needs before root mode (Intel: the VM-exit
    /// stack), and prints it.
    fn allocate() -> Result<Self, Self::Error>;

    /// Creates the control structure and makes it current.
    ///
    /// # Safety
    ///
    /// Root mode.
    unsafe fn create(&mut self) -> Result<(), Self::Error>;

    /// Stops using it, before [`HardwareVirtualization::disable`]. Prints its
    /// own failure.
    ///
    /// # Safety
    ///
    /// Root mode, with this the current vCPU; the guest is not running.
    unsafe fn release(&self) -> Result<(), Self::Error>;

    /// Checks the control structure answers as expected, and prints it.
    ///
    /// # Safety
    ///
    /// The current vCPU.
    unsafe fn self_test(&self) -> Result<(), Self::Error>;

    /// Which events exit, for a `kind` guest offered `features`, and prints it.
    ///
    /// # Safety
    ///
    /// The current vCPU.
    unsafe fn configure_controls(
        &self,
        capabilities: &ControlCapabilities,
        kind: GuestKind,
        features: OptionalFeatures,
    ) -> Result<(), Self::Error>;

    /// Points the vCPU at the nested page tables, and prints it.
    ///
    /// # Safety
    ///
    /// The current vCPU; `paging` is never freed.
    unsafe fn set_nested_paging(&self, paging: &backend::NestedPaging) -> Result<(), Self::Error>;

    /// Points the vCPU at the MSR and I/O intercept maps, and prints it.
    ///
    /// # Safety
    ///
    /// The current vCPU; `intercepts` is never freed.
    unsafe fn set_intercepts(&self, intercepts: &backend::Intercepts) -> Result<(), Self::Error>;

    /// The state Aleph0 runs on after an exit, with CR4 `cr4`, and prints it.
    ///
    /// # Safety
    ///
    /// The current vCPU; the tables are never freed.
    unsafe fn configure_host_state(&self, tables: &HostTables, cr4: u64) -> Result<(), Self::Error>;

    /// Where the exit path enters, on its own stack, and prints it.
    ///
    /// # Safety
    ///
    /// The current vCPU.
    unsafe fn configure_exit_entry(&self) -> Result<(), Self::Error>;

    /// Which CR0/CR4 bits the guest owns and what it reads for the others.
    ///
    /// # Safety
    ///
    /// The current vCPU.
    unsafe fn configure_cr_intercepts(&self, controls: &CrControls) -> Result<(), Self::Error>;

    /// The guest's starting state, and its report lines.
    ///
    /// # Safety
    ///
    /// The current vCPU, with the controls configured; the host GDT is live
    /// and stable.
    unsafe fn set_guest_state(&self, entry: GuestEntry) -> Result<(), Self::Error>;

    /// Enters the guest and returns once it has stopped (the test guest).
    ///
    /// # Safety
    ///
    /// The current vCPU, fully configured; everything it points at is never
    /// freed.
    unsafe fn run(&self) -> GuestStop<Self::Error>;

    /// Enters the guest in place: returns as the guest, or why the entry
    /// failed (the firmware).
    ///
    /// # Safety
    ///
    /// As for [`run`](Self::run); a stop of this guest halts.
    unsafe fn run_in_place(&self) -> Result<(), GuestStop<Self::Error>>;

    /// Bytes of the exit stack used and its size, `None` if the backend has
    /// no exit stack.
    ///
    /// # Safety
    ///
    /// The guest has stopped: no exit handler runs.
    unsafe fn exit_stack_usage(&self) -> Option<(usize, usize)>;

    /// Prints why a vCPU step failed.
    fn report_error(error: Self::Error);
}

/// The vCPU stopped at a VM exit (exit context): one reader per detail of
/// the exit, the changes made before it resumes, and its report lines.
pub trait VmExit: Sized {
    type Error: Copy + Debug;
    /// What the history keeps of an exit, printed in its line.
    type Record: Copy + core::fmt::Display;

    /// The hypercall instruction's name, for the reports ("VMCALL").
    const HYPERCALL: &'static str;
    /// A record for history slots nothing was written to.
    const EMPTY_RECORD: Self::Record;
    /// Raw reason codes with a per-reason counter of their own.
    const REASON_COUNTERS: usize;

    /// The vCPU stopped at the exit on this CPU.
    ///
    /// # Safety
    ///
    /// Exit context.
    unsafe fn current() -> Self;

    // Readers. Each reads the backend's state once; a second call returns
    // the same value.

    /// What kind of exit.
    fn reason(&self) -> Result<ExitReason, Self::Error>;
    /// The backend's raw reason code, for the reports and counters.
    fn reason_code(&self) -> Result<u32, Self::Error>;
    /// The exception or NMI of an [`ExitReason::ExceptionOrNmi`] exit.
    fn exception_or_nmi(&self) -> Result<Option<InjectedEvent>, Self::Error>;
    /// An event whose delivery the exit interrupted, to re-deliver.
    fn interrupted_event(&self) -> Result<Option<InjectedEvent>, Self::Error>;
    /// The STI and MOV SS shadows and NMI blocking.
    fn interruptibility(&self) -> Result<Interruptibility, Self::Error>;
    /// The access of an [`ExitReason::ControlRegisterAccess`] exit.
    fn cr_access(&self) -> Result<CrAccess, Self::Error>;
    /// The access of an [`ExitReason::IoInstruction`] exit.
    fn io_access(&self) -> Result<IoAccess, Self::Error>;
    /// The fault of an [`ExitReason::NestedPageFault`] or
    /// [`ExitReason::NestedPagingMisconfig`] exit.
    fn nested_page_fault(&self) -> Result<NestedPageFault, Self::Error>;
    /// Whether the exit was a fault in an IRET that had ended NMI blocking.
    fn faulted_nmi_unblocking_iret(&self) -> Result<bool, Self::Error>;
    /// The guest's privilege level.
    fn guest_cpl(&self) -> Result<u8, Self::Error>;
    /// Where the guest stopped.
    fn guest_rip(&self) -> Result<u64, Self::Error>;
    /// The length of the exiting instruction.
    fn instruction_length(&self) -> Result<u64, Self::Error>;
    /// The guest's RSP (not among the saved registers).
    fn guest_rsp(&self) -> Result<u64, Self::Error>;
    /// The guest's CR4.
    fn guest_cr4(&self) -> Result<u64, Self::Error>;
    /// What the history keeps of this exit.
    fn record(&self) -> Self::Record;
    /// Whether a field the exit was read from could not be read (the report
    /// then prints the exit in full).
    fn unreadable(&self) -> bool;

    // Changes before the guest resumes.

    /// Resumes at `rip`.
    fn set_rip(&mut self, rip: u64) -> Result<(), Self::Error>;
    /// Changes the shadows and NMI blocking.
    fn set_interruptibility(&mut self, state: Interruptibility) -> Result<(), Self::Error>;
    /// Delivers `event` at the next entry.
    fn inject_event(&mut self, event: InjectedEvent) -> Result<(), Self::Error>;
    /// Exits as soon as the guest can take an NMI (`on`), or not.
    fn set_nmi_window(&mut self, on: bool) -> Result<(), Self::Error>;
    /// Exits as soon as the guest can take an interrupt (`on`), or not.
    fn set_interrupt_window(&mut self, on: bool) -> Result<(), Self::Error>;
    /// Emulates a MOV to CR0/CR4; on failure, names the step.
    fn write_cr(&mut self, write: CrWrite) -> Result<(), (&'static str, Self::Error)>;

    // Reports.

    /// Prints what was read about exit `number`: reason, its details, where
    /// the guest stopped, `registers` (RAX to RDX), what could not be read.
    fn report_read(&self, number: u64, registers: [u64; 4]);
    /// Prints the guest's state at a stop.
    fn dump(&self, number: u64, registers: &GuestRegisters);
    /// The number, name and entry-failure flag of a raw reason code.
    fn describe_reason(code: u32) -> ReasonDescription;
    /// `event` as the backend encodes it, for the reports.
    fn encode_event(event: InjectedEvent) -> u32;
    /// `state` as the backend encodes it, for the reports.
    fn encode_interruptibility(state: Interruptibility) -> u32;
}

/// The second-level page tables (EPT / NPT): the map (`Self`), and the
/// tables the exit handler changes ([`Tables`](Self::Tables)).
pub trait NestedPaging: Sized {
    /// What the CPU's nested paging offers.
    type Support: Copy;
    /// The part the exit handler owns: memory types, hiding, walks.
    type Tables;
    /// A page mapped read-only that can be made writable.
    type ProtectedPage: Copy;
    type Error: Copy + Debug + PartialEq + Eq;
    /// Why a flush after a change failed.
    type FlushError: Copy + Debug + PartialEq + Eq;

    /// The tables' name, for the reports ("EPT").
    const NAME: &'static str;
    /// The flush instruction's name, for the reports ("INVEPT").
    const FLUSH: &'static str;

    /// Whether Aleph0's map can be built on this CPU.
    fn check_support(
        capabilities: &ControlCapabilities,
        physical_address_bits: u8,
    ) -> Result<Self::Support, Self::Error>;
    /// The serial lines about it.
    fn report_support(support: Self::Support);
    /// The largest guest-physical address bits the CPU translates.
    fn physical_address_bits(support: Self::Support) -> u8;
    /// Extra tables to keep for the guest's MTRR writes.
    fn spare_tables(mtrr: MtrrCapabilities) -> usize;

    /// The identity map, with `protected_pages` read-only.
    ///
    /// # Safety
    ///
    /// Boot services active; memory identity-mapped.
    unsafe fn build(
        protected_pages: &[u64],
        support: Self::Support,
        mtrrs: &Mtrrs,
        spare_tables: usize,
    ) -> Result<(Self, Self::Tables), Self::Error>;
    /// The serial lines about the map.
    fn report(&self, tables: &Self::Tables, protected_pages: &[u64]);
    /// The end of RAM the map covers.
    fn memory_map_end(&self) -> u64;

    /// Redirects Aleph0's pages away from the guest, keeping `visible`.
    ///
    /// # Safety
    ///
    /// Before the guest runs: the CPU has not walked the tables.
    unsafe fn hide(
        tables: &mut Self::Tables,
        allocations: impl Iterator<Item = Allocation>,
        mtrrs: &Mtrrs,
        visible: &[Allocation],
    ) -> Result<Hidden, Self::Error>;
    /// The serial lines about what hiding did.
    fn report_hidden(hidden: Hidden);
    /// Gives the pages the memory types `mtrrs` gives them, and flushes.
    ///
    /// # Safety
    ///
    /// Exit context; the tables are the published ones.
    unsafe fn apply_memory_types(tables: &mut Self::Tables, mtrrs: &Mtrrs) -> Result<Retyped, Self::FlushError>;
    /// Which flush a change to these tables needs.
    fn flush_type(tables: &Self::Tables) -> Flush;
    /// The flush's name, for the reports.
    fn flush_name(flush: Flush) -> &'static str;
    /// Spare tables left.
    fn free_tables(tables: &Self::Tables) -> usize;
    /// Where `gpa` ends up.
    fn walk(tables: &Self::Tables, gpa: u64) -> WalkEnd;
    /// The text of a walk's end, for the reports.
    fn fmt_walk_end(end: WalkEnd, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
    /// Prints the walk of `gpa` at exit `number`; `tables` is `None` when
    /// they are not available.
    ///
    /// # Safety
    ///
    /// Exit context.
    unsafe fn report_walk(tables: Option<&Self::Tables>, number: u64, gpa: u64);
    /// Prints the regions of `tables` between `start` and `end` at exit
    /// `number`.
    fn report_regions(tables: &Self::Tables, number: u64, start: u64, end: u64);
    /// Test only: breaks the entry for `gpa`.
    ///
    /// # Safety
    ///
    /// Before the guest runs.
    #[cfg(feature = "ept-misconfig-test")]
    unsafe fn misconfigure(tables: &Self::Tables, gpa: u64);

    /// The protected page holding `gpa`.
    fn protected_page_for(gpa: u64) -> Option<Self::ProtectedPage>;
    /// Its GPA.
    fn protected_page_gpa(page: Self::ProtectedPage) -> u64;
    /// The protected NMI stack page, 0 if none.
    fn nmi_stack_page() -> u64;
    /// Makes `page` writable, and flushes.
    ///
    /// # Safety
    ///
    /// Exit context.
    unsafe fn grant_write(page: Self::ProtectedPage) -> Result<Flush, Self::FlushError>;

    /// Prints why building the map failed.
    fn report_error(error: Self::Error);
}

/// Which MSR and I/O accesses exit (MSR/I/O bitmaps, MSRPM/IOPM).
pub trait Intercepts: Sized {
    type Error: Copy + Debug + PartialEq + Eq;

    /// Intercepts every MTRR MSR and the policy lists for `kind`.
    fn allocate(kind: GuestKind) -> Result<Self, Self::Error>;
    /// Whether the map has a bit for `msr`; outside it every access exits.
    fn covers_msr(msr: u32) -> bool;
    /// The serial lines about it.
    fn report(&self, kind: GuestKind, mtrr: MtrrCapabilities);
    /// Prints why allocating failed.
    fn report_error(error: Self::Error);
}

/// The hypercall instruction (VMCALL / VMMCALL).
pub trait Hypercall {
    /// From the guest: Aleph0's own call with `rcx` (`hv::vmcall`).
    ///
    /// # Safety
    ///
    /// Only in the guest Aleph0 runs, which exits at it and resumes past it.
    unsafe fn call(rcx: u64);

    /// From root mode: the guest's hypercall in `registers`, to the
    /// hypervisor underneath (Hyper-V); its results replace them.
    ///
    /// # Safety
    ///
    /// Exit context under Hyper-V, at CPL 0, with the host IDT loaded, whose
    /// #UD gate recovers through [`recovery_point`](Self::recovery_point).
    unsafe fn forward(registers: &mut HypercallRegisters) -> Result<(), Refused>;

    /// Where the host fault handler resumes when the forward is refused.
    fn recovery_point() -> RecoveryPoint;
}
