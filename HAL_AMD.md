# AMD backend (SVM): what to build, and what does not fit yet

Nothing is written for AMD. This is the plan, to review before any code.

Rules (same as the Intel redo):
- Trait, type and member names are the ones in `src/hal/mod.rs` and
  `src/hal/types.rs`. Nothing new is invented here. A new trait member appears
  only in section 5, as a proposal for you to accept or rename.
- AMD names are AMD's own (APM Vol. 2: VMCB, VMRUN, EXITCODE, EXITINFO1/2,
  NPT, TLB_CONTROL, GIF).
- Where I am not sure of an AMD detail it says **verify** with the APM place to
  read. Nothing marked verify should be coded before it is read.
- Intel stays as it is. AMD adds files; shared code (`bring_up`, `hv`, `host`,
  `resident`, `x86`) does not change unless section 5 says so.

Status: the interface is in place and the Intel backend is behind it. The Intel
unit tests pass (run by you on x86). Not run yet: the Hyper-V VM (default and
`-TestGuest`). A mistake found there can still change the interface, and then
this file changes too.

---

## 1. Layout

Cargo: add `amd = []` next to `intel = []`; exactly one of them is on.

`src/hal/mod.rs` (the only change in shared files):

```rust
#[cfg(feature = "intel")] pub mod intel;
#[cfg(feature = "amd")]   pub mod amd;

#[cfg(feature = "intel")] pub use intel as backend;
#[cfg(feature = "amd")]   pub use amd as backend;

#[cfg(not(any(feature = "intel", feature = "amd")))]
compile_error!("no hardware backend selected: build with `intel` or `amd`");
#[cfg(all(feature = "intel", feature = "amd"))]
compile_error!("`intel` and `amd` are mutually exclusive");
```

`src/hal/amd/`, file for file like `src/hal/intel/`:

| Intel file | AMD file | Holds |
|---|---|---|
| `mod.rs` | `mod.rs` | one struct per trait, re-exported |
| `virtualization.rs`, `vmxon.rs`, `msr.rs` | `virtualization.rs`, `msr.rs` | detect, EFER.SVME, VM_HSAVE_PA, SVM MSR numbers |
| `vcpu.rs`, `region.rs`, `guest_state.rs` | `vcpu.rs`, `vmcb/` | VMCB allocation, controls, guest/host state |
| `vmcs/{fields,launch,mod}.rs` | `vmcb/{fields,run,mod}.rs` | VMCB offsets, the VMRUN loop (assembly) |
| `vm_exit.rs`, `vmexit/{decode,dump,mod}.rs` | `vm_exit.rs`, `vmexit/{decode,dump,mod}.rs` | readers, EXITCODE constants, `Record`, reports |
| `nested_paging.rs`, `ept/{mod,walk}.rs` | `nested_paging.rs`, `npt/{mod,walk}.rs` | NPT build, hide, walk |
| `intercepts.rs`, `exit_bitmaps.rs` | `intercepts.rs`, `msrpm_iopm.rs` | MSRPM / IOPM |
| `hypercall.rs` | `hypercall.rs` | VMMCALL stub |

The test guest in `hv/guest.rs` gets a second `global_asm!` under
`#[cfg(feature = "amd")]` with `vmmcall` instead of `vmcall`.

Unit tests: `tests/unit/hal/amd/{vm_exit,virtualization,msr}.rs`, the same
shape as the Intel ones (raw EXITCODE / EXITINFO values in, neutral types out).

---

## 2. VMX against SVM, in one table

| | Intel VMX | AMD SVM |
|---|---|---|
| Control structure | VMCS: opaque, VMREAD/VMWRITE | VMCB: a 4 KiB page of plain memory, control area 0x000, state save area 0x400 |
| Enter root mode | CR4.VMXE, VMXON region | EFER.SVME, VM_HSAVE_PA (host save area) |
| Enter the guest | VMLAUNCH / VMRESUME, exit jumps to HOST_RIP | VMRUN; an exit **returns** to the next instruction, no exit entry |
| Registers at exit | the stub pushes all 15 | VMRUN saves only RAX, RSP, RIP in the VMCB; the loop saves the rest |
| Extra state | in the VMCS | VMLOAD / VMSAVE (FS, GS, TR, LDTR, KernelGsBase, STAR, LSTAR, CSTAR, SFMASK, SYSENTER_*) |
| Exit reason | VM_EXIT_REASON + qualification | EXITCODE + EXITINFO1/2 + EXITINTINFO |
| Entry failure | VM-entry failure flag in the reason | EXITCODE = VMEXIT_INVALID (-1) |
| Nested paging | EPT, own entry format, EPTP | NPT, the normal long-mode table format, N_CR3 |
| TLB flush | INVEPT, immediate | TLB_CONTROL / ASID, applied at the next VMRUN |
| Interrupt shadow, NMI blocking | GUEST_INTERRUPTIBILITY_STATE (STI, MOV SS, NMI, ...) | one INTERRUPT_SHADOW bit; no NMI-blocked bit |
| Event injection | VM_ENTRY_INTERRUPTION_INFO | EVENTINJ |
| Hypercall | VMCALL | VMMCALL |
| Interrupts during root mode | RFLAGS.IF | GIF: CLGI before VMRUN, STGI after the exit |

---

## 3. Trait by trait

`HardwareVirtualization`

| Member | AMD |
|---|---|
| `NAME` | `"SVM"` |
| `CPUID_FEATURE` | leaf 0x8000_0001, ECX bit 2 (SVM) |
| `GUEST_FORBIDDEN_CR4` | 0: SVME is in EFER, not CR4 (see 5.6) |
| `ENABLE_BIT` | `"EFER.SVME"` |
| `type ControlCapabilities` | CPUID 0x8000_000A: EAX revision, EBX NASID, EDX feature bits (NP, NRIPS, DecodeAssists, FlushByAsid, VmcbClean, ...) |
| `type Error` | enabling can fail only by firmware / CPU state: its own small enum |
| `detect()` | `NotSupportedByCpu` without the CPUID bit; `DisabledByFirmware` if VM_CR (MSR 0xC001_0114) SVMDIS is set; `Supported` otherwise. **verify** the SVM-lock case (CPUID 0x8000_000A EDX bit 2), APM 15.30 |
| `control_capabilities()` | read CPUID 0x8000_000A |
| `report_control_capabilities` | serial lines in the Intel report's style |
| `optional_features` | `rdtscp`, `invpcid`, `xsaves`: intercept bits exist, so offered when the CPU has the instruction; `waitpkg`: false (Intel only) |
| `report_guest_features` | same line as Intel |
| `cr_constraints()` | no fixed bits: `CrConstraints` with must-be-one 0 and may-be-one all ones (SVM does not constrain CR0/CR4 in root mode) |
| `enable()` | set EFER.SVME (MSR 0xC000_0080 bit 12), write VM_HSAVE_PA (MSR 0xC001_0117) with a zeroed 4 KiB page |
| `disable(self)` | clear EFER.SVME |
| `original_cr0/4` | the values read in `enable` (AMD changes neither) |
| `enabled()` | EFER.SVME |
| `report_error` | prints the enum |

`Vcpu`

| Member | AMD |
|---|---|
| `allocate()` | a 4 KiB VMCB, the host save area, and the hypervisor stack of the run loop (see 5.3) |
| `create(&mut self)` | zero the VMCB; nothing to make current |
| `release(&self)` | nothing |
| `self_test(&self)` | write and read back a VMCB field (it is memory, so this is a sanity check only) |
| `configure_controls(..)` | intercept vectors at 0x000–0x00F, ASID (non-zero; 0 is the host), TLB_CONTROL, NP_ENABLE. **VMRUN intercept must be set** or VMRUN fails. Which intercepts match `ExitReason`: section 4 |
| `set_nested_paging(..)` | NP_ENABLE (control 0x090), N_CR3 (0x098); gPAT in the save area |
| `set_intercepts(..)` | MSRPM_BASE_PA, IOPM_BASE_PA |
| `configure_host_state(..)` | host state goes to the host save area by VMRUN; VMLOAD/VMSAVE page for the extra state; `HostTables` loaded by the run loop |
| `configure_exit_entry()` | nothing: VMRUN returns by itself |
| `configure_cr_intercepts(..)` | CR0/CR4 read and write intercepts, since SVM has no mask / read-shadow fields (the shadow is kept by the backend, 5.5) |
| `set_guest_state(entry)` | the VMCB save area: segments, CR0/3/4, EFER (SVME forced, 5.6), RFLAGS, RIP, RSP, RAX, IDTR/GDTR |
| `run()` | the VMRUN loop: CLGI, VMLOAD, VMRUN, VMSAVE, STGI, save registers, call the handler, repeat |
| `run_in_place()` | see 5.3 |
| `exit_stack_usage()` | `Some(..)` of the hypervisor stack, if 5.3 is accepted |
| `report_error` | prints the enum |

`VmExit` readers, from the VMCB control area after VMRUN returns:

| Member | AMD |
|---|---|
| `reason()` | EXITCODE (0x070) → `ExitReason`, table in section 4 |
| `reason_code()` | EXITCODE truncated to `u32` (-1 = 0xFFFF_FFFF) |
| `exception_or_nmi()` | EXITCODE 0x40+v (exception v), 0x61 (NMI); EXITINFO1 = error code |
| `interrupted_event()` | EXITINTINFO (0x088), valid bit 31, error code at 0x08C |
| `interruptibility()` | INTERRUPT_SHADOW (0x068) + the backend's NMI flag (5.1) |
| `cr_access()` | EXITCODE 0x00–0x0F read CRn / 0x10–0x1F write CRn; with decode assists EXITINFO1 bit 63 valid and bits 3:0 the GPR. CLTS / LMSW **verify** (APM 15.8, 15.33) |
| `io_access()` | EXITINFO1: bit 0 TYPE (1 = IN), 2 STR, 3 REP, 4/5/6 SZ8/SZ16/SZ32, 31:16 port |
| `nested_page_fault()` | EXITINFO2 = GPA; EXITINFO1 bit 0 P, 1 W, 2 U, 3 RSV, 4 ID (fetch), 32 final translation, 33 guest page-table access |
| `faulted_nmi_unblocking_iret()` | `false` |
| `guest_cpl()` | save area CPL byte (0x4CB, **verify**) |
| `guest_rip()` / `guest_rsp()` | save area RIP 0x578 / RSP 0x5D8 (**verify** offsets against APM Appendix B) |
| `instruction_length()` | next RIP (nRIP, control 0x0C8) minus RIP; needs NRIPS (5.8) |
| `guest_cr4()` | save area CR4 |
| `record()` / `unreadable()` | EXITCODE + EXITINFO1/2 in `Record`; `unreadable()` is always false (memory) |

`VmExit` changes:

| Member | AMD |
|---|---|
| `set_rip` | save area RIP |
| `set_interruptibility` | INTERRUPT_SHADOW and the backend's NMI flag |
| `inject_event` | EVENTINJ (0x0A8): vector 7:0, type 10:8, error-code-valid 11, valid 31, error code 63:32 |
| `set_nmi_window(on)` | intercept IRET (the NMI is unblocked when the guest executes it) |
| `set_interrupt_window(on)` | V_IRQ with a VINTR intercept |
| `write_cr` | save area CRn gets `real`; the backend keeps `requested` as what MOV-from-CR returns |

`VmExit` reports and consts: `HYPERCALL` = `"VMMCALL"`; `report_read`,
`dump`, `describe_reason`, `encode_event`, `encode_interruptibility` print
the AMD values in the Intel lines' style; `EMPTY_RECORD`; `REASON_COUNTERS`
(5.7).

`NestedPaging`

| Member | AMD |
|---|---|
| `NAME` / `FLUSH` | `"NPT"` / `"TLB_CONTROL"` |
| `check_support` | CPUID 0x8000_000A EDX bit 0 (NP) and the physical address width |
| `physical_address_bits` | CPUID 0x8000_0008 EAX[7:0] |
| `spare_tables`, `apply_memory_types`, `Retyped` | depends on 5.4 |
| `build` | identity map in the ordinary long-mode format (4 levels; 4 KiB / 2 MiB / 1 GiB); protected pages R/W = 0; **verify** that U/S must be 1 in every NPT entry |
| `hide` | same idea as EPT: redirect to the shadow page, protect the code pages |
| `flush_type` | `SingleContext` = TLB_CONTROL flush this ASID; `AllContexts` = flush all |
| `walk` / `report_walk` / `report_regions` / `fmt_walk_end` | walk the long-mode format; permissions come from P, R/W, NX; the memory type is from 5.4 |
| `misconfigure` | set a reserved bit in the entry (test only) |
| `protected_page_for`, `protected_page_gpa`, `nmi_stack_page`, `grant_write` | the same logic: R/W = 1, then the flush |

`Intercepts`

| Member | AMD |
|---|---|
| `allocate(kind)` | MSRPM (8 KiB, 2 bits per MSR, three MSR ranges) and IOPM (12 KiB, 1 bit per port); MTRR MSRs and the shared `hv::intercepted` lists, plus what the backend needs (5.6) |
| `covers_msr(msr)` | whether the MSR lies in the three MSRPM ranges (0–0x1FFF, 0xC000_0000–0xC000_1FFF, 0xC001_0000–0xC001_1FFF); outside them every access exits |
| `report`, `report_error` | serial lines |

`Hypercall`

| Member | AMD |
|---|---|
| `call(rcx)` | `vmmcall` |
| `forward(registers)` | the Hyper-V hypercall instruction on AMD is `vmmcall`; the stub is the Intel one with that instruction. **verify** how Hyper-V refuses it |
| `recovery_point()` | the labels of that stub |

---

## 4. Neutral types on AMD

`ExitReason` from EXITCODE (APM Appendix C):

| `ExitReason` | EXITCODE | Intercept to set |
|---|---|---|
| `ExceptionOrNmi` | 0x40–0x5F, 0x61 | exception bitmap for the vectors Aleph0 handles, NMI |
| `InterruptWindow` | 0x64 VINTR | VINTR |
| `NmiWindow` | 0x74 IRET | IRET |
| `Cpuid` | 0x72 | CPUID |
| `Hlt` | 0x78 | HLT |
| `Hypercall` | 0x81 VMMCALL | VMMCALL |
| `ControlRegisterAccess` | 0x00–0x1F | CR0/CR4 read and write |
| `IoInstruction` | 0x7B IOIO | IOPM |
| `Rdmsr` / `Wrmsr` | 0x7C MSR; EXITINFO1 bit 0 says read (0) or write (1) | MSRPM |
| `Mwait` / `Monitor` | 0x8B / 0x8A | MWAIT, MONITOR |
| `NestedPageFault` | 0x400 NPF | NP_ENABLE |
| `NestedPagingMisconfig` | 0x400 with EXITINFO1 bit 3 (RSV) | — |
| `Xsetbv` | 0x8D | XSETBV |
| `EntryFailure(code)` | -1 VMEXIT_INVALID | — |
| `Other(code)` | anything else | — |

`EventKind` ↔ EVENTINJ / EXITINTINFO type (bits 10:8): 0 external interrupt,
2 NMI, 3 hardware exception, 4 software interrupt. AMD has no separate
"privileged software exception" and "software exception" kinds, so those
two `EventKind`s are encoded as 3 (hardware exception) or 4 (software
interrupt) when injected — **verify** what Aleph0 relies on (5.9). `Other(u8)`
keeps the raw type.

`Interruptibility`: `sti_shadow` and `mov_ss_shadow` both come from the one
INTERRUPT_SHADOW bit (5.2); `nmi_blocked` from the backend's flag (5.1);
`other` is 0.

`CrAccess`, `IoAccess`, `NestedPageFault`, `GuestRegisters`, `GuestKind`,
`GuestEntry`, `HostTables`, `CrConstraints`, `OptionalFeatures`,
`CpuidBit`, `HypercallRegisters`, `Refused`, `RecoveryPoint`: unchanged;
only the decoders and the code behind them differ.

---

## 5. Where the Intel-shaped interface does not fit

Each is a decision for you. My proposal is the first line; none is coded.

- [ ] **5.1 NMI blocking is not in the VMCB.** Intel reports "blocked by NMI".
  AMD does not. Proposal: the backend keeps a flag. Set when it injects an NMI
  or sees an NMI exit; cleared at the IRET intercept (`set_nmi_window`).
  `interruptibility()` returns it as `nmi_blocked`. No trait change.
- [ ] **5.2 One shadow bit.** `Interruptibility` has `sti_shadow` and
  `mov_ss_shadow`, AMD has one. Proposal: report it as `sti_shadow`, never set
  `mov_ss_shadow`. `after_skipped_instruction` clears both, so it still works.
  Check that no code needs to tell them apart (grep `mov_ss_shadow` in `hv/`).
- [ ] **5.3 No exit entry, and `run_in_place`.** On Intel the exit jumps to a
  host RIP on its own stack, and `launch_in_place` returns "as the guest". On
  AMD, VMRUN returns to the host. For the firmware to keep running as the
  guest, `run_in_place` must switch to a separate hypervisor stack, build the
  guest state from the current registers (resuming at its own return
  address), and run the VMRUN loop there, never returning. Proposal: that, with
  `configure_exit_entry` a no-op and `exit_stack_usage` returning that stack.
  HAL_STRUCTURE.md said `None` for AMD; this replaces it.
- [ ] **5.4 Memory types under NPT.** EPT entries carry a memory type, which
  `apply_memory_types`, `spare_tables(MtrrCapabilities)` and `Retyped` use to
  follow the guest's MTRRs. NPT entries have none: the type comes from the
  guest PAT, the host PAT and the host MTRRs. **verify** APM 15.25.8. If
  confirmed, on AMD `apply_memory_types` returns an empty `Retyped`,
  `spare_tables` returns 0, and `WalkEnd::Mapped::memory_type` is the host's.
  No trait change, but the guest's MTRR writes then have no effect on AMD;
  note it in the Intel/AMD differences.
- [ ] **5.5 CR0/CR4 shadows.** SVM has no guest/host mask and no read shadow.
  Proposal: intercept every CR0/CR4 read and write; the backend holds
  `requested` per register and returns it for MOV-from-CR. Slower on AMD,
  correct. No trait change.
- [ ] **5.6 EFER.SVME and `GUEST_FORBIDDEN_CR4`.** VMRUN rejects a guest whose
  EFER.SVME is 0 (**verify**, APM 15.5.1), so the VMCB EFER has SVME set while
  the guest must not see it. Proposal: the backend adds EFER to its own MSR
  intercepts in `Intercepts::allocate` and hides the bit on RDMSR / WRMSR.
  `GUEST_FORBIDDEN_CR4` is 0. A guest MOV to CR4 needs no AMD rule.
- [ ] **5.7 Reason numbers and counters.** EXITCODE reaches 0x400 (NPF) and
  -1 (VMEXIT_INVALID), while `REASON_COUNTERS` is one counter per raw code
  (Intel: 128) and `ReasonDescription::number` is a `u16`. Two ways, your
  choice:
  - (a) No trait change: `REASON_COUNTERS = 0x401` (about 8 KiB of
    counters). `reason_code()` returns the EXITCODE (NPF is 0x400);
    VMEXIT_INVALID (-1 = 0xFFFF_FFFF) is out of range, so it lands in the
    existing "other reasons" counter (`OTHER_REASONS` in `hv/vmexit/policy.rs`).
    **verify** that `number: u16` in `ReasonDescription` can carry it as 0xFFFF.
  - (b) A new trait member that maps a raw code to a dense counter index.
    Smaller memory, but a name I would have to invent: tell me the name.
  I recommend (a): nothing new in the interface.
- [ ] **5.8 Instruction length.** Intel always gives it. AMD gives nRIP only with
  NRIPS (CPUID 0x8000_000A EDX bit 3). Proposal: `detect()` returns
  `NotSupportedByCpu` without NRIPS (all Zen CPUs have it), so `instruction_length`
  is always nRIP − RIP and nothing decodes instructions.
- [ ] **5.9 Software-exception event kinds.** Intel has three software kinds,
  AMD has one (type 4). Check which ones Aleph0 builds (`InjectedEvent`
  has only NMI, #GP, #UD constants, all hardware exceptions or NMI). If none of
  the software kinds is ever built, only re-delivery of a vectored event needs
  them, and `Other(u8)` covers it.
- [ ] **5.10 `Flush` timing.** INVEPT acts at once; TLB_CONTROL acts at the next
  VMRUN. Every flush in `grant_write` / `apply_memory_types` happens in exit
  context, and the next thing is VMRUN, so the difference should not show.
  Proposal: set TLB_CONTROL in the VMCB and nothing else.
- [ ] **5.11 GIF and the host NMI.** With GIF = 0 between VMRUN and STGI, an NMI
  stays pending. The `host-nmi-test` path (NMI during the exit handler) needs
  STGI at the right place in the loop. Design when the loop is written.

---

## 6. Order of work

1. Read: APM Vol. 2 chapter 15 and Appendices B and C, resolve every **verify**
   above, answer section 5.
2. `amd` feature, `hal/mod.rs` cfg switch, empty `hal/amd` that fails to
   compile only where a trait member is missing (`cargo check --no-default-features
   --features amd`).
3. `virtualization.rs`, `msr.rs`: detect, enable, disable.
4. `vmcb/` fields and the VMRUN loop with the built-in test guest
   (`test-guest`), CPUID exit only.
5. `vm_exit.rs` decoders with their unit tests, then the rest of the exits.
6. `npt/`, `intercepts.rs`, `hypercall.rs`.
7. Firmware guest (`run_in_place`).

Done when AMD launches the built-in guest and handles its exits on QEMU with
`-cpu max,svm=on` (HAL_PLAN.md, phase 5).

Not proven by QEMU: real-hardware behaviour of NPT memory types, GIF / NMI,
and Hyper-V's nested SVM. Real AMD hardware is needed before AMD is called
working.

---

## 7. References

- AMD64 APM Vol. 2, System Programming (24593): chapter 15 Secure Virtual
  Machine, Appendix B VMCB layout, Appendix C SVM intercept exit codes.
- Linux `arch/x86/kvm/svm/` (`svm.c`, `nested.c`): the closest working
  reference for every item in section 5.
- RVM1.5 `src/arch/x86_64/amd/` (vmcb, npt, vcpu).
- SimpleSvm (tandasat): small, readable VMRUN loop.
- HAL_PLAN.md section 6, HAL_MAPPING.md: how other hypervisors split Intel / AMD.
