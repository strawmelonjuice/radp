// rad-portable
// Copyright (C) 2026 MLC 'Strawmelonjuice' Bloeiman and contributors.
//
// This software is licensed under the European Union Public Licence (EUPL) v1.2.
// You may not use this work except in compliance with the Licence.
// You may obtain a copy of the Licence at: https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-12

use std::env;
use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;
use symlink::{symlink_auto, symlink_dir};

static TERMINATION_SIGNAL: AtomicI32 = AtomicI32::new(0);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("rad-portable, copyright (C) 2026 MLC 'Strawmelonjuice' Bloeiman and contributors.");
    let mut args = env::args().skip(1);
    // Grab the given path or...
    let raw_storage = PathBuf::from(
        args.next()
            // Show em a guide if they misdid the thing
            .ok_or("usage: rad-portable <path-to-storage-radicle-dir>")?,
    );

    // Okay, work the path into a... path we can use
    let storage_root = fs::canonicalize(&raw_storage).map_err(|e| {
        format!(
            "cannot resolve storage path {}: {}",
            raw_storage.display(),
            e
        )
    })?;
    let storage_meta = fs::symlink_metadata(&storage_root)?;
    if !storage_meta.is_dir() {
        return Err(format!(
            "storage path is not a directory: {}",
            storage_root.display()
        )
        .into());
    }
    let storage_data_dir = if storage_root.file_name().is_some_and(|n| n == "data") {
        storage_root.clone()
    } else {
        storage_root.join("data")
    };
    // New grounds !
    if !storage_data_dir.exists() {
        eprint!(
            "Data directory {0} does not exist. Create it and run `rad auth --alias a-portable-radicle` with RAD_HOME={0} and empty RAD_PASSPHRASE? [y/N]: ",
            storage_data_dir.display()
        );
        io::stderr().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_ascii_lowercase();
        if answer == "y" || answer == "yes" {
            fs::create_dir_all(&storage_data_dir)?;
            if !Command::new("rad")
                .args(["auth", "--alias", "a-portable-radicle"])
                .env("RAD_HOME", &storage_data_dir)
                .env("RAD_PASSPHRASE", "")
                .status()?
                .success()
            {
                return Err(
                    "`rad auth --alias a-portable-radicle` exited with non-zero status.".into(),
                );
            }
        } else {
            return Err("aborted initialization of Radicle data directory".into());
        }
    }
    let canonical_data_dir = fs::canonicalize(&storage_data_dir)?;
    eprintln!(
        "Using portable data directory: {}",
        canonical_data_dir.display()
    );
    let storage_dev = fs::symlink_metadata(&canonical_data_dir)?.dev();

    // Okay now we ready to get cracking. Wait who are we!?
    let uid = unsafe { getuid() };
    let ppid = unsafe { getppid() };
    install_termination_handlers()?;
    if let Some(signal) = take_termination_signal() {
        eprintln!("Received {} before startup; exiting.", signal_name(signal));
        return Ok(());
    }

    // Stop any node if it's there and we don't know about it.
    let original_rad_home = {
        let output = Command::new("rad")
            .args(["path"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok();
        if let Some(output) = output {
            if !output.status.success() {
                None
            } else {
                let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if raw.is_empty() {
                    None
                } else {
                    let path = PathBuf::from(raw);
                    Some(path)
                }
            }
        } else {
            None
        }
    };
    if let Some(signal) = take_termination_signal() {
        eprintln!("Received {} before startup; exiting.", signal_name(signal));
        return Ok(());
    }
    let runtime_base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let runtime_dir = create_runtime_dir(&runtime_base, std::process::id())?;
    let runtime_dir: &Path = &runtime_dir;
    eprintln!(
        "And using mapped runtime directory: {}",
        runtime_dir.display()
    );
    let runtime_node_dir = runtime_dir.join("node");

    let mut restart_local_node = false;
    let mut fish_rad_home_set = false;
    let session_result = (|| -> Result<(), Box<dyn std::error::Error>> {
        restart_local_node = stop_rad_node(None)?;
        if let Some(signal) = take_termination_signal() {
            eprintln!("Received {} during startup.", signal_name(signal));
            return Ok(());
        }
        fs::create_dir_all(&runtime_node_dir)?;
        eprint!("Linking Radicle home...");
        for entry in fs::read_dir(&canonical_data_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            eprint!("\rLinking {} ...", name.display());
            let src = entry.path();
            if name == "node" && src.is_dir() {
                for node_entry in fs::read_dir(&src)? {
                    let node_entry = node_entry?;
                    let node_name = node_entry.file_name();
                    if node_name == "control.sock" || node_entry.file_type()?.is_socket() {
                        continue;
                    }
                    symlink_auto(node_entry.path(), runtime_node_dir.join(&node_name))?;
                }
            } else if src.is_dir() {
                symlink_dir(&src, runtime_dir.join(&name))?;
            } else {
                symlink_auto(&src, runtime_dir.join(&name))?;
            }
        }
        eprintln!("\rLinked Radicle home.");

        if let Some(signal) = take_termination_signal() {
            eprintln!("Received {} during startup.", signal_name(signal));
            return Ok(());
        }
        fish_rad_home_set = true;
        if !set_fish_rad_home(runtime_dir)? {
            return Err("failed to set universal RAD_HOME in fish".into());
        }
        if let Some(signal) = take_termination_signal() {
            eprintln!("Received {} during startup.", signal_name(signal));
            return Ok(());
        }
        stop_rad_node(Some(runtime_dir))?;
        if let Some(signal) = take_termination_signal() {
            eprintln!("Received {} during startup.", signal_name(signal));
            return Ok(());
        }
        eprintln!("Starting portable node in {}", runtime_dir.display());
        let start_status = Command::new("rad")
            .args(["node", "start"])
            .env("RAD_HOME", runtime_dir)
            .status()?;
        if !start_status.success() {
            return Err(format!(
                "Starting Radicle node failed with status {}. The used RAD_HOME was '{}'",
                start_status,
                runtime_dir.display()
            )
            .into());
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let exit_reason = Arc::new(std::sync::Mutex::new(String::from("user requested stop")));

        // File system watcher
        {
            let stop_flag = Arc::clone(&stop_flag);
            let exit_reason = Arc::clone(&exit_reason);
            let watch_path = canonical_data_dir.clone();

            thread::spawn(move || {
                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }

                    match fs::symlink_metadata(&watch_path) {
                        Err(_) => {
                            request_stop(
                                &stop_flag,
                                &exit_reason,
                                format!("storage path disappeared: {}", watch_path.display()),
                            );
                            break;
                        }
                        Ok(meta) => {
                            if !meta.is_dir() || meta.dev() != storage_dev {
                                request_stop(
                                    &stop_flag,
                                    &exit_reason,
                                    format!(
                                        "storage unmounted or remounted (dev {} -> {})",
                                        storage_dev,
                                        meta.dev()
                                    ),
                                );
                                break;
                            }
                        }
                    }

                    if !Path::new(&format!("/proc/{}", ppid)).exists() {
                        request_stop(
                            &stop_flag,
                            &exit_reason,
                            format!("parent process {} exited", ppid),
                        );
                        break;
                    }

                    thread::sleep(Duration::from_millis(250));
                }
            });
        }

        // Radicle node status watcher
        {
            let stop_flag = Arc::clone(&stop_flag);
            let exit_reason = Arc::clone(&exit_reason);
            let rad_dir = runtime_dir.to_path_buf();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(3000));

                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }

                    match rad_node_status(&rad_dir) {
                        Ok(Some(raw)) => {
                            if !raw.contains("running with") {
                                request_stop(
                                    &stop_flag,
                                    &exit_reason,
                                    "The radicle node has externally been stopped.".to_string(),
                                );
                                break;
                            }
                            // It's still going!
                        }
                        Ok(None) => {
                            eprintln!(
                                "No longer able to watch Radicle node: no Radicle profile was found."
                            );
                            eprintln!(
                                "`rad stop` will no longer automatically stop radicle-portable."
                            );
                            break;
                        }
                        Err(error) => {
                            eprintln!("No longer able to watch Radicle node: {error}");
                            eprintln!(
                                "`rad stop` will no longer automatically stop radicle-portable."
                            );
                            break;
                        }
                    };
                    thread::sleep(Duration::from_millis(600));
                }
            });
        }

        eprintln!("Active RAD_HOME: {}", runtime_dir.display());
        eprintln!(
            "Watching storage dev {} at {} (ppid {})",
            storage_dev,
            canonical_data_dir.display(),
            ppid
        );
        // SIGINT,SIGTERM, SIGHUP and SIGQUIT all work.
        eprintln!("Send [control+c] or run `rad node stop` to stop cleanly.");
        eprintln!();
        eprintln!(
            "If this drive gets prematurely disconnected, an attempt will be made to stop radicle before more damage occurs."
        );

        while !stop_flag.load(Ordering::Relaxed) {
            if let Some(signal) = take_termination_signal() {
                request_stop(
                    &stop_flag,
                    &exit_reason,
                    format!("received {}", signal_name(signal)),
                );
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let reason = exit_reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();

        eprintln!("Stopping node ({reason})...");

        Ok(())
    })();

    let cleanup_result = cleanup_session(
        runtime_dir,
        &canonical_data_dir,
        storage_dev,
        original_rad_home,
        restart_local_node,
        fish_rad_home_set,
        uid,
    );
    match (session_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(session_error), Ok(())) => Err(session_error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(session_error), Err(cleanup_error)) => {
            Err(format!("{session_error}; cleanup also failed: {cleanup_error}").into())
        }
    }
}

fn rad_node_status(rad_home: &Path) -> io::Result<Option<String>> {
    rad_node_status_for(Some(rad_home))
}

fn rad_node_status_for(rad_home: Option<&Path>) -> io::Result<Option<String>> {
    let mut command = Command::new("rad");
    command
        .args(["node", "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = rad_home {
        command.env("RAD_HOME", path);
    }
    let output = command.output()?;
    if !output.status.success() {
        if is_missing_profile_error(&output.stdout, &output.stderr) {
            return Ok(None);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(io::Error::other(format!(
            "`rad node status` exited with {}: {} {}",
            output.status,
            stderr.trim(),
            stdout.trim()
        )));
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.contains("Node is"))
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| io::Error::other("could not parse `rad node status` output"))
}

fn is_missing_profile_error(stdout: &[u8], stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).contains("Radicle profile not found in")
        || String::from_utf8_lossy(stderr).contains("Radicle profile not found in")
}

fn stop_rad_node(rad_home: Option<&Path>) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(status_line) = rad_node_status_for(rad_home)? else {
        return Ok(false);
    };
    if !status_line.contains("running with") {
        return Ok(true);
    }

    let mut command = Command::new("rad");
    command.args(["node", "stop"]);
    if let Some(path) = rad_home {
        command.env("RAD_HOME", path);
    }
    let status = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("failed to stop the Radicle node".into());
    }
    Ok(true)
}

fn set_fish_rad_home(path: &Path) -> io::Result<bool> {
    Ok(Command::new("fish")
        .args(["-c", "set -Ux RAD_HOME $argv[1]"])
        .arg(path)
        .status()?
        .success())
}

fn install_termination_handlers() -> io::Result<()> {
    for signal in [1, 2, 3, 15] {
        let previous =
            unsafe { signal_handler(signal, handle_termination_signal as *const () as usize) };
        if previous == usize::MAX {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

extern "C" fn handle_termination_signal(signal: i32) {
    TERMINATION_SIGNAL.store(signal, Ordering::SeqCst);
}

fn take_termination_signal() -> Option<i32> {
    match TERMINATION_SIGNAL.swap(0, Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        15 => "SIGTERM",
        _ => "termination signal",
    }
}

fn request_stop(stop_flag: &AtomicBool, exit_reason: &std::sync::Mutex<String>, reason: String) {
    let mut exit_reason = exit_reason
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !stop_flag.load(Ordering::Relaxed) {
        *exit_reason = reason;
        stop_flag.store(true, Ordering::SeqCst);
    }
}

fn create_runtime_dir(base: &Path, pid: u32) -> io::Result<PathBuf> {
    let dir = base.join(format!("radp.{pid}"));
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder.create(&dir)?;

    if let Err(error) = fs::set_permissions(&dir, Permissions::from_mode(0o700)) {
        let _ = fs::remove_dir(&dir);
        return Err(error);
    }

    match fs::canonicalize(&dir) {
        Ok(path) => Ok(path),
        Err(error) => {
            let _ = fs::remove_dir(&dir);
            Err(error)
        }
    }
}

fn cleanup_session(
    runtime_dir: &Path,
    storage_data_dir: &Path,
    storage_dev: u64,
    original_rad_home: Option<PathBuf>,
    restart_local_node: bool,
    fish_rad_home_set: bool,
    uid: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut errors = Vec::new();

    if let Err(error) = stop_rad_node(Some(runtime_dir)) {
        errors.push(format!("stopping radicle-portable: {error}"));
    }
    match storage_on_device(storage_data_dir, storage_dev) {
        Ok(true) => {
            if let Err(error) = copy_new_logs(runtime_dir, storage_data_dir, storage_dev) {
                errors.push(format!("persisting node logs: {error}"));
            }
        }
        Ok(false) => {
            eprintln!(
                "Skipping log persistence: storage directory is unavailable or on a different device."
            );
        }
        Err(error) => {
            errors.push(format!("checking storage before log persistence: {error}"));
        }
    }

    if fish_rad_home_set {
        match original_rad_home.as_ref() {
            Some(path) if path != runtime_dir => {
                eprintln!("Restoring RAD_HOME to {}.", path.display());
                match set_fish_rad_home(path) {
                    Ok(true) => {}
                    Ok(false) => errors.push("failed to restore RAD_HOME in fish".to_owned()),
                    Err(error) => errors.push(format!("restoring RAD_HOME in fish: {error}")),
                }
            }
            _ => {
                eprintln!("Resetting RAD_HOME.");
                match Command::new("fish")
                    .args(["-c", "set -Ue RAD_HOME"])
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(_) => errors.push("failed to reset RAD_HOME in fish".to_owned()),
                    Err(error) => errors.push(format!("resetting RAD_HOME in fish: {error}")),
                }
            }
        }
    }

    match fs::symlink_metadata(runtime_dir) {
        Ok(meta) if meta.is_dir() && meta.uid() == uid => {
            if let Err(error) = fs::remove_dir_all(runtime_dir) {
                errors.push(format!(
                    "removing runtime directory {}: {error}",
                    runtime_dir.display()
                ));
            }
        }
        Ok(meta) => errors.push(format!(
            "refusing to remove runtime path {} (directory: {}, owner uid: {})",
            runtime_dir.display(),
            meta.is_dir(),
            meta.uid()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => errors.push(format!(
            "checking runtime directory {}: {error}",
            runtime_dir.display()
        )),
    }

    if restart_local_node {
        eprintln!("Attempting to restart node...");
        let mut command = Command::new("rad");
        command.args(["node", "start"]);
        if let Some(path) = original_rad_home.as_deref() {
            command.env("RAD_HOME", path);
        }
        match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => errors.push(format!(
                "restarting local Radicle node exited with {status}"
            )),
            Err(error) => errors.push(format!("restarting local Radicle node: {error}")),
        }
    } else {
        eprintln!("No configured local Radicle profile to restart.");
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}

fn storage_on_device(storage_data_dir: &Path, expected_dev: u64) -> io::Result<bool> {
    match fs::symlink_metadata(storage_data_dir) {
        Ok(metadata) => Ok(metadata.is_dir() && metadata.dev() == expected_dev),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn copy_new_logs(runtime_dir: &Path, storage_data_dir: &Path, expected_dev: u64) -> io::Result<()> {
    let node_dir = match fs::canonicalize(runtime_dir.join("node")) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let storage_node_dir = storage_data_dir.join("node");
    let mut created_storage_node_dir = false;

    for entry in fs::read_dir(node_dir)? {
        let entry = entry?;
        let source = entry.path();
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !name.to_string_lossy().contains("log")
        {
            continue;
        }

        if !created_storage_node_dir {
            if !storage_on_device(storage_data_dir, expected_dev)? {
                return Err(io::Error::other(
                    "storage directory disappeared or changed device during log persistence",
                ));
            }
            fs::create_dir_all(&storage_node_dir)?;
            created_storage_node_dir = true;
        }
        if !storage_on_device(storage_data_dir, expected_dev)? {
            return Err(io::Error::other(
                "storage directory disappeared or changed device during log persistence",
            ));
        }
        fs::copy(&source, storage_node_dir.join(&name))?;
        eprintln!("Copied log file - {}", name.to_string_lossy());
    }
    Ok(())
}

unsafe extern "C" {
    fn getuid() -> u32;
    fn getppid() -> i32;
    #[link_name = "signal"]
    fn signal_handler(signal: i32, handler: usize) -> usize;
}

#[cfg(test)]
mod tests {
    use super::{
        copy_new_logs, create_runtime_dir, handle_termination_signal, is_missing_profile_error,
        request_stop, storage_on_device, take_termination_signal,
    };
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("radp-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn copies_new_regular_logs_and_skips_symlinked_logs() {
        let root = test_directory();
        let runtime_node = root.join("runtime/node");
        let storage_node = root.join("storage/node");
        fs::create_dir_all(&runtime_node).unwrap();
        fs::create_dir_all(&storage_node).unwrap();

        fs::write(storage_node.join("node.log"), "persisted old log").unwrap();
        symlink(storage_node.join("node.log"), runtime_node.join("node.log")).unwrap();
        fs::write(runtime_node.join("node.log.3"), "new rotated log").unwrap();
        fs::write(runtime_node.join("socket-data"), "not a log").unwrap();

        let storage_dev = fs::symlink_metadata(root.join("storage")).unwrap().dev();
        copy_new_logs(&root.join("runtime"), &root.join("storage"), storage_dev).unwrap();

        assert_eq!(
            fs::read_to_string(storage_node.join("node.log")).unwrap(),
            "persisted old log"
        );
        assert_eq!(
            fs::read_to_string(storage_node.join("node.log.3")).unwrap(),
            "new rotated log"
        );
        assert!(!storage_node.join("socket-data").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn storage_guard_rejects_a_different_device_before_copying() {
        let root = test_directory();
        let runtime_node = root.join("runtime/node");
        let storage = root.join("storage");
        fs::create_dir_all(&runtime_node).unwrap();
        fs::create_dir(&storage).unwrap();
        fs::write(runtime_node.join("node.log.1"), "new log").unwrap();

        let storage_dev = fs::symlink_metadata(&storage).unwrap().dev();
        assert!(!storage_on_device(&storage, storage_dev ^ 1).unwrap());
        assert!(copy_new_logs(&root.join("runtime"), &storage, storage_dev ^ 1).is_err());
        assert!(!storage.join("node").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn termination_signal_is_deferred_to_the_main_loop() {
        handle_termination_signal(2);
        assert_eq!(take_termination_signal(), Some(2));
        assert_eq!(take_termination_signal(), None);
    }

    #[test]
    fn stop_request_preserves_the_first_reason() {
        let stop_flag = AtomicBool::new(false);
        let exit_reason = Mutex::new(String::new());

        request_stop(&stop_flag, &exit_reason, "first".to_owned());
        request_stop(&stop_flag, &exit_reason, "second".to_owned());

        assert!(stop_flag.load(Ordering::SeqCst));
        assert_eq!(*exit_reason.lock().unwrap(), "first");
    }

    #[test]
    fn refuses_to_reuse_an_existing_runtime_directory() {
        let root = test_directory();
        let existing = root.join("radp.200");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("keep"), "user data").unwrap();

        assert!(create_runtime_dir(&root, 200).is_err());
        assert_eq!(
            fs::read_to_string(existing.join("keep")).unwrap(),
            "user data"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recognizes_only_radicle_missing_profile_errors() {
        assert!(is_missing_profile_error(
            b"",
            b"Error: Radicle profile not found in '/home/user/.radicle'."
        ));
        assert!(is_missing_profile_error(
            b"Error: Radicle profile not found in '/home/user/.radicle'.",
            b""
        ));
        assert!(!is_missing_profile_error(
            b"",
            b"Error: failed to connect to the node"
        ));
    }
}
