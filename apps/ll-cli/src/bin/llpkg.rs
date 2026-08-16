use std::env;
use std::os::unix::process::CommandExt;
use std::process::{self, Command};

fn main() {
    let error = Command::new("ll-cli").args(env::args_os().skip(1)).exec();
    eprintln!("llpkg: failed to execute ll-cli: {error}");
    process::exit(127);
}
