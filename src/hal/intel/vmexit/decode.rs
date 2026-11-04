//! The VMCS encodings of an exit, moved unchanged from `hv::exit_decision`:
//! the exit reason (VM_EXIT_REASON), the interruption-information layout,
//! the exit qualifications of EPT, control-register and I/O exits, and the
//! guest-interruptibility bits. `hal::intel::vm_exit` decodes them into the
//! neutral types (`hal::types`).

use crate::hal::types::{IoSize, NMI_VECTOR};

/// VM-exit reason, split into the fields of the VM_EXIT_REASON VMCS field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitReason {
    /// Bits 15:0: which event caused the exit (Intel SDM Vol. 3D, Appendix C).
    pub basic: u16,
    /// Bit 31: VM entry itself failed. The guest never ran; `basic` says
    /// why (33 = invalid guest state, 34 = MSR loading, 41 = machine check).
    pub entry_failure: bool,
}

impl ExitReason {
    pub fn from_raw(raw: u32) -> Self {
        Self {
            basic: raw as u16,
            entry_failure: raw & (1 << 31) != 0,
        }
    }
}

/// Human-readable name for a basic exit reason. Only the reasons this
/// hypervisor is likely to meet early on are spelled out.
pub fn exit_reason_name(basic: u16) -> &'static str {
    match basic {
        0 => "exception or NMI",
        1 => "external interrupt",
        2 => "triple fault",
        3 => "INIT signal",
        7 => "interrupt window",
        8 => "NMI window",
        9 => "task switch",
        10 => "CPUID",
        12 => "HLT",
        18 => "VMCALL",
        28 => "control-register access",
        30 => "I/O instruction",
        31 => "RDMSR",
        32 => "WRMSR",
        33 => "VM-entry failure: invalid guest state",
        34 => "VM-entry failure: MSR loading",
        36 => "MWAIT",
        39 => "MONITOR",
        41 => "VM-entry failure: machine-check event",
        48 => "EPT violation",
        49 => "EPT misconfiguration",
        55 => "XSETBV",
        UNREADABLE_REASON => "VM_EXIT_REASON unreadable",
        _ => "unnamed exit reason",
    }
}

/// Reason the handler reports when VM_EXIT_REASON itself could not be read.
/// Basic exit reasons stop far below 0xFFFF, so it never names a real exit.
pub const UNREADABLE_REASON: u16 = 0xFFFF;

/// Basic exit reason of an exception or NMI exit.
pub const EXIT_REASON_EXCEPTION_OR_NMI: u16 = 0;
/// Basic exit reason of an interrupt-window exit: the guest can take an
/// external interrupt (RFLAGS.IF = 1, no STI or MOV SS blocking). Aleph0 turns
/// interrupt-window exiting on only to wait out an STI shadow that holds back
/// a pending NMI.
pub const EXIT_REASON_INTERRUPT_WINDOW: u16 = 7;
/// Basic exit reason of an NMI-window exit.
pub const EXIT_REASON_NMI_WINDOW: u16 = 8;
/// Basic exit reason of CPUID, which always exits.
pub const EXIT_REASON_CPUID: u16 = 10;
/// Basic exit reason of HLT, with HLT exiting on.
pub const EXIT_REASON_HLT: u16 = 12;
/// Basic exit reason of a VMCALL exit.
pub const EXIT_REASON_VMCALL: u16 = 18;
/// Basic exit reason of a control-register access: a MOV to a CR0/CR4 bit the
/// guest/host mask owns, CLTS, or LMSW.
pub const EXIT_REASON_CONTROL_REGISTER: u16 = 28;
/// Basic exit reason of IN/OUT (and INS/OUTS) to a port the I/O bitmaps
/// intercept.
pub const EXIT_REASON_IO_INSTRUCTION: u16 = 30;
/// Basic exit reason of RDMSR, which exits for MSRs the MSR bitmap
/// intercepts or does not cover.
pub const EXIT_REASON_RDMSR: u16 = 31;
/// Basic exit reason of WRMSR, which exits for the same MSRs as RDMSR.
pub const EXIT_REASON_WRMSR: u16 = 32;
/// Basic exit reason of MWAIT, with MWAIT exiting on (Hyper-V forces it).
pub const EXIT_REASON_MWAIT: u16 = 36;
/// Basic exit reason of MONITOR, with MONITOR exiting on (Hyper-V forces it).
pub const EXIT_REASON_MONITOR: u16 = 39;
/// Basic exit reason of an EPT violation: an access the EPT entries do not
/// allow.
pub const EXIT_REASON_EPT_VIOLATION: u16 = 48;
/// Basic exit reason of an EPT misconfiguration: an EPT entry the CPU
/// cannot use at all (for example W = 1 with R = 0). It stops the guest
/// (`ExitEvent::EptMisconfig`).
pub const EXIT_REASON_EPT_MISCONFIG: u16 = 49;
/// Basic exit reason of XSETBV, which always exits.
pub const EXIT_REASON_XSETBV: u16 = 55;

// Exit qualification bits of an EPT violation (Intel SDM Vol. 3C, "Exit
// Qualification for EPT Violations").
/// Bit 0: the access was a read.
pub const EPT_ACCESS_READ: u64 = 1 << 0;
/// Bit 1: the access was a write.
pub const EPT_ACCESS_WRITE: u64 = 1 << 1;
/// Bit 2: the access was an instruction fetch.
pub const EPT_ACCESS_FETCH: u64 = 1 << 2;
/// Bit 3: the EPT entries allowed reads of the page.
pub const EPT_PAGE_READABLE: u64 = 1 << 3;
/// Bit 4: the EPT entries allowed writes.
pub const EPT_PAGE_WRITABLE: u64 = 1 << 4;
/// Bit 5: the EPT entries allowed execution.
pub const EPT_PAGE_EXECUTABLE: u64 = 1 << 5;
/// Bit 7: GUEST_LINEAR_ADDRESS is valid.
pub const EPT_LINEAR_VALID: u64 = 1 << 7;
/// Bit 8 (with bit 7): the access was to the linear address the guest
/// used, not to one of the guest's own page tables during the walk.
pub const EPT_TRANSLATED_ACCESS: u64 = 1 << 8;
/// Bit 12: the violation happened during an IRET that ended NMI blocking
/// (undefined when IDT_VECTORING_INFO is valid). The IRET had already
/// unblocked NMIs when it faulted, so resuming at it needs blocking by NMI set
/// again by hand (`decode_faulted_nmi_unblocking_iret`).
pub const EPT_NMI_UNBLOCKING_IRET: u64 = 1 << 12;

/// An EPT violation's exit qualification (EXIT_QUALIFICATION), decoded with
/// the bits above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EptQualification(pub u64);

impl EptQualification {
    /// Whether the access was a write.
    pub fn is_write(self) -> bool {
        self.0 & EPT_ACCESS_WRITE != 0
    }

    /// The kind of access, for the report. Only one is named; a write wins,
    /// since that is the access Aleph0 handles.
    pub fn access_name(self) -> &'static str {
        if self.is_write() {
            "write"
        } else if self.0 & EPT_ACCESS_FETCH != 0 {
            "instruction fetch"
        } else if self.0 & EPT_ACCESS_READ != 0 {
            "read"
        } else {
            "no access bit set"
        }
    }

    /// What the EPT entries allowed for the page, from bits 5:3, as
    /// `(R, W, X)` with 1 for allowed.
    pub fn permissions(self) -> (u8, u8, u8) {
        (
            u8::from(self.0 & EPT_PAGE_READABLE != 0),
            u8::from(self.0 & EPT_PAGE_WRITABLE != 0),
            u8::from(self.0 & EPT_PAGE_EXECUTABLE != 0),
        )
    }

    /// Whether GUEST_LINEAR_ADDRESS is valid (bit 7).
    pub fn linear_valid(self) -> bool {
        self.0 & EPT_LINEAR_VALID != 0
    }
}

/// An interruption-information value: VM_EXIT_INTERRUPTION_INFO,
/// IDT_VECTORING_INFO and VM_ENTRY_INTERRUPTION_INFO share the layout.
///
/// ```text
///  31    30  13  12           11          10   8  7      0
/// ┌─────┬──────┬────────────┬────────────┬───────┬────────┐
/// │valid│ res. │NMI unblock.│ error code │ type  │ vector │
/// │     │      │ (exit only)│   valid    │       │        │
/// └─────┴──────┴────────────┴────────────┴───────┴────────┘
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptionInfo(pub u32);

impl InterruptionInfo {
    /// Bit 31: the field is valid.
    pub const VALID: u32 = 1 << 31;
    /// Interruption type 2 (bits 10:8): NMI.
    const TYPE_NMI: u32 = 2;
    /// Interruption type 3 (bits 10:8): hardware exception.
    const TYPE_HARDWARE_EXCEPTION: u32 = 3;
    /// Bit 11: deliver an error code (VM_ENTRY_EXCEPTION_ERROR_CODE).
    const DELIVER_ERROR_CODE: u32 = 1 << 11;
    /// Bit 12, in VM_EXIT_INTERRUPTION_INFO only: the exception was a fault
    /// in an IRET that ended NMI blocking.
    const NMI_UNBLOCKING_IRET: u32 = 1 << 12;
    /// IDT vector of #DF, for which bit 12 is undefined.
    const DF_VECTOR: u8 = 8;

    /// Whether the field describes an event at all (bit 31).
    pub fn is_valid(self) -> bool {
        self.0 & Self::VALID != 0
    }

    /// Whether this describes an NMI: valid, type 2, vector 2. Bit 12 (NMI
    /// unblocking due to IRET) is ignored.
    pub fn is_nmi(self) -> bool {
        self.is_valid() && (self.0 >> 8) & 0b111 == Self::TYPE_NMI && self.vector() == Some(NMI_VECTOR)
    }

    /// Whether this field, read as IDT_VECTORING_INFO, reports an NMI whose
    /// delivery the exit interrupted: valid, vector 2, interruption type 2
    /// (NMI) or 3 (hardware exception). Vector 2 is the NMI vector and has no
    /// hardware exception, but a hypervisor underneath (Hyper-V) reports the
    /// interrupted NMI with type 3, so both types count. Re-delivery re-arms
    /// it as a true NMI (an NMI (`InjectedEvent::NMI`), type 2), not a vector-2
    /// hardware exception; `ExitInfo::redelivery` does that.
    pub fn is_nmi_vectoring(self) -> bool {
        self.is_valid()
            && self.vector() == Some(NMI_VECTOR)
            && matches!(self.event_type(), Self::TYPE_NMI | Self::TYPE_HARDWARE_EXCEPTION)
    }

    /// Whether this field, read as VM_EXIT_INTERRUPTION_INFO, reports a
    /// hardware exception raised by an IRET that had ended NMI blocking (bit
    /// 12). The SDM leaves bit 12 undefined for a double fault, so #DF never
    /// counts.
    pub fn is_nmi_unblocking_iret_fault(self) -> bool {
        self.is_valid()
            && self.event_type() == Self::TYPE_HARDWARE_EXCEPTION
            && self.vector() != Some(Self::DF_VECTOR)
            && self.0 & Self::NMI_UNBLOCKING_IRET != 0
    }

    /// Whether VM entry also delivers VM_ENTRY_EXCEPTION_ERROR_CODE (bit 11).
    pub fn delivers_error_code(self) -> bool {
        self.0 & Self::DELIVER_ERROR_CODE != 0
    }

    /// The vector (bits 7:0), if the field is valid.
    pub fn vector(self) -> Option<u8> {
        self.is_valid().then_some(self.0 as u8)
    }

    /// The interruption type (bits 10:8).
    const fn event_type(self) -> u32 {
        (self.0 >> 8) & 0b111
    }

    /// This field re-armed for VM entry from IDT_VECTORING_INFO: the valid
    /// bit and the event kept, bits 30:12 cleared. Bit 12 (NMI unblocking
    /// due to IRET) is an exit-only bit, undefined at entry, and bits 30:13
    /// are reserved there.
    pub fn for_reentry(self) -> Self {
        Self(self.0 & !0x7FFF_F000)
    }
}

/// The CPL from the guest's SS access rights (GUEST_SS_ACCESS_RIGHTS): its
/// DPL, bits 6:5.
pub fn cpl_from_ss(access_rights: u64) -> u8 {
    (access_rights >> 5) as u8 & 0b11
}

/// The access type of a control-register exit (exit qualification bits 5:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrAccess {
    MovToCr,
    MovFromCr,
    Clts,
    Lmsw,
}

/// A control-register access's exit qualification (Intel SDM Vol. 3C, "Exit
/// Qualification for Control-Register Accesses").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrQualification(pub u64);

impl CrQualification {
    /// Bits 3:0: the control-register number.
    pub fn cr(self) -> u8 {
        (self.0 & 0xF) as u8
    }

    /// Bits 5:4: the access type.
    pub fn access(self) -> CrAccess {
        match (self.0 >> 4) & 0b11 {
            0 => CrAccess::MovToCr,
            1 => CrAccess::MovFromCr,
            2 => CrAccess::Clts,
            // bits 5:4 are 2 bits, so the last arm is 3.
            _ => CrAccess::Lmsw,
        }
    }

    /// Bits 11:8: the general-purpose register of a MOV, in the instruction
    /// encoding's numbering (0 RAX, 1 RCX, 2 RDX, 3 RBX, 4 RSP, 5 RBP, 6 RSI,
    /// 7 RDI, 8-15 R8-R15), not `RegisterFrame`'s field order. RSP (4) is
    /// GUEST_RSP, not in the frame; the handler resolves the value.
    pub fn gpr(self) -> u8 {
        ((self.0 >> 8) & 0xF) as u8
    }
}

/// An I/O instruction's exit qualification (Intel SDM Vol. 3C, "Exit
/// Qualification for I/O Instructions"):
///
/// ```text
/// bits 2:0   size − 1: 0 = 1 byte, 1 = 2 bytes, 3 = 4 bytes
/// bit  3     direction: 1 = IN, 0 = OUT
/// bit  4     string instruction (INS/OUTS)
/// bit  5     REP prefix
/// bit  6     operand: 1 = immediate port, 0 = DX
/// bits 31:16 port
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoQualification(pub u64);

impl IoQualification {
    /// The access size, or `None` for the encodings the SDM leaves unused.
    pub fn size(self) -> Option<IoSize> {
        match self.0 & 0b111 {
            0 => Some(IoSize::Byte),
            1 => Some(IoSize::Word),
            3 => Some(IoSize::Dword),
            _ => None,
        }
    }

    /// Whether it is IN (bit 3); OUT otherwise.
    pub fn is_in(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Whether it is INS or OUTS (bit 4).
    pub fn is_string(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Whether it has a REP prefix (bit 5).
    pub fn is_rep(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    /// The port (bits 31:16).
    pub fn port(self) -> u16 {
        (self.0 >> 16) as u16
    }
}

/// Guest interruptibility bit 0: blocking by STI. The Intel SDM (Vol. 3C,
/// checks on guest non-register state) says: "A processor may require bit 0
/// (blocking by STI) to be 0 if the valid bit is 1 and the interruption type
/// is NMI." So an NMI is injected only when this bit is clear too.
pub const BLOCKING_BY_STI: u32 = 1 << 0;
/// Guest interruptibility bit 1: blocking by MOV SS. VM entry refuses to
/// inject an NMI while it is set.
pub const BLOCKING_BY_MOV_SS: u32 = 1 << 1;
/// Guest interruptibility bit 3: blocking by NMI. With virtual NMIs this is
/// an injected NMI the guest has not ended with IRET yet; VM entry refuses
/// to inject another while it is set.
pub const BLOCKING_BY_NMI: u32 = 1 << 3;
