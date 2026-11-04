//! Why bring-up stopped, and how that gets printed.

use uefi::Status;
use uefi::boot::MemoryType;

use crate::hal::{HardwareVirtualization, Intercepts, NestedPaging, Vcpu, VmExit, backend};
use crate::host::HostError;
use crate::hv::guest::GuestMemoryError;
use crate::hv::vmexit::VcpuStats;
use crate::x86::apic::LocalApicError;
use crate::x86::context_tables::{GdtError, TssError};
use crate::x86::mtrr::MtrrError;

use super::teardown::TeardownError;

/// The backend's errors, one per trait.
pub type VirtualizationError = <backend::HardwareVirtualization as HardwareVirtualization>::Error;
pub type VcpuError = <backend::Vcpu as Vcpu>::Error;
pub type NestedPagingError = <backend::NestedPaging as NestedPaging>::Error;
pub type InterceptsError = <backend::Intercepts as Intercepts>::Error;

/// Why bring-up stopped. Each step returns one of these, so the sequence
/// reads top to bottom with `?` and [`report_error`] prints the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BringUpError {
    Tss(TssError),
    Gdt(GdtError),
    GuestMemory(GuestMemoryError),
    /// Allocating the MSR and I/O intercept maps failed.
    Intercepts(InterceptsError),
    /// The CPU has no virtualization extension.
    NotSupportedByCpu,
    /// The CPU has it, but firmware disabled it.
    DisabledByFirmware,
    /// An MTRR MSR holds a value the CPU would refuse, or the MTRRs hold
    /// something Aleph0 cannot copy into EPT. Before VMXON.
    Mtrr(MtrrError),
    /// The CPU lacks the nested paging Aleph0 needs, or building or
    /// changing the tables failed. Before root mode.
    NestedPaging(NestedPagingError),
    /// The test guest could not read the Local APIC's registers in xAPIC
    /// mode: its page tables or EPT do not map their page to itself, EPT
    /// with UC. Before VMXON.
    LocalApic(LocalApicError),
    /// Building the page tables, GDT, TSS or IDT that root mode runs on
    /// failed. Before root mode.
    Host(HostError),
    /// Entering root mode failed.
    Virtualization(VirtualizationError),
    /// A vCPU step failed: allocating it, creating or configuring it, its
    /// guest state, entering or resuming the guest. The backend's error
    /// keeps what its report needs, read when the error was made.
    Vcpu(VcpuError),
    /// GDTR, TR, the TSS base or IDTR of the context tables changed between
    /// activation and the launch. The guest state was copied from the
    /// activated tables, so the guest would start on stale ones.
    ContextTablesChanged,
    /// The UEFI memory map, which the resident allocations are checked
    /// against, could not be read. Before the launch.
    ResidentMemoryMapUnavailable,
    /// The resident allocation at `address` lies in memory the OS may reuse
    /// after `ExitBootServices`: its memory-map region has type
    /// `memory_type`, or no region holds it (`None`). Before the launch.
    NotResident {
        address: u64,
        memory_type: Option<MemoryType>,
    },
    /// `count` resident allocations came after `crate::x86::page`'s list was full,
    /// so they could not be checked. Before the launch.
    ResidentAllocationsUnlisted(usize),
    /// `crate::x86::page` listed no resident allocation, not even the image copy,
    /// so the check would have checked nothing. Before the launch.
    NoResidentAllocations,
    /// The `ExitBootServices` event could not be created. Before the launch.
    ExitBootServicesEvent(Status),
    /// The guest stopped at exit `number`, which the exit handler did not
    /// resume: it does not handle that exit, or it is an expected VMCALL the
    /// handler could not step past. Its lines on serial say which.
    UnexpectedExit {
        /// The backend's raw reason code (`VmExit::describe_reason`).
        number: u64,
        reason: u32,
    },
    /// The CPU's MTRRs read differently after the guest than before: a
    /// guest write reached them instead of the guest's copy.
    MtrrsChanged,
    /// The exit handler wrote the lowest word of the VM-exit stack, of this
    /// many bytes: it used all of it and may have run on into the memory
    /// below, which nothing guards.
    VmExitStackFull(usize),
    /// The guest finished, but its exit statistics differ from what the
    /// test guest does (`guest::expected_stats`): an emulated exit is
    /// missing or came twice.
    UnexpectedExitStats(VcpuStats),
    /// Leaving root mode failed. Returned only when nothing failed
    /// before teardown; otherwise that earlier error is returned and this
    /// one was printed when it happened.
    Teardown(TeardownError),
}

impl From<LocalApicError> for BringUpError {
    fn from(error: LocalApicError) -> Self {
        Self::LocalApic(error)
    }
}

impl From<TssError> for BringUpError {
    fn from(error: TssError) -> Self {
        Self::Tss(error)
    }
}

impl From<GdtError> for BringUpError {
    fn from(error: GdtError) -> Self {
        Self::Gdt(error)
    }
}

impl From<GuestMemoryError> for BringUpError {
    fn from(error: GuestMemoryError) -> Self {
        Self::GuestMemory(error)
    }
}

impl From<MtrrError> for BringUpError {
    fn from(error: MtrrError) -> Self {
        Self::Mtrr(error)
    }
}

impl From<HostError> for BringUpError {
    fn from(error: HostError) -> Self {
        Self::Host(error)
    }
}

impl From<TeardownError> for BringUpError {
    fn from(error: TeardownError) -> Self {
        Self::Teardown(error)
    }
}

/// The extension's name, for the log lines.
const VMX: &str = <backend::HardwareVirtualization as HardwareVirtualization>::NAME;

/// Prints why bring-up stopped.
pub fn report_error(error: BringUpError) {
    match error {
        BringUpError::Tss(error) => log!("Context TSS allocation failed: {error:?}"),
        // Only this variant comes after LGDT/LTR; every other GdtError is
        // returned before the CPU's tables are touched.
        BringUpError::Gdt(GdtError::ActivationVerificationFailed) => {
            log!("Context GDT/TSS activation failed verification; the CPU may already use the new tables.")
        }
        BringUpError::Gdt(error) => log!("Context GDT/TSS setup failed: {error:?}; GDTR/TR unchanged."),
        BringUpError::GuestMemory(GuestMemoryError::NotIdentityMapped {
            virtual_address,
            physical,
        }) => log!(
            "Guest memory setup failed: page {virtual_address:#018x} is mapped at {physical:#018x}, not identity-mapped."
        ),
        BringUpError::GuestMemory(error) => log!("Guest memory setup failed: {error:?}"),
        BringUpError::Intercepts(error) => backend::Intercepts::report_error(error),
        BringUpError::NotSupportedByCpu => log!("{VMX} not supported by this CPU."),
        BringUpError::DisabledByFirmware => log!("{VMX} supported by CPU but disabled by firmware."),
        BringUpError::Mtrr(error) => log!("MTRR setup failed: {error}."),
        BringUpError::NestedPaging(error) => backend::NestedPaging::report_error(error),
        BringUpError::LocalApic(LocalApicError::GuestMapping { address, translation }) => log!(
            "The page tables the guest shares do not map the Local APIC's registers at {address:#x} to themselves: {translation:?}."
        ),
        BringUpError::LocalApic(LocalApicError::EptMapping { address, end }) => {
            log!("EPT does not map the Local APIC's registers at {address:#x} to themselves, UC: {end}.")
        }
        BringUpError::Host(error) => log!("Host environment setup failed: {error:?}"),
        BringUpError::Virtualization(error) => backend::HardwareVirtualization::report_error(error),
        BringUpError::Vcpu(error) => backend::Vcpu::report_error(error),
        BringUpError::ContextTablesChanged => {
            log!("Context descriptor tables changed after VMX/UEFI calls; the guest was not launched.")
        }
        BringUpError::ResidentMemoryMapUnavailable => {
            log!(
                "The UEFI memory map is unavailable, so the resident allocations went unchecked; the guest was not launched."
            )
        }
        BringUpError::NotResident {
            address,
            memory_type: Some(memory_type),
        } => log!(
            "The resident allocation at {address:#018x} is {memory_type:?}, which the OS may reuse after ExitBootServices; the guest was not launched."
        ),
        BringUpError::NotResident {
            address,
            memory_type: None,
        } => log!("The resident allocation at {address:#018x} is in no memory-map region; the guest was not launched."),
        BringUpError::ResidentAllocationsUnlisted(count) => log!(
            "{count} resident allocation(s) came after the list was full and went unchecked; the guest was not launched."
        ),
        BringUpError::NoResidentAllocations => {
            log!("No resident allocation was listed, not even the image copy; the guest was not launched.")
        }
        BringUpError::ExitBootServicesEvent(status) => {
            log!("The ExitBootServices event could not be created: {status:?}; the firmware was not launched.")
        }
        BringUpError::UnexpectedExit { number, reason } => {
            let reason = backend::VmExit::describe_reason(reason);
            log!(
                "Guest stopped at VM exit #{number}: reason={} ({}){}; Aleph0 did not resume the guest.",
                reason.number,
                reason.name,
                if reason.entry_failure { " [VM entry failed]" } else { "" },
            )
        }
        BringUpError::MtrrsChanged => {
            log!("The CPU's MTRRs changed while the guest ran: a guest write reached them, not its copy.")
        }
        BringUpError::VmExitStackFull(size) => log!(
            "The exit handler used all {size} bytes of the VM-exit stack and may have overwritten the memory below it."
        ),
        BringUpError::UnexpectedExitStats(stats) => log!(
            "Guest finished, but its exit statistics were {stats}; expected {}.",
            crate::hv::guest::expected_stats(&stats),
        ),
        BringUpError::Teardown(TeardownError::Release(error)) => backend::Vcpu::report_error(error),
        BringUpError::Teardown(TeardownError::Disable(error)) => backend::HardwareVirtualization::report_error(error),
    }
}
