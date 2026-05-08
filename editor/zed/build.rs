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
    if let Ok(value) = std::env::var("CARGO_TARGET_DIR") {
        // The check is gated on a non-empty value because some
        // wrappers explicitly export an empty string to clear the
        // variable.
        if !value.is_empty() {
            panic!(
                "\n\n\
                 Zed extension build refusing to run with CARGO_TARGET_DIR=\"{value}\".\n\
                 \n\
                 Zed expects the built `.wasm` at\n\
                 `<extension>/target/wasm32-wasip1/release/zed_keron_extension.wasm`,\n\
                 but with `CARGO_TARGET_DIR` set, cargo writes it to the global cache\n\
                 instead. Launch Zed without that variable, e.g.:\n\
                 \n\
                     CARGO_TARGET_DIR= open -a Zed   (macOS GUI)\n\
                     CARGO_TARGET_DIR= zed .         (terminal)\n\
                 \n\
                 or remove the `set -gx CARGO_TARGET_DIR …` from your shell rc and\n\
                 reopen Zed.\n\n"
            );
        }
    }
}
