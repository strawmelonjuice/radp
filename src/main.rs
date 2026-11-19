// rad-portable
// Copyright (C) 2026 MLC 'Strawmelonjuice' Bloeiman and contributors.
//
// This software is licensed under the European Union Public Licence (EUPL) v1.2.
// You may not use this work except in compliance with the Licence.
// You may obtain a copy of the Licence at: https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-12

use std::env;
use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use symlink::{symlink_auto, symlink_dir};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("rad-portable, copyright (C) 2026 MLC 'Strawmelonjuice' Bloeiman and contributors.");
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
    let initial_dev = storage_meta.dev();

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

    // Okay now we ready to get cracking. Wait who are we!?
    let uid = unsafe { getuid() };
    let ppid = unsafe { getppid() };

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
    let _ = stop_rad_node(None);

    let runtime_dir = {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);

        let dir = base.join(format!("radp-{}", uid));

        if let Ok(meta) = fs::symlink_metadata(&dir) {
            let _ = stop_rad_node(Some(&dir));
            if meta.file_type().is_symlink() {
                fs::remove_file(&dir)?;
            } else if meta.uid() == uid {
                fs::remove_dir_all(&dir)?;
            } else {
                return Err(format!(
                    "refusing to remove {} owned by uid {}, expected {}",
                    dir.display(),
                    meta.uid(),
                    uid
                )
                .into());
            }
        }

        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&dir)?;
        fs::set_permissions(&dir, Permissions::from_mode(0o700))?;

        fs::canonicalize(&dir)
    }?;
    let runtime_dir: &Path = &runtime_dir;
    let runtime_node = runtime_dir.join("node");
    fs::create_dir_all(&runtime_node)?;
    for entry in fs::read_dir(&canonical_data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let src = entry.path();

        if name == "node" {
            if src.is_dir() {
                for node_entry in fs::read_dir(&src)? {
                    let node_entry = node_entry?;
                    let node_name = node_entry.file_name();
                    if node_name == "control.sock" || node_entry.file_type()?.is_socket() {
                        continue;
                    }
                    symlink_auto(node_entry.path(), runtime_node.join(&node_name))?;
                }
            }
        } else if src.is_dir() {
            symlink_dir(&src, runtime_dir.join(&name))?;
        } else {
            symlink_auto(&src, runtime_dir.join(&name))?;
        }
    }

    // Setting fish universal var
    let runtime_dir_str = runtime_dir.to_string_lossy().to_string();
    let cmd = format!(
        "set -Ux RAD_HOME '{}'",
        runtime_dir_str.replace('\\', "\\\\").replace('\'', "\\'")
    );
    if !Command::new("fish").args(["-c", &cmd]).status()?.success() {
        return Err("failed to set universal RAD_HOME in fish".into());
    }

    let _ = stop_rad_node(Some(runtime_dir));
    let start_status = Command::new("rad")
        .args(["node", "start"])
        .env("RAD_HOME", &runtime_dir_str)
        .status()?;
    if !start_status.success() {
        return Err(format!(
            "Starting Radicle node failed with status {}. The used RAD_HOME was '{}'",
            start_status, runtime_dir_str
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
                        *exit_reason.lock().unwrap() =
                            format!("storage path disappeared: {}", watch_path.display());
                        stop_flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(meta) => {
                        if !meta.is_dir() || meta.dev() != initial_dev {
                            *exit_reason.lock().unwrap() = format!(
                                "storage unmounted or remounted (dev {} -> {})",
                                initial_dev,
                                meta.dev()
                            );
                            stop_flag.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }

                if !Path::new(&format!("/proc/{}", ppid)).exists() {
                    *exit_reason.lock().unwrap() = format!("parent process {} exited", ppid);
                    stop_flag.store(true, Ordering::SeqCst);
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
        let rad_dir = runtime_dir_str.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(3000));

            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

                let runtime_dir_string: &str = &rad_dir;
                match Command::new("fish")
                    .args([
                        "-c",
                        format!(
                            "RAD_HOME='{}' rad node status | grep 'Node is'",
                            runtime_dir_string
                                .replace('\\', "\\\\")
                                .replace('\'', "\\'")
                        )
                        .as_str(),
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .output()
                {
                    Ok(output) => {
                        if !output.status.success() {
                            eprintln!(
                                "No longer able to watch Radicle node: `rad node status` exited with non-zero error code."
                            );
                            eprintln!(
                                "`rad stop` will no longer automatically stop radicle-portable."
                            );
                            break;
                        }
                        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if raw.is_empty() {
                            eprintln!(
                                "No longer able to watch Radicle node: Could not parse `rad node status` output."
                            );
                            eprintln!(
                                "`rad stop` will no longer automatically stop radicle-portable."
                            );
                            break;
                        } else if !raw.contains("running with") {
                            *exit_reason.lock().unwrap() =
                                "The radicle node has gone down.".to_string();
                            stop_flag.store(true, Ordering::SeqCst);
                            break;
                        }
                        // It's still going!
                    }
                    Err(_) => {
                        eprintln!(
                            "No longer able to watch Radicle node: `rad node status` exited with non-zero error code."
                        );
                        eprintln!("`rad stop` will no longer automatically stop radicle-portable.");
                        break;
                    }
                };
                thread::sleep(Duration::from_millis(600));
            }
        });
    }

    // User input watcher
    {
        let stop_flag = Arc::clone(&stop_flag);
        let exit_reason = Arc::clone(&exit_reason);

        thread::spawn(move || {
            let stdin = io::stdin();
            let mut lines = stdin.lock().lines();
            let _ = lines.next();
            if !stop_flag.swap(true, Ordering::SeqCst) {
                *exit_reason.lock().unwrap() = String::from("User input triggered stop");
            }
        });
    }

    println!("Active RAD_HOME: {}", runtime_dir_str);
    println!(
        "Watching storage dev {} at {} (ppid {})",
        initial_dev,
        canonical_data_dir.display(),
        ppid
    );
    // Now keep running in foreground until stopped!
    println!("Press Enter or run `rad node stop` to stop cleanly.");
    println!();
    println!(
        "If this drive gets prematurely disconnected, an attempt will be made to stop radicle before more damage occurs."
    );

    while !stop_flag.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }

    let reason = exit_reason
        .lock()
        .map_err(|_| "exit reason mutex poisoned")?
        .clone();

    // shut down and cleanup time
    eprintln!("Stopping node ({reason})...");
    {
        let runtime_dir: &Path = runtime_dir;
        let _ = stop_rad_node(Some(runtime_dir));
        if let Ok(node_dir) = fs::canonicalize(runtime_dir.join("./node")) {
            let node_store_dir = storage_data_dir.join("./node");
            for entry in fs::read_dir(&node_dir)? {
                let entry = entry?;
                let src = entry.path();

                if let (Ok(name), Ok(meta)) =
                    (entry.file_name().into_string(), fs::symlink_metadata(&src))
                {
                    if meta.file_type().is_symlink() || !name.contains("log") {
                        continue;
                    };
                    let dest = node_store_dir.join(&name);
                    match fs::copy(&src, &dest) {
                        Ok(..) => {
                            println!(
                                "Copied log file - {}",
                                // &src.display()
                                &name
                            )
                        }
                        Err(er) => {
                            eprintln!("Couldn't copy log file - {}: {}", &name, er)
                        }
                    };
                } else {
                    continue;
                };
            }
        }

        match original_rad_home {
            Some(original_runtime_dir) if runtime_dir != original_runtime_dir => {
                println!(
                    "Restoring RAD_HOME to {} and starting local Radicle node.",
                    original_runtime_dir.display()
                );
                let cmd = format!(
                    "set -Ux RAD_HOME '{}'",
                    original_runtime_dir
                        .to_string_lossy()
                        .replace('\\', "\\\\")
                        .replace('\'', "\\'")
                );
                let _ = Command::new("fish").args(["-c", &cmd]).status();
            }
            _ => {
                println!("Resetting RAD_HOME and attempting to restore local Radicle node.");
                let _ = Command::new("fish")
                    .args(["-c", "set -e RAD_HOME"])
                    .status();
            }
        }

        let Ok(meta) = fs::symlink_metadata(runtime_dir) else {
            return Ok(());
        };
        if meta.file_type().is_symlink() {
            let _ = fs::remove_file(runtime_dir);
            return Ok(());
        }
        if meta.uid() != uid {
            eprintln!(
                "Skipping cleanup of {}: owned by uid {}, expected {}",
                runtime_dir.display(),
                meta.uid(),
                uid
            );
            return Ok(());
        }
        if let Ok(cwd) = env::current_dir()
            && cwd.starts_with(runtime_dir)
        {
            eprintln!(
                "Warning: current working directory ({}) is inside {}; remove_dir_all may invalidate cwd",
                cwd.display(),
                runtime_dir.display()
            );
        }
        let _ = fs::remove_dir_all(runtime_dir);
        let _ = Command::new("fish")
            .args([
                "-c",
                "echo \"Attempting to restart node...\"; rad node start",
            ])
            .status();
    };

    Ok(())
}

fn stop_rad_node(runtime_dir: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    // Let's start by checking if the targeted node is even running.

    let cmd: String = match runtime_dir {
        Some(runtime_directory) => {
            format!(
                "RAD_HOME='{}' rad node status | grep 'Node is'",
                format!("{}", runtime_directory.display())
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'")
            )
        }
        _ => "rad node status | grep 'Node is'".to_string(),
    };
    match Command::new("fish")
        .args(["-c", cmd.as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                return Err(
                    "`rad node status` exited with non-zero error code, which means radicle-portable cannot peacefully shut it down.".into()
                );
            }
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if raw.is_empty() {
                return Err(
                    "`rad node status` had some unparseable output, which means radicle-portable cannot peacefully shut it down.".into()
                );
            } else if !raw.contains("running with") {
                return Ok(());
            }

            let status = match runtime_dir {
                Some(dir) => Command::new("fish")
                    .args([
                        "-c",
                        &format!(
                            "RAD_HOME='{}' rad node stop",
                            dir.to_str()
                                .unwrap_or("~/.radicle")
                                .replace('\\', "\\\\")
                                .replace('\'', "\\'")
                        ),
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?,
                _ => Command::new("fish")
                    .args(["-c", "rad node stop"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?,
            };

            if !status.success() {
                return Err("Failed peacefully shutting down radicle node.".into());
            }
            Ok(())
        }
        Err(_) => {
            Err(
                "`rad node status` exited with non-zero error code, which means radicle-portable cannot peacefully shut it down.".into()
            )
        }
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
    fn getppid() -> i32;
}
