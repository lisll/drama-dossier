//! 设备指纹：把「这台机器」压成一个不泄露原始信息的稳定标识。
//!
//! 只做**本地**用途（例如把一份凭据绑定到本机）。这里刻意不采集 MAC、序列号等
//! 强标识，也**不外发**任何字段；输出只有摘要，原始值不落盘、不进日志。
//!
//! 稳定性分级：Linux 的 `/etc/machine-id` 在重装系统前是稳定的，据此判定
//! `stable = true`；只有主机名/CPU 型号时只能算弱指纹，`stable = false`。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::command::{CommandRunner, CommandSpec};
use crate::digest::{sha256_bytes, short};

/// 指纹里用到的一个来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintComponent {
    /// 来源类别（`machine_id` / `hostname` / `cpu` / `os` / `arch`）。
    pub kind: String,
    /// 读取位置的可读描述（文件路径或命令名），用于排查「为什么指纹变了」。
    pub source: String,
    /// 该来源的摘要前缀（前 12 位十六进制）。
    pub digest: String,
    pub available: bool,
}

/// 设备指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceFingerprint {
    /// 32 位十六进制标识（SHA-256 截断）。
    pub id: String,
    pub os: String,
    pub arch: String,
    /// 是否有跨重启稳定的来源（machine-id）。
    pub stable: bool,
    pub components: Vec<FingerprintComponent>,
    /// 已知局限（例如「只有主机名，改名字就变」）。
    #[serde(default)]
    pub limitation: Option<String>,
}

impl DeviceFingerprint {
    /// 给界面/日志用的短标识。
    pub fn short_id(&self) -> String {
        short(&self.id, 12)
    }

    /// 可读说明（不含任何原始值）。
    pub fn describe(&self) -> String {
        let available: Vec<&str> = self
            .components
            .iter()
            .filter(|component| component.available)
            .map(|component| component.kind.as_str())
            .collect();
        format!(
            "{} / {}，来源 {}，{}",
            self.os,
            self.arch,
            if available.is_empty() {
                "无".to_string()
            } else {
                available.join(" + ")
            },
            if self.stable {
                "稳定标识可用"
            } else {
                "仅弱标识（不稳定）"
            }
        )
    }
}

/// 计算设备指纹。`salt` 用于把一个应用实例的指纹与其它用途区分开。
pub fn compute_device_fingerprint(
    runner: &dyn CommandRunner,
    salt: Option<&str>,
) -> DeviceFingerprint {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();

    let mut components = Vec::new();
    let machine_id = read_first_line(Path::new("/etc/machine-id"))
        .or_else(|| read_first_line(Path::new("/var/lib/dbus/machine-id")));
    components.push(component(
        "machine_id",
        "/etc/machine-id",
        machine_id.as_deref(),
    ));

    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| run_trimmed(runner, "hostname", &[]));
    components.push(component("hostname", "hostname", hostname.as_deref()));

    let cpu = read_cpu_model()
        .or_else(|| run_trimmed(runner, "sysctl", &["-n", "machdep.cpu.brand_string"]));
    components.push(component(
        "cpu",
        if cfg!(target_os = "linux") {
            "/proc/cpuinfo"
        } else {
            "sysctl machdep.cpu.brand_string"
        },
        cpu.as_deref(),
    ));

    components.push(FingerprintComponent {
        kind: "os".to_string(),
        source: "std::env::consts::OS".to_string(),
        digest: short(&sha256_bytes(os.as_bytes()), 12),
        available: true,
    });
    components.push(FingerprintComponent {
        kind: "arch".to_string(),
        source: "std::env::consts::ARCH".to_string(),
        digest: short(&sha256_bytes(arch.as_bytes()), 12),
        available: true,
    });

    let stable = machine_id.is_some();
    let mut canonical = format!("v1|{os}|{arch}");
    if let Some(value) = &machine_id {
        canonical.push_str("|mid=");
        canonical.push_str(value);
    }
    if let Some(value) = &hostname {
        canonical.push_str("|host=");
        canonical.push_str(value);
    }
    if let Some(value) = &cpu {
        canonical.push_str("|cpu=");
        canonical.push_str(value);
    }
    if let Some(salt) = salt {
        canonical.push_str("|salt=");
        canonical.push_str(salt);
    }

    let limitation = if stable {
        None
    } else {
        Some(
            "本机没有 /etc/machine-id，指纹只由主机名与 CPU 型号派生，改主机名就会变；\
             不要把这种弱指纹用于长期授权"
                .to_string(),
        )
    };

    DeviceFingerprint {
        id: sha256_bytes(canonical.as_bytes()),
        os,
        arch,
        stable,
        components,
        limitation,
    }
}

fn component(kind: &str, source: &str, value: Option<&str>) -> FingerprintComponent {
    FingerprintComponent {
        kind: kind.to_string(),
        source: source.to_string(),
        digest: value
            .map(|value| short(&sha256_bytes(value.as_bytes()), 12))
            .unwrap_or_else(|| "-".to_string()),
        available: value.is_some(),
    }
}

fn read_first_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn read_cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("model name") {
            let value = value.trim_start().trim_start_matches(':').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        // ARM 上没有 model name，退化为 Hardware / Processor。
        if let Some(value) = line.strip_prefix("Hardware") {
            let value = value.trim_start().trim_start_matches(':').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn run_trimmed(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Option<String> {
    let output = runner
        .run(&CommandSpec::new(program).args(args.iter().copied()))
        .ok()?;
    if !output.success() {
        return None;
    }
    let value = output.stdout.lines().next()?.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeRunner};

    #[test]
    fn fingerprint_is_deterministic_for_same_inputs() {
        let runner = FakeRunner::new()
            .on("hostname", CommandOutput::ok("lab-node-01\n"))
            .on("sysctl", CommandOutput::ok("Apple M1\n"));
        let first = compute_device_fingerprint(&runner, Some("drama-dossier"));
        let second = compute_device_fingerprint(&runner, Some("drama-dossier"));
        assert_eq!(first.id, second.id);
        assert_eq!(first.id.len(), 64);
        assert_eq!(first.short_id().len(), 12);
        assert_eq!(first.components.len(), 5);
    }

    #[test]
    fn salt_changes_the_identifier() {
        let runner = FakeRunner::new()
            .on("hostname", CommandOutput::ok("lab-node-01\n"))
            .on("sysctl", CommandOutput::ok("Apple M1\n"));
        let plain = compute_device_fingerprint(&runner, None);
        let salted = compute_device_fingerprint(&runner, Some("other-app"));
        assert_ne!(plain.id, salted.id);
    }

    #[test]
    fn fingerprint_degrades_gracefully_without_optional_sources() {
        // 假执行器不会提供 `hostname` / `sysctl`；而 `HOSTNAME` 环境变量可能被真实
        // 环境设置，所以这里只断言「结构自洽」，不假设某一路来源一定缺失。
        let runner = FakeRunner::new();
        let fingerprint = compute_device_fingerprint(&runner, None);
        assert_eq!(fingerprint.id.len(), 64);
        assert!(fingerprint.components.len() == 5);
        for component in &fingerprint.components {
            assert!(component.available || component.digest == "-");
            if component.available {
                assert_eq!(component.digest.len(), 12);
            }
        }
        // 弱指纹必须显式提示，不能默默当成稳定标识。
        assert_eq!(fingerprint.limitation.is_some(), !fingerprint.stable);
        if !fingerprint.stable {
            assert!(fingerprint.describe().contains("弱标识"));
        }
    }

    #[test]
    fn real_environment_fingerprint_smoke_test() {
        let runner = crate::command::SystemRunner::new();
        let fingerprint = compute_device_fingerprint(&runner, Some("drama-dossier-test"));
        assert_eq!(fingerprint.id.len(), 64);
        assert_eq!(fingerprint.os, std::env::consts::OS);
        // 本机是 Linux，machine-id 应当存在（存在则必须是稳定指纹）。
        if Path::new("/etc/machine-id").exists() {
            assert!(fingerprint.stable);
            assert!(fingerprint.limitation.is_none());
            let machine_id = fingerprint
                .components
                .iter()
                .find(|component| component.kind == "machine_id")
                .unwrap();
            assert!(machine_id.available);
        }
    }
}
