//! The neutral types of the hardware interface: what the shared code
//! (`bring_up`, `hv`, `host`, `resident`) passes to and gets from the
//! traits in `hal/mod.rs`. Nothing here is a backend's encoding; each
//! backend converts its own to and from these (HAL_STRUCTURE.md section 8).

use core::fmt;

use crate::x86::mtrr::MemoryType;
use crate::x86::segment::DescriptorTable;

use super::backend;
use super::{NestedPaging as _, VmExit as _};

/// Whether the CPU's virtualization extension can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capabilities {
    /// The CPU has it and firmware allows entering root mode.
    Supported,
    /// The CPU does not report it.
    NotSupportedByCpu,
    /// The CPU has it, but firmware disabled it (locked off).
    DisabledByFirmware,
}

/// What kind of VM exit happened: the reasons Aleph0 handles, each from
/// its own reader in the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// An exception or NMI in the guest (`VmExit::exception_or_nmi` says
    /// which).
    ExceptionOrNmi,
    /// The guest can take an external interrupt (interrupt window).
    InterruptWindow,
    /// The guest can take an NMI (NMI window).
    NmiWindow,
    Cpuid,
    Hlt,
    /// The hypercall instruction (VMCALL / VMMCALL).
    Hypercall,
    /// A control-register access (`VmExit::cr_access` says which).
    ControlRegisterAccess,
    /// IN/OUT/INS/OUTS (`VmExit::io_access` says which).
    IoInstruction,
    Rdmsr,
    Wrmsr,
    Mwait,
    Monitor,
    /// An access the nested page tables do not allow
    /// (`VmExit::nested_page_fault` says which).
    NestedPageFault,
    /// A nested page-table entry the CPU refuses.
    NestedPagingMisconfig,
    Xsetbv,
    /// VM entry failed: the guest never ran. The backend's raw reason code.
    EntryFailure(u32),
    /// Any other exit, which Aleph0 does not handle. The backend's raw code.
    Other(u32),
}

/// A raw reason code as the reports print it (`VmExit::describe_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasonDescription {
    /// The number the reports show and the per-reason counters use.
    pub number: u16,
    /// Its name.
    pub name: &'static str,
    /// VM entry failed: the guest never ran.
    pub entry_failure: bool,
}

/// How an event is delivered to the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    ExternalInterrupt,
    Nmi,
    HardwareException,
    SoftwareInterrupt,
    PrivilegedSoftwareException,
    SoftwareException,
    /// A kind Aleph0 never builds, kept as the backend's raw value so a
    /// re-delivery re-arms it as it was.
    Other(u8),
}

/// IDT vector of NMI.
pub const NMI_VECTOR: u8 = 2;

/// An event delivered at the next VM entry, or one an exit reports
/// (event injection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectedEvent {
    pub kind: EventKind,
    pub vector: u8,
    /// The error code delivered with it, if it carries one.
    pub error_code: Option<u32>,
    /// The instruction length, for a software interrupt or exception, which
    /// the CPU needs to land RIP past the instruction.
    pub instruction_len: Option<u32>,
}

impl InjectedEvent {
    /// An NMI.
    pub const NMI: Self = Self {
        kind: EventKind::Nmi,
        vector: NMI_VECTOR,
        error_code: None,
        instruction_len: None,
    };
    /// #GP with error code 0, delivered before the faulting instruction
    /// runs again.
    pub const GENERAL_PROTECTION: Self = Self {
        kind: EventKind::HardwareException,
        vector: 13,
        error_code: Some(0),
        instruction_len: None,
    };
    /// #UD, which has no error code.
    pub const INVALID_OPCODE: Self = Self {
        kind: EventKind::HardwareException,
        vector: 6,
        error_code: None,
        instruction_len: None,
    };

    /// Whether this is an NMI.
    pub fn is_nmi(self) -> bool {
        self.kind == EventKind::Nmi && self.vector == NMI_VECTOR
    }

    /// Whether VM entry needs the instruction length for this event:
    /// software interrupts and exceptions.
    pub fn needs_instruction_len(self) -> bool {
        matches!(
            self.kind,
            EventKind::SoftwareInterrupt | EventKind::PrivilegedSoftwareException | EventKind::SoftwareException
        )
    }
}

/// The event as the backend encodes it, for the reports (`{:#010x}`).
impl fmt::LowerHex for InjectedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&backend::VmExit::encode_event(*self), f)
    }
}

/// What holds back interrupts and NMIs in the guest right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interruptibility {
    /// Blocking by STI: the instruction after STI.
    pub sti_shadow: bool,
    /// Blocking by MOV SS / POP SS: the instruction after it.
    pub mov_ss_shadow: bool,
    /// An NMI is being handled: no NMI until its IRET.
    pub nmi_blocked: bool,
    /// The backend's other bits, which Aleph0 leaves as they are.
    pub other: u32,
}

impl Interruptibility {
    /// Nothing blocked.
    pub const NONE: Self = Self {
        sti_shadow: false,
        mov_ss_shadow: false,
        nmi_blocked: false,
        other: 0,
    };

    /// Whether anything stops an NMI from being injected now.
    pub fn blocks_nmi_injection(self) -> bool {
        self.sti_shadow || self.mov_ss_shadow || self.nmi_blocked
    }

    /// Whether the STI shadow is the only thing that stops an NMI injection.
    pub fn only_sti_shadow(self) -> bool {
        self.sti_shadow && !self.mov_ss_shadow && !self.nmi_blocked
    }

    /// After Aleph0 skips the instruction the guest exited on: the STI and
    /// MOV SS shadows end, the rest stays.
    ///
    /// Both last until the end of the instruction after STI or MOV SS. When
    /// that instruction is the one skipped, the CPU never ends them, so the
    /// handler must, as KVM does whenever it skips an instruction. Example: an
    /// idle loop's `sti; hlt` exits on the HLT inside the STI shadow. Resumed
    /// past it with the shadow still set, the guest would keep interrupts
    /// blocked through one more instruction it never asked to protect.
    pub fn after_skipped_instruction(self) -> Self {
        Self {
            sti_shadow: false,
            mov_ss_shadow: false,
            ..self
        }
    }

    /// With NMI blocking cleared, for re-delivering an NMI whose delivery
    /// faulted. VM entry refuses to inject an NMI while it is set, and the
    /// faulted delivery set it, so re-delivery must clear it, as KVM's
    /// `__vmx_complete_interrupts` does when it re-queues an NMI from
    /// IDT-vectoring.
    pub fn after_nmi_redelivery(self) -> Self {
        Self {
            nmi_blocked: false,
            ..self
        }
    }
}

/// The state as the backend encodes it, for the reports (`{:#x}`).
impl fmt::LowerHex for Interruptibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&backend::VmExit::encode_interruptibility(*self), f)
    }
}

/// Which control register a write Aleph0 emulates goes to. Only CR0 and CR4
/// are owned, so only they can exit with Aleph0's masks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrNumber {
    Cr0,
    Cr4,
}

impl CrNumber {
    /// `"CR0"` or `"CR4"`, for the report.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cr0 => "CR0",
            Self::Cr4 => "CR4",
        }
    }
}

/// A MOV to CR0 or CR4 Aleph0 emulates: the guest reads `requested` back,
/// while the real register becomes `real` (forced into the range root mode
/// allows, `x86::cr::legal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrWrite {
    pub cr: CrNumber,
    /// The value the guest tried to load; also what it reads back.
    pub requested: u64,
    /// The new real register value.
    pub real: u64,
}

/// What a control-register access did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrAccessKind {
    MovTo,
    MovFrom,
    Clts,
    Lmsw,
}

/// A control-register access exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrAccess {
    /// The control register: 0, 3, 4 or 8.
    pub cr: u8,
    pub kind: CrAccessKind,
    /// The general-purpose register of a MOV, in the instruction encoding's
    /// numbering (0 RAX, 1 RCX, 2 RDX, 3 RBX, 4 RSP, 5 RBP, 6 RSI, 7 RDI,
    /// 8-15 R8-R15). RSP (4) is not among the saved registers; the handler
    /// reads it on its own.
    pub gpr: u8,
}

/// How many bytes an IN or OUT moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoSize {
    Byte,
    Word,
    Dword,
}

impl IoSize {
    /// Bytes moved: 1, 2 or 4.
    pub fn bytes(self) -> u8 {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
        }
    }

    /// The bits of RAX the access uses: AL, AX or EAX.
    pub fn mask(self) -> u64 {
        match self {
            Self::Byte => 0xFF,
            Self::Word => 0xFFFF,
            Self::Dword => 0xFFFF_FFFF,
        }
    }
}

/// An I/O instruction exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoAccess {
    pub port: u16,
    /// The access size, `None` for an encoding the CPU leaves unused.
    pub size: Option<IoSize>,
    /// IN (or INS); OUT (or OUTS) otherwise.
    pub is_in: bool,
    /// INS or OUTS.
    pub is_string: bool,
    /// With a REP prefix.
    pub is_rep: bool,
}

/// A nested page fault: an access the nested page tables do not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedPageFault {
    /// The guest-physical address accessed.
    pub gpa: u64,
    pub read: bool,
    pub write: bool,
    pub fetch: bool,
    /// What the entries allowed for the page.
    pub page_readable: bool,
    pub page_writable: bool,
    pub page_executable: bool,
    /// The guest's linear address is known (`VmExit::guest_linear_address`).
    pub linear_valid: bool,
    /// The access was to the final translation of a linear address, not to
    /// a guest page-table entry.
    pub translated_access: bool,
}

impl NestedPageFault {
    /// The fault Aleph0 handles: a write to a page mapped readable but not
    /// writable, through the linear address the guest used.
    pub fn is_protected_write(self) -> bool {
        self.write && self.page_readable && self.linear_valid && self.translated_access && !self.page_writable
    }
}

/// The 15 general-purpose registers the exit path saves, in the order the
/// Intel entry stub pushes them (RSP is the backend's, not here).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// Which guest Aleph0 runs: the regression guest or the firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestKind {
    /// Aleph0's built-in test guest (`hv::guest`), the regression mode. Its
    /// IDT has only the NMI gate, and its scripted hypercalls end the run. A
    /// stop returns to bring-up, and bring-up then leaves root mode.
    BuiltInTest,
    /// The firmware that loaded Aleph0, virtualized in place: it goes on
    /// running as the guest after bring-up returns. A stop cannot return
    /// (the firmware reused the stack bring-up ran on), so it halts.
    Firmware,
}

impl GuestKind {
    /// The guest this build runs: the test guest with the `test-guest`
    /// feature, the firmware otherwise.
    pub const SELECTED: Self = if cfg!(feature = "test-guest") {
        Self::BuiltInTest
    } else {
        Self::Firmware
    };

    /// Whether a stop of this guest returns to bring-up, through the frame
    /// the launch saved. The in-place launch saves none, so a stop of the
    /// firmware halts.
    pub const fn stop_returns_to_bring_up(self) -> bool {
        matches!(self, Self::BuiltInTest)
    }
}

/// Where the guest starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestEntry {
    /// At `rip` with `rsp` and its own IDT (the test guest).
    At { rip: u64, rsp: u64, idtr: DescriptorTable },
    /// Where the code that launches it goes on, as the guest (the
    /// firmware), with the firmware's IDT.
    InPlace { idtr: DescriptorTable },
}

/// The selectors Aleph0 runs with after a VM exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSelectors {
    pub cs: u16,
    pub ss: u16,
    pub ds: u16,
    pub es: u16,
    pub fs: u16,
    pub gs: u16,
    pub tr: u16,
}

/// The tables Aleph0 runs on after a VM exit (`host::HostEnvironment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTables {
    pub cr3: u64,
    pub gdtr_base: u64,
    pub idtr_base: u64,
    pub tr_base: u64,
    pub selectors: HostSelectors,
}

/// The CR0 and CR4 bits root mode fixes: a bit set in `*_must_be_one` must
/// be 1, a bit clear in `*_may_be_one` must be 0. The guest/host masks, read
/// shadows (`x86::cr::CrControls`) and the control-register decision follow
/// from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrConstraints {
    pub cr0_must_be_one: u64,
    pub cr0_may_be_one: u64,
    pub cr4_must_be_one: u64,
    pub cr4_may_be_one: u64,
}

impl CrConstraints {
    /// All zero: no bit fixed. A placeholder for exits that are not
    /// control-register accesses, which `exit_decision::decide` never reads
    /// it for.
    pub const NONE: Self = Self {
        cr0_must_be_one: 0,
        cr0_may_be_one: 0,
        cr4_must_be_one: 0,
        cr4_may_be_one: 0,
    };

    /// The CR0 bits root mode fixes, so the guest/host mask owns them
    /// (typically PE, NE and PG).
    pub fn cr0_owned(self) -> u64 {
        crate::x86::cr::owned(self.cr0_must_be_one, self.cr0_may_be_one)
    }

    /// The CR4 bits root mode fixes, so the guest/host mask owns them.
    pub fn cr4_owned(self) -> u64 {
        crate::x86::cr::owned(self.cr4_must_be_one, self.cr4_may_be_one)
    }
}

/// How the guest stopped, for bring-up. `E` is the backend's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestStop<E> {
    /// The last planned exit arrived after `exits` exits.
    Finished { exits: u64 },
    /// Exit `number` was not one Aleph0 handles; `reason` is the backend's
    /// raw reason code (`VmExit::describe_reason`).
    Unexpected { number: u64, reason: u32 },
    /// Entering the guest failed.
    LaunchFailed { error: E },
    /// Resuming the guest after an exit failed.
    ResumeFailed { error: E },
}

/// Which translations a change to the nested page tables flushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flush {
    /// The translations of this guest's tables only.
    SingleContext,
    /// Every guest's.
    AllContexts,
}

impl Flush {
    /// The backend's name for it, for the reports.
    pub fn name(self) -> &'static str {
        backend::NestedPaging::flush_name(self)
    }
}

/// What the entries allow for a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

/// Where a walk of the nested page tables ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkEnd {
    /// A page of `size` bytes maps the GPA to `hpa`, allowing `permissions`,
    /// of `memory_type`.
    Mapped {
        hpa: u64,
        size: u64,
        permissions: Permissions,
        memory_type: MemoryType,
    },
    /// Nothing maps the GPA.
    NotPresent,
    /// The last entry is one the CPU refuses.
    Misconfigured,
    /// The last entry points to `table`, which is not one of the tables: not
    /// followed.
    OutsideTables { table: u64 },
    /// The GPA has a bit set above what the walk translates.
    BeyondWalk,
}

/// The backend's text for it.
impl fmt::Display for WalkEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        backend::NestedPaging::fmt_walk_end(*self, f)
    }
}

/// What changed in the nested page tables after the guest's MTRRs did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retyped {
    /// Regions whose mapping changed.
    pub regions: usize,
    /// The address range they span, if any changed.
    pub span: Option<(u64, u64)>,
    /// Large pages split into a spare table.
    pub split: usize,
    /// Tables merged back into one large page.
    pub merged: usize,
    /// Regions of mixed types mapped uncached as one page: no spare table
    /// left.
    pub uncached: usize,
}

impl Retyped {
    /// Whether any mapping changed, so the CPU's translations need a flush.
    pub fn changed(&self) -> bool {
        self.regions > 0
    }
}

/// What hiding Aleph0's pages from the guest did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hidden {
    /// Data pages redirected to the shadow page.
    pub data_pages: usize,
    /// Code pages made read-only.
    pub code_pages: usize,
    /// Large pages split to do it.
    pub regions_split: usize,
}

/// Optional guest features beyond the minimum, each offered when the CPU's
/// virtualization extension allows it (CPUID names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionalFeatures {
    pub rdtscp: bool,
    pub invpcid: bool,
    pub xsaves: bool,
    /// TPAUSE, UMONITOR, UMWAIT.
    pub waitpkg: bool,
}

impl OptionalFeatures {
    /// None offered.
    pub const NONE: Self = Self {
        rdtscp: false,
        invpcid: false,
        xsaves: false,
        waitpkg: false,
    };
}

/// A CPUID register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "each backend names the register of its own CPUID bit")]
pub enum CpuidRegister {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

/// One CPUID bit: `mask` in `register` of leaf `leaf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuidBit {
    pub leaf: u32,
    pub register: CpuidRegister,
    pub mask: u32,
}

/// The registers a hypercall reads and writes (TLFS, "Hypercall Inputs" for
/// x64): RCX the input value, RDX and R8 the input and output GPAs (or, for
/// a fast hypercall, its input data, and RDX:R8 its output), RAX the result
/// value. Hyper-V may also update RCX, the rep start index, when it
/// continues a rep hypercall.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HypercallRegisters {
    pub rcx: u64,
    pub rdx: u64,
    pub r8: u64,
    pub rax: u64,
}

/// Hyper-V refused a forwarded instruction with the exception its
/// forwarder recovers from: #UD for the hypercall, #GP for RDMSR and WRMSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// Where the host fault handler resumes after `vector` at `instruction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPoint {
    pub instruction: *const u8,
    pub vector: u8,
    pub resume: *const u8,
}
