//! A local restrictive override; it never enables routing disabled in the session config.

use std::io;
use std::path::Path;
use tokio::io::AsyncReadExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Configured,
    RulesOnly,
    Off,
}

pub(super) async fn read(codex_home: &Path) -> io::Result<Mode> {
    let path = codex_home.join("agent-model-routing.mode");
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Mode::Configured),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid routing control file",
        ));
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(65)
        .read_to_end(&mut bytes)
        .await?;
    match bytes.as_slice() {
        b"configured" | b"configured\n" => Ok(Mode::Configured),
        b"rules-only" | b"rules-only\n" => Ok(Mode::RulesOnly),
        b"off" | b"off\n" => Ok(Mode::Off),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid routing control mode",
        )),
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
