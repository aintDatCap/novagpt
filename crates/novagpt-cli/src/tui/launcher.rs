//! Idempotent, no-clobber, workspace-local launcher installation.
use crate::settings;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

fn unsafe_target(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
pub(super) fn install_launcher(root: &Path) -> io::Result<String> {
    let local = settings::local_dir(root);
    let bin = local.join("bin");
    match fs::symlink_metadata(&bin) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(unsafe_target("invalid local bin directory"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&bin)?,
        Err(e) => return Err(e),
    }
    let name = if cfg!(windows) {
        "novagpt.exe"
    } else {
        "novagpt"
    };
    let dest = bin.join(name);
    let source = std::env::current_exe()?;
    let mut existing_binary = false;
    match fs::symlink_metadata(&dest) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(unsafe_target("invalid launcher executable target"));
        }
        Ok(_) => existing_binary = true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if source != dest {
                let mut from = fs::File::open(source)?;
                let mut to = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)?;
                io::copy(&mut from, &mut to)?;
                to.sync_all()?;
            }
        }
        Err(e) => return Err(e),
    }
    let script = local.join(if cfg!(windows) {
        "start.cmd"
    } else {
        "start.sh"
    });
    let mut existing_script = false;
    match fs::symlink_metadata(&script) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(unsafe_target("invalid launcher script target"));
        }
        Ok(_) => existing_script = true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let content = if cfg!(windows) {
                "@echo off\r\n\"%~dp0bin\\novagpt.exe\" --root \"%~dp0..\" %*\r\n"
            } else {
                "#!/bin/sh\nHERE=\"$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\"\nexec \"$HERE/bin/novagpt\" --root \"$HERE/..\" \"$@\"\n"
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&script)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
            }
        }
        Err(e) => return Err(e),
    }
    Ok(format!(
        "Launcher: {} (binario {}, script {}; file esistenti non sovrascritti)",
        script.display(),
        if existing_binary {
            "gia presente"
        } else {
            "installato"
        },
        if existing_script {
            "gia presente"
        } else {
            "installato"
        }
    ))
}
