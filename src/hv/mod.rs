//! The hypervisor's own logic, shared by every backend: what each VM exit
//! is (`exit_decision`), the exit handler and its reports (`vmexit`), what
//! the guest is offered (`features`), Hyper-V's interface (`hyperv`),
//! Aleph0's own hypercalls (`vmcall`) and the built-in test guest (`guest`).

pub mod exit_decision;
pub mod features;
pub mod guest;
pub mod hyperv;
pub mod vmcall;
pub mod vmexit;
