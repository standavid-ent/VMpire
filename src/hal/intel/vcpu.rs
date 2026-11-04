//! `hal::Vcpu` for VT-x: the VMCS and the VM-exit stack, over the original
//! functions of `vmcs`, `guest_state` and `vmexit`. The lines it prints are
//! the ones bring-up printed around each VMCS step.

use super::guest_state::{GuestState, GuestStateError};
use super::intercepts::Intercepts;
use super::nested_paging::NestedPaging;
use super::virtualization::secondary_controls;
use super::vmcs::{self, DesiredControls, GuestStop as VmcsStop, VmcsError, VmcsRegion, fields};
use super::vmexit::{self, VM_EXIT_STACK_SIZE, VmExitStack, VmExitStackError};
use crate::hal::intel::msr::VmcsControlMsrs;
use crate::hal::types::{GuestEntry, GuestKind, GuestStop, HostTables, OptionalFeatures};
use crate::x86::cr::CrControls;

/// The VMCS, once created, and the stack VM exits run on.
pub struct Vcpu {
    stack: VmExitStack,
    vmcs: Option<VmcsRegion>,
}

/// Why a VMCS step failed. A VmFailValid's VM-instruction error number is
/// read when the error is made: after teardown it can no longer be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuError {
    /// The VM-exit stack could not be allocated.
    ExitStack(VmExitStackError),
    /// A VMCS step failed; `operation` names it for the report.
    Vmcs {
        operation: &'static str,
        error: VmcsError,
        instruction_error: Option<u32>,
    },
    /// Building or writing the guest state failed.
    GuestState {
        error: GuestStateError,
        instruction_error: Option<u32>,
    },
    /// VMCLEAR failed at teardown, so the VMCS data may not have been written
    /// back.
    Vmclear(VmcsError),
    /// VMRESUME failed after an exit the handler resumed.
    ResumeFailed {
        error: VmcsError,
        instruction_error: Option<u32>,
    },
}

/// Tags a VMCS failure with the step it came from, for use with `map_err`.
/// Reads the VM-instruction error number now, while the VMCS is still
/// current.
fn vmcs_step(operation: &'static str) -> impl FnOnce(VmcsError) -> VcpuError {
    move |error| VcpuError::Vmcs {
        operation,
        error,
        // SAFETY: a VmFailValid came from a VMX instruction in VMX
        // operation, and the VMCS is not cleared until teardown, after this
        // runs.
        instruction_error: unsafe { vmcs::instruction_error_for(error) },
    }
}

/// Converts a guest-state failure, for use with `map_err`. Reads the
/// VM-instruction error number now, while the VMCS is still current.
fn guest_state_error(error: GuestStateError) -> VcpuError {
    let instruction_error = match error {
        // SAFETY: a VmFailValid came from a VMX instruction in VMX operation,
        // and the VMCS is not cleared until teardown, after this runs.
        GuestStateError::Vmcs { error, .. } => unsafe { vmcs::instruction_error_for(error) },
        _ => None,
    };
    VcpuError::GuestState {
        error,
        instruction_error,
    }
}

/// The neutral stop for the way `vmcs::launch` says the guest stopped.
fn stop(stop: VmcsStop) -> GuestStop<VcpuError> {
    match stop {
        VmcsStop::Finished { exits } => GuestStop::Finished { exits },
        VmcsStop::Unexpected { number, reason } => GuestStop::Unexpected {
            number,
            reason: u32::from(reason.basic) | (u32::from(reason.entry_failure) << 31),
        },
        VmcsStop::LaunchFailed {
            error,
            instruction_error,
        } => GuestStop::LaunchFailed {
            error: VcpuError::Vmcs {
                operation: "launch",
                error,
                instruction_error,
            },
        },
        VmcsStop::ResumeFailed {
            error,
            instruction_error,
        } => GuestStop::ResumeFailed {
            error: VcpuError::ResumeFailed {
                error,
                instruction_error,
            },
        },
    }
}

/// Prints a VMCS failure with the VM-instruction error number captured when
/// it happened. Reads nothing from the VMCS: by now it may be cleared.
fn report_vmcs_error(operation: &str, error: VmcsError, instruction_error: Option<u32>) {
    match (error, instruction_error) {
        (VmcsError::VmFailValid, Some(code)) => {
            let name = vmcs::vm_instruction_error_name(code);
            log!("VMCS {operation} failed: {name} ({code})");
        }
        (VmcsError::VmFailValid, None) => {
            log!("VMCS {operation} failed: VmFailValid, error number unreadable");
        }
        (VmcsError::ReadbackMismatch { field, written, read }, _) => {
            log!("VMCS {operation} failed: field {field:#06x} read back {read:#x}, but {written:#x} was written.");
        }
        _ => log!("VMCS {operation} failed: {error:?}"),
    }
}

impl Vcpu {
    fn vmcs(&self) -> &VmcsRegion {
        self.vmcs.as_ref().expect("the VMCS is created before it is used")
    }
}

impl crate::hal::Vcpu for Vcpu {
    type Error = VcpuError;

    fn allocate() -> Result<Self, VcpuError> {
        let stack = VmExitStack::allocate().map_err(VcpuError::ExitStack)?;
        log!(
            "VM-exit stack allocated: top={:#018x} size={VM_EXIT_STACK_SIZE}.",
            stack.top()
        );
        Ok(Self { stack, vmcs: None })
    }

    unsafe fn create(&mut self) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation.
        self.vmcs = Some(unsafe { VmcsRegion::create_current() }.map_err(vmcs_step("creation"))?);
        Ok(())
    }

    unsafe fn release(&self) -> Result<(), VcpuError> {
        // SAFETY: still in VMX operation, with this CPU's VMCS; VMXOFF comes
        // next.
        unsafe { self.vmcs().vmclear() }.map_err(|error| {
            log!("Teardown: VMCLEAR failed: {error:?}.");
            VcpuError::Vmclear(error)
        })
    }

    unsafe fn self_test(&self) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current.
        unsafe { vmcs::self_test() }.map_err(vmcs_step("self-test"))?;
        log!("VMCS self-test passed: VMREAD returned what VMWRITE stored.");
        Ok(())
    }

    unsafe fn configure_controls(
        &self,
        capabilities: &VmcsControlMsrs,
        kind: GuestKind,
        features: OptionalFeatures,
    ) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current.
        unsafe {
            vmcs::configure_controls(
                capabilities,
                DesiredControls::minimal_64_bit_guest(kind, secondary_controls(features)),
            )
        }
        .map_err(vmcs_step("control setup"))?;
        log!("VMCS controls for the {kind:?} guest written and read back successfully.");
        Ok(())
    }

    unsafe fn set_nested_paging(&self, paging: &NestedPaging) -> Result<(), VcpuError> {
        let eptp = paging.ept().eptp();
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current, and that the EPT tables are never freed.
        unsafe { vmcs::set_ept_pointer(eptp) }.map_err(vmcs_step("EPT pointer setup"))?;
        log!(
            "VMCS EPT pointer {:#018x} (WB, 4-level) written and read back.",
            eptp.raw()
        );
        Ok(())
    }

    unsafe fn set_intercepts(&self, intercepts: &Intercepts) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current, and that the bitmap pages are never freed.
        unsafe { intercepts.bitmaps().write_to_vmcs() }.map_err(vmcs_step("exit bitmap setup"))?;
        log!("VMCS MSR and I/O bitmap addresses written and read back.");
        Ok(())
    }

    unsafe fn configure_host_state(&self, tables: &HostTables, cr4: u64) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current, and that the host environment's pages are never freed.
        unsafe { vmcs::configure_host_state(tables, cr4) }.map_err(vmcs_step("host-state setup"))?;
        // SAFETY: as above.
        let exit_controls =
            unsafe { vmcs::vmread(fields::VM_EXIT_CONTROLS) }.map_err(vmcs_step("exit-control readback"))?;
        log!(
            "VMCS host fields written and read back; host PAT load={} EFER load={}; VM entry not attempted.",
            exit_controls & fields::VM_EXIT_LOAD_IA32_PAT != 0,
            exit_controls & fields::VM_EXIT_LOAD_IA32_EFER != 0,
        );
        Ok(())
    }

    unsafe fn configure_exit_entry(&self) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current. The exit stack and the handler stay in place for as long
        // as the guest can exit.
        unsafe { vmcs::configure_host_entry(self.stack.top(), vmexit::entry_address()) }
            .map_err(vmcs_step("host entry address setup"))?;
        log!("VMCS HOST_RSP/HOST_RIP written and read back; VM entry not attempted.");
        Ok(())
    }

    /// Writes the CR0/CR4 guest/host masks and read shadows, and prints
    /// them: which bits the guest owns, and the view it gets of them
    /// (CR4.VMXE hidden).
    unsafe fn configure_cr_intercepts(&self, controls: &CrControls) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current.
        unsafe { vmcs::configure_cr_shadowing(controls) }.map_err(vmcs_step("CR mask/shadow setup"))?;
        log!(
            "CR shadowing: CR0 mask={:#x} shadow={:#x}; CR4 mask={:#x} shadow={:#x} (VMXE hidden from the guest).",
            controls.cr0_mask,
            controls.cr0_shadow,
            controls.cr4_mask,
            controls.cr4_shadow,
        );
        Ok(())
    }

    /// Writes the guest state and prints it. If VM entry later fails with
    /// "invalid guest state" (exit reason 33), these are the values the CPU
    /// rejected.
    unsafe fn set_guest_state(&self, entry: GuestEntry) -> Result<(), VcpuError> {
        // SAFETY: the caller guarantees VMX root operation with this VMCS
        // current, the controls written, and the host GDT live and stable.
        let state = unsafe { GuestState::from_current(entry) }.map_err(guest_state_error)?;
        // SAFETY: as above.
        unsafe { state.write() }.map_err(guest_state_error)?;
        state.report(format_args!("Guest"));
        if let GuestEntry::InPlace { .. } = entry {
            log!(
                "Guest rip, rsp and rflags are placeholders: VMLAUNCH writes its own, and the code after it goes on as the guest."
            );
        }
        log!("VMCS guest fields written and read back; VM entry not attempted.");
        Ok(())
    }

    unsafe fn run(&self) -> GuestStop<VcpuError> {
        // SAFETY: the caller guarantees a fully written current VMCS and that
        // everything it points at is never freed.
        stop(unsafe { vmcs::launch() })
    }

    unsafe fn run_in_place(&self) -> Result<(), GuestStop<VcpuError>> {
        // SAFETY: as for `run`; the handler halts at a stop of this guest.
        unsafe { vmcs::launch_in_place() }.map_err(stop)
    }

    unsafe fn exit_stack_usage(&self) -> Option<(usize, usize)> {
        // SAFETY: the caller guarantees the guest has stopped, so no exit
        // handler runs on the stack.
        Some((unsafe { self.stack.used() }, VM_EXIT_STACK_SIZE))
    }

    fn report_error(error: VcpuError) {
        match error {
            VcpuError::ExitStack(error) => log!("VM-exit stack allocation failed: {error:?}"),
            VcpuError::Vmcs {
                operation,
                error,
                instruction_error,
            } => report_vmcs_error(operation, error, instruction_error),
            VcpuError::GuestState {
                error:
                    GuestStateError::Vmcs {
                        field,
                        error: VmcsError::ReadbackMismatch { written, read, .. },
                    },
                ..
            } => log!("Guest-state field {field:#06x} read back {read:#x}, but {written:#x} was written."),
            VcpuError::GuestState {
                error: GuestStateError::Vmcs { field, error },
                instruction_error,
            } => {
                log!("Guest-state field {field:#06x} failed:");
                report_vmcs_error("guest-state write", error, instruction_error);
            }
            VcpuError::GuestState { error, .. } => log!("Guest state invalid: {error:x?}"),
            VcpuError::Vmclear(error) => log!("Leaving VMX operation failed: Vmclear({error:?})."),
            VcpuError::ResumeFailed {
                error,
                instruction_error,
            } => report_vmcs_error("resume", error, instruction_error),
        }
    }
}
