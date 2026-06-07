use sha2::{Digest, Sha256};
use tracing::warn;

// windows reads the registry MachineGuid
// macos reads the IOPlatformUUID
pub fn collect_hwid() -> Option<String> {
    let id = machine_id()?;
    let id = id.trim();
    if id.is_empty() {
        warn!("machine id was empty, cannot compute hwid");
        return None;
    }
    Some(sha256_hex(id))
}

#[cfg(target_os = "windows")]
fn machine_id() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let crypto = hklm
        .open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .map_err(|e| warn!("could not open cryptography registry key: {e}"))
        .ok()?;
    crypto
        .get_value::<String, _>("MachineGuid")
        .map_err(|e| warn!("could not read MachineGuid: {e}"))
        .ok()
}

#[cfg(target_os = "macos")]
fn machine_id() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| warn!("could not run ioreg: {e}"))
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find(|line| line.contains("IOPlatformUUID"))
        .and_then(|line| line.rsplit('=').next())
        .map(|value| value.trim().trim_matches('"').to_string())
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn test_hash() {
        let a = sha256_hex("9ca563a0-2773-4c7d-b157-30feb8bd8b6c");
        let b = sha256_hex("9ca563a0-2773-4c7d-b157-30feb8bd8b6c");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
