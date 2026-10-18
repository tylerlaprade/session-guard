//! A stand-in for a running terminal in the lockout tests: a process the
//! kernel names `alacritty`, alive until the test kills it.

fn main() {
    loop {
        std::thread::park();
    }
}
