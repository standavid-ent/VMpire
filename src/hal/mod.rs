//! Hardware abstraction layer: the traits and neutral types the shared code
//! (`bring_up`, `hv`, `host`, `resident`) uses, and the backends that
//! implement them. `intel/` is the original VT-x code. See HAL_INTERFACE.md
//! and HAL_STRUCTURE.md.

pub mod intel;
