# HAL plan: run Aleph0 on Intel and AMD

Notes from a planning session on 2026-11-03. Companion file: ARCHITECTURE.md
(file-by-file mapping and the Intel/AMD translation table).

## 1. What Aleph0 is

- An Intel VT-x hypervisor in Rust, booted as a UEFI application
  (repo: alephc0rporation/Aleph0-Virtualization-Hypervisor; license
  MIT OR Apache-2.0). About 15,600 lines in `src/`, 535 host unit tests.
- Type-1 style: it starts from firmware, then runs the OS as its guest. It
  sits between the OS and the hardware and only steps in for what it chooses
  to watch (CPU instructions, memory pages, interrupts).
- Long-term goal: game anti-cheat research from below the OS (guest memory
  introspection, EPT protection, split views, watching privileged changes).
- Today it is verified on Hyper-V with nested virtualization: VMX entry,
  VMCS, own host tables, EPT identity map with a write-protected page, MTRR
  emulation, CPUID/MSR/IO emulation, Hyper-V hypercall forwarding, NMI
  handling, virtualizing the running firmware in place, staying resident
  after ExitBootServices, hiding its own memory via EPT.
- Not done: booting Linux as a guest (issue #28 is next), multiple vCPUs,
  all anti-cheat features.
- Read first: README.md, ROADMAP.md, CONTRIBUTING.md, then `src/main.rs` ->
  `src/bring_up/mod.rs` -> `src/vmx/vmexit/handler.rs`, and `docs/EPT.md`,
  `docs/EMULATED_EXITS.md`.

## 2. The theory in one paragraph

An Intel CPU has a mode (VMX) where an OS runs as a guest. When the guest does
something sensitive, the CPU pauses it and jumps into the hypervisor (a VM
exit). The hypervisor decides what to do, then resumes the guest (VMRESUME).
A second set of page tables (EPT) lets the hypervisor control which physical
memory the guest can see and touch. The VMCS is the CPU data structure that
says what the guest starts with and what causes exits. AMD's version is SVM:
VMCB instead of VMCS, VMRUN instead of VMLAUNCH/VMRESUME, nested page tables
(NPT) instead of EPT.

## 3. The goal and decisions

- Goal: a hardware abstraction layer (HAL) so the shared hypervisor logic
  works on **Intel (VMX) and AMD (SVM)**.
- **AArch64 is out of scope** (decided). It would be a separate project of
  similar size, with different virtualization (EL2, GIC, no MSRs/CPUID).
- Not a rebuild. It is a refactor of the existing code, one small PR at a
  time, with Intel behavior unchanged.
- AMD is also x86-64, which is why Intel+AMD is a realistic pair.
- What a HAL is here: three layers.

```
Shared logic (arch-neutral)
   exit decisions, memory policy, logging, later the anti-cheat features
        | talks to
Arch interface (traits + neutral types)
   run guest, exit info, map/protect guest page, inject interrupt
        | implemented by
Backends: hal/intel/   hal/amd/
```

- The neutral side uses its own real definitions (for example
  `enum ExitReason { Cpuid, Rdmsr, Wrmsr, EptViolation(..), ... }`, named
  after the source's `EXIT_REASON_*` constants), not dummy
  numbers. Each backend translates its real encodings into them. Dummy values
  belong only in test fakes.
- Use generics (static dispatch), not trait objects: the exit path is hot.
- Keep every `// SAFETY:` comment correct (clippy enforces it).

## 4. Phases

Rule for every phase: it builds, all tests pass, Intel behavior is unchanged.

**Phase 0: setup** (mostly done)
- [x] C linker (`build-essential`) installed in WSL.
- [x] Baseline: `cargo test --target x86_64-unknown-linux-gnu` -> 535 passed,
      0 failed. (Earlier "543" was a count of `#[test]` markers; use 535.)
- [ ] QEMU and OVMF not installed (only needed for Phase 5).
- [ ] Read docs/EMULATED_EXITS.md, docs/EPT.md, vmexit/handler.rs.
- [ ] Open an issue describing the goal (CONTRIBUTING.md asks for this before
      design changes).

**Phase 1: map neutral vs Intel-only** -> done, see ARCHITECTURE.md.

**Phase 2: move code, change no logic**
- Intel-only files to `src/hal/intel/`, neutral files to shared modules,
  tests moved to match.
- Good first small PR: move the page-table level types (`Level`, `Pml4`,
  `Pdpt`, `Pd`, `Pt`, `TableLevel`) out of `ept/mod.rs` into a shared paging
  module, since `host/page_tables.rs` depends on them.

**Phase 3: define the interface, one trait per PR**
- `Console` first (tiny): serial now.
- Then `GuestMemory` (map, protect, flush guest pages), `VcpuControl`
  (launch, resume, guest registers), `VmxOperation`, `ExitBitmaps`, and
  `ExitInfo` / the `ExitReason` enum. Names follow the source code; see
  HAL_INTERFACE.md.
- `ExitInfo` is the hardest part: its fields are Intel concepts today.
- Done when shared code no longer imports from `hal/intel/`.

**Phase 4: fakes and tests**
- A fake implementation per trait so shared logic is tested without hardware.

**Phase 5: AMD SVM backend**
- `src/hal/amd/`: VMCB, VMRUN, NPT, SVM exit codes.
- Test on QEMU software emulation with SVM enabled (QEMU TCG supports SVM,
  not VMX), e.g. `-cpu max,svm=on`. No Hyper-V needed.
- Intel-shaped assumptions that leaked into the traits will show up here.
  Fix the interface, not the AMD code.
- Done when AMD launches a guest and handles a CPUID exit on QEMU.

## 5. Risks

- **Intel changes can break silently.** Unit tests cannot run VMX
  instructions. Phases 2-3 need a real run on an Intel machine with Hyper-V
  before merging. (This WSL install shows the `vmx` CPU flag, but the
  project's run scripts are PowerShell and drive Hyper-V on Windows.)
- **Interface shaped like Intel only.** Phase 5 is the check; leave time to
  rework traits afterward.
- **Merge conflicts.** The maintainer commits often (last commit 2026-10-31).
  Keep PRs small, rebase often, open an issue first.
- **QEMU forgives more than real chips.** AMD working in QEMU does not prove
  it works on real silicon.
- Do not commit to the maintainer's branch; work on your own fork.

## 6. Reference: how RVM1.5 splits Intel and AMD

Repo: rcore-os/RVM1.5, folder `src/arch/x86_64/` (read on GitHub; the gitlink
mirror returned no listing). Not every file was read.

```
src/arch/x86_64/
  intel/  ept.rs mod.rs structs.rs vcpu.rs vmexit.rs
  amd/    iommu.rs mod.rs npt.rs vcpu.rs vmexit.rs
  vmm.rs  (shared logic + the vendor switch)
  mod.rs, context.rs, cpu.rs, cpuid.rs, entry.rs, exception.rs,
  page_table.rs, percpu.rs, segmentation.rs, serial.rs, tables.rs
```

- A compile-time Cargo feature (`intel` or `amd`) picks the `vendor` module
  in `vmm.rs`; only one is built.
- Both folders export the same names: `Vcpu`, `NestedPageTable`,
  `check_hypervisor_feature()`. The rest of the code never mentions a vendor.
- One shared trait, `VcpuAccessGuestState` (regs, RIP, RSP, RFLAGS, control
  registers), implemented over VMCS fields on Intel and VMCB fields on AMD.
- The exit handler (CPUID, MSR, hypercall) lives once in `vmm.rs`.
- Page tables are one generic `Level4PageTable<Addr, Entry, Instr>` with
  `EPTEntry`/`EPTInstr` on Intel and `NPTEntry`/`NPTInstr` on AMD.
- Difference from Aleph0: RVM1.5 is lighter (MSR handlers are stubs); Aleph0
  emulates MTRRs and forwards Hyper-V calls, so its interface will be larger.
- A compile-time switch is the simplest model; runtime traits are also
  possible. Decide in the issue discussion.

## 7. Resources

Official manuals
- Intel SDM Vol. 3C (VMX): https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
  (use Intel's page; chapter numbers differ between editions)
- AMD64 APM Vol. 2, System Programming (publication 24593): from AMD's site.
  Appendix C lists SVM exit codes.
- AMD SVM reference 33047 (old, short intro):
  https://kib.kiev.ua/x86docs/AMD/Pacifica/33047-r3.01.pdf (third-party mirror)

Code to read
- RVM1.5: https://github.com/rcore-os/RVM1.5 (Intel+AMD in Rust)
- SimpleSvm (AMD, C++, educational): https://github.com/tandasat/simplesvm
- illusion-rs (Rust UEFI type-1; sources disagreed on its AMD support, check
  its README): https://github.com/memN0ps/illusion-rs
- Linux KVM `arch/x86/kvm/` (`vmx/` and `svm/` behind `kvm_x86_ops`): the best
  model for which operations belong in the interface (from memory, not
  verified in this session).
- No Rust project with a clean trait-based Intel/AMD layer was found.

Articles / books
- LevelBlue "Hypervisor development in Rust" series:
  https://www.levelblue.com/blogs/spiderlabs-blog/hypervisor-development-in-rust-for-security-researchers-part-1
- "Building a Hypervisor in Rust" (Leanpub):
  https://leanpub.com/buildingahypervisorinrust

Suggested reading order: KVM's `kvm_x86_ops` list, then RVM1.5, then AMD APM
Vol. 2 SVM chapters next to Intel SDM VMX chapters (VMCS vs VMCB, EPT vs NPT).

## 8. How to resume on another machine

Clone your fork, read these two files, and tell Claude Code: "read
hal-notes/HAL_PLAN.md and ARCHITECTURE.md and continue from Phase 0".
Open question still to answer: do you have an Intel machine with Hyper-V to
verify Intel behavior after each refactor step?
