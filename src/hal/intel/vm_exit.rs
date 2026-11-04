//! `hal::VmExit` for VT-x: the VMCS fields of the current exit, each read
//! once with VMREAD and kept, decoded into the neutral types with the
//! original decoders (`vmexit::decode`); the VMWRITEs before VMRESUME; the
//! report lines that print VMCS fields. Also the entry points the exit stub
//! calls (`vmexit_handler`, `vmresume_failed`), which run the shared exit
//! handler (`hv::vmexit`).

use core::cell::Cell;
use core::fmt;

use super::vmcs::fields::{
    CR0_READ_SHADOW, CR4_READ_SHADOW, EXIT_QUALIFICATION, GUEST_CR0, GUEST_CR4, GUEST_INTERRUPTIBILITY_STATE,
    GUEST_LINEAR_ADDRESS, GUEST_PHYSICAL_ADDRESS, GUEST_RIP, GUEST_RSP, GUEST_SS_ACCESS_RIGHTS,
    IDT_VECTORING_ERROR_CODE, IDT_VECTORING_INFO, VM_ENTRY_EXCEPTION_ERROR_CODE, VM_ENTRY_INSTRUCTION_LEN,
    VM_ENTRY_INTERRUPTION_INFO, VM_EXIT_INSTRUCTION_LEN, VM_EXIT_INTERRUPTION_INFO, VM_EXIT_REASON,
};
use super::vmcs::{self, GuestKind, VmcsError, encode_finished, encode_resume_failed, encode_unexpected};
use super::vmexit::decode::{
    BLOCKING_BY_MOV_SS, BLOCKING_BY_NMI, BLOCKING_BY_STI, CrAccess as CrAccessType, CrQualification,
    EPT_NMI_UNBLOCKING_IRET, EXIT_REASON_CONTROL_REGISTER, EXIT_REASON_CPUID, EXIT_REASON_EPT_MISCONFIG,
    EXIT_REASON_EPT_VIOLATION, EXIT_REASON_EXCEPTION_OR_NMI, EXIT_REASON_HLT, EXIT_REASON_INTERRUPT_WINDOW,
    EXIT_REASON_IO_INSTRUCTION, EXIT_REASON_MONITOR, EXIT_REASON_MWAIT, EXIT_REASON_NMI_WINDOW, EXIT_REASON_RDMSR,
    EXIT_REASON_VMCALL, EXIT_REASON_WRMSR, EXIT_REASON_XSETBV, EptQualification, ExitReason as VmxExitReason,
    InterruptionInfo, IoQualification, UNREADABLE_REASON, cpl_from_ss, exit_reason_name,
};
use super::vmexit::{RegisterFrame, dump, read_field};
use crate::hal::types::{
    CrAccess, CrAccessKind, CrNumber, CrWrite, EventKind, ExitReason, GuestRegisters, InjectedEvent, Interruptibility,
    IoAccess, NestedPageFault, ReasonDescription,
};
use crate::hv::exit_decision::ExitAction;
use crate::output::Phase;

/// The VMCS fields an exit is read from, in [`ENCODINGS`] order.
#[derive(Debug, Clone, Copy)]
enum Field {
    Reason,
    InterruptionInfo,
    Qualification,
    GuestPhysical,
    SsAccessRights,
    Rip,
    Length,
    Interruptibility,
    IdtVectoring,
    IdtVectoringErrorCode,
    GuestRsp,
    GuestCr4,
}

const FIELDS: usize = 12;

const ENCODINGS: [u32; FIELDS] = [
    VM_EXIT_REASON,
    VM_EXIT_INTERRUPTION_INFO,
    EXIT_QUALIFICATION,
    GUEST_PHYSICAL_ADDRESS,
    GUEST_SS_ACCESS_RIGHTS,
    GUEST_RIP,
    VM_EXIT_INSTRUCTION_LEN,
    GUEST_INTERRUPTIBILITY_STATE,
    IDT_VECTORING_INFO,
    IDT_VECTORING_ERROR_CODE,
    GUEST_RSP,
    GUEST_CR4,
];

/// The guest stopped at a VM exit, with each VMCS field kept once read: the
/// report printed after the VMWRITEs shows the values the exit had (RIP
/// before it moved past the instruction), as the handler's `ExitRead` did.
pub struct VmExit {
    values: [Cell<Option<Result<u64, VmcsError>>>; FIELDS],
}

impl VmExit {
    /// `field`, read now or when first asked for.
    fn read(&self, field: Field) -> Result<u64, VmcsError> {
        let slot = &self.values[field as usize];
        match slot.get() {
            Some(value) => value,
            None => {
                let value = read_field(ENCODINGS[field as usize]);
                slot.set(Some(value));
                value
            }
        }
    }

    /// `field` if it was read, without reading it.
    fn read_before(&self, field: Field) -> Option<Result<u64, VmcsError>> {
        self.values[field as usize].get()
    }

    /// VMWRITEs `value` to `field`.
    fn write(field: u32, value: u64) -> Result<(), VmcsError> {
        // SAFETY: exit context: VMX root operation, with the guest's VMCS
        // current.
        unsafe { vmcs::vmwrite(field, value) }
    }
}

/// The neutral reason of a VM_EXIT_REASON value.
pub(crate) fn decode_reason(raw: u32) -> ExitReason {
    let reason = VmxExitReason::from_raw(raw);
    if reason.entry_failure {
        return ExitReason::EntryFailure(raw);
    }
    match reason.basic {
        EXIT_REASON_EXCEPTION_OR_NMI => ExitReason::ExceptionOrNmi,
        EXIT_REASON_INTERRUPT_WINDOW => ExitReason::InterruptWindow,
        EXIT_REASON_NMI_WINDOW => ExitReason::NmiWindow,
        EXIT_REASON_CPUID => ExitReason::Cpuid,
        EXIT_REASON_HLT => ExitReason::Hlt,
        EXIT_REASON_VMCALL => ExitReason::Hypercall,
        EXIT_REASON_CONTROL_REGISTER => ExitReason::ControlRegisterAccess,
        EXIT_REASON_IO_INSTRUCTION => ExitReason::IoInstruction,
        EXIT_REASON_RDMSR => ExitReason::Rdmsr,
        EXIT_REASON_WRMSR => ExitReason::Wrmsr,
        EXIT_REASON_MWAIT => ExitReason::Mwait,
        EXIT_REASON_MONITOR => ExitReason::Monitor,
        EXIT_REASON_EPT_VIOLATION => ExitReason::NestedPageFault,
        EXIT_REASON_EPT_MISCONFIG => ExitReason::NestedPagingMisconfig,
        EXIT_REASON_XSETBV => ExitReason::Xsetbv,
        _ => ExitReason::Other(raw),
    }
}

/// The interruption type (bits 10:8) of each event kind.
fn event_type(kind: EventKind) -> u32 {
    match kind {
        EventKind::ExternalInterrupt => 0,
        EventKind::Nmi => 2,
        EventKind::HardwareException => 3,
        EventKind::SoftwareInterrupt => 4,
        EventKind::PrivilegedSoftwareException => 5,
        EventKind::SoftwareException => 6,
        EventKind::Other(kind) => u32::from(kind),
    }
}

/// The event an interruption-information value describes, `None` when not
/// valid. `error_code` is the error code it delivers, if bit 11 says it
/// delivers one.
fn event(info: InterruptionInfo, error_code: impl FnOnce() -> u32) -> Option<InjectedEvent> {
    let vector = info.vector()?;
    let kind = match (info.0 >> 8) & 0b111 {
        0 => EventKind::ExternalInterrupt,
        2 => EventKind::Nmi,
        3 => EventKind::HardwareException,
        4 => EventKind::SoftwareInterrupt,
        5 => EventKind::PrivilegedSoftwareException,
        6 => EventKind::SoftwareException,
        other => EventKind::Other(other as u8),
    };
    Some(InjectedEvent {
        kind,
        vector,
        error_code: info.delivers_error_code().then(error_code),
        instruction_len: None,
    })
}

/// The interruption-information value of `event`, for VM entry: valid, its
/// type and vector, and bit 11 when it delivers an error code.
fn interruption_info(event: InjectedEvent) -> InterruptionInfo {
    let deliver_error_code = if event.error_code.is_some() { 1 << 11 } else { 0 };
    InterruptionInfo(
        InterruptionInfo::VALID | (event_type(event.kind) << 8) | deliver_error_code | u32::from(event.vector),
    )
}

/// The neutral state of a GUEST_INTERRUPTIBILITY_STATE value.
pub(crate) fn decode_interruptibility(raw: u32) -> Interruptibility {
    Interruptibility {
        sti_shadow: raw & BLOCKING_BY_STI != 0,
        mov_ss_shadow: raw & BLOCKING_BY_MOV_SS != 0,
        nmi_blocked: raw & BLOCKING_BY_NMI != 0,
        other: raw & !(BLOCKING_BY_STI | BLOCKING_BY_MOV_SS | BLOCKING_BY_NMI),
    }
}

/// The GUEST_INTERRUPTIBILITY_STATE value of `state`.
fn interruptibility_value(state: Interruptibility) -> u32 {
    let bit = |set: bool, bit: u32| if set { bit } else { 0 };
    state.other
        | bit(state.sti_shadow, BLOCKING_BY_STI)
        | bit(state.mov_ss_shadow, BLOCKING_BY_MOV_SS)
        | bit(state.nmi_blocked, BLOCKING_BY_NMI)
}

/// What the history keeps of an exit: the fields its line shows, each `None`
/// when it could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub(crate) reason: Option<u32>,
    pub(crate) qualification: Option<u64>,
    pub(crate) rip: Option<u64>,
    pub(crate) length: Option<u64>,
    pub(crate) interruption_info: Option<u32>,
    pub(crate) idt_vectoring_info: Option<u32>,
}

/// A field of a record, or `?` when it could not be read.
struct RecordField {
    value: Option<u64>,
    /// Hexadecimal; decimal otherwise, for a count.
    hex: bool,
}

impl fmt::Display for RecordField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.value {
            Some(value) if self.hex => write!(f, "{value:#x}"),
            Some(value) => write!(f, "{value}"),
            None => f.write_str("?"),
        }
    }
}

impl fmt::Display for Record {
    /// `reason=32 (WRMSR) rip=0x7eae1028 len=2 qual=0x0 info=0x0
    /// vectoring=0x0`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = |value| RecordField { value, hex: true };
        match self.reason.map(VmxExitReason::from_raw) {
            Some(reason) => write!(
                f,
                "reason={} ({}){}",
                reason.basic,
                exit_reason_name(reason.basic),
                if reason.entry_failure { " [entry failed]" } else { "" },
            )?,
            None => f.write_str("reason=?")?,
        }
        write!(
            f,
            " rip={} len={} qual={} info={} vectoring={}",
            hex(self.rip),
            RecordField {
                value: self.length,
                hex: false,
            },
            hex(self.qualification),
            hex(self.interruption_info.map(u64::from)),
            hex(self.idt_vectoring_info.map(u64::from)),
        )
    }
}

/// Prints that the VMCS field `name` could not be read at exit `number`, if
/// `value` says so; prints nothing for a value that was read.
fn report_unreadable(number: u64, name: &str, value: Result<u64, VmcsError>) {
    if let Err(error) = value {
        crate::serial_println!("{} VM exit #{number}: {name} unreadable ({error:?})", crate::TAG);
    }
}

/// Prints the vector of an exception exit, from its VM_EXIT_INTERRUPTION_INFO
/// `info`. Exit qualification, printed by the caller, holds the faulting
/// address for a page fault (#PF, vector 14). NMI exits share reason 0 but
/// are not exceptions; the event line printed later reports them.
fn report_exception(number: u64, info: InterruptionInfo) {
    let tag = crate::TAG;
    if info.is_nmi() {
        return;
    }
    let raw = info.0;
    match info.vector() {
        Some(vector) => crate::serial_println!(
            "{tag} VM exit #{number}: guest exception vector={vector} (interruption info={raw:#010x})"
        ),
        None => crate::serial_println!("{tag} VM exit #{number}: interruption info not valid ({raw:#010x})"),
    }
}

/// Prints an EPT exit's addresses and what the EPT entries allowed.
fn report_ept_exit(number: u64, basic: u16, qualification: EptQualification, guest_physical: u64) {
    let tag = crate::TAG;
    if basic == EXIT_REASON_EPT_MISCONFIG {
        crate::serial_println!(
            "{tag} VM exit #{number}: EPT misconfiguration at GPA {guest_physical:#018x}: an EPT entry for it is invalid."
        );
        return;
    }

    let (r, w, x) = qualification.permissions();
    let access = qualification.access_name();
    // The linear address is only valid with qualification bit 7, and only
    // the report needs it, so it is read here.
    match qualification.linear_valid().then(|| read_field(GUEST_LINEAR_ADDRESS)) {
        Some(Ok(linear)) => crate::serial_println!(
            "{tag} VM exit #{number}: EPT violation at GPA {guest_physical:#018x} (linear {linear:#018x}): {access}; page R={r} W={w} X={x}."
        ),
        Some(Err(error)) => crate::serial_println!(
            "{tag} VM exit #{number}: EPT violation at GPA {guest_physical:#018x} (linear address unreadable: {error:?}): {access}; page R={r} W={w} X={x}."
        ),
        None => crate::serial_println!(
            "{tag} VM exit #{number}: EPT violation at GPA {guest_physical:#018x} (no linear address): {access}; page R={r} W={w} X={x}."
        ),
    }
}

/// The exception or NMI of a VM_EXIT_INTERRUPTION_INFO value, `None` when
/// not valid. Its error code is not read: Aleph0 never needs it.
pub(crate) fn decode_exception_or_nmi(raw: u32) -> Option<InjectedEvent> {
    event(InterruptionInfo(raw), || 0).map(|event| InjectedEvent {
        error_code: None,
        ..event
    })
}

/// The event an IDT_VECTORING_INFO value says the exit interrupted, with
/// `error_code` (IDT_VECTORING_ERROR_CODE) when it delivers one, re-armed
/// for VM entry: the valid bit and the event kept, bits 30:12 cleared.
/// Vector 2 is the NMI vector: it re-delivers as an NMI (type 2) whatever
/// type the vectoring field reports. Hyper-V reports the interrupted NMI as
/// a hardware exception (type 3), and re-arming that verbatim would inject a
/// #2 hardware exception, not an NMI.
pub(crate) fn decode_interrupted(raw: u32, error_code: impl FnOnce() -> u32) -> Option<InjectedEvent> {
    let info = InterruptionInfo(raw);
    if info.is_nmi_vectoring() {
        return Some(InjectedEvent::NMI);
    }
    event(info.for_reentry(), error_code)
}

/// The access of a control-register exit's qualification.
pub(crate) fn decode_cr_access(qualification: u64) -> CrAccess {
    let qualification = CrQualification(qualification);
    CrAccess {
        cr: qualification.cr(),
        kind: match qualification.access() {
            CrAccessType::MovToCr => CrAccessKind::MovTo,
            CrAccessType::MovFromCr => CrAccessKind::MovFrom,
            CrAccessType::Clts => CrAccessKind::Clts,
            CrAccessType::Lmsw => CrAccessKind::Lmsw,
        },
        gpr: qualification.gpr(),
    }
}

/// The access of an I/O exit's qualification.
pub(crate) fn decode_io_access(qualification: u64) -> IoAccess {
    let io = IoQualification(qualification);
    IoAccess {
        port: io.port(),
        size: io.size(),
        is_in: io.is_in(),
        is_string: io.is_string(),
        is_rep: io.is_rep(),
    }
}

/// The fault of an EPT violation's qualification at GUEST_PHYSICAL_ADDRESS
/// `gpa`.
pub(crate) fn decode_fault(qualification: u64, gpa: u64) -> NestedPageFault {
    use super::vmexit::decode::{
        EPT_ACCESS_FETCH, EPT_ACCESS_READ, EPT_ACCESS_WRITE, EPT_LINEAR_VALID, EPT_PAGE_EXECUTABLE, EPT_PAGE_READABLE,
        EPT_PAGE_WRITABLE, EPT_TRANSLATED_ACCESS,
    };
    let bit = |mask: u64| qualification & mask != 0;
    NestedPageFault {
        gpa,
        read: bit(EPT_ACCESS_READ),
        write: bit(EPT_ACCESS_WRITE),
        fetch: bit(EPT_ACCESS_FETCH),
        page_readable: bit(EPT_PAGE_READABLE),
        page_writable: bit(EPT_PAGE_WRITABLE),
        page_executable: bit(EPT_PAGE_EXECUTABLE),
        linear_valid: bit(EPT_LINEAR_VALID),
        translated_access: bit(EPT_TRANSLATED_ACCESS),
    }
}

/// Whether an exit with VM_EXIT_REASON `reason` and IDT_VECTORING_INFO
/// `vectoring` came from a fault in an IRET that had ended NMI blocking: bit
/// 12 of an EPT violation's qualification, or of an exception exit's
/// VM_EXIT_INTERRUPTION_INFO (Intel SDM Vol. 3C, "Exit Qualification for EPT
/// Violations" and "Information for VM Exits Due to Vectored Events"). Bit
/// 12 is undefined when the exit interrupted an event's delivery, so such an
/// exit never counts. `qualification` and `info` are read only for the
/// reason that uses them.
pub(crate) fn decode_faulted_nmi_unblocking_iret<E>(
    reason: u32,
    vectoring: u32,
    qualification: impl FnOnce() -> Result<u64, E>,
    info: impl FnOnce() -> Result<u32, E>,
) -> Result<bool, E> {
    let reason = VmxExitReason::from_raw(reason);
    if reason.entry_failure || InterruptionInfo(vectoring).is_valid() {
        return Ok(false);
    }
    Ok(match reason.basic {
        EXIT_REASON_EPT_VIOLATION => qualification()? & EPT_NMI_UNBLOCKING_IRET != 0,
        EXIT_REASON_EXCEPTION_OR_NMI => InterruptionInfo(info()?).is_nmi_unblocking_iret_fault(),
        _ => false,
    })
}

impl crate::hal::VmExit for VmExit {
    type Error = VmcsError;
    type Record = Record;

    const HYPERCALL: &'static str = "VMCALL";
    const EMPTY_RECORD: Record = Record {
        reason: None,
        qualification: None,
        rip: None,
        length: None,
        interruption_info: None,
        idt_vectoring_info: None,
    };
    /// Basic exit reasons 0 to 127, well past the highest the SDM defines.
    const REASON_COUNTERS: usize = 128;

    unsafe fn current() -> Self {
        Self {
            values: [const { Cell::new(None) }; FIELDS],
        }
    }

    fn reason(&self) -> Result<ExitReason, VmcsError> {
        self.reason_code().map(decode_reason)
    }

    fn reason_code(&self) -> Result<u32, VmcsError> {
        self.read(Field::Reason).map(|raw| raw as u32)
    }

    fn exception_or_nmi(&self) -> Result<Option<InjectedEvent>, VmcsError> {
        Ok(decode_exception_or_nmi(self.read(Field::InterruptionInfo)? as u32))
    }

    /// IDT_VECTORING_INFO, with IDT_VECTORING_ERROR_CODE when it delivers
    /// one. Vector 2 is the NMI vector: it re-delivers as an NMI (type 2)
    /// whatever type the vectoring field reports. Hyper-V reports the
    /// interrupted NMI as a hardware exception (type 3), and re-arming that
    /// verbatim would inject a #2 hardware exception, not an NMI.
    fn interrupted_event(&self) -> Result<Option<InjectedEvent>, VmcsError> {
        let vectoring = self.read(Field::IdtVectoring)? as u32;
        // Meaningful only when the field carries an error code; 0 when it is
        // unreadable, as before.
        Ok(decode_interrupted(vectoring, || {
            self.read(Field::IdtVectoringErrorCode).unwrap_or(0) as u32
        }))
    }

    fn interruptibility(&self) -> Result<Interruptibility, VmcsError> {
        self.read(Field::Interruptibility)
            .map(|raw| decode_interruptibility(raw as u32))
    }

    fn cr_access(&self) -> Result<CrAccess, VmcsError> {
        Ok(decode_cr_access(self.read(Field::Qualification)?))
    }

    fn io_access(&self) -> Result<IoAccess, VmcsError> {
        Ok(decode_io_access(self.read(Field::Qualification)?))
    }

    fn nested_page_fault(&self) -> Result<NestedPageFault, VmcsError> {
        let qualification = self.read(Field::Qualification)?;
        Ok(decode_fault(qualification, self.read(Field::GuestPhysical)?))
    }

    /// Bit 12 of an EPT violation's qualification, or of an exception exit's
    /// VM_EXIT_INTERRUPTION_INFO (Intel SDM Vol. 3C, "Exit Qualification for
    /// EPT Violations" and "Information for VM Exits Due to Vectored
    /// Events"). Bit 12 is undefined when the exit interrupted an event's
    /// delivery, so such an exit never counts.
    fn faulted_nmi_unblocking_iret(&self) -> Result<bool, VmcsError> {
        decode_faulted_nmi_unblocking_iret(
            self.read(Field::Reason)? as u32,
            self.read(Field::IdtVectoring)? as u32,
            || self.read(Field::Qualification),
            || self.read(Field::InterruptionInfo).map(|info| info as u32),
        )
    }

    /// SS.DPL from GUEST_SS_ACCESS_RIGHTS (Intel SDM Vol. 3C, "Checks on
    /// Guest Segment Registers": SS.DPL is the CPL).
    fn guest_cpl(&self) -> Result<u8, VmcsError> {
        self.read(Field::SsAccessRights).map(cpl_from_ss)
    }

    fn guest_rip(&self) -> Result<u64, VmcsError> {
        self.read(Field::Rip)
    }

    fn instruction_length(&self) -> Result<u64, VmcsError> {
        self.read(Field::Length)
    }

    fn guest_rsp(&self) -> Result<u64, VmcsError> {
        self.read(Field::GuestRsp)
    }

    fn guest_cr4(&self) -> Result<u64, VmcsError> {
        self.read(Field::GuestCr4)
    }

    fn record(&self) -> Record {
        Record {
            reason: self.read(Field::Reason).ok().map(|raw| raw as u32),
            qualification: self.read(Field::Qualification).ok(),
            rip: self.read(Field::Rip).ok(),
            length: self.read(Field::Length).ok(),
            interruption_info: self.read(Field::InterruptionInfo).ok().map(|info| info as u32),
            idt_vectoring_info: self.read(Field::IdtVectoring).ok().map(|info| info as u32),
        }
    }

    /// Every field the handler read, and the qualification, which the
    /// report prints for every exit.
    fn unreadable(&self) -> bool {
        [
            Field::Reason,
            Field::InterruptionInfo,
            Field::Qualification,
            Field::Rip,
            Field::Length,
            Field::Interruptibility,
            Field::IdtVectoring,
        ]
        .into_iter()
        .map(|field| self.read(field))
        .chain(
            [Field::GuestPhysical, Field::SsAccessRights]
                .into_iter()
                .filter_map(|field| self.read_before(field)),
        )
        .any(|value| value.is_err())
    }

    fn set_rip(&mut self, rip: u64) -> Result<(), VmcsError> {
        Self::write(GUEST_RIP, rip)
    }

    fn set_interruptibility(&mut self, state: Interruptibility) -> Result<(), VmcsError> {
        Self::write(GUEST_INTERRUPTIBILITY_STATE, interruptibility_value(state).into())
    }

    /// The event's error code when it carries one, its instruction length
    /// for a software interrupt or exception, then the interruption
    /// information, written last, as for an injection.
    fn inject_event(&mut self, event: InjectedEvent) -> Result<(), VmcsError> {
        if let Some(error_code) = event.error_code {
            Self::write(VM_ENTRY_EXCEPTION_ERROR_CODE, error_code.into())?;
        }
        if let Some(length) = event.instruction_len {
            Self::write(VM_ENTRY_INSTRUCTION_LEN, length.into())?;
        }
        Self::write(VM_ENTRY_INTERRUPTION_INFO, interruption_info(event).0.into())
    }

    fn set_nmi_window(&mut self, on: bool) -> Result<(), VmcsError> {
        // SAFETY: exit context: VMX root operation, with the guest's VMCS
        // current. `configure_controls` checked that the CPU allows
        // NMI-window exiting.
        unsafe { vmcs::set_nmi_window_exiting(on) }
    }

    fn set_interrupt_window(&mut self, on: bool) -> Result<(), VmcsError> {
        // SAFETY: exit context: VMX root operation, with the guest's VMCS
        // current. `configure_controls` checked that the CPU allows
        // interrupt-window exiting.
        unsafe { vmcs::set_interrupt_window_exiting(on) }
    }

    /// The guest reads `requested` back (the new read shadow), while
    /// GUEST_CRn keeps the VMX fixed bits (`real`).
    fn write_cr(&mut self, write: CrWrite) -> Result<(), (&'static str, VmcsError)> {
        let (guest_field, shadow_field) = match write.cr {
            CrNumber::Cr0 => (GUEST_CR0, CR0_READ_SHADOW),
            CrNumber::Cr4 => (GUEST_CR4, CR4_READ_SHADOW),
        };
        Self::write(shadow_field, write.requested).map_err(|error| ("set the control-register read shadow", error))?;
        Self::write(guest_field, write.real).map_err(|error| ("write the guest control register", error))
    }

    /// The reason, the qualification, an EPT exit's addresses, RIP and
    /// instruction length, RAX to RDX, and every field that could not be
    /// read.
    fn report_read(&self, number: u64, registers: [u64; 4]) {
        let tag = crate::TAG;
        let reason_field = self.read(Field::Reason);
        let reason = reason_field.ok().map(|raw| VmxExitReason::from_raw(raw as u32));
        match reason {
            Some(reason) => {
                crate::serial_println!(
                    "{tag} VM exit #{number}: reason={} ({}){}",
                    reason.basic,
                    exit_reason_name(reason.basic),
                    if reason.entry_failure {
                        " [VM entry failed; guest did not run]"
                    } else {
                        ""
                    },
                );
                if reason.basic == EXIT_REASON_EXCEPTION_OR_NMI
                    && !reason.entry_failure
                    && let Ok(info) = self.read(Field::InterruptionInfo)
                {
                    report_exception(number, InterruptionInfo(info as u32));
                }
            }
            None => report_unreadable(number, "VM_EXIT_REASON", reason_field),
        }
        let qualification = self.read(Field::Qualification);
        match qualification {
            Ok(qualification) => {
                crate::serial_println!("{tag} VM exit #{number}: qualification={qualification:#018x}")
            }
            Err(_) => report_unreadable(number, "EXIT_QUALIFICATION", qualification),
        }
        if let (Some(reason), Some(guest_physical)) = (reason, self.read_before(Field::GuestPhysical)) {
            match guest_physical {
                Ok(guest_physical) => report_ept_exit(
                    number,
                    reason.basic,
                    EptQualification(qualification.unwrap_or(0)),
                    guest_physical,
                ),
                Err(_) => report_unreadable(number, "GUEST_PHYSICAL_ADDRESS", guest_physical),
            }
        }
        match (self.read(Field::Rip), self.read(Field::Length)) {
            (Ok(rip), Ok(length)) => {
                crate::serial_println!("{tag} VM exit #{number}: guest rip={rip:#018x} instruction length={length}")
            }
            (rip, length) => {
                report_unreadable(number, "GUEST_RIP", rip);
                report_unreadable(number, "VM_EXIT_INSTRUCTION_LEN", length);
            }
        }
        let [rax, rbx, rcx, rdx] = registers;
        crate::serial_println!(
            "{tag} VM exit #{number}: guest rax={rax:#018x} rbx={rbx:#018x} rcx={rcx:#018x} rdx={rdx:#018x}"
        );
        report_unreadable(number, "VM_EXIT_INTERRUPTION_INFO", self.read(Field::InterruptionInfo));
        report_unreadable(
            number,
            "GUEST_INTERRUPTIBILITY_STATE",
            self.read(Field::Interruptibility),
        );
        let vectoring = self.read(Field::IdtVectoring);
        match vectoring.map(|info| InterruptionInfo(info as u32)) {
            Ok(vectoring) if vectoring.is_valid() => crate::serial_println!(
                "{tag} VM exit #{number}: during event delivery: IDT-vectoring info={:#010x} error code={:#x}",
                vectoring.0,
                self.read(Field::IdtVectoringErrorCode).unwrap_or(0) as u32,
            ),
            Ok(_) => {}
            Err(_) => report_unreadable(number, "IDT_VECTORING_INFO", vectoring),
        }
        if let Some(rights) = self.read_before(Field::SsAccessRights) {
            report_unreadable(number, "GUEST_SS_ACCESS_RIGHTS", rights);
        }
    }

    fn dump(&self, number: u64, registers: &GuestRegisters) {
        dump::report_guest(number, registers);
    }

    fn describe_reason(code: u32) -> ReasonDescription {
        let reason = VmxExitReason::from_raw(code);
        ReasonDescription {
            number: reason.basic,
            name: exit_reason_name(reason.basic),
            entry_failure: reason.entry_failure,
        }
    }

    fn encode_event(event: InjectedEvent) -> u32 {
        interruption_info(event).0
    }

    fn encode_interruptibility(state: Interruptibility) -> u32 {
        interruptibility_value(state)
    }
}

/// Handler return value that tells the entry stub to resume the guest.
/// Every other value is a stop value (see `vmcs/launch.rs`).
const RESUME_GUEST: u64 = 0;

/// What the handler returns to the entry stub for `action` at exit
/// `number`: [`RESUME_GUEST`], or the stop value `vmcs::launch` returns.
fn stub_return(action: ExitAction, number: u64, raw_reason: u32) -> u64 {
    match action {
        ExitAction::Resume(_) => RESUME_GUEST,
        ExitAction::Finished => encode_finished(number),
        ExitAction::Unexpected => encode_unexpected(number, raw_reason),
    }
}

/// VM-exit handler the entry stub calls: runs the shared exit handler
/// (`hv::vmexit::handle_exit`) on this exit, then tells the stub what to do.
/// It returns [`RESUME_GUEST`] after the VMCS is prepared for the guest to
/// go on (the stub then executes VMRESUME), or, for the test guest, a stop
/// value (the stub then returns it from `vmcs::launch` to bring-up).
#[unsafe(no_mangle)]
extern "efiapi" fn vmexit_handler(frame: *mut RegisterFrame) -> u64 {
    // SAFETY: the entry stub passes a pointer to the frame it just pushed on
    // the exit stack. It stays put until this returns and nothing else
    // touches it meanwhile; the stub pops it into the guest's registers
    // afterwards, so writes here reach the guest.
    let frame = unsafe { &mut *frame };
    // SAFETY: the stub runs this at a VM exit: VMX root operation, with the
    // guest's VMCS current.
    let mut exit = unsafe { <VmExit as crate::hal::VmExit>::current() };
    let (action, number) = crate::hv::vmexit::handle_exit(&mut exit, frame);
    let raw_reason = exit
        .read(Field::Reason)
        .map_or(u32::from(UNREADABLE_REASON), |raw| raw as u32);
    stub_return(action, number, raw_reason)
}

/// Called by the entry stub when VMRESUME falls through instead of entering
/// the guest; `rflags` is RFLAGS right after it, and `frame` the guest's
/// registers, which the stub pushed again. Prints why, then the same stop
/// report as an exit that stops the guest, and returns the stop value the
/// stub hands back to bring-up, or halts for the firmware.
#[unsafe(no_mangle)]
extern "efiapi" fn vmresume_failed(rflags: u64, frame: *const RegisterFrame) -> u64 {
    // The handler put the guest's phase back before VMRESUME; this is exit
    // context again.
    crate::output::set_phase(Phase::VmExit);
    let guest = GuestKind::SELECTED;
    let tag = crate::TAG;
    match vmcs::resume_failure(rflags) {
        // SAFETY: a VmFailValid implies VMX operation with a current VMCS,
        // so the error number is readable.
        VmcsError::VmFailValid => match unsafe { vmcs::vm_instruction_error() } {
            Ok(code) => crate::serial_println!(
                "{tag} VMRESUME failed: {} ({code})",
                vmcs::vm_instruction_error_name(code),
            ),
            Err(error) => crate::serial_println!("{tag} VMRESUME failed: VmFailValid, error unreadable ({error:?})"),
        },
        error => crate::serial_println!("{tag} VMRESUME failed: {error:?}"),
    }
    crate::serial_println!(
        "{tag} VMRESUME failed; stopping the guest; {}.",
        crate::hv::vmexit::after_stop(guest)
    );
    // SAFETY: the stub passes the frame it just pushed on the exit stack,
    // which stays put until this returns.
    let frame = unsafe { &*frame };
    crate::hv::vmexit::stop_after_failed_resume(frame);
    encode_resume_failed(rflags)
}

#[cfg(test)]
#[path = "../../../tests/unit/hal/intel/vm_exit.rs"]
mod tests;
