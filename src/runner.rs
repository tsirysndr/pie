//! Step execution: run a shell script, stream both output channels into the
//! reporter as they arrive, and fail loudly.

use crate::ui::Step as StepUi;
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// Runs `script` under `bash -euo pipefail`, streaming output into `ui`.
///
/// stdout and stderr are read on separate threads and merged through a channel,
/// so a step that writes progress to stderr (which most build systems do) still
/// animates the spinner.
pub fn shell(
    script: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    ui: &mut StepUi,
) -> Result<()> {
    if !cwd.is_dir() {
        bail!("working directory {} does not exist", cwd.display());
    }

    let mut command = Command::new("bash");
    command
        .arg("-euo")
        .arg("pipefail")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (key, value) in env {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start bash (is it installed and on PATH?): {e}"))?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let (tx, rx) = mpsc::channel::<String>();
    let tx_err = tx.clone();

    let out_thread = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_thread = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx_err.send(line).is_err() {
                break;
            }
        }
    });

    for line in rx {
        ui.line(&line);
    }

    // Both senders are dropped by now, so the readers have finished.
    let _ = out_thread.join();
    let _ = err_thread.join();

    let status = child.wait()?;
    if !status.success() {
        match status.code() {
            Some(code) => bail!("command exited with status {code}"),
            None => bail!("command was killed by a signal"),
        }
    }
    Ok(())
}

/// Captures a command's stdout instead of streaming it. Used for the few places
/// that need a value rather than a log, such as reading a variable back out of a
/// generated Makefile.
pub fn capture(script: &str, cwd: &Path, env: &BTreeMap<String, String>) -> Result<String> {
    let mut command = Command::new("bash");
    command
        .arg("-euo")
        .arg("pipefail")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }

    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
