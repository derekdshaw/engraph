use std::path::Path;
use std::process::Command;

/// `git -C dir`, ready for the subcommand and its arguments.
pub fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    c
}
