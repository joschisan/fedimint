// Keep this bin as a real honggfuzz target, not `fn main() {}`. The Nix
// `quickFuzz` deps phase builds only this bin via `--bin noop` to populate
// the cargo artifacts cache. Cargo only wires honggfuzz's
// `cargo:rustc-link-lib=hfuzz` into a bin that actually pulls a symbol from
// the honggfuzz crate — a plain `fn main` elides `-lhfuzz` and the sancov
// runtime (`__sanitizer_cov_trace_pc_guard`, `__sancov_lowest_stack`, …)
// fails to resolve at link time.
use honggfuzz::fuzz;

fn main() {
    loop {
        fuzz!(|_data: &[u8]| {});
    }
}
