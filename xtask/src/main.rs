//! `cargo xtask <task>`: development tasks for the workspace.

mod dev;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

const USAGE: &str = "\
Usage: cargo xtask <task>

  dev                                      run the server and the web dev server
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["dev"] => dev::run(),
        _ => bail!("{USAGE}"),
    }
}

/// The workspace root: xtask lives one level below it.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace")
        .to_path_buf()
}
