//! Checks untrusted models in a separate `rojo` process.
//!
//! A malformed model can make a decoder abort the process: an allocation sized
//! from a count the file declares fails, or deeply nested XML overflows the
//! stack. Neither can be caught inside the process that decodes, so each
//! check runs in a child process with a memory cap and a time limit. A model
//! that crashes the decoder fails only its own import, and `rojo serve` keeps
//! running. The structural checks in `untrusted_model` still run first, so most
//! bad models are rejected with a specific reason before anything allocates.

use std::{
    io::{self, Read, Write},
    process, thread,
    time::Duration,
};

#[cfg(not(test))]
use std::{
    process::{Command, Stdio},
    time::Instant,
};

#[cfg(not(test))]
use anyhow::Context;

use super::studio::{validate_import_model, DECODE_STACK_BYTES, MAX_MODEL_BYTES};

/// The hidden `rojo` subcommand that runs one check. It reads the model from
/// stdin and writes the re-encoded model, or the reason it was rejected, to
/// stdout.
pub const SUBCOMMAND: &str = "check-untrusted-model";
/// The exit code for a model that was decoded and rejected. Any other failure
/// (a crash, the memory cap, being killed) means it could not be decoded.
const REJECTED: i32 = 3;
/// Well-formed models within the 5 MiB input limit decode to far less.
const CHILD_MEMORY_LIMIT: usize = 768 * 1024 * 1024;
#[cfg(not(test))]
const MAX_ERROR_BYTES: u64 = 16 * 1024;

/// Decodes and validates a model, returning it as a binary model when
/// `encode` is set and nothing otherwise.
fn check(bytes: &[u8], allow_scripts: bool, encode: bool) -> anyhow::Result<Vec<u8>> {
    let tree = validate_import_model(bytes, allow_scripts)?;
    if !encode {
        return Ok(Vec::new());
    }
    let mut binary = Vec::new();
    rbx_binary::to_writer(&mut binary, &tree, tree.root().children())?;
    anyhow::ensure!(
        binary.len() <= MAX_MODEL_BYTES,
        "Decoded import exceeds 5MiB"
    );
    Ok(binary)
}

/// Runs one check as the child process. Exits the process on rejection.
pub fn run_child(allow_scripts: bool, encode: bool) -> anyhow::Result<()> {
    crate::memory_cap::set_memory_limit(CHILD_MEMORY_LIMIT);
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_MODEL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    // The decoders recurse once per nesting level and need more stack than
    // the main thread has.
    let result = thread::Builder::new()
        .stack_size(DECODE_STACK_BYTES)
        .spawn(move || check(&bytes, allow_scripts, encode))?
        .join()
        .map_err(|_| anyhow::anyhow!("Import model could not be decoded"))?;
    let mut stdout = io::stdout().lock();
    match result {
        Ok(output) => {
            stdout.write_all(&output)?;
            stdout.flush()?;
            Ok(())
        }
        Err(error) => {
            let _ = write!(stdout, "{error:#}");
            let _ = stdout.flush();
            process::exit(REJECTED);
        }
    }
}

/// What `rojo studio import` and `rojo studio backup restore` do to a model
/// before sending it to Studio: the isolated check, then re-encoding it as a
/// binary model, which must fit the 5 MiB limit too. The server runs this on
/// an automatic backup before saving it, so it only saves backups that
/// restore would accept. Returns the re-encoded model.
pub(crate) fn prepare_restore(
    bytes: Vec<u8>,
    allow_scripts: bool,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    check_untrusted_model(bytes, allow_scripts, true, timeout)
}

/// Validates an untrusted model without risking the calling process, and
/// returns it as a binary model when `encode` is set. Blocks for at most
/// about `timeout`.
pub(crate) fn check_untrusted_model(
    bytes: Vec<u8>,
    allow_scripts: bool,
    encode: bool,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    #[cfg(not(test))]
    return check_in_child(bytes, allow_scripts, encode, timeout);
    // Unit tests run inside the test harness, which is not the `rojo` binary
    // and cannot run the subcommand. The child is tested end to end instead.
    #[cfg(test)]
    {
        let _ = timeout;
        thread::Builder::new()
            .stack_size(DECODE_STACK_BYTES)
            .spawn(move || check(&bytes, allow_scripts, encode))?
            .join()
            .map_err(|_| anyhow::anyhow!("Import model could not be decoded"))?
    }
}

#[cfg(not(test))]
fn check_in_child(
    bytes: Vec<u8>,
    allow_scripts: bool,
    encode: bool,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    // On Linux, `current_exe` names a deleted file once the binary has been
    // replaced while serve runs, but `/proc/self/exe` still runs this one.
    #[cfg(target_os = "linux")]
    let executable = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe()
        .context("Could not find the Rojo executable to check the import")?;
    let mut command = Command::new(executable);
    command.arg(SUBCOMMAND);
    if allow_scripts {
        command.arg("--allow-scripts");
    }
    if encode {
        command.arg("--encode");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A server started without a console must not open one per check.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .context("Could not start the import model check")?;
    let mut stdin = child
        .stdin
        .take()
        .context("The import check has no stdin")?;
    let mut stdout = child
        .stdout
        .take()
        .context("The import check has no stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("The import check has no stderr")?;
    // The pipes are serviced on their own threads so a full pipe cannot stall
    // the child, and output beyond the limits is drained and discarded.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stdout)
            .take(MAX_MODEL_BYTES as u64 + 1)
            .read_to_end(&mut output);
        let _ = io::copy(&mut stdout, &mut io::sink());
        output
    });
    let errors = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stderr).take(MAX_ERROR_BYTES).read_to_end(&mut output);
        let _ = io::copy(&mut stderr, &mut io::sink());
        output
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        let exited = match child.try_wait() {
            Ok(exited) => exited,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("Could not wait for the import model check");
            }
        };
        if let Some(status) = exited {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    let _ = writer.join();
    let output = reader.join().unwrap_or_default();
    let errors = errors.join().unwrap_or_default();
    let Some(status) = status else {
        anyhow::bail!("Checking the import model took too long");
    };
    match status.code() {
        Some(0) => {
            anyhow::ensure!(
                output.len() <= MAX_MODEL_BYTES,
                "Decoded import exceeds 5MiB"
            );
            Ok(output)
        }
        Some(REJECTED) => anyhow::bail!("{}", String::from_utf8_lossy(&output).trim()),
        _ => {
            log::warn!(
                "Rejected an import model that crashed its decoder ({status}): {}",
                String::from_utf8_lossy(&errors).trim()
            );
            anyhow::bail!("Import model could not be decoded")
        }
    }
}
