//! `dossier-artifacts`：产物戳记的一致性校验（Tier 3 的消费端），以及报审包的「生成文档」。
//!
//! 生产端是各个管线（例如 `dossier-inpaint` 默认写 `<输出>.stamp.json`）；
//! 这里负责回答「这一批产物是不是同一套管线、同一个工具版本、同一组参数、同一批输入出的」。
//!
//! 为什么要有独立的命令行：一致性问题往往在**跑完之后**才被发现（一次跑批里混进了
//! 旧工具产出的分集），这时候已经不可能靠退出码拦住，只能拿戳记来审。
//!
//! `render-package`：服务端调用。stdin 读 JSON 请求（[`RenderRequest`]），stdout 写 JSON 结果
//! （[`dossier_core::RenderedDocuments`]），不碰磁盘。
//!
//! 退出码：`0` 一致 / 成功；`1` 发现不一致；`2` 戳记读不出来、请求不合法（错误写到 stderr）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dossier_core::pipeline_version::{load_and_check, ArtifactStamp, STAMP_SUFFIX};
use dossier_core::{render_documents, RenderRequest};

#[derive(Parser)]
#[command(
    name = "dossier-artifacts",
    about = "产物戳记校验：这一批产物是不是同一套流程出的"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 读一组戳记（文件或目录）并做一致性校验
    Check {
        /// 戳记文件（`*.stamp.json`）或包含戳记的目录；可重复给出
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// 输出 JSON（便于脚本消费）
        #[arg(long)]
        json: bool,
    },
    /// 生成报审包文档：stdin 读 JSON 请求，stdout 写 JSON 结果，不碰磁盘
    RenderPackage,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    match &cli.command {
        Command::RenderPackage => render_package(),
        Command::Check { paths, json } => {
            let stamps = collect_stamps(paths)?;
            if stamps.is_empty() {
                return Err("没有找到任何戳记文件（*.stamp.json）".to_string());
            }
            let report = load_and_check(&stamps).map_err(|error| error.to_string())?;
            if *json {
                let text = serde_json::to_string_pretty(&report)
                    .map_err(|error| format!("报告序列化失败：{error}"))?;
                println!("{text}");
            } else {
                println!(
                    "戳记 {} 份，管线：{}",
                    report.stamps_checked,
                    if report.pipelines.is_empty() {
                        "(无)".to_string()
                    } else {
                        report.pipelines.join(", ")
                    }
                );
                for issue in &report.issues {
                    println!(
                        "  [不一致] {:?}（{}）：{}",
                        issue.kind, issue.pipeline, issue.detail
                    );
                }
                if report.is_consistent() {
                    println!("结论：一致");
                } else {
                    println!(
                        "结论：发现 {} 处不一致，涉及管线：{}",
                        report.issues.len(),
                        report.affected_pipelines().join(", ")
                    );
                }
            }
            Ok(if report.is_consistent() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

/// 把命令行给出的路径展开成戳记文件列表：目录递归找 `*.stamp.json`，文件按原样收下。
///
/// 重名与重复输入只保留一次：同一份戳记被算两次会让「戳记数」这个指标失真。
fn collect_stamps(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut collected: Vec<PathBuf> = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found: Vec<PathBuf> = Vec::new();
            walk(path, &mut found)?;
            found.sort();
            for candidate in found {
                if !collected.contains(&candidate) {
                    collected.push(candidate);
                }
            }
        } else if path.is_file() {
            if !collected.contains(path) {
                collected.push(path.clone());
            }
        } else {
            return Err(format!("路径不存在：{}", path.display()));
        }
    }
    Ok(collected)
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("无法读取目录 {}：{error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("无法读取目录项：{error}"))?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, found)?;
        } else if path
            .file_name()
            .map(|name| name.to_string_lossy().ends_with(STAMP_SUFFIX))
            .unwrap_or(false)
        {
            found.push(path);
        }
    }
    Ok(())
}

/// 让「戳记能被读成 ArtifactStamp」这件事在编译期可见（避免只做 JSON 字符串处理的错觉）。
#[allow(dead_code)]
fn load_one(path: &Path) -> Result<ArtifactStamp, String> {
    ArtifactStamp::load(path).map_err(|error| error.to_string())
}

/// 请求体上限：不含扫描件的报审包请求不会超过 1 MiB（500 集也只有几百 KiB）。
const MAX_RENDER_INPUT: u64 = 1 << 20;

fn render_package() -> Result<ExitCode, String> {
    let mut input = String::new();
    std::io::stdin()
        .take(MAX_RENDER_INPUT + 1)
        .read_to_string(&mut input)
        .map_err(|error| format!("读取请求失败：{error}"))?;
    if input.len() as u64 > MAX_RENDER_INPUT {
        return Err("请求太大".to_string());
    }
    let request: RenderRequest =
        serde_json::from_str(&input).map_err(|error| format!("请求格式不对：{error}"))?;
    let rendered = render_documents(&request).map_err(|error| error.to_string())?;
    let text =
        serde_json::to_string(&rendered).map_err(|error| format!("结果序列化失败：{error}"))?;
    println!("{text}");
    Ok(ExitCode::SUCCESS)
}
