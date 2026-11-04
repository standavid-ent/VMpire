//! Unit tests for `src/hal/intel/virtualization.rs`: the optional guest
//! features a secondary capability offers, and the controls they request.

use super::*;
use crate::hal::HardwareVirtualization as _;
use crate::hal::intel::msr::ControlCapability;

/// The capability MSRs with secondary capability `secondary` (allowed bits
/// in the high half), the rest 0.
fn with_secondary(secondary: u64) -> VmcsControlMsrs {
    VmcsControlMsrs {
        basic: 0,
        pinbased: ControlCapability(0),
        primary: ControlCapability(0),
        exit: ControlCapability(0),
        entry: ControlCapability(0),
        secondary: ControlCapability(secondary << 32),
        ept_vpid_cap: 0,
    }
}

#[test]
fn hyper_v_offers_every_optional_control() {
    // The secondary capability Hyper-V reports (docs/EPT.md):
    // `required=0x00000000 allowed=0x065018ae`.
    let features = HardwareVirtualization::optional_features(&with_secondary(0x0650_18AE));
    assert_eq!(
        features,
        OptionalFeatures {
            rdtscp: true,
            invpcid: true,
            xsaves: true,
            waitpkg: true,
        }
    );
    // Bits 1 (EPT), 3, 12, 20 and 26.
    assert_eq!(secondary_controls(features), 0x0410_100A);
}

#[test]
fn a_control_the_cpu_does_not_allow_is_not_requested() {
    let ept_only = HardwareVirtualization::optional_features(&with_secondary(u64::from(SECONDARY_ENABLE_EPT)));
    assert_eq!(ept_only, OptionalFeatures::NONE);
    assert_eq!(secondary_controls(ept_only), SECONDARY_ENABLE_EPT);
    let no_rdtscp =
        HardwareVirtualization::optional_features(&with_secondary(0x0650_18AE & !u64::from(SECONDARY_ENABLE_RDTSCP)));
    assert!(!no_rdtscp.rdtscp);
    assert_eq!(secondary_controls(no_rdtscp) & SECONDARY_ENABLE_RDTSCP, 0);
}
