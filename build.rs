//! Ensure new test cases are picked up without needing to clean the build.

fn main() {
    println!("cargo:rerun-if-changed=test_cases");

    println!("cargo:rustc-check-cfg=cfg(docker_available)");
    println!("cargo:rerun-if-env-changed=DOCKER_HOST");

    if std::env::var("DOCKER_HOST").is_ok() {
        println!("cargo:rustc-cfg=docker_available");
    }
}
