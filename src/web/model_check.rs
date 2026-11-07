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

/// What a check gives back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckOutput {
    /// Nothing: the model is only validated, as an import is.
    Validate,
    /// The model re-encoded as a binary model, validated again as the import
    /// that receives it would be.
    Encode,
    /// Like `Encode`, but a line `<class>\t<source hash>` for each root of
    /// the re-encoded model instead of the model.
    ReportSources,
}

/// Decodes and validates a model, and gives back what `output` asks for.
fn check(bytes: &[u8], allow_scripts: bool, output: CheckOutput) -> anyhow::Result<Vec<u8>> {
    let tree = validate_import_model(bytes, allow_scripts)?;
    if output == CheckOutput::Validate {
        return Ok(Vec::new());
    }
    let mut binary = Vec::new();
    rbx_binary::to_writer(&mut binary, &tree, tree.root().children())?;
    anyhow::ensure!(
        binary.len() <= MAX_MODEL_BYTES,
        "Decoded import exceeds 5MiB"
    );
    // Re-encoding can change the model, for example by joining instances of
    // one class into one chunk, so what is passed on is checked again, as
    // the import that receives it checks it.
    let tree = validate_import_model(&binary, allow_scripts)?;
    if output == CheckOutput::Encode {
        return Ok(binary);
    }
    let mut report = String::new();
    for root in tree.root().children() {
        let instance = tree
            .get_by_ref(*root)
            .ok_or_else(|| anyhow::anyhow!("The re-encoded model lost an instance"))?;
        let hash = match instance.properties.get(&rbx_dom_weak::ustr("Source")) {
            Some(rbx_dom_weak::types::Variant::String(source)) => {
                super::verify::hash_source(source).0.to_string()
            }
            _ => "-".to_owned(),
        };
        report.push_str(&format!("{}\t{hash}\n", instance.class));
    }
    Ok(report.into_bytes())
}

/// Runs one check as the child process. Exits the process on rejection.
pub fn run_child(allow_scripts: bool, encode: bool, report_sources: bool) -> anyhow::Result<()> {
    let output = if report_sources {
        CheckOutput::ReportSources
    } else if encode {
        CheckOutput::Encode
    } else {
        CheckOutput::Validate
    };
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
        .spawn(move || check(&bytes, allow_scripts, output))?
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
/// before sending it to Studio, which the server's import check then runs on
/// what it receives: the isolated check, then re-encoding it as a binary
/// model within the 5 MiB limit, then the import check on that. Returns the
/// re-encoded model.
pub(crate) fn validate_for_restore(
    bytes: Vec<u8>,
    allow_scripts: bool,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    check_untrusted_model(bytes, allow_scripts, true, timeout)
}

/// [`validate_for_restore`], reporting for each root of the model restore
/// would send its class and the hash of its source (`None` without one),
/// all within the same bounded child process.
pub(crate) fn restore_report(
    bytes: Vec<u8>,
    timeout: Duration,
) -> anyhow::Result<Vec<(String, Option<u32>)>> {
    let output = run_check(bytes, true, CheckOutput::ReportSources, timeout)?;
    String::from_utf8(output)
        .map_err(|_| anyhow::anyhow!("The model check gave an unreadable report"))?
        .lines()
        .map(|line| {
            let (class, hash) = line
                .split_once('\t')
                .ok_or_else(|| anyhow::anyhow!("The model check gave an unreadable report"))?;
            let hash =
                if hash == "-" {
                    None
                } else {
                    Some(hash.parse().map_err(|_| {
                        anyhow::anyhow!("The model check gave an unreadable report")
                    })?)
                };
            Ok((class.to_owned(), hash))
        })
        .collect()
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
    let output = if encode {
        CheckOutput::Encode
    } else {
        CheckOutput::Validate
    };
    run_check(bytes, allow_scripts, output, timeout)
}

fn run_check(
    bytes: Vec<u8>,
    allow_scripts: bool,
    output: CheckOutput,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    #[cfg(not(test))]
    return check_in_child(bytes, allow_scripts, output, timeout);
    // Unit tests run inside the test harness, which is not the `rojo` binary
    // and cannot run the subcommand. The child is tested end to end instead.
    #[cfg(test)]
    {
        let _ = timeout;
        thread::Builder::new()
            .stack_size(DECODE_STACK_BYTES)
            .spawn(move || check(&bytes, allow_scripts, output))?
            .join()
            .map_err(|_| anyhow::anyhow!("Import model could not be decoded"))?
    }
}

#[cfg(not(test))]
fn check_in_child(
    bytes: Vec<u8>,
    allow_scripts: bool,
    output: CheckOutput,
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
    match output {
        CheckOutput::Validate => {}
        CheckOutput::Encode => {
            command.arg("--encode");
        }
        CheckOutput::ReportSources => {
            command.arg("--encode").arg("--report-sources");
        }
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

/// A small model whose two groups of ModuleScripts each carry a 33 MiB
/// source in a chunk of their own, under the 64 MiB chunk limit, but which
/// re-encoding joins into one chunk over it, as the import of a restored
/// model would then see.
#[cfg(test)]
pub(crate) fn two_group_fixture() -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
        let compressed = lz4_flex::block::compress(data);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&compressed);
    }
    fn string(data: &mut Vec<u8>, text: &[u8]) {
        data.extend_from_slice(&(text.len() as u32).to_le_bytes());
        data.extend_from_slice(text);
    }
    // Referents as the format stores them: each the difference from the
    // last, zigzag-encoded, big-endian, and interleaved byte by byte.
    fn referents(data: &mut Vec<u8>, values: &[i32]) {
        let mut last = 0;
        let encoded: Vec<[u8; 4]> = values
            .iter()
            .map(|&value| {
                let delta = value - last;
                last = value;
                (((delta << 1) ^ (delta >> 31)) as u32).to_be_bytes()
            })
            .collect();
        for byte in 0..4 {
            for value in &encoded {
                data.push(value[byte]);
            }
        }
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"<roblox!\x89\xff\x0d\x0a\x1a\x0a");
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    let source = vec![b'x'; 33 * 1024 * 1024];
    for class in 0..2u32 {
        let mut data = class.to_le_bytes().to_vec();
        string(&mut data, b"ModuleScript");
        data.push(0);
        data.extend_from_slice(&1u32.to_le_bytes());
        referents(&mut data, &[class as i32]);
        chunk(&mut bytes, b"INST", &data);
    }
    for class in 0..2u32 {
        for (name, value) in [(&b"Name"[..], &b"Module"[..]), (b"Source", &source)] {
            let mut data = class.to_le_bytes().to_vec();
            string(&mut data, name);
            data.push(0x01);
            string(&mut data, value);
            chunk(&mut bytes, b"PROP", &data);
        }
    }
    let mut data = vec![0];
    data.extend_from_slice(&2u32.to_le_bytes());
    referents(&mut data, &[0, 1]);
    referents(&mut data, &[-1, -1]);
    chunk(&mut bytes, b"PRNT", &data);
    bytes.extend_from_slice(b"END\0");
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&9u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(b"</roblox>");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_validation_checks_what_re_encoding_makes() {
        let bytes = two_group_fixture();
        assert!(bytes.len() <= MAX_MODEL_BYTES, "{} bytes", bytes.len());
        // As it is, it passes the import check.
        check_untrusted_model(bytes.clone(), true, false, Duration::from_secs(60)).unwrap();
        // Re-encoded, its two groups become one chunk over the limit, which
        // the import of the restored model would refuse, so restore does.
        let error = validate_for_restore(bytes.clone(), true, Duration::from_secs(60)).unwrap_err();
        assert!(
            format!("{error:#}").contains("oversized chunk"),
            "{error:#}"
        );
        assert!(restore_report(bytes, Duration::from_secs(60)).is_err());
    }

    #[test]
    fn restore_reports_each_root_with_its_source_hash() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let script = dom.insert(
            root,
            InstanceBuilder::new("Script").with_property("Source", "print(1)"),
        );
        let folder = dom.insert(root, InstanceBuilder::new("Folder"));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[script, folder]).unwrap();
        let report = restore_report(bytes, Duration::from_secs(60)).unwrap();
        assert_eq!(
            report,
            vec![
                (
                    "Script".to_owned(),
                    Some(super::super::verify::hash_source("print(1)").0)
                ),
                ("Folder".to_owned(), None),
            ]
        );
    }
}
