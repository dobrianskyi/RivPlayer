use std::{
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};

const URI_PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub fn file_uri(path: &Path) -> String {
    format!(
        "file://{}",
        percent_encode(path.as_os_str().as_bytes(), URI_PATH)
    )
}

pub fn reveal(path: &Path) -> Result<()> {
    let path = std::path::absolute(path)?;
    let folder = path.parent().context("No parent folder")?;
    if Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--timeout",
            "3",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("['{}']", file_uri(&path)))
        .arg("")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }

    let status = Command::new("xdg-open")
        .arg(folder)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Could not start the file manager")?;
    ensure!(status.success(), "Could not open {}", folder.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    #[test]
    fn encodes_native_path_for_file_manager() {
        let path = Path::new(std::ffi::OsStr::from_bytes(
            b"/tmp/video clips/it's 100%\xff.mp4",
        ));
        assert_eq!(
            file_uri(path),
            "file:///tmp/video%20clips/it%27s%20100%25%FF.mp4"
        );
    }
}
