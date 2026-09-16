fn main() {
    // The shared gk-guest layout: sections at STACK_TOP and up, pure-text
    // exec segment, W^X PHDRS — the same script the C guests link with.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{manifest_dir}/../link.ld");
    println!("cargo:rerun-if-changed=../link.ld");
}
