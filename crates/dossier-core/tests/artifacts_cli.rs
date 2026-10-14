//! `dossier-artifacts check` 的端到端用例：真的起一个子进程，断言**退出码**。
//!
//! 退出码是这条命令唯一的机器可读契约（CI/脚本靠它判断这批产物能不能放行），
//! 所以这里不复用库函数，而是跑二进制。

use std::path::{Path, PathBuf};
use std::process::Command;

use dossier_core::checkpoint::input_fingerprint;
use dossier_core::pipeline_version::{pipeline_version, ArtifactStamp, ToolStamp, ERASE_PIPELINE};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dossier-artifacts-cli-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_stamp(dir: &Path, name: &str, tool_version: &str, params: &str) -> PathBuf {
    let output = dir.join(format!("{name}.mp4"));
    std::fs::write(&output, name.as_bytes()).unwrap();
    let mut stamp = ArtifactStamp::new(
        ERASE_PIPELINE,
        pipeline_version(ERASE_PIPELINE).unwrap(),
        name,
        input_fingerprint(&[("params", params)]),
    )
    .with_tool(ToolStamp {
        name: "dossier-inpaint".to_string(),
        version: Some(tool_version.to_string()),
        program: Some("/usr/local/bin/dossier-inpaint".to_string()),
    })
    .with_inputs(vec![input_fingerprint(&[("source", "episode")])]);
    stamp.add_output(&output).unwrap();
    let path = dir.join(format!("{name}.mp4.stamp.json"));
    stamp.save(&path).unwrap();
    path
}

#[test]
fn consistent_stamps_exit_zero() {
    let dir = temp_dir("consistent");
    write_stamp(&dir, "ep01", "0.1.0", "dilate=4");
    write_stamp(&dir, "ep02", "0.1.0", "dilate=4");
    let result = Command::new(env!("CARGO_BIN_EXE_dossier-artifacts"))
        .arg("check")
        .arg(&dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert_eq!(result.status.code(), Some(0), "stdout={stdout}");
    assert!(stdout.contains("结论：一致"), "stdout={stdout}");
}

#[test]
fn mixed_tool_versions_exit_nonzero_and_name_the_skew() {
    let dir = temp_dir("skew");
    write_stamp(&dir, "ep01", "0.1.0", "dilate=4");
    write_stamp(&dir, "ep02", "0.2.0", "dilate=4");
    let result = Command::new(env!("CARGO_BIN_EXE_dossier-artifacts"))
        .arg("check")
        .arg(&dir)
        .arg("--json")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert_eq!(result.status.code(), Some(1), "stdout={stdout}");
    assert!(stdout.contains("tool_version_mixed"), "stdout={stdout}");
}

#[test]
fn missing_path_is_a_usage_error() {
    let result = Command::new(env!("CARGO_BIN_EXE_dossier-artifacts"))
        .arg("check")
        .arg("/nonexistent/dossier/stamps")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("路径不存在"));
}
