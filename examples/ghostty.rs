//! A stand-in for Ghostty in the fresh-terminal tests: each argument is one
//! terminal, started as a child the way Ghostty starts each surface.

use std::process::Command;

fn main() {
    let terminals: Vec<_> = std::env::args()
        .skip(1)
        .map(|terminal| {
            let mut words = terminal.split_whitespace();
            Command::new(words.next().expect("empty terminal command"))
                .args(words)
                .spawn()
                .expect("failed to start terminal")
        })
        .collect();
    for mut terminal in terminals {
        terminal.wait().expect("failed to wait for terminal");
    }
}
