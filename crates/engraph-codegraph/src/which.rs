//! `PATH` lookup for the tools engraph runs. `Command` resolves `PATH` on
//! spawn by itself; this is for deciding before the spawn.

/// The file names `tool` can have: on Windows, also the `.exe` of a binary
/// and the `.cmd` shim npm installs for pnpm and yarn.
const SUFFIXES: &[&str] = if cfg!(windows) {
    &["", ".exe", ".cmd"]
} else {
    &[""]
};

/// Whether `tool` is a file in a directory of `PATH`.
pub(crate) fn on_path(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        SUFFIXES
            .iter()
            .any(|s| dir.join(format!("{tool}{s}")).is_file())
    })
}
