# HAL interface: design for approval

Branch `hal-redo`, started from the original code (commit 582adf4). Nothing
is implemented yet. This file lists every hardware-specific thing the shared
code does today and the trait member or neutral type that replaces it.
**Every name below needs your approval before code changes.**

The first attempt is kept in `git stash` (`stash@{0}`, "HAL attempt 1").

---

## 0. Rules

1. **Shared code never sees Intel.** No VMCS field, VMX constant, EPT bit,
   exit-reason number, Intel type or Intel error outside `src/hal/intel/`.
   Shared code calls trait functions and uses neutral types only.
2. **Every hardware-specific action or check is a trait function**,
   implemented in `hal/intel/` now and `hal/amd/` later. Checks on exit data
   are readers: the backend decodes, shared code asks.
3. **Names come from the virtualization literature** (vCPU, VM exit, exit
   reason, event injection, interrupt shadow, nested paging, intercept,
   hypercall), as KVM, the Intel SDM and the AMD APM share them. Inside
   `hal/intel/` the original names stay.
4. **Backend chosen at compile time**: Cargo feature `intel` (default) or
   `amd`; `hal::backend` is that backend's module, so shared code writes
   `backend::Vcpu::run()` (the trait `hal::Vcpu` in scope), never
   `VmcsRegion`.
5. **Static dispatch** (generics/aliases, no trait objects), as HAL_PLAN.md
   says. Intel behavior unchanged; each step builds, tests pass, and the
   Hyper-V run matches before the next.

```
bring_up  hv  host  resident  x86 (shared)
        │ uses only
        ▼
hal/mod.rs     traits + `pub use intel as backend;` (by feature)
hal/types.rs   neutral types (ExitReason, InjectedEvent, …)
        ▲ implemented by
hal/intel/     VMX, VMCS, EPT, MSR/IO bitmaps, entry stub (original code)
hal/amd/       later: SVM, VMCB, NPT, MSRPM/IOPM
```

---

## 1. Traits (names to approve)

| Trait | Role | Intel today | AMD later | Literature |
|---|---|---|---|---|
| `HardwareVirtualization` | detect, enable, disable the CPU's virtualization extension; its capabilities | `vmx::detect`, `vmxon`, `VmxOperation`, `read_vmcs_control_msrs`, `CrFixed` | CPUID SVM, EFER.SVME, VM_HSAVE_PA | "virtualization extensions" (VT-x / AMD-V); KVM `hardware_enable` |
| `Vcpu` | the virtual CPU: create, configure, run, read and change guest state, inject events | `VmcsRegion`, `vmcs::*`, `GuestState`, `launch`, entry stub | VMCB, VMRUN | "vCPU"; KVM `kvm_x86_ops` vCPU ops |
| `VmExit` | the vCPU stopped at an exit: one reader per detail of the exit, and the changes made before it resumes | `VM_EXIT_REASON`, `EXIT_QUALIFICATION`, `*_INFO` fields | EXITCODE, EXITINFO1/2, EXITINTINFO | "VM exit", "exit reason", "exit information" |
| `NestedPaging` | the second-level page tables: build, protect, retype, flush, walk | `Ept`, `TypeTables`, `ProtectedPage`, INVEPT | NPT, TLB flush via ASID | "nested paging" / SLAT; KVM "TDP" |
| `Intercepts` | which MSR and I/O accesses exit | `ExitBitmaps` (MSR + I/O bitmaps) | MSRPM, IOPM | "intercept" (APM), "MSR/IO bitmaps" (SDM) |
| `Hypercall` | the hypercall instruction: Aleph0's own call, and forwarding to Hyper-V | `vmcall`, `hyperv_forward_hypercall` stub | `vmmcall` | "hypercall" |

The names HAL_PLAN.md used (`VcpuControl`, `GuestMemory`, `VmxOperation`,
`ExitBitmaps`) are replaced: the last two are Intel words.

The backend, by feature, in `hal/mod.rs`. Each backend module exports one
type per trait, with the trait's name; the full list is in HAL_STRUCTURE.md.

```rust
#[cfg(feature = "intel")] pub use intel as backend;
// shared code: use crate::hal::{backend, Vcpu as _};  backend::Vcpu::create()
```

---

## 2. Neutral types (`hal/types.rs`, names to approve)

| Neutral type | Replaces (Intel) | Contents | Literature |
|---|---|---|---|
| `ExitReason` (enum) | `ExitReason { basic, entry_failure }` + 15 `EXIT_REASON_*` | `ExceptionOrNmi`, `InterruptWindow`, `NmiWindow`, `Cpuid`, `Hlt`, `Hypercall`, `ControlRegisterAccess`, `IoInstruction`, `Rdmsr`, `Wrmsr`, `Mwait`, `Monitor`, `NestedPageFault`, `NestedPagingMisconfig`, `Xsetbv`, `EntryFailure(u32)`, `Other(u32)` | HAL_PLAN.md: enum named after the source's reasons |
| `InjectedEvent` | `InterruptionInfo` (VM-entry/exit layout) | `kind` (`ExternalInterrupt`, `Nmi`, `HardwareException`, `SoftwareInterrupt`, `SoftwareException`), `vector`, `error_code: Option<u32>`, `instruction_len: Option<u32>` | "event injection" (SDM, APM EVENTINJ) |
| `Interruptibility` | `GUEST_INTERRUPTIBILITY_STATE` bits | `sti_shadow`, `mov_ss_shadow`, `nmi_blocked` (bools) | "interrupt shadow"; KVM `KVM_X86_SHADOW_INT_STI/MOV_SS` |
| `CrAccess` | `CrQualification` (bits 3:0, 5:4, 11:8) | `cr: u8`, `kind` (`MovTo`, `MovFrom`, `Clts`, `Lmsw`), `gpr: u8` | "control-register access" |
| `IoAccess` | `IoQualification` | `port`, `size: IoSize`, `is_in`, `is_string`, `is_rep` | "I/O instruction" |
| `NestedPageFault` | `EptQualification` + `GUEST_PHYSICAL_ADDRESS` | `gpa`, `read`, `write`, `fetch`, `page_readable`, `page_writable`, `page_executable`, `linear_valid` | "nested page fault" (APM NPF), "EPT violation" (SDM) |
| `Permissions` | EPT R/W/X bits in `WalkEnd::Mapped::access` | `read`, `write`, `execute` | page permissions |
| `GuestRegisters` | `RegisterFrame` | the 15 GPRs the exit path saves | "guest registers" |
| `ExitAction` | `ExitAction` + stub's `RESUME_GUEST` / `encode_*` values | `Resume`, `Finished`, `Unexpected` (backend encodes for its own run loop) | — (already neutral) |
| `GuestStop` | `GuestStop` with `VmcsError` | same variants, error is the backend's `Error` | — |
| `CrConstraints` | `CrFixed` (VMX FIXED0/FIXED1 MSRs) | `cr0_must_be_one`, `cr0_may_be_one`, `cr4_must_be_one`, `cr4_may_be_one` | "fixed bits" are Intel's word |
| `OptionalFeatures` | `GuestFeatures::secondary` (VMX secondary-control bits) | `rdtscp`, `invpcid`, `xsaves`, `waitpkg` (bools) | CPUID feature names |
| `Capabilities` | `VmxCapabilities` | `Supported`, `NotSupportedByCpu`, `DisabledByFirmware` | — |
| `GuestKind`, `GuestEntry`, `HostTables`, `HostSelectors`, `IoSize`, `CrNumber`, `CrWrite`, `ResumePlan`, `Emulation`, `ExitEvent`, `ExitState` | same | unchanged: already neutral; move out of `vmx` | — |

`ResumePlan` stays shared, but its Intel parts become neutral:
`inject_exception: Option<InjectedEvent>`, `redeliver: Option<InjectedEvent>`,
and `interruptibility(found: Interruptibility) -> Interruptibility`.

Errors: each trait has `type Error: Debug + Display`. `BringUpError` wraps
them as `Virtualization(..)`, `Vcpu(..)`, `NestedPaging(..)`,
`Intercepts(..)`. The Intel types (`VmcsError`, `VmxFail`, `VmxOnError`,
`GuestStateError`, `EptError`, `ExitBitmapsError`) stay inside `hal/intel`.

---

## 3. Every Intel use in shared code → its replacement

### 3.1 Bring-up (`bring_up/mod.rs`, `report.rs`, `error.rs`, `teardown.rs`)

| Today | Replacement |
|---|---|
| `vmx::detect()` → `VmxCapabilities` | `HardwareVirtualization::detect() -> Capabilities` |
| `msr::read_vmcs_control_msrs()` → `VmcsControlMsrs`; `report::vmx_capabilities` | `HardwareVirtualization::control_capabilities() -> Self::ControlCapabilities` (associated type); the backend prints its own capability lines |
| `ept::check_support(controls.secondary, controls.ept_vpid_cap, phys_bits)` | `NestedPaging::check_support(&caps, phys_bits) -> Self::Support`; `report::ept_support` moves into the backend |
| `vmxon::enter_vmx_root_operation()`, `VmxOperation::{original_cr0, original_cr4}` | `HardwareVirtualization::enable() -> Self`, `original_cr0()`, `original_cr4()` |
| teardown: `VmxOperation::leave`, `cr::CR4_VMXE` clear | `HardwareVirtualization::disable(self)` (clears its own enable bit) |
| `CrFixed::read()`, `CrControls::new(..)`, `vmcs::configure_cr_shadowing` | `HardwareVirtualization::cr_constraints() -> CrConstraints`; `Vcpu::configure_cr_intercepts(&CrControls)` |
| `VmcsRegion::create_current()`, teardown `vmcs.vmclear()` | `Vcpu::create() -> Self`, `Vcpu::release(&self)` |
| `vmcs::self_test()` | `Vcpu::self_test()` |
| `GuestFeatures::read(controls.secondary)` | `HardwareVirtualization::optional_features(&caps) -> OptionalFeatures`; `GuestFeatures` built from it (shared) |
| `DesiredControls::minimal_64_bit_guest(kind, secondary)` + `vmcs::configure_controls` | `Vcpu::configure_controls(&caps, kind, OptionalFeatures)` |
| `vmcs::set_ept_pointer(ept.eptp())`, its log line | `Vcpu::set_nested_paging(&NestedPaging)`; log in the backend |
| `ExitBitmaps::allocate(kind)`, `bitmaps.write_to_vmcs()`, `report::exit_bitmaps` | `Intercepts::allocate(kind)`, `Vcpu::set_intercepts(&Intercepts)`; report in the backend |
| `vmcs::configure_host_state(&HostTables, cr4)`, `vmread(VM_EXIT_CONTROLS)` log | `Vcpu::configure_host_state(&HostTables, cr4)`; log in the backend |
| `VmExitStack::allocate/top/used`, `configure_host_entry(top, vmexit::entry_address())` | inside the Intel `Vcpu` (AMD's VMRUN returns like a call and needs none); shared asks `Vcpu::exit_stack_usage() -> Option<(usize, usize)>` for the "bytes used" line |
| `GuestState::from_current(entry)`, `.write()`, `report::guest_state` | `Vcpu::set_guest_state(entry: GuestEntry)`; report in the backend |
| `vmcs::launch()` → `GuestStop`, `launch_in_place()` | `Vcpu::run() -> GuestStop`, `Vcpu::run_in_place() -> Result<(), GuestStop>` |
| `Ept::build(..)`, `ept::spare_pts`, `types.hide(..)`, `types.walk(eptp, gpa)`, `types.misconfigure`, `ept.memory_map_end()`, `report::ept`/`hidden` | `NestedPaging::build(..)`, `spare_tables(mtrr)`, `hide(..)`, `walk(gpa) -> WalkEnd` (+ `report_walk` for the entries), `misconfigure(gpa)` (test feature), `memory_map_end()`; reports in the backend |
| `BringUpError` variants holding Intel errors | hold the traits' `Error` types |

### 3.2 Exit path (`vmexit/handler.rs`, `report.rs`, `history.rs`, `stats.rs`, `policy.rs`, `dump.rs`)

| Today | Replacement |
|---|---|
| entry stub calls `vmexit_handler(frame) -> u64` (`RESUME_GUEST` or `encode_*`) | backend's run loop calls shared `hv::handle_exit(&mut GuestRegisters) -> ExitAction`; Intel encodes the stub value itself |
| `vmresume_failed` (VMRESUME fell through) | inside the Intel backend; calls shared `hv::report_stop` |
| `read_field(VM_EXIT_REASON)` + `ExitReason::from_raw` | `VmExit::reason() -> Result<ExitReason, Error>` |
| `EXIT_REASON_*` comparisons (≈25 in decide, handler, report) | `match` on `ExitReason` variants |
| `exit_reason_name(basic)` | `ExitReason::name()` (shared, per variant); raw values via `VmExit::raw() -> RawExit` for the report |
| `read_field(EXIT_QUALIFICATION)` + `CrQualification(q).cr()/access()/gpr()` | `VmExit::cr_access() -> CrAccess` |
| `IoQualification(q).port()/size()/is_in()/is_string()/is_rep()` | `VmExit::io_access() -> IoAccess` |
| `EptQualification(q)…` + `GUEST_PHYSICAL_ADDRESS` | `VmExit::nested_page_fault() -> NestedPageFault` |
| `q & EPT_NMI_UNBLOCKING_IRET`, `InterruptionInfo::is_nmi_unblocking_iret_fault` | `VmExit::faulted_nmi_unblocking_iret() -> bool` |
| `VM_EXIT_INTERRUPTION_INFO` → `InterruptionInfo` | `VmExit::exception_or_nmi() -> Option<InjectedEvent>` |
| `IDT_VECTORING_INFO` + `IDT_VECTORING_ERROR_CODE` | `VmExit::interrupted_event() -> Option<InjectedEvent>` (with error code) |
| `GUEST_INTERRUPTIBILITY_STATE` + `BLOCKING_BY_*` masks | `VmExit::interruptibility() -> Interruptibility` |
| `GUEST_SS_ACCESS_RIGHTS` + `cpl_from_ss` | `VmExit::guest_cpl() -> u8` |
| `GUEST_RIP`, `VM_EXIT_INSTRUCTION_LEN`, `GUEST_RSP`, `GUEST_CR4` | `VmExit::guest_rip()`, `instruction_length()`, `guest_rsp()`, `guest_cr4()` |
| `GUEST_LINEAR_ADDRESS` (report) | `VmExit::guest_linear_address()` |
| `EPT_POINTER` + `Eptp::from_raw` (report's walk) | `NestedPaging::walk(gpa)` (the backend knows its root) |
| `vmwrite(GUEST_RIP, next)` | `VmExit::set_rip(next)` |
| `vmwrite(GUEST_INTERRUPTIBILITY_STATE, ..)` | `VmExit::set_interruptibility(Interruptibility)` |
| `vmwrite(VM_ENTRY_INTERRUPTION_INFO, NMI)`, `queue_exception`, `write_redelivery` | `VmExit::inject_event(InjectedEvent)` |
| `set_nmi_window_exiting`, `set_interrupt_window_exiting` | `VmExit::set_nmi_window(on)`, `VmExit::set_interrupt_window(on)` |
| `cr_write_fields` + two vmwrites | `VmExit::write_cr(CrWrite)` |
| `ProtectedPage::grant_write()` → `InveptType` | `NestedPaging::grant_write(page) -> Flush` |
| `InveptType` (report) | `Flush` (neutral: `SingleContext`, `AllContexts`); name per backend |
| `dump::report_guest(number, frame)` | `VmExit::dump(number, &GuestRegisters)` |
| per-reason counters indexed by Intel number (`policy::counter`) | indexed by `ExitReason` variant |
| `history::ExitRecord { reason: u32, qualification, interruption_info, idt_vectoring_info }` | stores `ExitReason` and the backend's raw values for printing via `VmExit::raw() -> RawExit` |

### 3.3 Decide (`exit_decision/*`)

| Today | Replacement |
|---|---|
| `ExitInfo { reason: ExitReason, interruption_info, interruptibility: u32, idt_vectoring, qualification: u64, guest_physical, cr_fixed: CrFixed, .. }` | `ExitInfo` keeps its role, with neutral fields: `reason: ExitReason`, `event: Option<InjectedEvent>`, `interruptibility: Interruptibility`, `interrupted: Option<InjectedEvent>`, `cr_access`, `io_access`, `fault: NestedPageFault`, `cr_constraints: CrConstraints`, `faulted_nmi_unblocking_iret: bool` |
| `match exit.reason.basic { EXIT_REASON_CPUID => .. }` | `match exit.reason { ExitReason::Cpuid => .. }` |
| `scenario::BLOCKING_BY_*` masks | `Interruptibility` fields |
| `ResumePlan::interruptibility(u32)`, `after_skipped_instruction`, `after_nmi_redelivery` | same functions on `Interruptibility` |
| `InterruptionInfo::{NMI, GENERAL_PROTECTION, INVALID_OPCODE}` | `InjectedEvent::{NMI, GENERAL_PROTECTION, INVALID_OPCODE}` |
| `cpuid_policy` hides `CPUID_1_ECX_VMX` | hides `HardwareVirtualization::CPUID_FEATURE` (Intel: leaf 1 ECX bit 5; AMD: 0x8000_0001 ECX bit 2) |
| `control_register`: `CR4_VMXE` → #GP | `HardwareVirtualization::GUEST_FORBIDDEN_CR4` (Intel: VMXE; AMD: 0, SVME is in EFER) |
| `exit_bitmaps::{msr_bit, MsrAccess}` in MSR routing | `Intercepts::covers_msr(msr) -> bool` |
| `INTERCEPTED_MSRS`, `INTERCEPTED_PORTS` | shared policy lists (move to `hv`), handed to `Intercepts::allocate` |

### 3.4 Elsewhere

| File | Today | Replacement |
|---|---|---|
| `host/handlers.rs` | `vmcs::set_nmi_window_exiting(true)` in the host NMI handler | `VmExit::current().set_nmi_window(true)` |
| `host/gdt.rs`, `host/mod.rs` | `vmcs::{HostSelectors, HostTables}` | neutral `HostSelectors`, `HostTables` in `hal/types` |
| `host/page_tables.rs` | `ept::level::{Level, Pml4, Pdpt, Pd, TableLevel}`, `SIZE_1G`, `PDPT_SPAN` | plain x86: moved to `x86/paging` (shared, not a trait) |
| `features.rs` | `SECONDARY_ENABLE_*`, `secondary_request`, `ControlCapability` | `OptionalFeatures` (bools); the control bits live in `hal/intel` |
| `apic.rs` | `ept::WalkEnd` | neutral `WalkEnd` with `Permissions` |
| `hyperv.rs` | `vmcall` in the forwarding stub | `Hypercall::forward(..)` + `Hypercall::recovery_point()` |
| `resident/mod.rs` | `asm!("vmcall")` | `Hypercall::call(rcx)` |
| `guest.rs` (test guest program) | three `vmcall`s in `global_asm!` | program under `#[cfg(feature = "intel")]`; AMD gets its own with `vmmcall` |
| `cpuid.rs`, `msr.rs` | `is_vmx_supported`, `feature_control_vmx_enabled`, `IA32_VMX_*` | into `hal/intel` (used only by `detect`/`capabilities`) |
| `segment.rs` | VMX access-rights format (`ACCESS_RIGHTS_UNUSABLE`, `SegmentDescriptor::access_rights`) | into `hal/intel` (used only by guest/host state) |

Plain x86 that stays shared (no trait): `page`, `paging`, `mtrr`,
`context_tables`, `segment` (GDT/IDT parts), `cr` (CR access, XSETBV),
`msr` (RDMSR/WRMSR, MSR numbers), `cpuid` (non-VMX parts), `apic`.

---

## 4. Order of work

**Status (2026-11-05): steps 1 to 5 done on branch `hal-redo`** (commits
849fc40 and 0075a1b). The grep in step 5 finds nothing outside `src/hal/`.
What changed from the approved list while implementing: HAL_STRUCTURE.md
section 10. Next: run the tests and the Hyper-V scripts on the Intel
machine; then fakes and AMD.

Each step: builds, clippy clean, tests unchanged, then you run the Hyper-V
scripts on the Intel machine.

1. **Move files** (no logic change): Intel-only to `hal/intel/`, plain x86
   to `x86/`, the hypervisor logic to `hv/`, host tables to `host/`.
2. **Neutral types** in `hal/types.rs` (section 2), with conversions in
   `hal/intel` from the Intel encodings.
3. **Traits** in `hal/mod.rs` (section 1) with the Intel impls calling the
   original code; role aliases by feature.
4. **Switch shared code** to the traits, file by file (section 3).
5. **Check**: `grep -r "hal::intel\|VmcsRegion\|EXIT_REASON_\|vmcs::" src` outside
   `src/hal/` returns nothing.
6. **Fakes** (HAL_PLAN Phase 4), then AMD (Phase 5).

---

Full signatures of every trait member and every field of every type:
**HAL_STRUCTURE.md**, as a checklist.

## 5. Decisions for you

1. Approve or change the trait names (section 1).
2. Approve or change the neutral type names (section 2).
3. `ExitReason` is an enum (HAL_PLAN.md says so), and every detail of an exit
   has its own reader (`cr_access`, `io_access`, `nested_page_fault`, …).
   OK?
4. Logs that print Intel details (VMCS fields, EPT layout, MSR capability
   values) move into the backend, so shared code prints only neutral lines.
   The serial output stays the same on Intel. OK?
