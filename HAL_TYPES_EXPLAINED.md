# HAL types and traits, explained

Companion to `src/hal/types.rs` and `src/hal/mod.rs` (the draft interface),
HAL_INTERFACE.md (what moves where) and HAL_MAPPING.md (file mapping).

**Naming rule:** every name here is a name the source code already uses (a
type, field, method, constant or function), or a name from HAL_PLAN.md.
Nothing is invented. Where a source type changes shape, it keeps its name.

The interface exists for three jobs: **handling an exit**, **setting up**,
and **guest memory**.

---

# Part 1: handling one exit

## The flow in the source today

```
CPU exit ─▶ vmexit_handler(frame)              hv/handler.rs:171
              read_exit(frame) -> ExitRead     every VMCS field, as read (Result per field)
              judge(...)                       builds ExitInfo from ExitRead, then calls
                decide(guest, state, exit)     -> (ExitEvent, ExitState)      hv/decide/mod_ch.rs:877
              carry_out(event, read, ...)      event.action():
                ExitAction::Resume(plan)       the ResumePlan
                  plan.restoring_nmi_blocking(&exit)
                  plan.redelivering(exit.redelivery(..))
                  prepare_resume(read, plan, frame)   VMWRITEs, emulation, RIP
                otherwise: stop
```

The example used below: the guest writes a character to the serial port.

```asm
mov dx, 0x3F8
mov al, 'A'
out dx, al
```

## `ExitReason`: why the guest stopped

**Today** (`hv/decide/mod_ch.rs:56`):

```rust
pub struct ExitReason {
    pub basic: u16,          // 30 for our `out`
    pub entry_failure: bool,
}
```

`decide()` compares `basic` with the `EXIT_REASON_*` constants (30 =
`EXIT_REASON_IO_INSTRUCTION`). Those numbers are Intel's. AMD reports the
same `out` as `0x7B`.

**In the HAL** (`hal/types.rs`): an enum, one variant per `EXIT_REASON_*`
constant, named after it:

```rust
pub enum ExitReason {
    ExceptionOrNmi,                        // EXIT_REASON_EXCEPTION_OR_NMI
    InterruptWindow,                       // EXIT_REASON_INTERRUPT_WINDOW
    NmiWindow,                             // EXIT_REASON_NMI_WINDOW
    Cpuid,                                 // EXIT_REASON_CPUID
    Hlt,                                   // EXIT_REASON_HLT
    Vmcall,                                // EXIT_REASON_VMCALL
    ControlRegister(CrQualification),      // EXIT_REASON_CONTROL_REGISTER
    IoInstruction(IoQualification),        // EXIT_REASON_IO_INSTRUCTION
    Rdmsr, Wrmsr,                          // EXIT_REASON_RDMSR / _WRMSR
    Mwait, Monitor,                        // EXIT_REASON_MWAIT / _MONITOR
    EptViolation(EptQualification),        // EXIT_REASON_EPT_VIOLATION
    EptMisconfig,                          // EXIT_REASON_EPT_MISCONFIG
    Xsetbv,                                // EXIT_REASON_XSETBV
    EntryFailure(u16),                     // today's `entry_failure` = true, with `basic`
    Unnamed(u16),                          // today's "unnamed exit reason", with `basic`
}
```

The backend turns its number into the variant: Intel `30 =>
IoInstruction(..)`, AMD `0x7B => IoInstruction(..)`. `decide()` matches the
variant and never sees a number.

`Unnamed(basic)` keeps the real number for any exit not in the list, so the
report still prints it (as `exit_reason_name` does today with "unnamed exit
reason").

## The three qualifications: the details of an exit

Knowing "it was an `out`" is not enough. To run it for the guest, Aleph0
needs the port, how many bytes, and in or out. The CPU packs those into one
number, the **exit qualification** (VMCS field `EXIT_QUALIFICATION`).

### `IoQualification`

**Today** (`hv/decide/emulation_ch.rs:326`): `pub struct IoQualification(pub
u64)`, a wrapper around the raw number, with methods that pick out the bits:
`port()`, `size()`, `is_in()`, `is_string()`, `is_rep()`.

**In the HAL**: same name, but the backend runs those methods once and stores
the answers. Each field has the name of the method it replaces:

```rust
pub struct IoQualification {
    pub port: u16,              // 0x3F8
    pub size: Option<IoSize>,   // Some(Byte). None = an encoding the SDM leaves unused
    pub is_in: bool,            // false: it's an OUT
    pub is_string: bool,        // OUTS
    pub is_rep: bool,           // REP prefix
}
```

Why: AMD packs the same facts in a different bit layout (its `EXITINFO1`).
With the fields already filled, `decide()` doesn't care which layout it came
from.

### `CrQualification`

**Today** (`hv/decide/control_register.rs:69`): `CrQualification(pub
u64)` with methods `cr()`, `access()`, `gpr()`.

**In the HAL**: the same three as fields:

```rust
pub struct CrQualification {
    pub cr: u8,           // which control register: 4 for `mov cr4, rax`
    pub access: CrAccess, // MovToCr, MovFromCr, Clts, Lmsw  (source enum, unchanged)
    pub gpr: u8,          // which general register holds the value: 0 = RAX
}
```

The value itself is already in `ExitInfo::cr_source` (the source reads it in
`cr_source_value`).

### `EptQualification`

**Today** (`hv/decide/mod_ch.rs:176`): `EptQualification(pub u64)` with the
bits named by constants `EPT_ACCESS_READ`, `EPT_ACCESS_WRITE`,
`EPT_ACCESS_FETCH`, `EPT_PAGE_READABLE`, `EPT_PAGE_WRITABLE`,
`EPT_PAGE_EXECUTABLE`, `EPT_LINEAR_VALID`, `EPT_TRANSLATED_ACCESS`,
`EPT_NMI_UNBLOCKING_IRET`.

**In the HAL**: one bool per constant, named after it:

```rust
pub struct EptQualification {
    pub access_read: bool,      // what the guest tried
    pub access_write: bool,
    pub access_fetch: bool,
    pub page_readable: bool,    // what the page allowed
    pub page_writable: bool,
    pub page_executable: bool,
    pub linear_valid: bool,     // GUEST_LINEAR_ADDRESS is meaningful
    pub translated_access: bool,
    pub nmi_unblocking_iret: bool,  // the fault was in an IRET that ended NMI blocking
}
```

`access_write && !page_writable` = the guest wrote a read-only page.
`is_protected_write` (today's method) also compares with the protected page,
which is `ExitInfo::protected_page`, so it stays in `decide()`.

`nmi_unblocking_iret` is what `ResumePlan::restoring_nmi_blocking` needs.

## `ExitInfo`: everything read at one exit

**Today** (`hv/decide/mod_ch.rs:355`): built by `judge` from `ExitRead`. In
the HAL the backend fills it in `read_exit`. Same name, same fields, with
these changes:
- `qualification` is gone: it's decoded into `reason` (above).
- `instruction_len` added: today `read.length`, passed separately to
  `exit.redelivery(..)`.
- `guest_linear_address` added: today read separately in `report`
  (`GUEST_LINEAR_ADDRESS`).

```rust
pub struct ExitInfo {
    pub reason: ExitReason,
    pub interruption_info: InterruptionInfo,  // VM_EXIT_INTERRUPTION_INFO
    pub guest_rax: u64,
    pub guest_rcx: u64,
    pub guest_rdx: u64,
    pub interruptibility: u32,                // GUEST_INTERRUPTIBILITY_STATE
    pub idt_vectoring: InterruptionInfo,      // IDT_VECTORING_INFO
    pub idt_vectoring_error_code: u32,
    pub instruction_len: Option<u32>,
    pub guest_physical: u64,
    pub guest_linear_address: u64,
    pub protected_page: u64,
    pub stack_page: u64,
    pub mtrr: MtrrCapabilities,
    pub hyperv: bool,
    pub guest_cpl: u8,
    pub cr_source: u64,
    pub cr_fixed: CrFixed,
    pub guest_cr4: u64,
    pub xcr0_supported: u64,
}
```

Why each field is there:

| Field | Needed for |
|---|---|
| `guest_rax`, `guest_rcx`, `guest_rdx` | CPUID leaf/subleaf (EAX/ECX), MSR number (ECX) and value (EDX:EAX), the OUT value (AL/AX/EAX). For our `out`, `guest_rax` = `'A'`. |
| `instruction_len` | Moving RIP past the instruction (`advance_rip`), and re-delivering a software interrupt. |
| `interruption_info` | For `ExceptionOrNmi`: which exception or NMI it was. |
| `interruptibility` | Whether the guest can take an NMI or interrupt right now (bit 0 STI, bit 1 MOV SS, bit 3 NMI blocking). |
| `idt_vectoring`, `idt_vectoring_error_code` | The interrupt the exit cut off (next section). |
| `guest_physical` | For `EptViolation`: the page the guest touched. |
| `guest_linear_address` | For the report, when `linear_valid`. |
| `protected_page`, `stack_page` | Aleph0's own read-only pages, to recognize its own trap. |
| `mtrr` | Which MTRR MSRs the guest has, to emulate them. |
| `hyperv` | Whether to forward Hyper-V hypercalls and synthetic MSRs. |
| `guest_cpl` | For `Vmcall`: kernel or user. |
| `cr_source`, `cr_fixed` | For `ControlRegister`: the value written, and the bits VMX requires. |
| `guest_cr4`, `xcr0_supported` | For `Cpuid` and `Xsetbv`. |

## `InterruptionInfo` and `Redelivery`: interrupts

**`InterruptionInfo`** (`hv/decide/mod_ch.rs:237`) is an interrupt or
exception, packed in a `u32`: vector (bits 7:0), interruption type (10:8),
error-code bit (11), valid (31). It is used for:
- the exception or NMI that caused an exit (`ExitInfo::interruption_info`),
- the interrupt an exit cut off (`ExitInfo::idt_vectoring`),
- the exception Aleph0 injects (`ResumePlan::inject_exception`, constants
  `GENERAL_PROTECTION`, `INVALID_OPCODE`, `NMI`).

It moves **unchanged**. AMD's EVENTINJ field has the same layout in its low 32
bits, so a raw `u32` works for both.

**The interrupt the exit cut off.** If the CPU was in the middle of handing
an interrupt to the guest when the exit happened, the interrupt is not
delivered. The CPU saves it in `IDT_VECTORING_INFO`. Aleph0 must give it to
the guest again on resume, or the guest loses it.

**`Redelivery`** (`hv/decide/mod_ch.rs:344`, unchanged) is that saved
interrupt, ready to give back:

```rust
pub struct Redelivery {
    pub info: InterruptionInfo,        // the saved interrupt
    pub error_code: Option<u32>,
    pub instruction_len: Option<u32>,  // only for software interrupts (INT n)
}
```

`ExitInfo::redelivery(instruction_len)` builds it from `idt_vectoring`
(`None` if nothing was cut off), and `ResumePlan::redelivering(redelivery)`
puts it in the plan.

## After `decide()`: `ExitEvent` → `ExitAction` → `ResumePlan`

All of this is in `hv/decide/mod_ch.rs` and **stays there, unchanged**.
None of it touches the VMCS, so it's already shared code. `hal/mod.rs`
only imports `ResumePlan`, because `VcpuControl::prepare_resume` takes one.

There are three steps between "we read the exit" and "we resume the guest":

```
decide(guest, state, exit)  ->  (ExitEvent, ExitState)    what the exit turned out to be
event.action()              ->  ExitAction                resume or stop?
ExitAction::Resume(plan)    ->  ResumePlan                exactly what to do before resuming
prepare_resume(plan)        ->  VMWRITEs                  backend does it
```

### `ExitEvent`: what the exit turned out to be (`mod_ch.rs:512`)

`decide()` looks at the `ExitInfo` and gives the exit a name. For our `out`
to the serial port it returns `ExitEvent::Emulated(Emulation::Out { .. })`.

| `ExitEvent` | Meaning | `action()` gives |
|---|---|---|
| `Emulated(Emulation)` | an instruction Aleph0 runs for the guest (CPUID, RDMSR, WRMSR, IN, OUT, XSETBV, hypercall, ...) | `Resume(PAST_INSTRUCTION + emulate)` |
| `ControlRegisterWrite(CrWrite)` | a MOV to CR0/CR4 Aleph0 allows | `Resume(PAST_INSTRUCTION + write_cr)` |
| `GuestGeneralProtection` | RDMSR/WRMSR/XSETBV the guest gets #GP for | `Resume(INJECT_GP)` |
| `ControlRegisterGeneralProtection` | a CR write a real CPU would refuse | `Resume(INJECT_GP)` |
| `GuestInvalidOpcode` | MWAIT, MONITOR, or a VMCALL nobody takes | `Resume(INJECT_UD)` |
| `Halted` | HLT | `Resume(PAST_INSTRUCTION)` |
| `BootServicesEnded`, `TestVmcall` | Aleph0's own VMCALLs | `Resume(PAST_INSTRUCTION)` |
| `EptWriteGranted` | the guest wrote its protected page: allow it, retry | `Resume(RETRY_AFTER_GRANT)` |
| `NmiInjected`, `NmiWindowInjected`, `InterruptWindowInjected` | an NMI the guest can take now | `Resume(INJECT_NMI)` |
| `NmiDeferred` | an NMI the guest can't take yet | `Resume(WAIT_FOR_NMI_WINDOW)` |
| `NmiDeferredPastSti` | an NMI held back only by an STI shadow | `Resume(WAIT_FOR_STI_SHADOW)` |
| `MainVmcall`, `GuestNmiHandler` | test-guest checkpoints | `Resume(PAST_INSTRUCTION)` |
| `LastMainVmcall` | the test guest's last VMCALL | `Finished` |
| `EptViolation`, `EptMisconfig`, `EptWriteMissing`, `DataPageMismatch`, `ControlRegisterUnhandled`, `Unexpected` | something went wrong | `Unexpected` |

### `ExitAction`: resume or stop (`mod_ch.rs:823`)

```rust
pub enum ExitAction {
    Resume(ResumePlan),  // go back to the guest, after doing what the plan says
    Finished,            // the test guest finished as expected: stop
    Unexpected,          // an exit Aleph0 doesn't handle: stop, report
}
```

### `ResumePlan`: what to do before resuming (`mod_ch.rs:590`)

**What it is, in one sentence:** the to-do list Aleph0 runs before letting
the guest continue.

After an exit, `decide()` has worked out what happened (`ExitEvent`). Before
the guest can go on, some jobs may be needed first: skip the instruction, run
it for the guest, write a control register, give the guest a fault or an NMI,
give back an interrupt the exit cut off, open a window, unlock the protected
page. `ResumePlan` holds one switch per job. `prepare_resume` does the jobs
that are switched on, then the guest resumes.

Each field is one job:

```rust
pub struct ResumePlan {
    pub advance_rip: bool,
    pub inject_nmi: bool,
    pub nmi_window: Option<bool>,
    pub interrupt_window: Option<bool>,
    pub grant_write: bool,
    pub emulate: Option<Emulation>,
    pub inject_exception: Option<InterruptionInfo>,
    pub redeliver: Option<Redelivery>,
    pub clear_nmi_blocking: bool,
    pub restore_nmi_blocking: bool,
    pub write_cr: Option<CrWrite>,
}
```

| Field | The question it answers | What `prepare_resume` does on Intel | AMD (later) |
|---|---|---|---|
| `advance_rip` | Skip the instruction that exited? Yes after emulating it. No after a fault, so the instruction runs again. | `GUEST_RIP += VM_EXIT_INSTRUCTION_LEN` | `rip = nRIP` |
| `emulate` | Run an instruction for the guest? `Some(Emulation)` says which. | runs it in root mode, writes the result into the saved registers | same, shared |
| `write_cr` | Write a control register for the guest? `CrWrite { cr, requested, real }` | `GUEST_CRn = real`, read shadow `= requested` | VMCB CRn |
| `inject_exception` | Give the guest a fault? `Some(INVALID_OPCODE)` or `Some(GENERAL_PROTECTION)` | `VM_ENTRY_INTERRUPTION_INFO` (+ error code 0) | `EVENTINJ` |
| `inject_nmi` | Give the guest an NMI now? | `VM_ENTRY_INTERRUPTION_INFO = NMI` | `EVENTINJ`, type NMI |
| `redeliver` | Give back the interrupt the exit cut off? (`Redelivery`) | VM-entry interruption info, error code, instruction length | `EVENTINJ` |
| `nmi_window` | Exit as soon as the guest can take an NMI? `Some(true)` on, `Some(false)` off, `None` leave it | NMI-window exiting control bit | IRET intercept |
| `interrupt_window` | Exit as soon as the guest can take an interrupt? Same `Option<bool>` | interrupt-window exiting control bit | VINTR |
| `grant_write` | Make the protected page writable? | EPT write bit + INVEPT (`ProtectedPage::grant_write`) | NPT write bit + TLB flush |
| `clear_nmi_blocking` | Clear "NMIs blocked" before re-delivering an NMI whose delivery faulted? | `GUEST_INTERRUPTIBILITY_STATE` bit 3 off | interrupt shadow / NMI state |
| `restore_nmi_blocking` | Set "NMIs blocked" again after a fault in an IRET that had ended it? | `GUEST_INTERRUPTIBILITY_STATE` bit 3 on | same |

Only one event can be injected per VM entry, so `redeliver` is never set
together with `advance_rip`, `inject_nmi` or `inject_exception`.

**Ready-made plans** (constants on `ResumePlan`):

| Constant | What it sets |
|---|---|
| `AS_IS` (private) | nothing: the base the others change |
| `PAST_INSTRUCTION` | `advance_rip` |
| `INJECT_NMI` | `inject_nmi`, both windows off |
| `WAIT_FOR_NMI_WINDOW` | NMI window on, interrupt window off |
| `WAIT_FOR_STI_SHADOW` | interrupt window on, NMI window off |
| `RETRY_AFTER_GRANT` | `grant_write`; RIP stays, so the write runs again |
| `INJECT_GP` | `inject_exception = GENERAL_PROTECTION` |
| `INJECT_UD` | `inject_exception = INVALID_OPCODE` |

**Adjustments** `carry_out` makes before `prepare_resume`:

| Method | What it does |
|---|---|
| `keeping_pending_nmi(next)` | turns the NMI window on if an NMI is still pending |
| `restoring_nmi_blocking(&exit)` | sets `restore_nmi_blocking` after a fault in an NMI-unblocking IRET |
| `redelivering(redelivery)` | adds the cut-off interrupt; `None` if the plan already injects something |
| `refused_with(exception)` | Hyper-V refused a forwarded access: RIP stays, the guest gets the exception |

Our `out` example:
`decide` → `ExitEvent::Emulated(Emulation::Out { port: 0x3F8, size: Byte, value: 0x41 })`
→ `action()` → `Resume(ResumePlan { emulate: Some(..), advance_rip: true, .. })`
→ `prepare_resume` runs the `out` and moves RIP past it.

### `Emulation`: the instruction run for the guest (`hv/decide/emulation_ch.rs:42`)

| Variant | The guest ran | What Aleph0 does |
|---|---|---|
| `Cpuid { leaf, subleaf, guest_cr4 }` | CPUID | runs it, hides what the guest isn't offered, copies the CR4 bits |
| `Xsetbv { value }` | XSETBV | runs it (the value was already checked) |
| `Rdmsr { msr }` / `Wrmsr { msr, value }` | RDMSR / WRMSR of an intercepted MSR | runs it |
| `MtrrRead { msr }` / `MtrrWrite { msr, value }` | RDMSR / WRMSR of an MTRR | reads or writes the guest's private MTRR copy; a write re-types EPT |
| `In { port, size }` / `Out { port, size, value }` | IN / OUT | runs it on the real port |
| `Hypercall { input }` | VMCALL meant for Hyper-V | forwards it to Hyper-V |
| `SyntheticRead { msr }` / `SyntheticWrite { msr, value }` | Hyper-V synthetic MSR | forwards it to Hyper-V |


`CrWrite`, `CrNumber`, `CrAccess` and `IoSize` move to `hal/types.rs`,
because the backend's decoded qualifications and `prepare_resume` use them.

---

# Part 2: setting up (`bring_up::run`)

Before the first exit can happen, Aleph0 has to turn VMX on and describe two
things to the CPU: **the guest** (what runs, where it starts) and **the host**
(where the CPU lands when an exit sends it back into Aleph0). Every type in
this part is an input or a result of that setup.

The order, from `bring_up::run` (`bring_up/mod_ch.rs:198`):

```
1. activate_context_tables      Aleph0's GDT copy + TSS
2. allocate the exit stack and the guest pages
3. check_vmx_support            can this CPU do VMX and EPT? -> VmxCapabilities
   read the MTRRs, Ept::build, ExitBitmaps::allocate, HostEnvironment::build
4. enter_vmx_root_operation     VMXON -> VmxOperation
   VmcsRegion::create_current   the VMCS
5. finalize_memory              check resident, hide Aleph0's pages
   configure_cr_shadowing, prepare_vmcs
6. launch / launch_in_place     the guest starts
7. teardown                     VMXOFF, give the CPU back to the firmware
```

## `VmxCapabilities`: can this CPU do VMX at all?

**What it is.** The answer to the first question of the whole setup: *is
VMX available on this machine?* It has exactly three possible answers
(`hal/intel/mod.rs:42`):

| Value | Meaning | What `bring_up` does |
|---|---|---|
| `Supported` | the CPU has VMX and the firmware left it enabled | goes on |
| `NotSupportedByCpu` | CPUID says there is no VMX (an old CPU, or a VM without nested virtualization) | stops with `BringUpError::VmxNotSupportedByCpu` |
| `DisabledByFirmware` | the CPU has VMX, but the BIOS locked it off (the `IA32_FEATURE_CONTROL` register says so) | stops with `BringUpError::VmxDisabledByFirmware` |

**Why there are two "no" answers.** They have different fixes. Not
supported means this machine can't run Aleph0. Disabled by firmware means
the user can turn VMX on in the BIOS setup.

**Who produces it.** `detect()` (`hal/intel/mod.rs:57`) checks CPUID
first, and only then reads the MSR, because that MSR may not exist on a CPU
without VMX. `check_vmx_support` (`bring_up/mod_ch.rs:473`) calls it.

**On AMD** the same three answers apply: CPUID has an SVM bit, and the
`VM_CR` register can disable it. That is why this type needs no change.

## `GuestKind`: which guest do we run?

**What it is.** Aleph0 can start one of two different guests, and a lot of
setup depends on which. It is chosen when Aleph0 is built:
`GuestKind::SELECTED` is `BuiltInTest` with the `test-guest` Cargo feature,
and `Firmware` otherwise (`hal/intel/vmcs/mod.rs:110`).

| Value | The guest | How it ends |
|---|---|---|
| `BuiltInTest` | a small test program Aleph0 carries (`hv/test_guest_ch.rs`). It makes scripted VMCALLs and checks that Aleph0 answers right. This is the regression test. | its last VMCALL stops it, and control returns to `bring_up`, which does teardown (`stop_returns_to_bring_up()` is true) |
| `Firmware` | the real UEFI firmware that is already running, which then boots the OS. Aleph0 slides underneath it. | never. A stop halts the machine, because the firmware has reused the stack the launch ran on |

**What changes with it** (these are the places that read `GuestKind`):
- `ExitBitmaps::allocate(kind)`: the test guest traps some MSRs and ports,
  the firmware traps none (`intercepts.rs:136`).
- The exception bitmap in `vmcs/mod.rs:138`: every exception exits for the
  test guest, none for the firmware (it has its own IDT).
- `decide()` (`hv/decide/mod_ch.rs:881`): only the test guest has the
  scripted VMCALLs.
- `bring_up` (`mod_ch.rs:508`): builds a different `GuestEntry`.

**Why it moves to `hal/types.rs`.** It sits inside `vmcs` today, so `decide`
and `report` import VMCS code just to ask "test or firmware?". It has
nothing to do with the VMCS.

## `GuestEntry`: where does the guest begin?

**What it is.** The starting point of the guest: where it starts running, on
which stack, with which interrupt table. `GuestKind` picks the variant
(`bring_up/mod_ch.rs:508-520`).

| Value | Used for | What it holds |
|---|---|---|
| `At { rip, rsp, idtr }` | `BuiltInTest` | a fresh start: the test guest's own code address, stack, and a tiny IDT with only the NMI gate |
| `InPlace { idtr }` | `Firmware` | no new start. The code right after the launch instruction keeps going, but now as the guest. Only the IDT is given (the firmware's own) |

**Why `InPlace` is special.** Aleph0 is itself running inside the firmware.
`launch_in_place` copies the current RIP, RSP and RFLAGS into the guest, so
from the firmware's point of view nothing happened: its function call
returns, and from then on everything it does is a guest. That is how Aleph0
gets under a firmware that is already running. The `At` fields are
placeholders (0, 0, `0x2`) until `launch_in_place` writes the real ones
(`guest_state.rs:216`).

**Used by** `GuestState::from_current(entry)` and `GuestState::write`, which
fill the guest-state part of the VMCS.

## `HostTables` and `HostSelectors`: where the CPU lands after an exit

**What they are.** When the guest exits, the CPU leaves the guest's world
and has to enter Aleph0's. The guest's page tables, GDT, IDT and stack are
the guest's own and can't be trusted, so the CPU is told in advance what to
load. `HostTables` is that list:

```rust
pub struct HostTables {
    pub cr3: u64,                  // page tables to switch to
    pub gdtr_base: u64,            // GDT
    pub idtr_base: u64,            // IDT (so a fault in Aleph0 goes to Aleph0's handler)
    pub tr_base: u64,              // TSS
    pub selectors: HostSelectors,  // cs, ss, ds, es, fs, gs, tr: which GDT entries to load
}
```

**Why Aleph0 builds its own.** After ExitBootServices the OS owns memory and
may overwrite the firmware's tables. If an exit landed on tables the OS had
overwritten, Aleph0 would crash on its first instruction. So
`HostEnvironment::build` (`host/mod_ch.rs:76`) makes private copies: a page
table that identity-maps memory, a GDT that keeps the firmware's selector
numbers, a TSS with interrupt stacks, an IDT.

**Where it goes.** `configure_host_state(&tables, cr4)` writes it into the
VMCS host-state fields. Two more host values go separately, to
`configure_host_entry(stack_top, entry_address)`: the exit stack to run on
and the address of the exit handler.

**Intel vs AMD.** Intel loads all of this automatically on every exit and
jumps to `entry_address`. On AMD, `VMRUN` returns like a function call, so
there is no entry address and less to load. That is why the HAL boundary
is "handle one exit" and not the run loop.

## `GuestFeatures`: what does the guest get to use?

**What it is.** Some CPU instructions only work in a guest if the hypervisor
switches them on for it (RDTSCP, INVPCID, XSAVES, the UMWAIT family).
`GuestFeatures` records what Aleph0 switched on (`hv/features_ch.rs:122`):

```rust
pub struct GuestFeatures {
    pub secondary: u32,  // the Intel "secondary controls" that were requested
    pub xcr0: u64,       // the XCR0 bits the guest's XSETBV may set
}
```

**Why it matters: two things must agree.** If a feature is switched off and
CPUID still says it exists, the guest will use it and get #UD. So when
Aleph0 answers CPUID for the guest, it hides every feature it did not switch
on (`OPTIONAL_CONTROLS` pairs each control with the CPUID bits to hide).
`secondary_request` builds the list: EPT, plus each optional control the CPU
allows.

**Who reads it.** `decide` (for CPUID and XSETBV), and `bring_up` (to write
the controls). `xcr0` is 0 when XSETBV can't run in root mode, so every
XSETBV gets #GP.

**On AMD.** `secondary` is an Intel control bitmask. AMD has no such field,
so it will need its own way to fill this (open question in HAL_INTERFACE.md).

---

# Part 3: guest memory (EPT)

The guest thinks it uses physical memory directly. Actually every "physical"
address it uses goes through a second set of page tables that Aleph0 owns:
**EPT** on Intel (**NPT** on AMD). Aleph0 uses it to make a page read-only,
to hide its own memory, and to apply the memory types the guest asked for
through its MTRRs. `Ept::build` makes it (`ept/mod.rs:1095`).

## `InveptType`: flush what the CPU remembers

**What it is.** The CPU caches EPT translations. After Aleph0 changes an
entry (for example makes the protected page writable), the old translation
may still be cached and the change has no effect. `INVEPT` flushes the cache.
`InveptType` says how much (`ept/mod.rs:152`):

| Value | Flushes |
|---|---|
| `SingleContext` (1) | translations of one EPT root (this guest's tables) |
| `AllContext` (2) | translations of every root |

**Who uses it.** `ProtectedPage::grant_write` returns it, and so does
`TypeTables::invept`. The caller then runs the flush with it. `EptSupport`
records which type the CPU supports.

## `EptWalk` and `WalkEnd`: where does this address really go?

**What it is.** A debugging aid. When an exit is an EPT violation or
misconfiguration, the report wants to show *why*. `TypeTables::walk` follows
the guest-physical address through the EPT tables the way the CPU does,
level by level, and reports how it ended (`ept/walk.rs:224`):

| `WalkEnd` | Means |
|---|---|
| `Mapped { hpa, size, access, memory_type }` | found: the address maps to `hpa`, in a page of `size` bytes, with these permissions and this memory type |
| `NotPresent` | the last entry has read = write = execute = 0: nothing is mapped |
| `Misconfigured` | the last entry is one the CPU refuses (an EPT misconfiguration) |
| `OutsideTables { table }` | an entry points to a page that is not one of Aleph0's tables, so the walk stops |
| `BeyondWalk` | the address has bits above the 48 a 4-level walk translates |

`EptWalk { gpa, entries, end }` also keeps the (up to four) entries read on
the way, for the printout. `x86/apic_ch.rs` uses `WalkEnd` too.

## `Retyped` and `Hidden`: what a memory operation did

**What they are.** Result summaries, printed in the log. Neither is used for
decisions.

- **`Retyped`** (`ept/mod.rs:1575`): the guest wrote one of its MTRR
  registers, which says "this memory range is cached / uncached / ...".
  Aleph0 keeps a private copy of the guest's MTRRs and re-types the EPT
  entries to match (`TypeTables::apply`). `Retyped` counts what changed:
  `regions` (2 MiB regions changed), `span` (first start to last end),
  `split` (regions broken into 4 KiB pages because their pages now differ in
  type), `merged` (regions joined back), `uncached` (regions made uncached
  as a whole for lack of a spare table).
- **`Hidden`** (`ept/mod.rs:979`): Aleph0 lives in ordinary memory, and
  the OS (or a cheat) could scan it. `TypeTables::hide` changes the EPT so
  the guest can't see Aleph0's pages. `Hidden` counts what it did:
  `data_pages` (redirected to a blank "shadow" page), `code_pages` (code that
  must stay executable, kept read-only instead), `regions_split` (2 MiB
  regions broken up to hide one page inside).

---

# Part 4: the traits

A trait is a list of functions that a backend must provide. Shared code
(`bring_up`, `hv`) calls only these, and `hal/intel` implements them (AMD
later). Every method is named after the source function it stands for.
Errors are each trait's own `type Error`, so the source error types stay
(`VmxOnError`, `VmxFail`, `VmcsError`, `GuestStateError`, `EptError`,
`ExitBitmapsError`).

## `VmxOperation`: turn VMX on and off

**What it is.** The CPU only accepts VMX instructions after a switch is
flipped, and flipping it back is part of giving the machine back. This trait
is that switch (`hal/intel/vmxon.rs:52`).

| Method | What it does | When |
|---|---|---|
| `detect() -> VmxCapabilities` | CPUID + `IA32_FEATURE_CONTROL`. Changes nothing | start of `check_vmx_support` |
| `enter_vmx_root_operation()` | sets CR4.VMXE, applies the fixed CR bits, prepares the VMXON region, executes VMXON. Returns the `VmxOperation` value | step 4 |
| `leave(self)` | executes VMXOFF. Takes `self`, so it can only be done once | `teardown` |

Between `enter` and `leave` the CPU is in "VMX root operation": the state
where the VMCS instructions work. Outside it they raise #UD.

## `VcpuControl`: one virtual CPU, its setup, and its exits

**What it is.** The control of one virtual CPU. On Intel that is one VMCS: a
block of CPU state that holds the guest's registers, the host's, and the
rules for what causes an exit, read and written only with VMREAD and VMWRITE.
It covers three jobs.

**Job 1: create and fill it** (today `bring_up::prepare_vmcs`, in this order):

| Method | What it sets |
|---|---|
| `create_current()` | allocates the VMCS, VMCLEAR, VMPTRLD (makes it the current one) |
| `configure_controls(capabilities, desired)` | which events cause exits (the pin, primary, secondary, exit and entry controls) |
| `set_ept_pointer(eptp)` | where the EPT tables are |
| `write_to_vmcs(bitmaps)` | the MSR and I/O bitmaps (`ExitBitmaps`) |
| `configure_host_state(tables, cr4)` | `HostTables`: where the CPU lands on exit |
| `configure_host_entry(stack_top, entry_address)` | the exit stack and the handler address |
| `configure_cr_shadowing(controls)` | which CR0/CR4 bits the guest owns, and the values it reads back |
| `write_guest_state(entry)` | the guest's starting registers from `GuestEntry` (**name open**: the source does `GuestState::from_current` then `GuestState::write`) |
| `self_test()` | writes and reads back a VMCS field to prove the VMCS works |

**Job 2: start the guest:**

| Method | What it does |
|---|---|
| `launch()` | VMLAUNCH for the test guest. Returns a `GuestStop` when the guest stops |
| `launch_in_place()` | VMLAUNCH for the firmware. On success the code after it already runs as the guest |

**Job 3: handle each exit.** These are the two hardware-touching steps of
`vmexit_handler`:

| Method | What it does |
|---|---|
| `read_exit()` | the VMREADs: reads the exit reason, qualification, registers, interrupt info, and returns a filled `ExitInfo` |
| `prepare_resume(plan)` | the VMWRITEs: carries out the `ResumePlan` (see Part 1) |
| `set_nmi_window_exiting(on)` | "exit as soon as the guest can take an NMI". Also called by the host NMI handler, outside any exit |
| `set_interrupt_window_exiting(on)` | the same for an interrupt |

## `GuestMemory`: the EPT tables

**What it is.** The control of the guest's second page tables (Part 3). It
stands for `Ept`, `TypeTables` and `ProtectedPage` (`ept/mod.rs`).

| Method | What it does | Called when |
|---|---|---|
| `build(protected_pages, ..)` | identity map of RAM with the MTRR types, with `protected_pages` read-only | step 3 |
| `grant_write()` | makes the protected page writable; returns the `InveptType` to flush with | the guest wrote the protected page (`EptWriteGranted`) |
| `apply()` | re-types the entries after the guest wrote its MTRRs; returns `Retyped` | an `MtrrWrite` emulation |
| `hide()` | hides Aleph0's own pages from the guest; returns `Hidden` | `finalize_memory` |
| `invept()` | which `InveptType` to flush with | after any change |
| `walk(gpa)` | `EptWalk` of one address | the stop report |

## `ExitBitmaps`: which MSRs and ports exit

**What it is.** A list, in memory, with one bit per MSR and per I/O port:
bit set means "exit when the guest uses this", bit clear means "let it run
on the real hardware". Without it every MSR and port access would exit and
the guest would be very slow. Intel's layout is a 4 KiB MSR bitmap plus two
4 KiB I/O bitmaps (A and B). `allocate(kind)` builds it for the `GuestKind`:
the test guest traps `INTERCEPTED_MSRS` and `INTERCEPTED_PORTS`, the
firmware traps none (`intercepts.rs:134`). `VcpuControl::write_to_vmcs`
then hands it to the CPU.

| Method | What it does |
|---|---|
| `allocate(kind)` | builds the bitmaps for this guest |

---

# How it connects

```
bring_up::run (shared):
  detect()                          -> VmxCapabilities
  ept      = GuestMemory::build(..)
  bitmaps  = ExitBitmaps::allocate(kind)
  vmx      = VmxOperation::enter_vmx_root_operation()
  vmcs     = VcpuControl::create_current()
  vmcs.configure_controls(); vmcs.set_ept_pointer(&ept); vmcs.write_to_vmcs(&bitmaps)
  vmcs.configure_host_state(&tables, cr4); vmcs.configure_host_entry(stack_top, entry)
  vmcs.configure_cr_shadowing(); vmcs.write_guest_state(entry); vmcs.self_test()
  vmcs.launch_in_place()

every exit (vmexit_handler):
  exit           = vmcs.read_exit()                 -> ExitInfo (reason decoded)
  (event, state) = decide(guest, state, exit)       shared, unchanged
  ExitAction::Resume(plan) = event.action()
  vmcs.prepare_resume(plan)
```

---

# Still open

- **`write_guest_state`** has no source name (see the table above).
- **The trait names `VmxOperation` and `ExitBitmaps` are Intel words.** They
  are the source names, but AMD will implement them too.
- **No trait method for VMCALL yet.** It's used by `resident` (Aleph0's own
  call) and `hv/hyperv` (forwarding to Hyper-V). AMD uses VMMCALL.
- **`GuestFeatures::secondary`** is an Intel control bitmask.
- **The dump** (`hal/intel/dump.rs`: `DUMP_LINES`, `saved_registers`) is
  not in any trait.
- **Parameters still typed with Intel-only types** are left as comments in
  `hal/mod.rs`: `VmcsControlMsrs`, `DesiredControls`, `CrControls`, `Eptp`,
  `EptSupport`, `Mtrrs`, `Allocation`, `GuestStop`, `RegisterFrame`,
  `ExitRead`.
