// sherpa-onnx-sys only sets an rpath for itself; add one for this binary so
// the prebuilt shared libs (SHERPA_ONNX_LIB_DIR) are found at run time.
fn main() {
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    if let Ok(dir) = std::env::var("SHERPA_ONNX_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
