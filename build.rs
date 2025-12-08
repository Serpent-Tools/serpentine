//! Ensure new test cases are picked up without needing to clean the build.

fn main() {
    println!("cargo:rerun-if-changed=test_cases");
}
