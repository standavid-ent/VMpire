//! `hal::NestedPaging` for VT-x: the EPT map (`Ept`) and the tables the exit
//! handler changes (`TypeTables`), over the original functions of `ept`.
//! The lines it prints are the ones bring-up and the exit report printed.

use core::fmt;

use super::ept::{
    self, EPT_EXECUTE, EPT_READ, EPT_WRITE, Ept, EptError, EptSupport, Eptp, InveptType, ProtectedPage, TypeTables,
};
use super::instruction::VmxFail;
use super::vmcs::fields::EPT_POINTER;
use super::vmexit::read_field;
use crate::hal::ControlCapabilities;
use crate::hal::types::{Flush, Hidden, Permissions, Retyped, WalkEnd};
use crate::x86::mtrr::{MtrrCapabilities, Mtrrs};
use crate::x86::page::Allocation;

/// The EPT identity map `Ept::build` made.
pub struct NestedPaging(Ept);

impl NestedPaging {
    /// The original map, for the VMCS.
    pub(super) fn ept(&self) -> &Ept {
        &self.0
    }
}

fn flush(invept: InveptType) -> Flush {
    match invept {
        InveptType::SingleContext => Flush::SingleContext,
        InveptType::AllContext => Flush::AllContexts,
    }
}

fn invept(flush: Flush) -> InveptType {
    match flush {
        Flush::SingleContext => InveptType::SingleContext,
        Flush::AllContexts => InveptType::AllContext,
    }
}

/// The neutral walk end of an EPT walk.
fn walk_end(end: ept::WalkEnd) -> WalkEnd {
    match end {
        ept::WalkEnd::Mapped {
            hpa,
            size,
            access,
            memory_type,
        } => WalkEnd::Mapped {
            hpa,
            size,
            permissions: Permissions {
                read: access & EPT_READ != 0,
                write: access & EPT_WRITE != 0,
                execute: access & EPT_EXECUTE != 0,
            },
            memory_type,
        },
        ept::WalkEnd::NotPresent => WalkEnd::NotPresent,
        ept::WalkEnd::Misconfigured => WalkEnd::Misconfigured,
        ept::WalkEnd::OutsideTables { table } => WalkEnd::OutsideTables { table },
        ept::WalkEnd::BeyondWalk => WalkEnd::BeyondWalk,
    }
}

/// The EPT walk end of a neutral one, for its text.
fn ept_walk_end(end: WalkEnd) -> ept::WalkEnd {
    match end {
        WalkEnd::Mapped {
            hpa,
            size,
            permissions,
            memory_type,
        } => ept::WalkEnd::Mapped {
            hpa,
            size,
            access: if permissions.read { EPT_READ } else { 0 }
                | if permissions.write { EPT_WRITE } else { 0 }
                | if permissions.execute { EPT_EXECUTE } else { 0 },
            memory_type,
        },
        WalkEnd::NotPresent => ept::WalkEnd::NotPresent,
        WalkEnd::Misconfigured => ept::WalkEnd::Misconfigured,
        WalkEnd::OutsideTables { table } => ept::WalkEnd::OutsideTables { table },
        WalkEnd::BeyondWalk => ept::WalkEnd::BeyondWalk,
    }
}

impl crate::hal::NestedPaging for NestedPaging {
    type Support = EptSupport;
    type Tables = TypeTables;
    type ProtectedPage = ProtectedPage;
    type Error = EptError;
    type FlushError = VmxFail;

    const NAME: &'static str = "EPT";
    const FLUSH: &'static str = "INVEPT";

    fn check_support(capabilities: &ControlCapabilities, physical_address_bits: u8) -> Result<EptSupport, EptError> {
        ept::check_support(capabilities.secondary, capabilities.ept_vpid_cap, physical_address_bits)
    }

    /// What the CPU's EPT offers: what Aleph0 requires of it, and what the
    /// map may use.
    fn report_support(support: EptSupport) {
        log!(
            "EPT supported: 4-level walk, WB, 2 MiB pages, INVEPT {}; 1 GiB pages={}; MAXPHYADDR {} bits.",
            support.invept.name(),
            support.rules.huge_pages,
            support.rules.physical_address_bits,
        );
    }

    fn physical_address_bits(support: EptSupport) -> u8 {
        support.rules.physical_address_bits
    }

    fn spare_tables(mtrr: MtrrCapabilities) -> usize {
        ept::spare_pts(mtrr)
    }

    unsafe fn build(
        protected_pages: &[u64],
        support: EptSupport,
        mtrrs: &Mtrrs,
        spare_tables: usize,
    ) -> Result<(Self, TypeTables), EptError> {
        // SAFETY: the caller's obligation, as for `Ept::build`.
        let (ept, types) = unsafe { Ept::build(protected_pages, support, mtrrs, spare_tables) }?;
        Ok((Self(ept), types))
    }

    /// The EPT identity map `Ept::build` made, and how its entries map each
    /// run of it: read back from the entries, so the lines show what the CPU
    /// walks.
    fn report(&self, types: &TypeTables, protected_pages: &[u64]) {
        let ept = &self.0;
        let layout = ept.layout();
        log!(
            "EPT built: identity 0x0..{:#x} ({} GiB), {} table pages: {} PDPT(s); {} PD(s), for 0x0..{:#x}, with {} PTs for 2 MiB regions mapped in 4 KiB pages and {} spare PTs for the guest's MTRR writes; {} GiB above as 1 GiB pages; {} pages read-only (R=1 W=0 X=0).",
            ept.end(),
            ept.end() >> 30,
            ept.table_pages(),
            layout.pdpts(),
            layout.pd_gibs(),
            layout.pd_end(),
            ept.split_regions(),
            ept.spare_pts(),
            layout.huge_gibs(),
            protected_pages.len(),
        );
        for page in protected_pages {
            log!("EPT read-only page: {page:#018x}.");
        }
        let ram = types.ram().ranges();
        log!(
            "EPT memory types: the MTRRs' for the {} RAM range(s) of the UEFI memory map, UC for everything else (MMIO, reserved memory, holes).",
            ram.len()
        );
        for &(start, end) in ram {
            log!("EPT RAM {start:#014x}..{end:#014x}");
        }
        for run in types.region_runs(0, ept.end()) {
            log!("EPT {:#014x}..{:#014x}: {}", run.start, run.end, run.mapping);
        }
    }

    fn memory_map_end(&self) -> u64 {
        self.0.memory_map_end()
    }

    unsafe fn hide(
        types: &mut TypeTables,
        allocations: impl Iterator<Item = Allocation>,
        mtrrs: &Mtrrs,
        visible: &[Allocation],
    ) -> Result<Hidden, EptError> {
        // SAFETY: the caller's obligation, as for `TypeTables::hide`.
        let hidden = unsafe { types.hide(allocations, mtrrs, visible) }?;
        Ok(Hidden {
            data_pages: hidden.data_pages,
            code_pages: hidden.code_pages,
            regions_split: hidden.regions_split,
        })
    }

    /// How many of Aleph0's own pages EPT now hides from the guest.
    fn report_hidden(hidden: Hidden) {
        log!(
            "Aleph0 pages hidden from the guest: {} data page(s) redirected to the EPT shadow, \
             {} code page(s) read-only, {} region(s) split into a PT.",
            hidden.data_pages,
            hidden.code_pages,
            hidden.regions_split,
        );
    }

    unsafe fn apply_memory_types(types: &mut TypeTables, mtrrs: &Mtrrs) -> Result<Retyped, VmxFail> {
        // SAFETY: the caller's obligation, as for `TypeTables::apply`.
        let retyped = unsafe { types.apply(mtrrs) }?;
        Ok(Retyped {
            regions: retyped.regions,
            span: retyped.span,
            split: retyped.split,
            merged: retyped.merged,
            uncached: retyped.uncached,
        })
    }

    fn flush_type(types: &TypeTables) -> Flush {
        flush(types.invept())
    }

    fn flush_name(flush: Flush) -> &'static str {
        invept(flush).name()
    }

    fn free_tables(types: &TypeTables) -> usize {
        types.free_pts()
    }

    fn walk(types: &TypeTables, gpa: u64) -> WalkEnd {
        walk_end(types.walk(types.eptp(), gpa).end)
    }

    fn fmt_walk_end(end: WalkEnd, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&ept_walk_end(end), f)
    }

    /// The walk of `gpa` through the EPT tables EPT_POINTER names: each entry
    /// on its path, what the CPU refuses in it, and how the walk ended
    /// (`ept::walk`), as KVM prints the hierarchy of an EPT entry with
    /// reserved bits set. The walk reads only Aleph0's own table pages.
    unsafe fn report_walk(types: Option<&TypeTables>, number: u64, gpa: u64) {
        let tag = crate::TAG;
        let eptp = match read_field(EPT_POINTER) {
            Ok(raw) => Eptp::from_raw(raw),
            Err(error) => {
                crate::serial_println!("{tag} VM exit #{number}: EPT_POINTER unreadable ({error:?}); no EPT walk.");
                return;
            }
        };
        let Some(types) = types else {
            crate::serial_println!("{tag} VM exit #{number}: the EPT tables are not available; no EPT walk.");
            return;
        };
        let walk = types.walk(eptp, gpa);
        crate::serial_println!(
            "{tag} VM exit #{number}: EPT walk of GPA {gpa:#018x} from EPTP {:#x}:",
            eptp.raw()
        );
        for entry in walk.entries() {
            crate::serial_println!(
                "{tag} VM exit #{number}:   {} entry {:#x} at {:#x} = {:#018x}: {}",
                entry.level.name(),
                entry.index,
                entry.address,
                entry.raw,
                entry.faults,
            );
        }
        crate::serial_println!("{tag} VM exit #{number}:   {}.", walk.end);
    }

    fn report_regions(types: &TypeTables, number: u64, start: u64, end: u64) {
        let tag = crate::TAG;
        for run in types.region_runs(start, end) {
            crate::serial_println!(
                "{tag} VM exit #{number}: EPT {:#014x}..{:#014x}: {}",
                run.start,
                run.end,
                run.mapping
            );
        }
    }

    #[cfg(feature = "ept-misconfig-test")]
    unsafe fn misconfigure(types: &TypeTables, gpa: u64) {
        // SAFETY: the caller's obligation, as for `TypeTables::misconfigure`.
        unsafe { types.misconfigure(gpa) }
    }

    fn protected_page_for(gpa: u64) -> Option<ProtectedPage> {
        ept::protected_page_for(gpa)
    }

    fn protected_page_gpa(page: ProtectedPage) -> u64 {
        page.gpa
    }

    fn nmi_stack_page() -> u64 {
        ept::nmi_stack_page()
    }

    unsafe fn grant_write(page: ProtectedPage) -> Result<Flush, VmxFail> {
        // SAFETY: the caller's obligation, as for `ProtectedPage::grant_write`.
        unsafe { page.grant_write() }.map(flush)
    }

    fn report_error(error: EptError) {
        match error {
            EptError::RangeTooLarge { end, limit } => log!(
                "EPT setup failed: the memory map reaches {end:#x}, past {limit:#x}, the end of the guest-physical address space (MAXPHYADDR, at most the 48 bits a 4-level EPT walk translates)."
            ),
            error => log!("EPT setup failed: {error:?}"),
        }
    }
}
