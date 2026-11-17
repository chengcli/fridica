//! Activate an owner-configured virtual environment before a worker launch.
use super::{process::Launch, shell};
use anyhow::{bail, Result};
use std::path::Path;

pub fn activate(
    mut launch: Launch,
    transport: &str,
    environment: &str,
    home: Option<&Path>,
) -> Result<Launch> {
    if environment.is_empty() {
        return Ok(launch);
    }
    if !(environment.starts_with('/') || environment.starts_with("~/"))
        || environment.chars().any(char::is_control)
        || environment.split('/').any(|part| part == "..")
    {
        bail!("invalid Python environment path");
    }
    let environment = if transport == "local" {
        match (environment.strip_prefix("~/"), home) {
            (Some(tail), Some(home)) => home.join(tail).to_string_lossy().into_owned(),
            _ => environment.to_owned(),
        }
    } else {
        environment.to_owned()
    };
    let script = format!(
        "fridica_python_env={}\n\
         if [ ! -r \"$fridica_python_env/bin/activate\" ] || [ ! -x \"$fridica_python_env/bin/python\" ]; then\n\
         echo 'fridica: configured Python environment unavailable' >&2; exit 97; fi\n\
         if ! . \"$fridica_python_env/bin/activate\" >/dev/null 2>&1; then\n\
         echo 'fridica: Python environment activation failed' >&2; exit 97; fi\n\
         exec \"$@\"", shell::path(&environment));
    let wrap = |command: Vec<String>| {
        let mut args = vec![
            "/bin/sh".into(),
            "-c".into(),
            script.clone(),
            "fridica-python".into(),
        ];
        args.extend(command);
        args
    };
    match transport {
        "local" => launch.argv = wrap(launch.argv),
        "ssh" => {
            if launch.argv.first().map(String::as_str) != Some("ssh") {
                bail!("Python activation requires an SSH launch");
            }
            let command = launch
                .argv
                .last_mut()
                .ok_or_else(|| anyhow::anyhow!("missing SSH command"))?;
            *command = shell::join(&wrap(vec!["/bin/sh".into(), "-c".into(), command.clone()]));
        }
        _ => bail!("Python environment requires local or SSH transport"),
    }
    Ok(launch)
}

/// Fixed wrapper diagnostics are safe to classify without exposing shell output.
pub fn refused(status: i32, stderr: &[u8]) -> bool {
    // SSH may prepend a login banner; classify only our final fixed diagnostic.
    status == 97
        && [
            b"fridica: configured Python environment unavailable\n".as_slice(),
            b"fridica: Python environment activation failed\n".as_slice(),
        ]
        .iter()
        .any(|message| stderr.ends_with(message))
}
