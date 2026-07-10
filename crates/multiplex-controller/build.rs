//! Expose the compile-time target triple to the controller at build time.
//! `TARGET` is only visible to build scripts, not to normal `env!()` use.

fn main() {
    println!(
        "cargo:rustc-env=TARGET_TRIPLE={}",
        std::env::var("TARGET").expect("cargo always sets TARGET for build scripts")
    );
}
