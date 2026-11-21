//! Summon single-lookup provider. No arguments declines the stream-mode probe.
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let name = args.next();
    let Some(name) = name.as_deref().and_then(std::ffi::OsStr::to_str) else {
        eprintln!("usage: fridica-secret <reference>");
        return ExitCode::FAILURE;
    };
    if args.next().is_some() {
        eprintln!("expected one secret reference");
        return ExitCode::FAILURE;
    }
    match fridica::cli::secret::get(name) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fridica-secret: {error}");
            ExitCode::FAILURE
        }
    }
}
