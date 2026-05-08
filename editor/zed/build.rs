//! Pre-build guard.
//!
//! Zed's `compile_rust_extension` shells out to
//! `cargo build --target wasm32-wasip1 --release --manifest-path
//! <ext>/Cargo.toml` and then looks for the artefact at
//! `<ext>/target/wasm32-wasip1/release/<crate>.wasm`. If the parent
//! environment exports `CARGO_TARGET_DIR` (common with sccache or
//! shared-cache fish setups), cargo writes the `.wasm` to the global
//! cache instead of the extension-local `target/`, and Zed gives up
//! with a generic "failed to compile Rust extension" message.
//!
//! Fail at build time with a precise message so users don't chase
//! the wrong tail.

fn main() {
    // Surface a few env values that have caused install failures
    // before; they appear under `Compiling zed-keron-extension …` in
    // `zed: open log` and make it obvious whether the bad env is the
    // active culprit.
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    println!(
        "cargo:warning=zed-keron-extension build.rs: CARGO_TARGET_DIR={target_dir:?}"
    );
    println!(
        "cargo:warning=zed-keron-extension build.rs: PATH first entries={:?}",
        path.split(':').take(5).collect::<Vec<_>>()
    );

    if !target_dir.is_empty() {
        panic!(
            "\n\n\
             Zed extension build refusing to run with CARGO_TARGET_DIR=\"{target_dir}\".\n\
             \n\
             Zed expects the built `.wasm` at\n\
             `<extension>/target/wasm32-wasip1/release/zed_keron_extension.wasm`,\n\
             but with `CARGO_TARGET_DIR` set, cargo writes it to the global cache\n\
             instead. Launch Zed without that variable, e.g.:\n\
             \n\
                 CARGO_TARGET_DIR= open -a Zed   (macOS GUI)\n\
                 CARGO_TARGET_DIR= zed .         (terminal)\n\
             \n\
             or open Zed from the Dock after fully quitting any Zed instance\n\
             that was launched from a fish session that still had the variable\n\
             exported.\n\n"
        );
    }
}
