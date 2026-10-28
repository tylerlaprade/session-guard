//! Whether kid mode is on: a Kanata layer under which a child uses the Mac, so
//! a tab opened then would take the screen from them. Kanata greets each new
//! TCP client with a `LayerChange` naming its current layer.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

const KANATA_PLIST: &str = "/Library/LaunchDaemons/com.tylerlaprade.kanata.plist";
// The tests stand in for Kanata's launch daemon with their own plist.
const KANATA_PLIST_VARIABLE: &str = "SESSION_GUARD_KANATA_PLIST";
const KID_LAYER: &str = "kid";
const CONNECT_PATIENCE: Duration = Duration::from_secs(1);
// Kanata sends its greeting as soon as a client connects; dotfiles' kid-lock
// waits as long for it.
const GREETING_PATIENCE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KidMode {
    Off,
    On,
    Unknown(String),
}

pub fn kid_mode() -> KidMode {
    match read() {
        Ok(mode) => mode,
        Err(error) => KidMode::Unknown(format!("{error:#}")),
    }
}

fn read() -> Result<KidMode> {
    let plist = std::env::var_os(KANATA_PLIST_VARIABLE)
        .map_or_else(|| PathBuf::from(KANATA_PLIST), PathBuf::from);
    let contents = match std::fs::read_to_string(&plist) {
        Ok(contents) => contents,
        // Kid mode is a Kanata layer, so a Mac without Kanata has none.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(KidMode::Off),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", plist.display()));
        }
    };
    let address: SocketAddr = kanata_address(&contents)
        .with_context(|| format!("no --port in {}", plist.display()))?
        .parse()
        .with_context(|| format!("unreadable --port in {}", plist.display()))?;
    // Nor while Kanata is not listening.
    let Ok(stream) = TcpStream::connect_timeout(&address, CONNECT_PATIENCE) else {
        return Ok(KidMode::Off);
    };
    stream.set_read_timeout(Some(GREETING_PATIENCE))?;
    for line in BufReader::new(stream).lines() {
        let line = line.context("Kanata sent no layer")?;
        if let Some(layer) = layer_of(&line) {
            return Ok(if layer == KID_LAYER {
                KidMode::On
            } else {
                KidMode::Off
            });
        }
    }
    anyhow::bail!("Kanata closed the connection before naming its layer")
}

/// The address after `--port` among the launch daemon's program arguments.
fn kanata_address(plist: &str) -> Option<&str> {
    let (_, after) = plist.split_once("<string>--port</string>")?;
    let (_, value) = after.split_once("<string>")?;
    value
        .split_once("</string>")
        .map(|(address, _)| address.trim())
}

fn layer_of(line: &str) -> Option<String> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    message
        .get("LayerChange")?
        .get("new")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_comes_from_kanatas_program_arguments() {
        let plist = "<key>ProgramArguments</key>\n<array>\n  <string>/Users/tyler/.local/bin/kanata</string>\n  <string>--no-wait</string>\n  <string>--port</string>\n  <string>127.0.0.1:41471</string>\n</array>";
        assert_eq!(kanata_address(plist), Some("127.0.0.1:41471"));
        assert_eq!(kanata_address("<string>--no-wait</string>"), None);
    }

    #[test]
    fn only_a_layer_change_names_the_layer() {
        assert_eq!(
            layer_of(r#"{"LayerChange":{"new":"kid"}}"#).as_deref(),
            Some("kid")
        );
        assert_eq!(layer_of(r#"{"ConfigFileReload":{}}"#), None);
        assert_eq!(layer_of("not json"), None);
    }
}
