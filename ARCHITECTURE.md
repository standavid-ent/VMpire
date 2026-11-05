# Aleph0: what is Intel-only and what is shared

Phase 1 of the HAL plan (see HAL_PLAN.md). Written 2026-11-03 from a read of
`src/` in Aleph0-Virtualization-Hypervisor (commit eb0b525, master).

Method: for every file, its `use` lines and definitions were read. Keyword
counts alone were misleading ("EPT" also matches "exception"), so imports are
the main evidence. The "Mixed" files were not read line by line yet; each one
needs a closer look before it is split.

Key fact: AMD is also x86-64. A lot of the old `src/vmx/` was plain x86 code (page
tables, GDT/IDT, MTRRs, MSRs) that an AMD backend needs as well. Only the
virtualization-specific part is Intel-only.

## A. Neutral: move to shared code as they are

| File | What it does |
|---|---|
| `main.rs` | UEFI entry point |
| `output.rs`, `serial.rs`, `port.rs` | Logging, serial port (COM2), port I/O |
| `resident/memory.rs`, `resident/pe.rs` | Which memory types the OS reclaims; PE image relocation |
| `x86/page.rs` | Page allocation in resident memory types |
| `x86/paging.rs` | Walks ordinary x86 4-level page tables |
| `x86/mtrr/mod.rs`, `registers.rs` | MTRR decoding and the guest's MTRR copy (AMD has MTRRs too) |
| `x86/context_tables.rs` | GDT copy and TSS layout |
| `host/idt.rs` | Aleph0's own interrupt table |
| `hv/vmcall.rs` | Aleph0's own hypercall numbers (only constants) |

## B. Intel-only: move to `hal/intel/`

| File | What it does | AMD counterpart |
|---|---|---|
| `hal/intel/vmcs/mod.rs`, `fields.rs`, `launch.rs` | VMCS lifecycle, field encodings, VMLAUNCH | VMCB layout, VMRUN |
| `hal/intel/vmxon.rs`, `region.rs` | VMXON/VMXOFF and the revision-stamped page | Set EFER.SVME, `VM_HSAVE_PA` host save area |
| `hal/intel/instruction.rs` | VMX failure flags (`VmxFail`), `vmx_asm!` | None (SVM has no such flags) |
| `hal/intel/ept/mod.rs`, `walk.rs` | EPT tables (the biggest file, ~1,900 lines) | NPT tables |
| `hal/intel/guest_state.rs` | Fills the VMCS guest-state fields | Fills the VMCB save area |
| `hal/intel/intercepts.rs` | Intel MSR and I/O bitmaps | MSRPM / IOPM (different layout) |
| `hal/intel/entry.rs` | Assembly exit stub, VMRESUME path | `vmrun` loop, GIF handling |
| `hal/intel/dump.rs` | Prints VMCS fields at a stop | Prints VMCB fields |

## C. Mixed: split before moving

| File | Neutral part | Intel part |
|---|---|---|
| `hv/decide/mod.rs` | `decide()`, `ExitEvent`, `ResumePlan` | `ExitReason` numbers (lines ~109-146), `ExitInfo` built from VMCS fields, EPT qualification bits |
| `hv/decide/emulation.rs`, `control_register.rs`, `scenario.rs` | Nearly all of it (no VMCS imports) | They consume the Intel-shaped `ExitInfo` |
| `hv/handler.rs` | Flow: read, decide, act, report | VMCS reads/writes, INVEPT |
| `hv/report.rs` | Report text | EPT pointer, VMCS fields |
| `hv/guest_mtrrs.rs` | The guest's MTRR copy | EPT retype and INVEPT |
| `hv/history.rs`, `stats.rs`, `policy.rs` | All logic | They store Intel exit-reason numbers; neutral once exit types are |
| `x86/msr.rs` | `rdmsr`/`wrmsr`, EFER, PAT, FS/GS base | `IA32_VMX_*`, `ControlCapability`, imports VMCS fields |
| `x86/cr.rs` | CR0-CR4 access, XSETBV | CR4.VMXE, `CrFixed` (VMX fixed bits), read shadows |
| `hv/features.rs` | CPUID/XSAVE checks | VMX secondary controls |
| `x86/cpuid.rs` | Everything else | `is_vmx_supported()` |
| `x86/segment.rs` | Descriptor encoding | VMCS access-rights format |
| `x86/apic.rs` | Local APIC | Uses `ept::WalkEnd` |
| `host/mod.rs`, `gdt.rs`, `handlers.rs`, `page_tables.rs` | Aleph0's own host tables and fault handlers | Return VMCS-shaped types (`HostSelectors`, `HostTables`); NMI handler writes the VMCS; page tables reuse EPT level types |
| `hv/hyperv.rs` | The Hyper-V protocol | Forwarding uses `vmcall` (AMD: `vmmcall`) |
| `resident/mod.rs` | Staying resident | One VMCALL to say boot services ended |
| `hv/test_guest.rs` | Test guest's checks | VMCALL inside guest code |
| `bring_up/mod.rs`, `error.rs`, `report.rs`, `teardown.rs` | The startup order | Every step is a VMX step |

## Findings worth acting on first

1. **Page-table level types live inside EPT.** `host/page_tables.rs` borrows
   `Level`, `Pml4`, `Pdpt`, `Pd`, `TableLevel` from `ept/mod.rs`. Today it
   gets them through `hal::types`; moving them to a shared paging module is
   still open.
2. **`ExitInfo` is the center of the split.** Its fields are Intel concepts
   (interruption info, interruptibility state, IDT vectoring, exit
   qualification). Designing a neutral version is the main design task.
3. **`hv/decide/` is already well separated.** It never touches the VMCS;
   only the data passed into it is Intel-shaped.

## Intel to AMD translation table

Addresses, registers and numbers the HAL has to translate.

| Concept | Intel (VMX) | AMD (SVM) | Checked |
|---|---|---|---|
| Control structure | VMCS, fields by numeric encoding | VMCB, fields by byte offset | concept |
| Enable | CR4.VMXE + VMXON | EFER.SVME + `VM_HSAVE_PA` | from memory |
| Capability check | `IA32_FEATURE_CONTROL`, `IA32_VMX_*` MSRs | `VM_CR` (SVMDIS), CPUID | from memory |
| Start guest | VMLAUNCH, then VMRESUME | VMRUN | concept |
| Guest memory | EPT | NPT (nested paging) | concept |
| CPUID exit | reason 10 | `0x72` | yes |
| HLT exit | reason 12 | `0x78` | yes |
| I/O exit | reason 30 | `0x7B` (IOIO) | yes |
| MSR exit | reasons 31 (RDMSR) and 32 (WRMSR) | `0x7C` (one code; ExitInfo1 says read or write) | yes |
| Guest memory fault | EPT violation, reason 48 | Nested page fault `0x400` | yes |
| Hypercall | VMCALL, reason 18 | VMMCALL, `0x81` | NOT confirmed; check AMD APM Vol. 2 Appendix C |
| Intercept maps | MSR bitmap + I/O bitmaps A/B | MSRPM + IOPM | concept |
| Page-table entry bits | EPT R/W/X + memory type | Normal x86 bits | concept |

Note the structural difference: Intel splits RDMSR/WRMSR into two exit reasons,
AMD uses one code plus a flag. A neutral exit enum must hide this.

## Where Aleph0's Intel exit reasons are defined

`src/hv/decide/mod.rs` lines ~109-146: `EXIT_REASON_*` constants
(exception/NMI 0, interrupt window 7, NMI window 8, CPUID 10, HLT 12,
VMCALL 18, control register 28, I/O 30, RDMSR 31, WRMSR 32, MWAIT 36,
MONITOR 39, EPT violation 48, EPT misconfig 49, XSETBV 55), and the EPT
qualification bits at ~151-171.
