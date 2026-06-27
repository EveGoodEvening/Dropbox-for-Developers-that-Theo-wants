use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os_family: OsFamily,
    pub os_version: Option<String>,
    pub architecture: Architecture,
    pub capabilities: PlatformCapabilities,
    pub machine_id: MachineId,
}

impl Platform {
    pub fn detect() -> Self {
        let os_family = OsFamily::detect();
        let architecture = Architecture::detect();
        Self {
            os_version: detect_os_version(&os_family),
            capabilities: PlatformCapabilities::for_os(&os_family),
            machine_id: MachineId::detect(),
            os_family,
            architecture,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsFamily {
    Linux,
    Macos,
    Windows,
    Other(String),
}

impl OsFamily {
    pub fn detect() -> Self {
        match env::consts::OS {
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            "windows" => Self::Windows,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Other(value) => value.as_str(),
        }
    }
}

impl fmt::Display for OsFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    Aarch64,
    Arm,
    Other(String),
}

impl Architecture {
    pub fn detect() -> Self {
        match env::consts::ARCH {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            "arm" => Self::Arm,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Arm => "arm",
            Self::Other(value) => value.as_str(),
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineId {
    pub value: String,
    pub provenance: MachineIdProvenance,
}

const APP_MACHINE_ID_PREFIX: &str = "dropbox-dev-";
const APP_MACHINE_ID_DOMAIN_SEPARATOR: &[u8] = b"dropbox-dev:machine-id:v1\0";

impl MachineId {
    pub fn detect() -> Self {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Some(source) = read_machine_id_file(path) {
                return Self::from_source(
                    &source,
                    MachineIdProvenance::MachineIdFile(PathBuf::from(path)),
                );
            }
        }

        for key in ["HOSTNAME", "COMPUTERNAME"] {
            if let Ok(value) = env::var(key) {
                let value = value.trim().to_owned();
                if !value.is_empty() {
                    return Self::from_source(&value, MachineIdProvenance::Environment(key));
                }
            }
        }

        let fallback_source = format!("{}-{}-unknown", env::consts::OS, env::consts::ARCH);
        Self::from_source(&fallback_source, MachineIdProvenance::Fallback)
    }

    pub fn derive_app_scoped(source: &str) -> Option<String> {
        let source = source.trim();
        if source.is_empty() {
            return None;
        }

        Some(derive_app_machine_id(source.as_bytes()))
    }

    pub fn is_app_scoped_value(value: &str) -> bool {
        let Some(hex) = value.strip_prefix(APP_MACHINE_ID_PREFIX) else {
            return false;
        };

        hex.len() == 64 && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }

    fn from_source(source: &str, provenance: MachineIdProvenance) -> Self {
        let value = derive_app_machine_id(source.trim().as_bytes());
        Self { value, provenance }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineIdProvenance {
    MachineIdFile(PathBuf),
    Environment(&'static str),
    ConfigFile(PathBuf),
    Fallback,
}

impl fmt::Display for MachineIdProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MachineIdFile(path) => write!(formatter, "machine-id-file:{}", path.display()),
            Self::Environment(key) => write!(formatter, "environment:{key}"),
            Self::ConfigFile(path) => write!(formatter, "config-file:{}", path.display()),
            Self::Fallback => formatter.write_str("fallback"),
        }
    }
}

fn derive_app_machine_id(source: &[u8]) -> String {
    let mut material = Vec::with_capacity(APP_MACHINE_ID_DOMAIN_SEPARATOR.len() + source.len());
    material.extend_from_slice(APP_MACHINE_ID_DOMAIN_SEPARATOR);
    material.extend_from_slice(source);

    let digest = sha256(&material);
    let mut value = String::with_capacity(APP_MACHINE_ID_PREFIX.len() + 64);
    value.push_str(APP_MACHINE_ID_PREFIX);
    push_hex(&mut value, &digest);
    value
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const H0: [u32; 8] = [
        0x6a09e667,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98,
        0x71374491,
        0xb5c0fbcf,
        0xe9b5dba5,
        0x3956c25b,
        0x59f111f1,
        0x923f82a4,
        0xab1c5ed5,
        0xd807aa98,
        0x12835b01,
        0x243185be,
        0x550c7dc3,
        0x72be5d74,
        0x80deb1fe,
        0x9bdc06a7,
        0xc19bf174,
        0xe49b69c1,
        0xefbe4786,
        0x0fc19dc6,
        0x240ca1cc,
        0x2de92c6f,
        0x4a7484aa,
        0x5cb0a9dc,
        0x76f988da,
        0x983e5152,
        0xa831c66d,
        0xb00327c8,
        0xbf597fc7,
        0xc6e00bf3,
        0xd5a79147,
        0x06ca6351,
        0x14292967,
        0x27b70a85,
        0x2e1b2138,
        0x4d2c6dfc,
        0x53380d13,
        0x650a7354,
        0x766a0abb,
        0x81c2c92e,
        0x92722c85,
        0xa2bfe8a1,
        0xa81a664b,
        0xc24b8b70,
        0xc76c51a3,
        0xd192e819,
        0xd6990624,
        0xf40e3585,
        0x106aa070,
        0x19a4c116,
        0x1e376c08,
        0x2748774c,
        0x34b0bcb5,
        0x391c0cb3,
        0x4ed8aa4a,
        0x5b9cca4f,
        0x682e6ff3,
        0x748f82ee,
        0x78a5636f,
        0x84c87814,
        0x8cc70208,
        0x90befffa,
        0xa4506ceb,
        0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64) * 8;
    let padded_len = (input.len() + 9).div_ceil(64) * 64;
    let mut message = Vec::with_capacity(padded_len);
    message.extend_from_slice(input);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut hash = H0;
    let mut schedule = [0_u32; 64];

    for chunk in message.chunks_exact(64) {
        for (index, word) in schedule.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }

        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let mut a = hash[0];
        let mut b = hash[1];
        let mut c = hash[2];
        let mut d = hash[3];
        let mut e = hash[4];
        let mut f = hash[5];
        let mut g = hash[6];
        let mut h = hash[7];

        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        hash[0] = hash[0].wrapping_add(a);
        hash[1] = hash[1].wrapping_add(b);
        hash[2] = hash[2].wrapping_add(c);
        hash[3] = hash[3].wrapping_add(d);
        hash[4] = hash[4].wrapping_add(e);
        hash[5] = hash[5].wrapping_add(f);
        hash[6] = hash[6].wrapping_add(g);
        hash[7] = hash[7].wrapping_add(h);
    }

    let mut output = [0_u8; 32];
    for (chunk, value) in output.chunks_exact_mut(4).zip(hash) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    output
}

fn push_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    for byte in bytes {
        output.push(char::from(HEX[(byte >> 4) as usize]));
        output.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub case_sensitive_paths: bool,
    pub supports_symlinks: bool,
    pub supports_posix_permissions: bool,
    pub supports_file_ids: bool,
    pub supports_fsevents: bool,
    pub supports_inotify: bool,
}

impl PlatformCapabilities {
    pub fn for_os(os_family: &OsFamily) -> Self {
        match os_family {
            OsFamily::Linux => Self {
                case_sensitive_paths: true,
                supports_symlinks: true,
                supports_posix_permissions: true,
                supports_file_ids: true,
                supports_fsevents: false,
                supports_inotify: true,
            },
            OsFamily::Macos => Self {
                case_sensitive_paths: false,
                supports_symlinks: true,
                supports_posix_permissions: true,
                supports_file_ids: true,
                supports_fsevents: true,
                supports_inotify: false,
            },
            OsFamily::Windows => Self {
                case_sensitive_paths: false,
                supports_symlinks: false,
                supports_posix_permissions: false,
                supports_file_ids: true,
                supports_fsevents: false,
                supports_inotify: false,
            },
            OsFamily::Other(_) => Self {
                case_sensitive_paths: true,
                supports_symlinks: false,
                supports_posix_permissions: false,
                supports_file_ids: false,
                supports_fsevents: false,
                supports_inotify: false,
            },
        }
    }
}

fn read_machine_id_file(path: &str) -> Option<String> {
    fs::read_to_string(Path::new(path))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn detect_os_version(os_family: &OsFamily) -> Option<String> {
    match os_family {
        OsFamily::Linux => read_linux_os_release(),
        OsFamily::Macos | OsFamily::Windows | OsFamily::Other(_) => None,
    }
}

fn read_linux_os_release() -> Option<String> {
    let contents = fs::read_to_string("/etc/os-release").ok()?;
    for key in ["PRETTY_NAME", "VERSION_ID"] {
        for line in contents.lines() {
            if let Some(value) = line.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')) {
                return Some(strip_quotes(value));
            }
        }
    }
    None
}

fn strip_quotes(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\"' && bytes[value.len() - 1] == b'\"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        let mut hex = String::new();
        push_hex(&mut hex, &sha256(b"abc"));

        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn app_scoped_machine_id_is_stable_and_not_raw() {
        let raw = "0123456789abcdef0123456789abcdef";
        let derived = MachineId::derive_app_scoped(raw).unwrap();

        assert!(MachineId::is_app_scoped_value(&derived));
        assert_ne!(derived, raw);
        assert_eq!(derived, MachineId::derive_app_scoped(raw).unwrap());
    }
}
