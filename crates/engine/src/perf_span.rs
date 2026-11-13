//! Opt-in Tracy zones for contributors profiling a frame.
//!
//! `perf_span!("streaming/plan_cells")` names the rest of the enclosing block as one Tracy zone
//! when the `tracy` feature is on, and expands to nothing otherwise: no guard, no code, and the
//! name is not evaluated. Reuse the names the profiling report already records so a capture and
//! the report agree. Keep these out of per-entity loops; a zone per entity perturbs the frame it
//! measures.

/// Name the rest of the enclosing block as a Tracy zone (see the module docs).
#[cfg(feature = "tracy")]
macro_rules! perf_span {
    ($name:literal) => {
        let _perf_span_guard = ::bevy::log::info_span!($name).entered();
    };
}

/// Name the rest of the enclosing block as a Tracy zone (see the module docs).
#[cfg(not(feature = "tracy"))]
macro_rules! perf_span {
    ($name:literal) => {};
}

pub(crate) use perf_span;

#[cfg(test)]
mod tests {
    /// The macro is usable as a statement, twice in one block, with or without the feature.
    #[test]
    fn zones_compile_as_statements() {
        perf_span!("test/outer");
        perf_span!("test/inner");
    }
}
