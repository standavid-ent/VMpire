# HAL structure: checklist for review

Every trait, every trait function, and every neutral type of the planned
interface, with its full signature. Tick a box when you accept the item, or
write the name you want next to it. Nothing here is implemented yet.

Companion: HAL_INTERFACE.md (why each item exists, and which Intel code it
replaces).

Reading guide:
- **Exit context** = the guest is stopped at a VM exit, on the CPU that runs
  it, in the backend's root mode. Only the exit handler (and the host NMI
  handler during it) is in exit context.
- `Result<T, Self::Error>`: each trait has its own error type; Intel's is
  its original error enum (`VmcsError`, `EptError`, …), kept inside
  `hal/intel`.
- "Intel: …" says what the Intel implementation calls (original code,
  unchanged). "AMD: …" is a note for later, nothing is written for AMD now.

---

## 1. Files

```
src/hal/
  mod.rs        the six traits below; `pub use intel as backend;` (feature "intel")
  types.rs      the neutral types below
  intel/        the original VMX code, implementing every trait
    mod.rs      pub struct HardwareVirtualization, Vcpu, VmExit,
                NestedPaging, Intercepts, Hypercall  (one per trait)
src/hv/         shared hypervisor logic: decide, exit handler, reports, stats
src/x86/        plain x86: paging, page, mtrr, msr, cr, cpuid, segment, apic, context tables
src/host/       Aleph0's own host tables and handlers
src/bring_up/, src/resident/   unchanged roles
```

How shared code calls the backend:

```rust
use crate::hal::{backend, Vcpu as _};
let vcpu = unsafe { backend::Vcpu::create() }?;
```

- [x] File layout (approved)
- [x] `hal::backend` chosen by Cargo feature (`intel` default, `amd` later)

---

## 2. Trait `HardwareVirtualization`

The CPU's virtualization extension (VT-x / AMD-V): is it there, its
capabilities, turning it on and off.

```rust
pub trait HardwareVirtualization: Sized {
    type ControlCapabilities: Copy;
    type Error: Copy + Debug + Display;
    const CPUID_FEATURE: CpuidBit;
    const GUEST_FORBIDDEN_CR4: u64;

    fn detect() -> Capabilities;
    unsafe fn control_capabilities() -> Self::ControlCapabilities;
    fn report_control_capabilities(capabilities: &Self::ControlCapabilities);
    fn optional_features(capabilities: &Self::ControlCapabilities) -> OptionalFeatures;
    unsafe fn cr_constraints() -> CrConstraints;
    unsafe fn enable() -> Result<Self, Self::Error>;
    unsafe fn disable(self) -> Result<(), Self::Error>;
    fn original_cr0(&self) -> u64;
    fn original_cr4(&self) -> u64;
}
```

| ✓ | Member | What it does | Intel | AMD (later) |
|---|---|---|---|---|
| [x] | `type ControlCapabilities` | what the CPU allows, read once | `VmcsControlMsrs` | CPUID 0x8000_000A |
| [x] | `type Error` | why enable/disable failed | `VmxOnError` / `VmxFail` | — |
| [x] | `const CPUID_FEATURE` | the CPUID bit that advertises this extension; the guest must not see it | leaf 1 ECX bit 5 (VMX) | 0x8000_0001 ECX bit 2 (SVM) |
| [x] | `const GUEST_FORBIDDEN_CR4` | CR4 bits a guest MOV to CR4 may not set (#GP) | `CR4_VMXE` | 0 |
| [x] | `detect()` | supported, absent, or disabled by firmware | `vmx::detect` | CPUID + VM_CR.SVMDIS |
| [x] | `control_capabilities()` | read the capability registers | `read_vmcs_control_msrs` | — |
| [x] | `report_control_capabilities(..)` | the serial lines about them | `report::vmx_capabilities` | — |
| [x] | `optional_features(..)` | which optional guest features the CPU can offer | `features::secondary_request` | — |
| [x] | `cr_constraints()` | CR0/CR4 bits that must / may be 1 while enabled | `CrFixed::read` | permissive |
| [x] | `enable()` | enter root mode | `enter_vmx_root_operation` (VMXON) | EFER.SVME, VM_HSAVE_PA |
| [x] | `disable(self)` | leave root mode, clear the enable bit | `VmxOperation::leave` (VMXOFF), clear CR4.VMXE | clear EFER.SVME |
| [x] | `original_cr0/4(&self)` | CR0/CR4 before `enable` | `VmxOperation::original_cr0/4` | — |

---

## 3. Trait `Vcpu`

The virtual CPU before and while it runs: create, configure, run.

```rust
pub trait Vcpu: Sized {
    type Error: Copy + Debug + Display;

    unsafe fn create() -> Result<Self, Self::Error>;
    unsafe fn release(&self) -> Result<(), Self::Error>;
    unsafe fn self_test(&self) -> Result<(), Self::Error>;
    unsafe fn configure_controls(
        &self,
        capabilities: &<backend::HardwareVirtualization as HardwareVirtualization>::ControlCapabilities,
        kind: GuestKind,
        features: OptionalFeatures,
    ) -> Result<(), Self::Error>;
    unsafe fn set_nested_paging(&self, paging: &backend::NestedPaging) -> Result<(), Self::Error>;
    unsafe fn set_intercepts(&self, intercepts: &backend::Intercepts) -> Result<(), Self::Error>;
    unsafe fn configure_host_state(&self, tables: &HostTables, cr4: u64) -> Result<(), Self::Error>;
    unsafe fn configure_cr_intercepts(&self, controls: &CrControls) -> Result<(), Self::Error>;
    unsafe fn set_guest_state(&self, entry: GuestEntry) -> Result<(), Self::Error>;
    unsafe fn run(&self) -> GuestStop<Self::Error>;
    unsafe fn run_in_place(&self) -> Result<(), GuestStop<Self::Error>>;
    unsafe fn exit_stack_usage(&self) -> Option<(usize, usize)>;
}
```

| ✓ | Member | What it does | Intel | AMD (later) |
|---|---|---|---|---|
| [x] | `type Error` | why a vCPU step failed | `VmcsError` (+ `GuestStateError`) | — |
| [x] | `create()` | allocate the control structure and make it current | `VmcsRegion::create_current` (VMCLEAR + VMPTRLD); also allocates the VM-exit stack | allocate the VMCB |
| [x] | `release(&self)` | stop using it before `disable` | `vmclear` | nothing |
| [x] | `self_test(&self)` | check the control structure answers as expected | `vmcs::self_test` | — |
| [x] | `configure_controls(..)` | which events exit, for this guest kind and features | `DesiredControls::minimal_64_bit_guest` + `configure_controls` | VMCB intercept vectors |
| [x] | `set_nested_paging(..)` | point the vCPU at the nested page tables | `set_ept_pointer(ept.eptp())` | VMCB nCR3 |
| [x] | `set_intercepts(..)` | point the vCPU at the MSR/IO intercept maps | `ExitBitmaps::write_to_vmcs` | MSRPM/IOPM addresses |
| [x] | `configure_host_state(..)` | the state Aleph0 runs on after an exit | `configure_host_state` (+ the VM_EXIT_CONTROLS log line) | host tables loaded by the run loop |
| [x] | `configure_cr_intercepts(..)` | which CR0/CR4 bits the guest owns, what it reads | `configure_cr_shadowing` | CR read/write intercepts |
| [x] | `set_guest_state(entry)` | the guest's starting state (+ its report lines) | `GuestState::from_current(entry)` + `write` + `report::guest_state` | VMCB save area |
| [x] | `run(&self)` | enter the guest until it stops; returns why | `vmcs::launch` | VMRUN loop |
| [x] | `run_in_place(&self)` | the running firmware becomes the guest; returns as the guest | `vmcs::launch_in_place` | — |
| [x] | `exit_stack_usage(&self)` | bytes of the exit stack used, and its size, after the guest stopped; `None` if the backend has none | `VmExitStack::used`, `VM_EXIT_STACK_SIZE` | `None` |

---

## 4. Trait `VmExit`

The vCPU stopped at a VM exit (exit context): one reader per detail of the
exit, then the changes made before it resumes.

```rust
pub trait VmExit: Sized {
    type Error: Copy + Debug + Display;

    unsafe fn current() -> Self;

    // Readers
    fn reason(&self) -> Result<ExitReason, Self::Error>;
    fn exception_or_nmi(&self) -> Result<Option<InjectedEvent>, Self::Error>;
    fn interrupted_event(&self) -> Result<Option<InjectedEvent>, Self::Error>;
    fn interruptibility(&self) -> Result<Interruptibility, Self::Error>;
    fn cr_access(&self) -> Result<CrAccess, Self::Error>;
    fn io_access(&self) -> Result<IoAccess, Self::Error>;
    fn nested_page_fault(&self) -> Result<NestedPageFault, Self::Error>;
    fn faulted_nmi_unblocking_iret(&self) -> Result<bool, Self::Error>;
    fn guest_cpl(&self) -> Result<u8, Self::Error>;
    fn guest_rip(&self) -> Result<u64, Self::Error>;
    fn instruction_length(&self) -> Result<u64, Self::Error>;
    fn guest_rsp(&self) -> Result<u64, Self::Error>;
    fn guest_cr4(&self) -> Result<u64, Self::Error>;
    fn guest_linear_address(&self) -> Result<u64, Self::Error>;
    fn raw(&self) -> RawExit;

    // Changes before the guest resumes
    fn set_rip(&mut self, rip: u64) -> Result<(), Self::Error>;
    fn set_interruptibility(&mut self, state: Interruptibility) -> Result<(), Self::Error>;
    fn inject_event(&mut self, event: InjectedEvent) -> Result<(), Self::Error>;
    fn set_nmi_window(&mut self, on: bool) -> Result<(), Self::Error>;
    fn set_interrupt_window(&mut self, on: bool) -> Result<(), Self::Error>;
    fn write_cr(&mut self, write: CrWrite) -> Result<(), (&'static str, Self::Error)>;

    // Stop report
    fn dump(&self, number: u64, registers: &GuestRegisters);
}
```

| ✓ | Member | What it answers / does | Intel | AMD (later) |
|---|---|---|---|---|
| [x] | `type Error` | a field could not be read or written | `VmcsError` | none (VMCB is memory) |
| [x] | `current()` | the vCPU stopped at the exit on this CPU | zero-size handle on the current VMCS | the VMCB pointer |
| [x] | `reason()` | what kind of exit | VM_EXIT_REASON → `ExitReason` | EXITCODE |
| [x] | `exception_or_nmi()` | the exception or NMI that caused an `ExceptionOrNmi` exit | VM_EXIT_INTERRUPTION_INFO | EXITCODE 0x40+v / 0x61 |
| [x] | `interrupted_event()` | an event whose delivery the exit interrupted (to re-deliver) | IDT_VECTORING_INFO + _ERROR_CODE | EXITINTINFO |
| [x] | `interruptibility()` | STI shadow, MOV-SS shadow, NMI blocked | GUEST_INTERRUPTIBILITY_STATE | INTERRUPT_SHADOW + NMI state |
| [x] | `cr_access()` | which CR, MOV to / MOV from / CLTS / LMSW, which register | EXIT_QUALIFICATION bits 11:0 | EXITCODE + EXITINFO1 |
| [x] | `io_access()` | port, size, IN/OUT, string, REP | EXIT_QUALIFICATION | EXITINFO1 |
| [x] | `nested_page_fault()` | GPA, read/write/fetch, what the page allowed, linear address valid | EXIT_QUALIFICATION + GUEST_PHYSICAL_ADDRESS | EXITINFO1/2 |
| [x] | `faulted_nmi_unblocking_iret()` | the exit was a fault in an IRET that had ended NMI blocking | bit 12 of qualification / interruption info | `false` |
| [x] | `guest_cpl()` | guest privilege level | GUEST_SS_ACCESS_RIGHTS DPL | VMCB CPL |
| [x] | `guest_rip()` | where the guest stopped | GUEST_RIP | VMCB RIP |
| [x] | `instruction_length()` | length of the exiting instruction | VM_EXIT_INSTRUCTION_LEN | nRIP − RIP |
| [x] | `guest_rsp()` | guest RSP (not in the saved registers) | GUEST_RSP | VMCB RSP |
| [x] | `guest_cr4()` | guest CR4 | GUEST_CR4 | VMCB CR4 |
| [x] | `guest_linear_address()` | linear address of a nested page fault, for the report | GUEST_LINEAR_ADDRESS | — |
| [x] | `raw()` | the backend's raw exit values, for history and stop reports only | reason, qualification, interruption info, IDT vectoring | EXITCODE, EXITINFO1/2, EXITINTINFO |
| [x] | `set_rip(rip)` | resume at `rip` | VMWRITE GUEST_RIP | VMCB RIP |
| [x] | `set_interruptibility(state)` | change shadows / NMI blocking | VMWRITE GUEST_INTERRUPTIBILITY_STATE | INTERRUPT_SHADOW |
| [x] | `inject_event(event)` | deliver this event at the next entry (NMI, exception, re-delivery) | VM_ENTRY_INTERRUPTION_INFO (+ error code, + instruction length) | EVENTINJ |
| [x] | `set_nmi_window(on)` | exit as soon as the guest can take an NMI | NMI-window exiting control | IRET intercept |
| [x] | `set_interrupt_window(on)` | exit as soon as the guest can take an interrupt | interrupt-window exiting control | VINTR intercept |
| [x] | `write_cr(write)` | emulate a MOV to CR0/CR4: what the guest reads, what really loads | CRn_READ_SHADOW + GUEST_CRn | VMCB CRn |
| [x] | `dump(number, registers)` | the guest's state at a stop | `dump::report_guest` | VMCB dump |

---

## 5. Trait `NestedPaging`

The second-level page tables (EPT / NPT).

```rust
pub trait NestedPaging: Sized {
    type Support: Copy;
    type Tables;
    type ProtectedPage: Copy;
    type Error: Copy + Debug + Display;

    fn check_support(
        capabilities: &<backend::HardwareVirtualization as HardwareVirtualization>::ControlCapabilities,
        physical_address_bits: u8,
    ) -> Result<Self::Support, Self::Error>;
    fn report_support(support: Self::Support);
    fn spare_tables(mtrr: MtrrCapabilities) -> usize;
    unsafe fn build(
        protected_pages: &[u64],
        support: Self::Support,
        mtrrs: &Mtrrs,
        spare_tables: usize,
    ) -> Result<(Self, Self::Tables), Self::Error>;
    fn report(&self, tables: &Self::Tables, protected_pages: &[u64]);
    fn memory_map_end(&self) -> u64;

    // On the tables (bring-up, then the exit handler)
    unsafe fn hide(
        tables: &mut Self::Tables,
        allocations: impl Iterator<Item = Allocation>,
        mtrrs: &Mtrrs,
        visible: &[Allocation],
    ) -> Result<Hidden, Self::Error>;
    unsafe fn apply_memory_types(tables: &mut Self::Tables, mtrrs: &Mtrrs) -> Result<Retyped, Self::Error>;
    fn flush_type(tables: &Self::Tables) -> Flush;
    fn free_tables(tables: &Self::Tables) -> usize;
    fn walk(tables: &Self::Tables, gpa: u64) -> WalkEnd;
    fn report_walk(tables: &Self::Tables, number: u64, gpa: u64);
    #[cfg(feature = "ept-misconfig-test")]
    unsafe fn misconfigure(tables: &Self::Tables, gpa: u64);

    // The protected pages (exit handler)
    fn protected_page_for(gpa: u64) -> Option<Self::ProtectedPage>;
    fn protected_page_gpa(page: Self::ProtectedPage) -> u64;
    fn nmi_stack_page() -> u64;
    unsafe fn grant_write(page: Self::ProtectedPage) -> Result<Flush, Self::Error>;
}
```

| ✓ | Member | What it does | Intel |
|---|---|---|---|
| [x] | `type Support` | what the CPU's nested paging offers | `EptSupport` |
| [x] | `type Tables` | the part the exit handler changes (types, hiding, walk) | `TypeTables` |
| [x] | `type ProtectedPage` | a page mapped read-only that can be made writable | `ProtectedPage` |
| [x] | `type Error` | why building or changing failed | `EptError` / `VmxFail` |
| [x] | `check_support(..)` | can Aleph0's map be built on this CPU | `ept::check_support` |
| [x] | `report_support(..)` | serial lines | `report::ept_support` |
| [x] | `spare_tables(mtrr)` | extra tables to keep for the guest's MTRR writes | `ept::spare_pts` |
| [x] | `build(..)` | identity map, protected pages read-only | `Ept::build` |
| [x] | `report(..)` | serial lines about the map | `report::ept` |
| [x] | `memory_map_end(&self)` | end of RAM the map covers (host page tables use it) | `Ept::memory_map_end` |
| [x] | `hide(..)` | redirect Aleph0's pages away from the guest | `TypeTables::hide` |
| [x] | `apply_memory_types(..)` | follow the guest's MTRRs | `TypeTables::apply` |
| [x] | `flush_type(..)` | which flush a change needs | `TypeTables::invept` → `Flush` |
| [x] | `free_tables(..)` | spare tables left | `TypeTables::free_pts` |
| [x] | `walk(.., gpa)` | where a GPA ends up | `TypeTables::walk(eptp, gpa).end` → `WalkEnd` |
| [x] | `report_walk(..)` | the walk's entries on serial | `report::report_ept_walk` |
| [x] | `misconfigure(..)` | test only: break an entry | `TypeTables::misconfigure` |
| [x] | `protected_page_for(gpa)` | the protected page holding `gpa` | `ept::protected_page_for` |
| [x] | `protected_page_gpa(page)` | its GPA | `ProtectedPage::gpa` |
| [x] | `nmi_stack_page()` | the protected NMI stack page | `ept::nmi_stack_page` |
| [x] | `grant_write(page)` | make it writable, flush | `ProtectedPage::grant_write` (INVEPT) |

---

## 6. Trait `Intercepts`

Which MSR and I/O accesses exit (MSR/IO bitmaps, MSRPM/IOPM).

```rust
pub trait Intercepts: Sized {
    type Error: Copy + Debug + Display;

    fn allocate(kind: GuestKind) -> Result<Self, Self::Error>;
    fn covers_msr(msr: u32) -> bool;
    fn report(&self, kind: GuestKind, mtrr: MtrrCapabilities);
}
```

| ✓ | Member | What it does | Intel |
|---|---|---|---|
| [x] | `type Error` | allocation failed | `ExitBitmapsError` |
| [x] | `allocate(kind)` | intercept every MTRR MSR plus the shared policy lists for `kind` (`hv::intercepted(kind)`) | `ExitBitmaps::allocate` |
| [x] | `covers_msr(msr)` | whether the map has a bit for `msr` (outside it, every access exits) | `msr_bit(msr, Read).is_some()` |
| [x] | `report(..)` | serial lines | `report::exit_bitmaps` |

---

## 7. Trait `Hypercall`

The hypercall instruction (VMCALL / VMMCALL).

```rust
pub trait Hypercall {
    unsafe fn call(rcx: u64);
    unsafe fn forward(registers: &mut HypercallRegisters) -> Result<(), Refused>;
    fn recovery_point() -> RecoveryPoint;
}
```

| ✓ | Member | What it does | Intel |
|---|---|---|---|
| [x] | `call(rcx)` | from the guest: Aleph0's own call (`hv::vmcall` values) | `asm!("vmcall")` in `resident` |
| [x] | `forward(registers)` | from root mode: the guest's hypercall, to Hyper-V | `hyperv_forward_hypercall` stub |
| [x] | `recovery_point()` | where the host fault handler resumes if Hyper-V refuses with #UD | `hyperv_hypercall_instruction` / `_refused` labels |

The test guest's own program (`guest.rs`, `global_asm!` with three
`vmcall`s) is guest code, not a call: it stays under
`#[cfg(feature = "intel")]`.

- [x] Test guest program per backend, by feature

---

## 8. Neutral types (`hal/types.rs`)

### `ExitReason`
```rust
pub enum ExitReason {
    ExceptionOrNmi, InterruptWindow, NmiWindow, Cpuid, Hlt, Hypercall,
    ControlRegisterAccess, IoInstruction, Rdmsr, Wrmsr, Mwait, Monitor,
    NestedPageFault, NestedPagingMisconfig, Xsetbv,
    EntryFailure(u32),   // the guest never ran; backend's raw code
    Other(u32),          // any reason Aleph0 does not handle; raw code
}
impl ExitReason { pub fn name(self) -> &'static str; pub fn counter(self) -> usize; }
```
- [x] enum name and every variant name
- [x] `name()` (report text, today `exit_reason_name`)
- [x] `counter()` (slot in the per-reason counters, today indexed by Intel number)

### `InjectedEvent`
```rust
pub struct InjectedEvent { pub kind: EventKind, pub vector: u8, pub error_code: Option<u32>, pub instruction_len: Option<u32> }
pub enum EventKind { ExternalInterrupt, Nmi, HardwareException, SoftwareInterrupt, PrivilegedSoftwareException, SoftwareException }
impl InjectedEvent { pub const NMI: Self; pub const GENERAL_PROTECTION: Self; pub const INVALID_OPCODE: Self; pub fn is_nmi(self) -> bool; }
```
- [x] `InjectedEvent`, its fields
- [x] `EventKind` and its variants
- [x] constants `NMI`, `GENERAL_PROTECTION`, `INVALID_OPCODE`

### `Interruptibility`
```rust
pub struct Interruptibility { pub sti_shadow: bool, pub mov_ss_shadow: bool, pub nmi_blocked: bool }
impl Interruptibility {
    pub fn blocks_nmi_injection(self) -> bool;     // any of the three
    pub fn only_sti_shadow(self) -> bool;          // today: `& NMI_INJECTION_BLOCKED == BLOCKING_BY_STI`
    pub fn after_skipped_instruction(self) -> Self;
    pub fn after_nmi_redelivery(self) -> Self;
}
```
- [x] struct and fields
- [x] the four methods

### Exit details
```rust
pub struct CrAccess { pub cr: u8, pub kind: CrAccessKind, pub gpr: u8 }
pub enum CrAccessKind { MovTo, MovFrom, Clts, Lmsw }
pub struct IoAccess { pub port: u16, pub size: Option<IoSize>, pub is_in: bool, pub is_string: bool, pub is_rep: bool }
pub enum IoSize { Byte, Word, Dword }                       // unchanged
pub struct NestedPageFault {
    pub gpa: u64, pub read: bool, pub write: bool, pub fetch: bool,
    pub page_readable: bool, pub page_writable: bool, pub page_executable: bool,
    pub linear_valid: bool, pub translated_access: bool,
}
impl NestedPageFault { pub fn is_protected_write(self) -> bool; pub fn access_name(self) -> &'static str; }
pub struct RawExit { pub values: [u64; 4], pub labels: [&'static str; 4] }
```
- [x] `CrAccess`, `CrAccessKind` (today `CrQualification`, `CrAccess`)
- [x] `IoAccess` (today `IoQualification`)
- [x] `NestedPageFault` and its two methods (today `EptQualification`)
- [x] `RawExit` (backend values with their names, for history and reports)

### Guest and host state
```rust
pub struct GuestRegisters { pub rax, rcx, rdx, rbx, rbp, rsi, rdi, r8 .. r15: u64 }   // today RegisterFrame
pub enum GuestEntry { At { rip: u64, rsp: u64, idtr: DescriptorTable }, InPlace { idtr: DescriptorTable } }
pub struct HostTables { pub cr3: u64, pub gdtr_base: u64, pub idtr_base: u64, pub tr_base: u64, pub selectors: HostSelectors }
pub struct HostSelectors { pub cs: u16, pub ss: u16, pub ds: u16, pub es: u16, pub fs: u16, pub gs: u16, pub tr: u16 }
pub enum GuestKind { BuiltInTest, Firmware }              // + SELECTED, stop_returns_to_bring_up
```
- [x] `GuestRegisters` (rename of `RegisterFrame`)
- [x] `GuestEntry`, `HostTables`, `HostSelectors`, `GuestKind` (unchanged, moved out of `vmx`)

### Control registers
```rust
pub struct CrConstraints { pub cr0_must_be_one: u64, pub cr0_may_be_one: u64, pub cr4_must_be_one: u64, pub cr4_may_be_one: u64 }
pub struct CrControls { .. }                              // unchanged (masks, read shadows)
pub enum CrNumber { Cr0, Cr4 }                             // unchanged
pub struct CrWrite { pub cr: CrNumber, pub requested: u64, pub real: u64 }   // unchanged
```
- [x] `CrConstraints` (today `CrFixed` with `cr0_fixed0/1`, `cr4_fixed0/1`)
- [x] `CrControls`, `CrNumber`, `CrWrite` unchanged

### Running and stopping
```rust
pub enum Capabilities { Supported, NotSupportedByCpu, DisabledByFirmware }   // today VmxCapabilities
pub enum GuestStop<E> {
    Finished { exits: u64 },
    Unexpected { number: u64, reason: ExitReason },
    LaunchFailed { error: E, instruction_error: Option<u32> },
    ResumeFailed { error: E, instruction_error: Option<u32> },
}
pub enum ExitAction { Resume(ResumePlan), Finished, Unexpected }            // unchanged
```
- [x] `Capabilities` (was `Support`; approved)
- [x] `GuestStop<E>` (error is the backend's)
- [x] `ExitAction` unchanged

### Nested paging results
```rust
pub enum Flush { SingleContext, AllContexts }                               // today InveptType
pub enum WalkEnd { Mapped { hpa: u64, size: u64, permissions: Permissions, memory_type: MemoryType },
                   NotPresent, Misconfigured, OutsideTables { table: u64 }, BeyondWalk }
pub struct Permissions { pub read: bool, pub write: bool, pub execute: bool }   // today access bits 2:0
pub struct Retyped { pub regions: usize, pub span: Option<(u64, u64)>, pub split: usize, pub merged: usize, pub uncached: usize }
pub struct Hidden { pub data_pages: usize, pub code_pages: usize, pub regions_split: usize }
```
- [x] `Flush`
- [x] `WalkEnd`, `Permissions`
- [x] `Retyped`, `Hidden` (unchanged fields)

### Features, CPUID, hypercalls
```rust
pub struct OptionalFeatures { pub rdtscp: bool, pub invpcid: bool, pub xsaves: bool, pub waitpkg: bool }   // today GuestFeatures::secondary bits
pub struct CpuidBit { pub leaf: u32, pub register: CpuidRegister, pub mask: u32 }
pub enum CpuidRegister { Eax, Ebx, Ecx, Edx }
pub struct HypercallRegisters { pub rcx: u64, pub rdx: u64, pub r8: u64, pub rax: u64 }   // unchanged
pub struct Refused;                                                         // unchanged
pub struct RecoveryPoint { pub instruction: u64, pub vector: u8, pub resume: u64 }
```
- [x] `OptionalFeatures` (and `GuestFeatures { features: OptionalFeatures, xcr0 }` in `hv`)
- [x] `CpuidBit`, `CpuidRegister`
- [x] `HypercallRegisters`, `Refused`, `RecoveryPoint`

### Shared decision types (in `hv`, using the types above)
```rust
pub struct ExitInfo {
    pub reason: ExitReason,
    pub event: Option<InjectedEvent>,          // exception_or_nmi()
    pub interrupted: Option<InjectedEvent>,    // interrupted_event()
    pub interruptibility: Interruptibility,
    pub cr_access: Option<CrAccess>, pub io_access: Option<IoAccess>, pub fault: Option<NestedPageFault>,
    pub faulted_nmi_unblocking_iret: bool,
    pub guest_rax: u64, pub guest_rcx: u64, pub guest_rdx: u64,
    pub protected_page: u64, pub stack_page: u64, pub mtrr: MtrrCapabilities, pub hyperv: bool,
    pub guest_cpl: u8, pub cr_source: u64, pub cr_constraints: CrConstraints, pub guest_cr4: u64, pub xcr0_supported: u64,
}
pub struct ResumePlan { .., pub inject_exception: Option<InjectedEvent>, pub redeliver: Option<InjectedEvent>, .. }
```
- [x] `ExitInfo` with neutral fields
- [x] `ResumePlan` with `InjectedEvent` (rest unchanged); `interruptibility(found: Interruptibility) -> Interruptibility`
- [x] `ExitEvent`, `ExitState`, `Emulation` unchanged

---

## 9. Open points to decide

- [x] Trait names: `HardwareVirtualization`, `Vcpu`, `VmExit`, `NestedPaging`, `Intercepts`, `Hypercall`
- [x] Serial lines with Intel details (VMCS field names, EPT layout, capability MSRs) are printed by the backend; shared code prints neutral lines. Intel output stays the same.
- [x] `NestedPaging` keeps two parts (`Self` + `Tables`) because the exit handler owns `Tables` while bring-up still needs the map (as today `Ept` + `TypeTables`).
- [x] Unsafe contracts (the original `// SAFETY:` comments stay unchanged in `hal/intel`): every `unsafe fn` documents its contract in neutral words ("root mode", "exit context") in `hal/mod.rs`.

---

## 10. Added or changed while implementing (please review)

Everything above is implemented (branch `hal-redo`). These are the
differences from the approved list, each needed to keep the Intel serial
output and behavior exactly as before.

**HardwareVirtualization**
- `const NAME` ("VMX") and `const ENABLE_BIT` ("CR4.VMXE"): shared log lines
  print the backend's names, so the Intel text stays the same.
- `fn enabled() -> bool`: teardown's "CR4.VMXE=0" line.
- `fn report_guest_features(features, xcr0, host_cr4)`: the "secondary
  controls 0x…" line prints Intel control bits, so the backend prints it.
- `fn report_error(error)`: each trait prints its own failures, with the
  original texts.

**Vcpu**
- `fn allocate()` + `unsafe fn create(&mut self)` instead of one `create()`:
  the VM-exit stack is allocated before VMXON, as before; the VMCS after.
- `unsafe fn configure_exit_entry(&self)`: HOST_RSP/HOST_RIP, its own step
  with its own log line, as before.
- `fn report_error(error)`.

**VmExit** (the exit handle; readers take `&self`, changes `&mut self`)
- The Intel `VmExit` keeps every field it reads, so the report printed
  after the changes shows the values the exit had (RIP before it moved).
- `fn reason_code()`, `fn describe_reason(code) -> ReasonDescription`,
  `const REASON_COUNTERS`: the per-reason counters and the "reason=10
  (CPUID)" text use the backend's raw code.
- `type Record` + `fn record()` + `const EMPTY_RECORD` instead of `raw() ->
  RawExit`: the history line's backend part ("reason=… rip=… len=… qual=…
  info=… vectoring=…"), printed by the backend.
- `fn unreadable() -> bool`: the print policy prints an exit in full when a
  field could not be read.
- `fn report_read(&self, number, registers)`: the "what was read" lines
  (reason, qualification, EPT addresses, RIP, unreadable fields).
- `fn encode_event`, `fn encode_interruptibility`: the hex values the event
  lines print (`{:#010x}` of an `InjectedEvent`, `{:#x}` of an
  `Interruptibility`).
- `const HYPERCALL` ("VMCALL"): the name in the report lines.
- Removed `guest_linear_address`: only the Intel report used it, and reads
  it itself.

**NestedPaging**
- `type FlushError` + `const FLUSH` ("INVEPT"): flush failures print as
  before ("… but INVEPT failed (…)").
- `const NAME` ("EPT"), `fn flush_name`, `fn physical_address_bits(support)`,
  `fn report_hidden`, `fn report_regions`, `fn report_error`.
- `fn walk(tables, gpa) -> WalkEnd` and `fn fmt_walk_end`: a walk's text
  comes from the backend (`WalkEnd` implements `Display` through it).
- `report_walk(tables: Option<&Tables>, …)`: prints "tables not available"
  as before.

**Intercepts**: `fn report_error`. The policy lists (`INTERCEPTED_MSRS`,
`INTERCEPTED_PORTS`, `intercepted(kind)`) moved to `hv/intercepted.rs`.

**Types**
- `ReasonDescription { number, name, entry_failure }`.
- `EventKind::Other(u8)`: an interruption type Aleph0 never builds, re-armed
  as it was.
- `Interruptibility::other`: bits Aleph0 does not change (Intel: SMI and
  enclave blocking) are written back as they were; plus
  `blocks_nmi_injection()`, `only_sti_shadow()`.
- `GuestStop::Unexpected { number, reason: u32 }` (raw code);
  `LaunchFailed { error }` / `ResumeFailed { error }`: the backend's error
  keeps the VM-instruction error number itself.
- `CrNumber`, `CrWrite`, `IoSize`, `NMI_VECTOR` live in `hal::types`
  (re-exported by `hv::exit_decision`).
- `RecoveryPoint` holds `*const u8` addresses, as the recovery table does.
- The page-table level types and `SIZE_2M`/`SIZE_1G`/`PDPT_SPAN` live in
  `x86::paging` (plain x86); `hal::intel::ept` re-exports them.

**Moved into `hal/intel`** (VMX-only, from `x86`): the VMX MSRs
(`hal/intel/msr.rs`), `is_vmx_supported`, the VMCS access-rights format
(`SegmentDescriptor::access_rights`, `ACCESS_RIGHTS_UNUSABLE`), and the
exit decoders (`hal/intel/vmexit/decode.rs`: `ExitReason`, `EXIT_REASON_*`,
`InterruptionInfo`, the qualifications, `BLOCKING_BY_*`).

**Known serial-text differences (failure or rare paths only)**
- "NMI (interruption info=…)": printed from the decoded NMI, so bit 12, if
  an NMI exit ever had it set, is not shown.
- "control-register access (qualification=…) Aleph0 does not handle":
  printed from the decoded access (register, type, GPR), so an LMSW's
  source-data bits are not shown.

**Tests**: the decision tests write exits as the Intel backend reads them
(`tests/unit/hv/exit_decision/raw_exit.rs`) and decode them with the
backend's own decoders, so every test value is the one it was. 537 test
markers (533 before; 4 new: Intel feature mapping, published features,
access rights).
