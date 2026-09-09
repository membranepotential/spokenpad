fn main() {
    // Native libraries are distributed beside the executable by sherpa-onnx-sys.
    // Dependency build scripts' link arguments don't propagate to this binary.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/..");
}
