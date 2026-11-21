//! How the embedded Python helpers are started.

/// The argv for a helper: the system interpreter, isolated (`-I`) and without
/// `site` (`-S`), given the helper's source and then its arguments.
pub fn argv(parts: Vec<String>) -> Vec<String> {
    let mut argv: Vec<String> = ["/usr/bin/python3", "-I", "-S", "-c"]
        .map(str::to_owned)
        .into();
    argv.extend(parts);
    argv
}
