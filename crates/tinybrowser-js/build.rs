fn main() {
    println!("cargo:rerun-if-changed=js/bootstrap.js");
    println!("cargo:rerun-if-changed=build.rs");
}
