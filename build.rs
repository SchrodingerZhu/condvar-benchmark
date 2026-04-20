fn main() {
    println!("cargo:rerun-if-changed=musl.c");
    println!("cargo:rerun-if-changed=llvm-mutex.cpp");
    println!("cargo:rerun-if-changed=llvm.cpp");
    println!("cargo:rerun-if-changed=llvm-new.cpp");
    println!("cargo:rustc-link-lib=pthread");

    cc::Build::new()
        .file("musl.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-pthread")
        .flag_if_supported("-O3")
        .compile("condvar_musl");

    cc::Build::new()
        .cpp(true)
        .file("llvm-mutex.cpp")
        .file("llvm.cpp")
        .file("llvm-new.cpp")
        .flag_if_supported("-std=c++17")
        .flag_if_supported("-pthread")
        .flag_if_supported("-O3")
        .compile("condvar_llvm");
}
