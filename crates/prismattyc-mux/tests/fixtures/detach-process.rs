#![forbid(unsafe_code)]

fn main() {
    let ready = std::env::var_os("PMUX_DETACH_FIXTURE_READY")
        .expect("PMUX_DETACH_FIXTURE_READY must identify the readiness file");
    std::fs::write(ready, b"ready").expect("write readiness file");
    std::thread::sleep(std::time::Duration::from_secs(30));
}
