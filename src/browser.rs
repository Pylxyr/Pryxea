//! Opens Pryxea's own pages in the default browser (first-run convenience).
//! It will only ever open an address on this machine.

use std::process::{Command, Stdio};

/// The program and arguments that open `url`, or `None` if the URL isn't a plain local address.
pub fn command_for(os: &str, url: &str) -> Option<(String, Vec<String>)> {
    let local = url.strip_prefix("http://127.0.0.1:").or_else(|| url.strip_prefix("http://localhost:"))?;
    // Digits for the port, then a plain path: nothing a shell or a URL handler could misread.
    if !local.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-')) {
        return None;
    }
    Some(match os {
        "windows" => ("rundll32".into(), vec!["url.dll,FileProtocolHandler".into(), url.into()]),
        "macos" => ("open".into(), vec![url.into()]),
        _ => ("xdg-open".into(), vec![url.into()]),
    })
}

/// Best effort: returns whether a browser launcher could be started.
pub fn open(url: &str) -> bool {
    let Some((program, args)) = command_for(std::env::consts::OS, url) else { return false };
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn().map(|mut child| drop(std::thread::spawn(move || child.wait()))).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_platform_gets_its_own_launcher() {
        let url = "http://127.0.0.1:8098/setup";
        assert_eq!(command_for("linux", url), Some(("xdg-open".into(), vec![url.into()])));
        assert_eq!(command_for("macos", url), Some(("open".into(), vec![url.into()])));
        assert_eq!(command_for("windows", url).unwrap().0, "rundll32");
        assert_eq!(command_for("freebsd", "http://localhost:80/").unwrap().0, "xdg-open");
    }

    #[test]
    fn nothing_but_local_plain_addresses_is_ever_opened() {
        for bad in ["https://evil.example/", "http://evil.example:80/", "http://127.0.0.1.evil.example/", "file:///etc/passwd", "http://127.0.0.1:8098/a b", "http://127.0.0.1:8098/&calc", "http://127.0.0.1:8098/\";rm", "javascript:alert(1)", ""] {
            assert_eq!(command_for("windows", bad), None, "{bad:?}");
            assert_eq!(command_for("linux", bad), None, "{bad:?}");
        }
    }
}
