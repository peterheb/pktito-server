// libpktito.a from a libpktito tree built with `make libpktito.a`: PKTITO_DIR,
// or ../libpktito beside this one. The archive is linked from a copy in
// OUT_DIR, as examples/rust does there: given -lpktito, macOS's linker takes
// libpktito.dylib over libpktito.a beside it, and `make shared` leaves one.
// It needs libpktito 0.9.6's interface (core/pktito.h).
fn main() {
    let dir = std::env::var("PKTITO_DIR")
        .unwrap_or_else(|_| format!("{}/../libpktito", env!("CARGO_MANIFEST_DIR")));
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::copy(format!("{dir}/libpktito.a"), format!("{out}/libpktito.a")).unwrap_or_else(|e| {
        panic!("{dir}/libpktito.a: {e} (make libpktito.a in a libpktito tree, and PKTITO_DIR=that tree)")
    });
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=pktito");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rerun-if-changed={dir}/libpktito.a");
    println!("cargo:rerun-if-env-changed=PKTITO_DIR");
}
