//! The VM-exit handler, shared by every backend, and what it keeps.
//!
//! On every VM exit the CPU loads host state from the VMCS and jumps to
//! HOST_RIP (`hal::intel::vmexit::entry_address`) with RSP = HOST_RSP
//! (`VmExitStack::top`).
//! Nothing else is set up for us: interrupts are off (host RFLAGS is forced
//! to 0x2), and the guest's general-purpose registers are still live in the
//! CPU, so the stub saves them before any Rust code can clobber them.
//!
//! `bring_up::run` enters the guest through `vmcs::launch` (the test guest)
//! or `vmcs::launch_in_place` (the firmware), so the guest's first VM exit
//! lands here. The handler (`handler.rs`) prints each exit over
//! serial, and `exit_decision::decide` says what it is: an instruction to
//! execute for the guest (CPUID, RDMSR, WRMSR, IN, OUT), an access to its
//! copy of its MTRRs (`guest_mtrrs.rs`), or a #GP to inject for an MSR it
//! cannot have or a value written to one that the CPU would refuse,
//! a write to the EPT-protected data page to allow and retry, a VMCALL to
//! resume past, an NMI to reinject, an NMI window to inject in, the last
//! planned VMCALL, or something unexpected. To resume, the
//! handler updates the VMCS and returns 0, and the stub executes VMRESUME. To
//! stop the test guest, it returns a stop value, and the stub jumps back
//! into `vmcs::launch`, which returns to bring-up (see `vmx/vmcs/launch.rs`).
//! A stop of the firmware has no launch frame to return to, so the handler
//! halts once it has printed the stop. The
//! lines that only print live in `report.rs`; how much of each exit they
//! print is `policy.rs`'s decision. Every exit is counted in the vCPU's
//! statistics (`stats.rs`). Every stop adds the guest's state and registers
//! (`dump.rs`), the statistics, and the last exits (`history.rs`).

mod guest_mtrrs;
mod handler;
mod history;
mod policy;
mod report;
mod stats;

pub use guest_mtrrs::{publish as publish_guest_mtrrs, registers as guest_mtrr_registers};
pub use stats::{VcpuStats, snapshot as vcpu_stats};

/// Prints the last VM exits, oldest first, each line after `context`: for
/// the panic handler, so a panic in exit context shows what led to it. Like
/// the panic handler, left out of host tests.
#[cfg(not(test))]
pub use history::report as report_exit_history;

pub use handler::{handle_exit, stop_after_failed_resume};
pub use report::after_stop;
