//! What Aleph0 offers the guest beyond the minimal controls, chosen once at
//! bring-up ([`GuestFeatures`]):
//!
//! - the optional features an OS guest needs (RDTSCP, INVPCID,
//!   XSAVES/XRSTORS, user wait and pause: `hal::types::OptionalFeatures`),
//!   each offered when the CPU's virtualization extension allows it. While
//!   one is not, its instructions raise #UD in the guest (on Intel: SDM Vol.
//!   3C, "Changes to Instruction Behavior in VMX Non-Root Operation");
//! - XSETBV, which always exits and which Aleph0 runs in root operation. That
//!   needs CR4.OSXSAVE in the host's CR4 ([`GuestFeatures::host_cr4`]).
//!
//! What is not offered is hidden from the guest's CPUID
//! ([`GuestFeatures::hidden`], applied by `exit_decision::cpuid_policy`), so
//! the guest never runs an instruction that would #UD where hardware would
//! have run it. The exit handler reads the choice back with [`published`].

use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::hal::types::{CrConstraints, OptionalFeatures};
use crate::x86::cr::CR4_OSXSAVE;

/// CPUID.1:ECX bit 26 (XSAVE): XSAVE, XGETBV, XSETBV and XCR0 exist.
pub const CPUID_1_ECX_XSAVE: u32 = 1 << 26;
/// CPUID.1:ECX bit 27 (OSXSAVE): a copy of CR4.OSXSAVE.
pub const CPUID_1_ECX_OSXSAVE: u32 = 1 << 27;
/// CPUID leaf of the XSAVE components: subleaf 0 reports in EDX:EAX the XCR0
/// bits the CPU supports.
const XSAVE_LEAF: u32 = 0xD;

/// CPUID bits, by register, the guest must not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HiddenCpuid {
    /// CPUID.1:ECX.
    pub leaf_1_ecx: u32,
    /// CPUID.(EAX=7,ECX=0):EBX.
    pub leaf_7_ebx: u32,
    /// CPUID.(EAX=7,ECX=0):ECX.
    pub leaf_7_ecx: u32,
    /// CPUID.(EAX=0xD,ECX=1):EAX.
    pub leaf_d_1_eax: u32,
    /// CPUID.80000001H:EDX.
    pub extended_edx: u32,
}

impl HiddenCpuid {
    /// Nothing hidden.
    pub const NONE: Self = Self {
        leaf_1_ecx: 0,
        leaf_7_ebx: 0,
        leaf_7_ecx: 0,
        leaf_d_1_eax: 0,
        extended_edx: 0,
    };

    /// The bits of both.
    fn or(self, other: Self) -> Self {
        Self {
            leaf_1_ecx: self.leaf_1_ecx | other.leaf_1_ecx,
            leaf_7_ebx: self.leaf_7_ebx | other.leaf_7_ebx,
            leaf_7_ecx: self.leaf_7_ecx | other.leaf_7_ecx,
            leaf_d_1_eax: self.leaf_d_1_eax | other.leaf_d_1_eax,
            extended_edx: self.extended_edx | other.extended_edx,
        }
    }
}

/// The CPUID bits of the instructions each optional feature lets the guest
/// run, hidden while it is not offered.
fn optional_cpuid(features: OptionalFeatures) -> [(bool, HiddenCpuid); 4] {
    [
        // RDTSCP (CPUID.80000001H:EDX bit 27) and RDPID ((7,0):ECX bit 22).
        (
            features.rdtscp,
            HiddenCpuid {
                extended_edx: 1 << 27,
                leaf_7_ecx: 1 << 22,
                ..HiddenCpuid::NONE
            },
        ),
        // INVPCID ((7,0):EBX bit 10).
        (
            features.invpcid,
            HiddenCpuid {
                leaf_7_ebx: 1 << 10,
                ..HiddenCpuid::NONE
            },
        ),
        // XSAVES/XRSTORS ((0xD,1):EAX bit 3).
        (
            features.xsaves,
            HiddenCpuid {
                leaf_d_1_eax: 1 << 3,
                ..HiddenCpuid::NONE
            },
        ),
        // TPAUSE, UMONITOR, UMWAIT: WAITPKG ((7,0):ECX bit 5).
        (
            features.waitpkg,
            HiddenCpuid {
                leaf_7_ecx: 1 << 5,
                ..HiddenCpuid::NONE
            },
        ),
    ]
}

/// What Aleph0 offers the guest, chosen at bring-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFeatures {
    /// The optional features offered
    /// (`HardwareVirtualization::optional_features`).
    pub features: OptionalFeatures,
    /// The XCR0 bits the guest's XSETBV may set: CPUID.(EAX=0xD,ECX=0):EDX:EAX,
    /// or 0 when XSETBV cannot run in root operation, so that every XSETBV
    /// gets #GP. Bit 0 (x87) is always reported, so 0 means only that.
    pub xcr0: u64,
}

impl GuestFeatures {
    /// No optional feature and no XSETBV: what the exit handler reads before
    /// bring-up has published.
    pub const NONE: Self = Self {
        features: OptionalFeatures::NONE,
        xcr0: 0,
    };

    /// The features for a CPU whose virtualization extension offers
    /// `features`, with CPUID.1:ECX `cpuid_1_ecx`, the CR4 bits root mode
    /// allows (`cr4_may_be_one`), and XCR0 bits `xcr0_supported`
    /// (CPUID.(0xD,0), meaningful only with XSAVE). XSETBV runs in root
    /// operation only with XSAVE and with CR4.OSXSAVE allowed in root mode.
    pub fn new(features: OptionalFeatures, cpuid_1_ecx: u32, cr4_may_be_one: u64, xcr0_supported: u64) -> Self {
        let xsetbv = cpuid_1_ecx & CPUID_1_ECX_XSAVE != 0 && cr4_may_be_one & CR4_OSXSAVE != 0;
        Self {
            features,
            xcr0: if xsetbv { xcr0_supported } else { 0 },
        }
    }

    /// Reads what [`GuestFeatures::new`] needs from the CPU besides
    /// `features` and `constraints`.
    pub fn read(features: OptionalFeatures, constraints: CrConstraints) -> Self {
        let cpuid_1_ecx = __cpuid(1).ecx;
        // Leaf 0xD exists whenever XSAVE does.
        let xcr0_supported = if cpuid_1_ecx & CPUID_1_ECX_XSAVE != 0 {
            let leaf = __cpuid_count(XSAVE_LEAF, 0);
            (u64::from(leaf.edx) << 32) | u64::from(leaf.eax)
        } else {
            0
        };
        Self::new(features, cpuid_1_ecx, constraints.cr4_may_be_one, xcr0_supported)
    }

    /// Whether Aleph0 runs the guest's XSETBV.
    pub fn xsetbv(self) -> bool {
        self.xcr0 != 0
    }

    /// The host's CR4 for the CR4 bring-up runs with (`cr4`): CR4.OSXSAVE
    /// added when Aleph0 runs XSETBV, which raises #UD in root operation
    /// without it. The guest's CR4 is captured on its own and keeps the
    /// firmware's OSXSAVE, and teardown loads the firmware's CR4 again.
    pub fn host_cr4(self, cr4: u64) -> u64 {
        if self.xsetbv() { cr4 | CR4_OSXSAVE } else { cr4 }
    }

    /// The CPUID bits the guest must not see: those of each optional feature
    /// not offered, and XSAVE when Aleph0 cannot run XSETBV.
    pub fn hidden(self) -> HiddenCpuid {
        let xsave = if self.xsetbv() {
            HiddenCpuid::NONE
        } else {
            HiddenCpuid {
                leaf_1_ecx: CPUID_1_ECX_XSAVE,
                ..HiddenCpuid::NONE
            }
        };
        optional_cpuid(self.features)
            .into_iter()
            .filter(|&(offered, _)| !offered)
            .fold(xsave, |hidden, (_, bits)| hidden.or(bits))
    }
}

/// The optional features as bits, for [`publish`].
fn feature_bits(features: OptionalFeatures) -> u8 {
    u8::from(features.rdtscp)
        | u8::from(features.invpcid) << 1
        | u8::from(features.xsaves) << 2
        | u8::from(features.waitpkg) << 3
}

/// The optional features of bits [`feature_bits`] made.
fn features_of(bits: u8) -> OptionalFeatures {
    OptionalFeatures {
        rdtscp: bits & 1 != 0,
        invpcid: bits & (1 << 1) != 0,
        xsaves: bits & (1 << 2) != 0,
        waitpkg: bits & (1 << 3) != 0,
    }
}

// What `publish` recorded, for the exit handler.
static FEATURES: AtomicU8 = AtomicU8::new(0);
static XCR0: AtomicU64 = AtomicU64::new(GuestFeatures::NONE.xcr0);

/// Records, before the guest runs, what the guest is offered.
pub fn publish(features: GuestFeatures) {
    FEATURES.store(feature_bits(features.features), Ordering::Relaxed);
    XCR0.store(features.xcr0, Ordering::Relaxed);
}

/// What [`publish`] recorded.
pub fn published() -> GuestFeatures {
    GuestFeatures {
        features: features_of(FEATURES.load(Ordering::Relaxed)),
        xcr0: XCR0.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/hv/features.rs"]
mod tests;
