//! `hal::Intercepts` for VT-x: the MSR and I/O bitmaps (`exit_bitmaps`).

use super::exit_bitmaps::{ExitBitmaps, ExitBitmapsError, MsrAccess, msr_bit};
use crate::hal::types::GuestKind;
use crate::hv::intercepted::intercepted;
use crate::x86::mtrr::{self, MtrrCapabilities};

/// The MSR bitmap and the two I/O bitmaps.
pub struct Intercepts(ExitBitmaps);

impl Intercepts {
    /// The original bitmaps, for the VMCS.
    pub(super) fn bitmaps(&self) -> &ExitBitmaps {
        &self.0
    }
}

impl crate::hal::Intercepts for Intercepts {
    type Error = ExitBitmapsError;

    fn allocate(kind: GuestKind) -> Result<Self, ExitBitmapsError> {
        ExitBitmaps::allocate(kind).map(Self)
    }

    fn covers_msr(msr: u32) -> bool {
        msr_bit(msr, MsrAccess::Read).is_some()
    }

    /// Where the bitmaps are and what they intercept: the MSRs passed
    /// through, every MTRR address, of which the guest's copy holds those
    /// `mtrr` lists, and the ports.
    fn report(&self, kind: GuestKind, mtrr: MtrrCapabilities) {
        let bitmaps = &self.0;
        log!(
            "Exit bitmaps allocated: MSR={:#018x} I/O A={:#018x} I/O B={:#018x}.",
            bitmaps.msr_address(),
            bitmaps.io_a_address(),
            bitmaps.io_b_address(),
        );
        let (msrs, ports) = intercepted(kind);
        log!("Intercepted MSRs (read and write) for the {kind:?} guest: {msrs:x?}; intercepted ports: {ports:x?}.");
        let addresses = mtrr::every_msr_address().count();
        match mtrr.msrs().count() {
            0 => log!(
                "Intercepted MTRR addresses (read and write): all {addresses}, which get #GP: the CPU has no MTRRs."
            ),
            count => log!(
                "Intercepted MTRR addresses (read and write): all {addresses}. The guest reads and writes a copy of the {count} MSRs the CPU has (IA32_MTRR_DEF_TYPE, {} fixed-range MSRs and {} variable ranges); the other {} get #GP.",
                if mtrr.fixed() { mtrr::FIXED_MSRS } else { 0 },
                mtrr.variable_count(),
                addresses - count,
            ),
        }
    }

    fn report_error(error: ExitBitmapsError) {
        log!("Exit bitmap allocation failed: {error:?}");
    }
}
