//! Ensure new test cases are picked up without needing to clean the build.

fn main() {
    #[cfg(test)]
    println!("cargo:rerun-if-changed=test_cases");
}
