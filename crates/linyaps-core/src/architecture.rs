use std::fmt;
use std::fs::File;
use std::io::Read;
use std::str::FromStr;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Architecture {
    #[default]
    Unknown,
    X86_64,
    Arm64,
    LoongArch64,
    Loong64,
    Sw64,
    RiscV64,
    Mips64,
}
#[derive(Debug, Error)]
#[error("unknown architecture: {0}")]
pub struct ArchitectureError(String);

impl Architecture {
    pub fn triplet(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::X86_64 => "x86_64-linux-gnu",
            Self::Arm64 => "aarch64-linux-gnu",
            Self::LoongArch64 | Self::Loong64 => "loongarch64-linux-gnu",
            Self::Sw64 => "sw_64-linux-gnu",
            Self::RiscV64 => "riscv64-linux-gnu",
            Self::Mips64 => "mips64el-linux-gnuabi64",
        }
    }

    pub fn current() -> Result<Self, ArchitectureError> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Arm64),
            "loongarch64" | "loong64" => Ok(if is_new_world_loongarch() {
                Self::Loong64
            } else {
                Self::LoongArch64
            }),
            "sw_64" | "sw64" => Ok(Self::Sw64),
            "riscv64" => Ok(Self::RiscV64),
            "mips64" | "mips64el" => Ok(Self::Mips64),
            architecture => Err(ArchitectureError(architecture.to_string())),
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unknown => "unknown",
            Self::X86_64 => "x86_64",
            Self::Arm64 => "arm64",
            Self::LoongArch64 => "loongarch64",
            Self::Loong64 => "loong64",
            Self::Sw64 => "sw64",
            Self::RiscV64 => "riscv64",
            Self::Mips64 => "mips64",
        })
    }
}

fn is_new_world_loongarch() -> bool {
    let Ok(mut executable) = File::open("/proc/self/exe") else {
        return false;
    };
    let mut elf_header = [0_u8; 52];
    if executable.read_exact(&mut elf_header).is_err()
        || elf_header[..4] != *b"\x7fELF"
        || elf_header[4] != 2
    {
        return false;
    }
    let flags = match elf_header[5] {
        1 => u32::from_le_bytes(elf_header[48..52].try_into().unwrap()),
        2 => u32::from_be_bytes(elf_header[48..52].try_into().unwrap()),
        _ => return false,
    };
    ((flags >> 6) & 1) == 1
}

impl FromStr for Architecture {
    type Err = ArchitectureError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "x86_64" => Ok(Self::X86_64),
            "arm64" => Ok(Self::Arm64),
            "loongarch64" => Ok(Self::LoongArch64),
            "loong64" => Ok(Self::Loong64),
            "sw64" => Ok(Self::Sw64),
            "riscv64" => Ok(Self::RiscV64),
            "mips64" => Ok(Self::Mips64),
            _ => Err(ArchitectureError(raw.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_upstream_names() {
        for name in [
            "x86_64",
            "arm64",
            "loongarch64",
            "loong64",
            "sw64",
            "riscv64",
            "mips64",
        ] {
            assert_eq!(name.parse::<Architecture>().unwrap().to_string(), name);
        }
        for name in ["unknown", "invalid_arch", "x86", "amd64", "", "X86_64"] {
            assert!(name.parse::<Architecture>().is_err(), "{name}");
        }
    }

    #[test]
    fn exposes_original_gnu_triplets() {
        for (architecture, name, triplet) in [
            (Architecture::Unknown, "unknown", "unknown"),
            (Architecture::X86_64, "x86_64", "x86_64-linux-gnu"),
            (Architecture::Arm64, "arm64", "aarch64-linux-gnu"),
            (
                Architecture::LoongArch64,
                "loongarch64",
                "loongarch64-linux-gnu",
            ),
            (Architecture::Loong64, "loong64", "loongarch64-linux-gnu"),
            (Architecture::Sw64, "sw64", "sw_64-linux-gnu"),
            (Architecture::Mips64, "mips64", "mips64el-linux-gnuabi64"),
            (Architecture::RiscV64, "riscv64", "riscv64-linux-gnu"),
        ] {
            assert_eq!(architecture.to_string(), name);
            assert_eq!(architecture.triplet(), triplet);
        }
    }

    #[test]
    fn default_architecture_is_unknown() {
        assert_eq!(Architecture::default(), Architecture::Unknown);
        assert_eq!(Architecture::default().to_string(), "unknown");
        assert_eq!(Architecture::default().triplet(), "unknown");
    }

    #[test]
    fn current_architecture_is_supported() {
        let architecture = Architecture::current().unwrap();
        assert_ne!(architecture, Architecture::Unknown);
        assert!(architecture.triplet().contains("linux-gnu"));
    }
}
