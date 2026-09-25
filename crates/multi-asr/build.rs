//! sherpa-onnx's build script copies its shared libraries into the target
//! directory next to the binaries, but its rpath flags only apply to its own
//! crate. Point the loader at the binary's directory ($ORIGIN) and its parent
//! (for test executables in `deps/`). Ship the libraries next to the binary.
fn main() {
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/..");
    println!("cargo:rerun-if-changed=build.rs");
}
