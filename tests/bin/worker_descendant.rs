//! E2E fixture that remains alive until its owning worker process tree dies.

fn main() {
    std::thread::sleep(std::time::Duration::from_secs(300));
}
