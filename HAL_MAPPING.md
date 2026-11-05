# HAL mapping: what changes, where it goes, and who controls what

Companion to HAL_PLAN.md (the phases) and ARCHITECTURE.md (Intel vs shared).
Written 2026-11-03 against `master` at 582adf4. Names follow the source code (see HAL_INTERFACE.md, naming rule). Layout decided: interface in `src/hal/`, backends in `src/hal/intel/` and `src/hal/amd/`. This file answers three
questions:

1. How do other hypervisors split Intel and AMD? (section 1)
2. How does control flow through Aleph0 today, and how will it flow behind a
   HAL? (sections 2-4)
3. For every file: what does it do, and what happens to it? (section 5)

Status words used below:

- **Shared**: plain x86-64 or hypervisor logic. Moves as is.
- **Intel**: only makes sense with VMX. Moves to `hal/intel/`.
- **Split**: part shared, part Intel. Cut in two before moving.

---

## 1. How other hypervisors do it

All five were checked in their source on GitHub on 2026-11-03 (file paths
given, so you can open them). They agree on one pattern:

> Shared code asks the backend for **"run the guest until it exits, and tell
> me why in neutral terms"**. Shared code decides what to do. The backend
> **applies** the decision to its own control structure (VMCS or VMCB).

### FreeBSD bhyve: the closest model for Aleph0

```
sys/amd64/vmm/
  vmm.c, x86.c, vmm_lapic.c, vmm_ioport.c, vmm_instruction_emul.c  shared
  intel/  vmx.c vmcs.c ept.c vmx_msr.c vmx_support.S  (+ vtd.c IOMMU)
  amd/    svm.c vmcb.c npt.c svm_msr.c svm_support.S  (+ amdvi IOMMU)
```

- **Interface**: `struct vmm_ops` in `sys/amd64/include/vmm.h`, a table of
  functions: `modinit`, `init`, `run`, `vcpu_init`, `getreg`/`setreg`,
  `getdesc`/`setdesc`, `getcap`/`setcap`, `vmspace_alloc`, ... Two instances:
  `vmm_ops_intel` and `vmm_ops_amd`.
- **Vendor choice**: in `vmm.c`, `DEFINE_VMMOPS_IFUNC` picks Intel or AMD
  **once at boot** (`vmm_is_intel()` / `vmm_is_svm()`). After that every call
  is direct, with no per-call check. One binary runs on both.
- **Neutral exit**: `struct vm_exit` with `enum vm_exitcode`
  (`VM_EXITCODE_INOUT`, `_RDMSR`, `_WRMSR`, `_HLT`, `_PAGING`, `_MONITOR`,
  `_MWAIT`, ...). `vmx.c` and `svm.c` each translate their hardware codes into
  it. It also has `inst_length`, so shared code can advance RIP without
  knowing the vendor.
- **Escape hatch**: `VM_EXITCODE_VMX` and `VM_EXITCODE_SVM` carry the raw
  vendor exit when no neutral code fits. Aleph0 should copy this
  (`ExitReason::Unnamed(basic)` in Aleph0), so an unknown exit still gets reported with its real
  number.
- **File layout lesson**: each vendor has the same set of files
  (`vmx.c`/`svm.c`, `vmcs.c`/`vmcb.c`, `ept.c`/`npt.c`, `vmx_msr.c`/`svm_msr.c`,
  `*_support.S`). Aleph0's `hal/intel/` and `hal/amd/` should match each
  other file for file the same way.

### Linux KVM: the complete list of operations

```
arch/x86/kvm/
  x86.c, emulate.c, cpuid.c, lapic.c, mtrr.c, hyperv.c, mmu/   shared
  vmx/   (vmx.c, vmcs*, nested.c, ...)
  svm/   (svm.c, nested.c, sev.c, ...)
```

- **Interface**: `struct kvm_x86_ops` in `arch/x86/include/asm/kvm_host.h`;
  the full list (135 operations) is in `arch/x86/include/asm/kvm-x86-ops.h`.
- **Vendor choice**: one module loads (`kvm-intel` or `kvm-amd`). Calls go
  through `static_call` (`kvm_x86_call(func)`), which patches each call site
  into a direct call. That is KVM's version of "no virtual calls on the hot
  path", the same reason HAL_PLAN.md picks generics.
- **Useful subset for Aleph0** (all present in `kvm-x86-ops.h`):
  `check_processor_compatibility`, `enable_virtualization_cpu`, `vcpu_create`,
  `vcpu_run`, `handle_exit`, `get_exit_info`, `skip_emulated_instruction`,
  `get_msr`/`set_msr`, `get_segment`/`set_segment`, `set_cr0`/`set_cr4`/`set_efer`,
  `get_rflags`, `cache_reg`, `get_interrupt_shadow`, `set_nmi_mask`,
  `inject_nmi`, `inject_irq`, `inject_exception`, `enable_nmi_window`,
  `enable_irq_window`, `load_mmu_pgd`, `flush_tlb_guest`, `get_mt_mask`.
  Almost every field of Aleph0's `ResumePlan` has a KVM op with the same job;
  section 4 lists them side by side.
- **Lesson**: KVM's MTRR (`mtrr.c`) and Hyper-V (`hyperv.c`) code is
  **shared**, not per vendor. The same should hold for Aleph0's
  `mtrr/` and `hyperv.rs`.

### Xen: the same table, different names

```
xen/arch/x86/hvm/
  hvm.c, emulate.c, mtrr.c, vlapic.c, viridian/ (Hyper-V interface) ...  shared
  vmx/  vmx.c vmcs.c entry.S intr.c ...
  svm/  svm.c vmcb.c entry.S intr.c asid.c ...
```

- **Interface**: `struct hvm_function_table` in
  `xen/arch/x86/include/asm/hvm/hvm.h`, one global `hvm_funcs`, called through
  `alternative_call` (patched into direct calls at boot, like bhyve's ifunc).
- Ops of interest: `vcpu_initialise`, `get_interrupt_shadow`/`set_interrupt_shadow`,
  `get_cpl`, `get_segment_register`, `update_guest_cr`, `update_guest_efer`,
  `get_guest_pat`/`set_guest_pat`, `inject_event`, and
  **`init_hypercall_page`**: it writes VMCALL on Intel and VMMCALL on AMD.
  That is the exact problem Aleph0's `hyperv.rs` forwarding has.
- **Lesson**: Xen's `viridian/` (its Hyper-V interface) is shared, and only
  the one instruction differs per vendor.

### RVM1.5 (Rust): compile-time switch

```
src/arch/x86_64/
  vmm.rs (shared exit handler + vendor switch), page_table.rs, cpuid.rs, ...
  intel/  ept.rs mod.rs structs.rs vcpu.rs vmexit.rs
  amd/    npt.rs mod.rs vcpu.rs vmexit.rs iommu.rs
```

- `vmm.rs` starts with `#[cfg(feature = "intel")] mod vendor;` /
  `#[cfg(feature = "amd")] mod vendor;`, then
  `pub use vendor::{check_hypervisor_feature, NestedPageTable, Vcpu};`.
  Only one vendor is in each binary.
- One trait, `VcpuAccessGuestState` (`regs`, `instr_pointer`, `rflags`,
  `cr`/`set_cr`, ...), implemented over VMCS on Intel and VMCB on AMD.
- Shared handlers in `vmm.rs`: `handle_cpuid`, `handle_msr_read/write`,
  `handle_hypercall`.
- **Lesson**: the simplest way to start. Its weakness: two binaries, and
  nothing forces the two `vendor` modules to export the same API. A trait
  would make the compiler check that.

### ArceOS `axvcpu` (Rust): trait + neutral exit enum

- `axvcpu/src/arch_vcpu.rs`: `pub trait AxArchVCpu: Sized` with associated
  types `CreateConfig`/`SetupConfig`, and `new`, `set_entry`, `set_ept_root`,
  `setup`, **`run() -> AxResult<AxVCpuExitReason>`**, `bind`/`unbind`,
  `set_gpr`, `inject_interrupt`, `set_return_value`.
- `axvcpu/src/exit.rs`: `enum AxVCpuExitReason { Hypercall, MmioRead,
  MmioWrite, SysRegRead, SysRegWrite, IoRead, IoWrite, ExternalInterrupt,
  NestedPageFault, Halt, FailEntry, ... }`.
- Splits by architecture (x86 VMX / AArch64 / RISC-V), not Intel/AMD: its
  x86 backend (`x86_vcpu`) has only `vmx/`. But its shape is the Rust shape
  Aleph0 wants: `Sized` trait (static dispatch), `run()` returns a neutral
  exit.

### cloud-hypervisor `hypervisor` crate (contrast only)

- `hypervisor/src/cpu.rs`: `trait Vcpu: Send + Sync`, `enum VmExit`.
  Backends `kvm/` and `mshv/`. It abstracts over **OS hypervisor APIs**, not
  over VMX/SVM, and uses trait objects. Not a model for Aleph0's hot path;
  listed so you know why it was ruled out.

### What Aleph0 takes from each

| Decision | Take from | Why |
|---|---|---|
| Neutral exit enum built by each backend | bhyve `vm_exit`, axvcpu `AxVCpuExitReason` | Shared code never sees a vendor number |
| Raw-code escape hatch `ExitReason::Unnamed` | bhyve `VM_EXITCODE_VMX/SVM` | Unknown exits stay readable in reports |
| Instruction length in the exit | bhyve `inst_length` | `advance_rip` works without vendor code |
| Pick vendor once, then direct calls | bhyve ifunc, KVM static_call, Xen alternatives | Hot path has no per-call check |
| Rust generics for that | axvcpu `Sized` trait | Compiler checks both backends offer the same API |
| MTRR and Hyper-V logic shared | KVM `mtrr.c`/`hyperv.c`, Xen `viridian/` | Only one instruction differs (VMCALL/VMMCALL) |
| Same file names in both vendor folders | bhyve, RVM1.5 | Easy to compare Intel and AMD side by side |

---

## 2. How control flows today

```
firmware
  └─ main.rs: serial on, copy image to resident memory, call run()
       └─ bring_up::run()                            (bring_up/mod.rs)
            1. activate_context_tables           GDT copy + TSS
            2. allocate exit stack, guest pages
            3. check_vmx_support, read MTRRs, build EPT,
               MSR/IO bitmaps, HostEnvironment (own CR3/GDT/IDT/TSS)
            4. VMXON, create VMCS                   ◀── Intel
            5. check_resident_memory, prepare_vmcs  ◀── Intel
            6. launch (test guest) or launch_in_place (firmware)  ◀── Intel
            7. teardown: VMXOFF, restore firmware tables          ◀── Intel

every VM exit (CPU jumps to HOST_RIP on the exit stack):
  vmexit/mod.rs   asm stub: save guest GPRs into RegisterFrame
  └─ handler.rs   vmexit_handler()
       1. read_exit   VMREADs -> ExitInfo               ◀── Intel
       2. judge       exit_decision::decide(...)        shared already
                        -> ExitEvent -> ExitAction -> ResumePlan
       3. carry_out   apply ResumePlan with VMWRITEs     ◀── Intel
                        (RIP, interruptibility, inject NMI/exception,
                         NMI/interrupt window, EPT write grant, CR write)
       4. report      print per policy; history; stats   shared, mostly
  stub: VMRESUME (or jump back to launcher to stop)     ◀── Intel
```

**The good news**: step 2 already has no VMCS access, and its output,
`ResumePlan` (`src/vmx/exit_decision/mod.rs:590`), is already written in
vendor-neutral terms: `advance_rip`, `inject_nmi`, `nmi_window`,
`interrupt_window`, `grant_write`, `emulate`, `inject_exception`,
`redeliver`, `clear_nmi_blocking`, `restore_nmi_blocking`, `write_cr`. It is
already the "decision" half of the bhyve/KVM pattern. The HAL work is mostly
about steps 1 and 3 and the bring-up steps 3-7.

---

## 3. How control flows behind the HAL

Names are the source's (see HAL_INTERFACE.md, naming rule). Only Intel is
built for now; the AMD column says what the same call would do later.

```
main.rs
  └─ bring_up::run()                       shared sequence, generic over the traits
       detect()                        hal/intel::detect      | CPUID SVM + VM_CR.SVMDIS
       GuestMemory::build()            Ept::build             | NPT build
       ExitBitmaps::allocate(kind)     MSR + I/O bitmaps      | MSRPM + IOPM
       VmxOperation::enter_vmx_root_operation()  VMXON        | EFER.SVME + VM_HSAVE_PA
       VcpuControl::create_current()   VMCLEAR + VMPTRLD      | VMCB page
       configure_controls, set_ept_pointer, write_to_vmcs,
       configure_host_state, configure_host_entry,
       configure_cr_shadowing, write_guest_state, self_test
                                       prepare_vmcs today     | fill VMCB
       launch() / launch_in_place()    VMLAUNCH               | first VMRUN
       VmxOperation::leave()           VMXOFF                 | clear EFER.SVME

every exit:
  Intel: asm stub at the configure_host_entry address -> vmexit_handler(frame)
  AMD:   VMRUN returns into a software loop          -> same shared steps
       1. read_exit()                  -> ExitInfo (ExitReason decoded)   backend
       2. judge -> decide(guest, state, exit) -> (ExitEvent, ExitState)   shared, unchanged
       3. carry_out: event.action() -> ExitAction::Resume(plan)
            plan.restoring_nmi_blocking, plan.redelivering                shared
            prepare_resume(plan)       VMWRITEs | VMCB writes             backend
       4. report, history, stats                                          shared
  Intel: VMRESUME | AMD: loop back to VMRUN
```

### The control points: who decides what

| What is controlled | Decided by (shared) | Carried out by (backend) |
|---|---|---|
| Startup order and error handling | `bring_up::run` | each trait method |
| Which exits happen | `ExitBitmaps::allocate(kind)` and `configure_controls` | Intel: bitmaps + VMCS controls; AMD: MSRPM, IOPM, VMCB intercept vectors |
| What each exit means | `decide()` | `read_exit` turns hardware codes into `ExitReason` |
| What happens next | `ExitEvent` -> `ExitAction::Resume(ResumePlan)` | `prepare_resume` turns the plan into VMWRITEs or VMCB writes |
| Guest memory: protect, hide, retype | shared policy: which pages | `GuestMemory`: `grant_write`, `hide`, `apply`, then `invept` |
| What gets printed | `policy`, `report`, `history`, `stats` | `hal/intel/dump.rs` (not in a trait yet) |
| Build-time switches | existing Cargo features (`os-stop-test`, `host-nmi-test`, ...) stay as they are | none |
| Tests | fakes per trait (HAL_PLAN.md Phase 4) | none |

### Structural difference to plan for early

On Intel, a VM exit **jumps** to `HOST_RIP` on a separate stack. That is why
`vmcs/launch.rs` has the setjmp/longjmp-like `launch` and
`vmx_return_to_launcher`. On AMD, `VMRUN` behaves like a **call**: after a
`#VMEXIT` the CPU continues at the next instruction, with host state restored
from the host save area. So the AMD backend is a loop:
`loop { vmrun; handle_exit; }`.

So the HAL boundary must be "handle one exit" (today `vmexit_handler`),
called from either the Intel stub or the AMD loop. It must **not** be "the
whole run loop". Keep `launch.rs` and the asm stub inside `hal/intel/`.

---

## 4. The interface (Phase 3)

The draft code is `src/hal/mod.rs` and `src/hal/types.rs`; every name is a
source name. Details are in HAL_INTERFACE.md.

```rust
// src/hal/mod.rs (signatures only)
pub trait VmxOperation: Sized {                  // struct VmxOperation, vmxon.rs
    type Error;
    fn detect() -> VmxCapabilities;
    unsafe fn enter_vmx_root_operation() -> Result<Self, Self::Error>;
    unsafe fn leave(self) -> Result<(), Self::Error>;
}

pub trait VcpuControl: Sized {                   // name from HAL_PLAN.md
    type Error;
    type GuestMemory: GuestMemory;
    type ExitBitmaps: ExitBitmaps;
    unsafe fn create_current() -> Result<Self, Self::Error>;
    unsafe fn configure_controls(&mut self) -> Result<(), Self::Error>;
    unsafe fn set_ept_pointer(&mut self, ept: &Self::GuestMemory) -> Result<(), Self::Error>;
    unsafe fn write_to_vmcs(&mut self, bitmaps: &Self::ExitBitmaps) -> Result<(), Self::Error>;
    unsafe fn configure_host_state(&mut self, tables: &HostTables, cr4: u64) -> Result<(), Self::Error>;
    unsafe fn configure_host_entry(&mut self, stack_top: u64, entry_address: u64) -> Result<(), Self::Error>;
    unsafe fn configure_cr_shadowing(&mut self) -> Result<(), Self::Error>;
    unsafe fn write_guest_state(&mut self, entry: GuestEntry) -> Result<(), Self::Error>; // name open
    unsafe fn self_test(&mut self) -> Result<(), Self::Error>;
    unsafe fn launch(&mut self);
    unsafe fn launch_in_place(&mut self) -> Result<(), Self::Error>;
    fn read_exit(&mut self) -> ExitInfo;
    fn prepare_resume(&mut self, plan: ResumePlan) -> Result<ResumePlan, Self::Error>;
    unsafe fn set_nmi_window_exiting(&mut self, on: bool) -> Result<(), Self::Error>;
    unsafe fn set_interrupt_window_exiting(&mut self, on: bool) -> Result<(), Self::Error>;
}

pub trait GuestMemory: Sized {                   // name from HAL_PLAN.md
    type Error;
    unsafe fn build(protected_pages: &[u64]) -> Result<Self, Self::Error>;
    unsafe fn grant_write(&self) -> Result<InveptType, Self::Error>;
    unsafe fn apply(&mut self) -> Result<Retyped, Self::Error>;
    unsafe fn hide(&mut self) -> Result<Hidden, Self::Error>;
    fn invept(&self) -> InveptType;
    fn walk(&self, gpa: u64) -> EptWalk;
}

pub trait ExitBitmaps: Sized {                   // struct ExitBitmaps, intercepts.rs
    type Error;
    fn allocate(kind: GuestKind) -> Result<Self, Self::Error>;
}
```

### `ExitReason` variants and the two vendors' numbers

`ExitReason` becomes an enum with one variant per `EXIT_REASON_*` constant.
Intel numbers are from `src/hv/decide/mod_ch.rs`. AMD numbers were checked
against Linux `arch/x86/include/uapi/asm/svm.h`.

| `ExitReason` variant | Intel basic reason | AMD exit code | Note |
|---|---|---|---|
| `ExceptionOrNmi` | 0 | `0x40 + vector` / `0x61` | AMD has one code per vector |
| `InterruptWindow` | 7 | `0x64` VINTR | |
| `NmiWindow` | 8 | none; usually an IRET intercept (`0x74`) | Needs a design in Phase 5 |
| `Cpuid` | 10 | `0x72` | |
| `Hlt` | 12 | `0x78` | |
| `Vmcall` | 18 | `0x81` (VMMCALL) | Confirmed; settles ARCHITECTURE.md's open item |
| `ControlRegister(CrQualification)` | 28 | `0x10` write CR0, `0x14` write CR4 | AMD: one code per CR |
| `IoInstruction(IoQualification)` | 30 | `0x7B` IOIO | Port, size, direction in different bits |
| `Rdmsr` / `Wrmsr` | 31 / 32 | `0x7C` + EXITINFO1 (0 read, 1 write) | Backend splits AMD's one code into two |
| `Mwait` / `Monitor` | 36 / 39 | `0x8B` / `0x8A` | |
| `EptViolation(EptQualification)` | 48 | `0x400` NPF | Qualification bits differ |
| `EptMisconfig` | 49 | none; NPF with reserved-bit error code | |
| `Xsetbv` | 55 | `0x8D` | |
| `EntryFailure(u16)` | bit 31 of exit reason (`entry_failure`) | `-1` (`VMEXIT_INVALID`) | |
| `Unnamed(u16)` | anything else | anything else | keeps the real number, like bhyve's escape hatch |

### `ResumePlan` field to backend action (with KVM's op for the same job)

| `ResumePlan` field | Intel today (handler.rs) | AMD (Phase 5) | KVM op |
|---|---|---|---|
| `advance_rip` | RIP + VM_EXIT_INSTRUCTION_LEN | RIP = VMCB `nRIP` (NRIP_SAVE) | `skip_emulated_instruction` |
| `inject_nmi` | VM-entry interruption info, type NMI | `EVENTINJ`, type NMI | `inject_nmi` |
| `inject_exception` | VM-entry interruption info + error code | `EVENTINJ` + error code | `inject_exception` |
| `nmi_window` | NMI-window exiting control | IRET intercept (no direct equivalent) | `enable_nmi_window` |
| `interrupt_window` | interrupt-window exiting control | V_IRQ + VINTR intercept | `enable_irq_window` |
| `clear/restore_nmi_blocking` | guest interruptibility state | `INT_SHADOW` / NMI-blocking state | `set_nmi_mask`, `get_interrupt_shadow` |
| `grant_write` | EPT entry write bit + INVEPT | NPT entry RW bit + TLB flush (ASID) | (MMU) |
| `write_cr` | GUEST_CRn + read shadow | VMCB save-area CRn (no read shadows: intercept reads instead) | `set_cr0`, `set_cr4` |
| `emulate` | run instruction in root, write frame (`execute`) | same, shared | `handle_exit` |
| `redeliver` | IDT-vectoring info -> entry injection | `EXITINTINFO` -> `EVENTINJ` | (part of `handle_exit`) |

The AMD column is from the AMD APM and KVM's `svm/` and should be checked in
APM Vol. 2 before Phase 5. The Intel column is what handler.rs does today.

**Note on execute-only pages**: EPT can make a page execute-only; NPT
cannot (it uses normal x86 entry bits: present, R/W, NX). Anti-cheat
"split view" ideas often rely on execute-only pages. Shared code must not
assume Intel's set (HAL_INTERFACE.md, open questions).

**Note on memory types**: EPT entries carry a memory type, so
`guest_mtrrs.rs` retypes EPT leaves after a guest MTRR write. NPT entries do
not have an EPT-style type field; the type comes from the PAT bits in the
NPT entry combined with the guest's. `GuestMemory::apply` (today
`TypeTables::apply`) therefore means something different on AMD. Decide its meaning in Phase 3 so it can be done on both.

---

## 5. File-by-file mapping

Columns: **what it does** (plain words), **status**, **goes to**, **what
changes**, **AMD counterpart**, **phase**. Paths in "goes to" are a proposal:

```
src/
  main.rs, output.rs
  bring_up/      shared startup sequence, generic over the traits
  resident/      staying alive after ExitBootServices
  x86/           plain x86-64 that both vendors need
  host/          Aleph0's own CR3/GDT/IDT/TSS and fault handlers
  hv/            shared hypervisor logic: decisions, reports, Hyper-V
  hal/
    mod.rs       the traits (VmxOperation, VcpuControl, GuestMemory, ExitBitmaps)
    types.rs     the types they mention: ExitReason, ExitInfo, the three
                 qualifications, InterruptionInfo, Redelivery, GuestKind,
                 GuestEntry, HostTables, InveptType, EptWalk, ...
    intel/       VMX backend (today's Intel-only code)
    amd/         SVM backend (Phase 5)
```

Dependencies point one way, so there are no cycles:

```
bring_up, hv, host, resident  ──use──▶  hal (traits + types)  ◀──implement──  hal/intel, hal/amd
```

`hal/types.rs` is why `ExitReason`, `ExitInfo` and the qualifications leave
`exit_decision/`: the traits name them, so they must live in `hal`. `decide()`
stays in `hv/` and imports them. Shared code never imports from `hal/intel`
or `hal/amd`; only `main.rs` chooses a backend.

Renaming `src/vmx/` is a large diff. Do it in steps: one folder per PR, with
the tests in `tests/unit/` moved in the same PR (each source file points to
its tests with `#[path = "../../tests/unit/..."]`, so both must move together).

### Top level

| File | What it does | Status | Goes to | What changes | AMD | Phase |
|---|---|---|---|---|---|---|
| `main.rs` | UEFI entry: turns on serial, copies the image into resident memory, calls `bring_up::run`, prints the result, checks the firmware timer still works | Shared | stays | Picks the backend (Intel only for now) and calls `bring_up::run` with it | none | 3 |
| `output.rs` | Where log lines go (serial and console) and the panic handler that prints without firmware | Shared | stays | Panic path prints exit history; history becomes neutral (Phase 3) | none | none |
| `crates/uart/src/{port,serial}.rs` | `in`/`out` instructions and the COM2 16550 serial driver; also used by the tools | Shared | stays (already its own crate) | Nothing. (ARCHITECTURE.md lists them under `src/`; they live in `crates/uart`) | none | none |

### `bring_up/`: the startup sequence

| File | What it does | Status | Goes to | What changes | AMD | Phase |
|---|---|---|---|---|---|---|
| `bring_up/mod.rs` | Runs the 7 steps (section 2): context tables, allocations, VMX check + EPT + bitmaps + host env, VMXON + VMCS, fill VMCS, launch, teardown | Split | `bring_up/mod.rs` | Becomes generic over the traits. Each VMX step becomes the trait method of the same name; the order, the checks and the error handling stay shared | same steps through `Amd` | 3 |
| `bring_up/error.rs` | `BringUpError`: why startup stopped, and how to print it | Split | `bring_up/error.rs` | Keep shared variants; VMX-specific errors (VmxFail, VMCS) come from each trait's `type Error` | SVM errors | 3 |
| `bring_up/report.rs` | Lines that only print state during startup | Split | `bring_up/report.rs` | VMX capability lines move to backend; rest stays | SVM capability lines | 3 |
| `bring_up/teardown.rs` | Hands the CPU back to firmware: restores CR3, GDTR, IDTR, then VMXOFF and CR0/CR4 | Split | `bring_up/teardown.rs` + `hal/intel` | Restoring firmware tables stays shared; "leave VMX" becomes `VmxOperation::leave` | clear EFER.SVME | 3 |

### `resident/`: staying alive after the OS takes over

| File | What it does | Status | Goes to | What changes | AMD | Phase |
|---|---|---|---|---|---|---|
| `resident/memory.rs` | Which UEFI memory types the OS may reclaim | Shared | stays | Nothing | none | none |
| `resident/pe.rs` | Applies PE base relocations when the image is copied to a new address | Shared | stays | Nothing | none | none |
| `resident/mod.rs` | Copies the image to resident memory, watches `ExitBootServices`, and tells Aleph0 about it with one VMCALL | Split | stays | The one `vmcall` needs a trait method (VMCALL / VMMCALL); open in HAL_INTERFACE.md | `vmmcall` | 3 |

### `x86/`: plain x86-64 that both vendors use (today in `src/vmx/`)

| File | What it does | Status | Goes to | What changes | AMD | Phase |
|---|---|---|---|---|---|---|
| `vmx/page.rs` | Allocates zeroed pages in memory types the OS will not reclaim; lists them for the check | Shared | `x86/page.rs` | Move only | none | 2 |
| `vmx/paging.rs` | Reads the current 4-level page tables: is this address mapped, writable, executable? | Shared | `x86/paging.rs` | Move only | none | 2 |
| `vmx/ept/mod.rs` → `level` module | Typed table levels `Level`, `Pml4`, `Pdpt`, `Pd`, `Pt`, `TableLevel` (today inside the EPT file) | Shared | `x86/page_table.rs` | **First PR.** `host/page_tables.rs:28` imports them from EPT; NPT will need them too | NPT reuses them | 2 |
| `vmx/context_tables.rs` | Copies the firmware GDT, adds a TSS, loads both; TSS layout | Shared | `x86/context_tables.rs` | Move only | none | 2 |
| `vmx/mtrr/mod.rs` | Decodes MTRRs into memory types per physical range | Shared | `x86/mtrr/mod.rs` | Move only (AMD has the same MTRRs) | none | 2 |
| `vmx/mtrr/registers.rs` | Which MTRR MSRs exist, which writes are invalid, reading them | Shared | `x86/mtrr/registers.rs` | Move only | none | 2 |
| `vmx/apic.rs` | Local APIC base, xAPIC/x2APIC mode, the register the test guest reads | Split (small) | `x86/apic.rs` | Uses `ept::WalkEnd` for one check; import `WalkEnd` from `hal/types.rs` | none | 3 |
| `vmx/cpuid.rs` | CPUID helpers and feature checks | Split (small) | `x86/cpuid.rs` + `hal/intel` | `is_vmx_supported()` moves to Intel; add vendor string for `detect()` | `is_svm_supported()` | 2 |
| `vmx/msr.rs` | MSR numbers and `rdmsr`/`wrmsr`; EFER, PAT, FS/GS base; also `IA32_VMX_*` capability MSRs | Split | `x86/msr.rs` + `hal/intel/caps.rs` | `IA32_VMX_*`, `ControlCapability`, and the VMCS import go to Intel | `VM_CR`, `VM_HSAVE_PA` | 2 |
| `vmx/cr.rs` | Read/write CR0-CR4, XSETBV; VMX fixed bits and read shadows | Split | `x86/cr.rs` + `hal/intel/cr.rs` | `CrFixed`, CR4.VMXE, `CrControls` (masks/shadows) go to Intel | AMD has no fixed bits or shadows | 2 |
| `vmx/segment.rs` | Reads selectors, bases, limits, access rights of the running CPU | Split | `x86/segment.rs` + `hal/intel` | VMCS access-rights format goes to Intel | VMCB `attrib` format (12-bit packed) | 2 |
| `vmx/features.rs` | What the guest is offered: secondary controls (RDTSCP, INVPCID, XSAVES, ...) and what CPUID hides | Split | `hv/features.rs` + `hal/intel` | "Hide from CPUID" list stays shared; choosing VMX controls goes to Intel | VMCB intercept bits | 3 |

### `host/`: what Aleph0 itself runs on during an exit

| File | What it does | Status | Goes to | What changes | AMD | Phase |
|---|---|---|---|---|---|---|
| `vmx/host/mod.rs` | `HostEnvironment`: builds Aleph0's own CR3, GDT, TSS, IDT; returns them as `HostSelectors`/`HostTables` for the VMCS host fields | Split | `host/mod.rs` | Building stays shared; writing them into HOST_* fields is `VcpuControl::configure_host_state` / `configure_host_entry` | Loaded before VMRUN; VMRUN restores them via host save area | 3 |
| `vmx/host/gdt.rs` | Aleph0's GDT, descriptors at the firmware's selector indices | Shared | `host/gdt.rs` | Drop the VMCS import (only a type) | none | 2 |
| `vmx/host/idt.rs` | Aleph0's 256-gate IDT and which stack each vector uses | Shared | `host/idt.rs` | Move only | none | 2 |
| `vmx/host/handlers.rs` | NMI handler (marks NMI pending for the guest) and fault handler (prints, halts, or recovers a refused Hyper-V forward) | Split | `host/handlers.rs` | NMI handler writes the VMCS (NMI-window); calls `VcpuControl::set_nmi_window_exiting` | On AMD with GIF=0 NMIs are held by the CPU; handler may matter less | 3 |
| `vmx/host/page_tables.rs` | Aleph0's identity-mapped page tables loaded on every exit | Shared after PR 1 | `host/page_tables.rs` | Import level types from `x86/page_table.rs` | Same tables; NPT could share this code | 2 |

### `hv/`: shared hypervisor logic (decisions, reports, protocols)

| File | What it does | Status | Goes to | What changes | AMD | Phase |
|---|---|---|---|---|---|---|
| `vmx/exit_decision/mod.rs` | `ExitInfo` (the exit as read), `ExitState` (kept between exits), `ExitEvent`, `ResumePlan`, `decide()` | Split | `hal/types.rs` (`ExitReason`, `ExitInfo`, `EptQualification`, `InterruptionInfo`, `Redelivery`) + `hv/decide/mod.rs` (`decide`, `ExitState`, `ExitEvent`, `ResumePlan`) + `hal/intel/exit.rs` (reason numbers, qualification bits, VMCS→`ExitReason`) | **The main design task.** `decide` matches on `ExitReason` variants instead of `EXIT_REASON_*`. `ResumePlan` keeps its fields and stays in `hv/decide` | `hal/amd/exit.rs` builds the same `ExitReason` from VMCB | 3 |
| `vmx/exit_decision/emulation.rs` | CPUID/XSETBV/MSR/IO/HLT decisions: passthrough plus policy (hide features, guest MTRR copy) | Shared | `hv/decide/emulation.rs` | Reads `IoQualification` decoded (fields instead of bit methods) | none | 3 |
| `vmx/exit_decision/control_register.rs` | MOV to CR0/CR4: emulate, inject #GP, or give up | Shared | `hv/decide/control_register.rs` | Takes `CrQualification` decoded (fields instead of bit methods) | AMD fills `CrQualification` from decode assist | 3 |
| `vmx/exit_decision/scenario.rs` | The test guest's own exits (protected-page write, VMCALLs, NMI reinjection) | Shared | `hv/decide/scenario.rs` | Uses `ExitInfo` / `ExitReason` from `hal/types.rs` | none | 3 |
| `vmx/vmexit/handler.rs` | Per exit: read, judge, carry out, report | Split | `hv/handler.rs` + `hal/intel/vcpu.rs` | Flow (judge, NMI fold-in, plan adjustments) stays shared in `vmexit_handler`; `read_exit` and `prepare_resume` VMREAD/VMWRITE code become `VcpuControl::read_exit` / `VcpuControl::prepare_resume` | `hal/amd/vcpu.rs` | 3 |
| `vmx/vmexit/report.rs` | Prints what an exit was and what was done | Split | `hv/report.rs` | Raw VMCS / EPTP lines stay in `hal/intel/dump.rs` (not in a trait yet) | VMCB lines | 3 |
| `vmx/vmexit/history.rs` | Ring buffer of the last exits, printed at a stop or panic | Shared after `ExitReason` | `hv/history.rs` | Stores the `ExitReason` enum instead of Intel numbers | none | 3 |
| `vmx/vmexit/stats.rs` | Counters per vCPU (what Aleph0 did), and Hyper-V forwards | Shared after `ExitReason` | `hv/stats.rs` | Same as history | none | 3 |
| `vmx/vmexit/policy.rs` | How much of each exit to print (rate limits, histogram) | Shared after `ExitReason` | `hv/policy.rs` | Keys on the `ExitReason` variant | none | 3 |
| `vmx/vmexit/guest_mtrrs.rs` | The guest's private MTRR copy, and re-typing EPT after the guest writes it | Split | `hv/guest_mtrrs.rs` | Copy stays; "retype EPT + INVEPT" becomes `GuestMemory::apply` + `invept` | NPT PAT bits (see note in section 4) | 3 |
| `vmx/hyperv.rs` | Forwards the guest's Hyper-V hypercalls and synthetic MSRs to the real Hyper-V | Split (one instruction) | `hv/hyperv.rs` | `vmcall` needs a trait method; open in HAL_INTERFACE.md. Same idea as Xen's `init_hypercall_page` | `vmmcall` | 3 |
| `vmx/vmcall.rs` | Aleph0's own hypercall numbers (constants only) | Shared | `hv/vmcall.rs` (rename `hypercall.rs` later) | Move only | same numbers | 2 |
| `vmx/guest.rs` | The built-in test guest: its code, data, stack, IDT pages and expected stats | Split | `hv/test_guest.rs` | Its asm uses `vmcall`; needs a VMMCALL variant or a patched byte | `vmmcall` | 3/5 |

### `hal/`: the interface (new files)

| File | What it holds | Cut out of | Phase |
|---|---|---|---|
| `hal/mod.rs` | Traits `VmxOperation`, `VcpuControl`, `GuestMemory`, `ExitBitmaps` | new (shaped by `vmexit/handler.rs`, `ept/mod.rs`, `exit_bitmaps.rs`) | 3 |
| `hal/types.rs` | `ExitReason`, `ExitInfo`, `IoQualification`, `CrQualification`, `EptQualification`, `InterruptionInfo`, `Redelivery`, `GuestKind`, `GuestEntry`, `HostTables`, `GuestFeatures`, `InveptType`, `EptWalk`, `Retyped`, `Hidden`, `VmxCapabilities` | `exit_decision/mod.rs`, `vmexit/mod.rs`, `ept/mod.rs`, `guest_state.rs` | 3 |
| `hal/fake.rs` (test only) | A fake per trait (HAL_PLAN.md Phase 4): returns chosen `ExitInfo`s, records `prepare_resume` calls | new | 4 |

### `hal/intel/`: the VMX backend (moves, mostly unchanged)

| File | What it does | Goes to | What changes | AMD counterpart (`hal/amd/`) | Phase |
|---|---|---|---|---|---|
| `vmx/mod.rs` | `detect()`: CPUID VMX bit + IA32_FEATURE_CONTROL lock/enable | `hal/intel/mod.rs` | Implements `VmxOperation::detect` | AMD `detect`: CPUID 0x8000_0001 ECX.SVM + VM_CR.SVMDIS | 2-3 |
| `vmx/vmxon.rs` | Sets CR4.VMXE and fixed bits, VMXON; undo with VMXOFF | `hal/intel/vmxon.rs` | Implements `VmxOperation::enter_vmx_root_operation` / `leave` | `svm.rs`: EFER.SVME, VM_HSAVE_PA | 2-3 |
| `vmx/region.rs` | The 4 KiB revision-stamped VMXON/VMCS regions | `hal/intel/region.rs` | Move | `vmcb.rs` (VMCB page + host save page) | 2 |
| `vmx/instruction.rs` | `VmxFail` from RFLAGS after a VMX instruction, `vmx_asm!` | `hal/intel/instruction.rs` | Move | none (SVM instructions do not fail this way) | 2 |
| `vmx/vmcs/mod.rs` | VMCLEAR/VMPTRLD/VMREAD/VMWRITE, controls setup, small setters | `hal/intel/vmcs/mod.rs` | Move | `vmcb.rs`: a `#[repr(C)]` struct, written as memory | 2 |
| `vmx/vmcs/fields.rs` | Field encodings and control bit names | `hal/intel/vmcs/fields.rs` | Move | VMCB offsets and intercept bits | 2 |
| `vmx/vmcs/launch.rs` | VMLAUNCH with setjmp/longjmp-style return when the guest stops | `hal/intel/launch.rs` | Move; implements `VcpuControl::launch` / `launch_in_place` | `run.rs`: `loop { vmrun; handle_exit }` | 2-3 |
| `vmx/vmexit/mod.rs` | Exit stack + asm stub that saves guest registers, calls the handler, VMRESUMEs | `hal/intel/entry.rs` | Calls `vmexit_handler` (shared) | asm around VMRUN: VMLOAD/VMSAVE, guest RAX lives in VMCB | 3 |
| `vmx/vmexit/dump.rs` | Prints guest state and the VMCS fields that explain a stop | `hal/intel/dump.rs` | Stays Intel-only (not in a trait yet) | Prints VMCB fields | 3 |
| `vmx/guest_state.rs` | Fills VMCS guest-state fields from the running CPU (for launch in place) | `hal/intel/guest_state.rs` | Implements `VcpuControl::write_guest_state` | Fill VMCB save area | 2-3 |
| `vmx/exit_bitmaps.rs` | MSR bitmap (4 KiB) and I/O bitmaps A/B: which MSRs/ports exit | `hal/intel/intercepts.rs` | Implements `ExitBitmaps` | MSRPM (8 KiB) + IOPM (12 KiB), different layout | 3 |
| `vmx/ept/mod.rs` | Builds the EPT identity map with memory types, read-only page, hidden ranges, spare tables | `hal/intel/ept/mod.rs` | Implements `GuestMemory` (level types leave in PR 1) | `npt.rs`: normal page-table entries, can reuse `host/page_tables.rs` code | 2-3 |
| `vmx/ept/walk.rs` | Re-walks EPT like the CPU to explain a violation or misconfig | `hal/intel/ept/walk.rs` | Implements `GuestMemory::walk` | NPT walk | 2-3 |
| (new) | VMCS → `ExitReason` + qualification decoding, and `EXIT_REASON_*` numbers | `hal/intel/exit.rs` | Cut out of `exit_decision/mod.rs` | `hal/amd/exit.rs`: VMCB EXITCODE/EXITINFO1/2 → `ExitReason` | 3 |
| (new) | `CrFixed`, `IA32_VMX_*`, `ControlCapability` | `hal/intel/caps.rs` | Cut out of `cr.rs` / `msr.rs` | `caps.rs`: CPUID 0x8000_000A (NRIP save, NPT, decode assists) | 2 |

### Tools and scripts

| Path | What it does | Status | What changes |
|---|---|---|---|
| `tools/ebs-harness` | Test app that overwrites reclaimable memory after `ExitBootServices` to prove Aleph0 does not depend on it; includes `resident/memory.rs` and `vmcall.rs` by path | Shared | Update `#[path]` includes when those files move |
| `tools/os-loader` | Loads an OS kernel for the guest tests | Shared | Check its includes too |
| `scripts/run*.ps1`, `read-serial.ps1` | Build and run on Hyper-V (Intel) | Intel run | Phase 5 adds a QEMU script: `-cpu max,svm=on` with OVMF |
| `scripts/linux/*` | Builds the Linux guest | Shared | Nothing |
| `.github/workflows/ci.yml` | CI | Shared | Add a build that compiles the AMD backend (and later a fake-backend test job) |

---

## 6. Order of PRs (each one: builds, 535 tests pass, Intel unchanged)

| # | PR | Files | Risk |
|---|---|---|---|
| 1 | Move page-table level types out of EPT | `ept/mod.rs`, `host/page_tables.rs`, new `x86/page_table.rs` | Low |
| 2 | Move plain x86 files to `x86/` | page, paging, context_tables, mtrr/ | Low (paths only) |
| 3 | Split `cpuid`, `msr`, `cr`, `segment` | Intel parts to `hal/intel/` | Low-medium |
| 4 | Move VMX-only files to `hal/intel/` | vmxon, region, instruction, vmcs/, ept/, guest_state, exit_bitmaps, dump | Low (paths only), big diff |
| 5 | A trait method for VMCALL (name open) | hyperv.rs, resident/mod.rs | Low |
| 6 | `ExitReason` enum + decoded qualifications + `ExitInfo`, Intel decoding in `hal/intel/exit.rs` | exit_decision/*, history, stats, policy | **High**: the core design |
| 7 | `VcpuControl`: `read_exit` / `prepare_resume` | handler.rs, report.rs | Medium; needs a real Hyper-V run |
| 8 | `GuestMemory` + `ExitBitmaps` traits | ept, guest_mtrrs, exit_bitmaps, apic | Medium |
| 9 | `VmxOperation` + `VcpuControl` setup methods, `bring_up::run` generic | bring_up/*, main.rs, host/ | Medium; needs a real Hyper-V run |
| 10 | Fakes per trait and tests for shared code | tests/ | Low |
| 11+ | `hal/amd/` (Phase 5) | new files | Shows which traits were shaped too much like Intel |

PRs 1-5 change no behavior and can be checked with unit tests alone. From PR
6 on, run the Hyper-V scripts before merging (HAL_PLAN.md, risks).
