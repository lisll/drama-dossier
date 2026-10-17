//! 处理配方的续跑校验与任务哈希（M4b，总方案 2.2 节）。
//!
//! 任务哈希：桌面端对「要处理的那批文件 + 第一层模板 + 权属材料 + 档位」算一个 SHA-256，
//! 配方绑定它，换任何一项本机验签就不过。规则写在 cloud/docs/COMMERCE.md；
//! Python 侧参考实现在 cloud/tests/commerce/version_task.py，两边用共享测试向量对拍。
//!
//! 续跑：配方已在服务端消费过，本机保存的配方原文只用于把同一个任务做完——
//! 重新验签名、机器码、任务哈希，但不再验有效期（有效期是防止囤配方；配方已用掉，
//! 再限时间没有意义，一部长剧本来就可能处理超过 24 小时）。参数固定在配方里，
//! 反复「续跑」产出的文件和第一次完全相同，换不出新版本。

use serde_json::Value;

use crate::envelope::{self, Purpose};
use crate::keys::TrustedKey;
use crate::manifest::is_sha256;
use crate::recipe::{parse_payload, Recipe, RecipeError, STRENGTHS};

/// 任务哈希描述的版本号。规则变了就换新值，新旧哈希互不通用。
pub const TASK_SCHEMA: &str = "version-task@1";

/// 续跑校验：验签名、机器码、任务，跳过有效期（见模块注释）。
pub fn verify_consumed(
    token: &str,
    trusted: &[TrustedKey],
    device_id: &str,
    task: &str,
) -> Result<Recipe, RecipeError> {
    let (payload, signed_by) = envelope::open(Purpose::Recipe, token, trusted)?;
    let recipe = parse_payload(&payload, signed_by)?;
    if recipe.device_id != device_id {
        return Err(RecipeError::WrongDevice);
    }
    if recipe.task != task {
        return Err(RecipeError::WrongTask);
    }
    Ok(recipe)
}

/// 任务哈希的输入：配方只能用在申请时那批文件、那份模板、那份权属材料、那个档位上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionTask {
    /// 分集（按文件名升序排好后传入），文件名只取文件本身的名字，不带目录。
    pub episodes: Vec<EpisodeInput>,
    /// 第一层模板 JSON 规范化后的 SHA-256（[`template_hash`]）。
    pub template: String,
    /// 权属材料文件的 SHA-256（小写十六进制）。
    pub rights: String,
    /// 强度档：light / medium / heavy。
    pub strength: String,
}

/// 一集的输入：文件名（不带目录）和文件内容的 SHA-256（小写十六进制）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeInput {
    pub name: String,
    pub sha256: String,
}

/// 把任务描述规范化成待哈希的字节：紧凑 JSON（无空白），对象键按字母序。
pub fn task_input_bytes(task: &VersionTask) -> Result<Vec<u8>, TaskHashError> {
    if !STRENGTHS.contains(&task.strength.as_str()) {
        return Err(TaskHashError::BadStrength);
    }
    if !is_sha256(&task.rights) {
        return Err(TaskHashError::BadHex("权属材料"));
    }
    if !is_sha256(&task.template) {
        return Err(TaskHashError::BadHex("模板"));
    }
    for episode in &task.episodes {
        if episode.name.is_empty()
            || episode.name.contains('/')
            || episode.name.contains('\\')
            || episode.name.contains('\0')
        {
            return Err(TaskHashError::BadName(episode.name.clone()));
        }
        if !is_sha256(&episode.sha256) {
            return Err(TaskHashError::BadHex("分集文件"));
        }
    }
    let episodes: Vec<Value> = task
        .episodes
        .iter()
        .map(|episode| {
            serde_json::json!({
                "name": episode.name,
                "sha256": episode.sha256,
            })
        })
        .collect();
    let object = serde_json::json!({
        "episodes": episodes,
        "rights": task.rights,
        "schema": TASK_SCHEMA,
        "strength": task.strength,
        "template": task.template,
    });
    canonical_json(&object)
}

/// 任务哈希 = SHA-256( 规范化 JSON )，小写十六进制。
pub fn version_task_hash(task: &VersionTask) -> Result<String, TaskHashError> {
    let bytes = task_input_bytes(task)?;
    Ok(sha256_hex(&bytes))
}

/// 模板 JSON 的规范化哈希：`SHA-256(canonical_json(模板))`。
/// 用户在界面里保存的模板只差空格、键顺序时哈希不变，改了内容才变。
pub fn template_hash(template: &Value) -> Result<String, TaskHashError> {
    let bytes = canonical_json(template)?;
    Ok(sha256_hex(&bytes))
}

/// 紧凑 JSON：无空白、非 ASCII 字符按 UTF-8 原样输出、对象键按字母序。
/// 和 Python 侧 `json.dumps(..., sort_keys=True, separators=(",", ":"), ensure_ascii=False)` 对拍。
fn canonical_json(value: &Value) -> Result<Vec<u8>, TaskHashError> {
    let mut buffer = Vec::new();
    write_canonical(value, &mut buffer)?;
    Ok(buffer)
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> Result<(), TaskHashError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(flag) => out.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Value::Number(number) => {
            let text = if let Some(int) = number.as_i64() {
                int.to_string()
            } else if let Some(int) = number.as_u64() {
                int.to_string()
            } else if let Some(float) = number.as_f64() {
                python_float_repr(float)?
            } else {
                return Err(TaskHashError::BadNumber);
            };
            out.extend_from_slice(text.as_bytes());
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_canonical(&map[*key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for character in text.chars() {
        match character {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{08}' => out.extend_from_slice(b"\\b"),
            '\u{0C}' => out.extend_from_slice(b"\\f"),
            // 其余控制字符（U+0000..=U+001F）按 \\u00XX 转义，其余字符（含中文）原样输出。
            control if (control as u32) < 0x20 => {
                let code = control as u32;
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(code >> 4) as usize]);
                out.push(HEX[(code & 0xF) as usize]);
            }
            plain => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(plain.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out.push(b'"');
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// 浮点数按 Python `repr(float)` 的写法输出：整数值带 `.0`（`1.0`），其余取最短往返表示。
/// Rust 和 Python 都用最短往返算法，十进制数字相同；差别只在整数值和科学计数法。
/// Python 在指数 < -4 或 ≥ 16 时改用科学计数法，模板里不会出现这种值，遇到直接拒绝，
/// 免得两边算出不同的哈希。
fn python_float_repr(value: f64) -> Result<String, TaskHashError> {
    if !value.is_finite() {
        return Err(TaskHashError::BadNumber);
    }
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
        return Err(TaskHashError::BadNumber);
    }
    let text = value.to_string();
    if text.contains('.') {
        Ok(text)
    } else {
        Ok(format!("{text}.0"))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskHashError {
    #[error("强度档取值不正确")]
    BadStrength,
    #[error("{0}的哈希不是小写十六进制")]
    BadHex(&'static str),
    #[error("分集文件名不能用作任务描述：{0}")]
    BadName(String),
    #[error("模板里有数字超出可表示范围")]
    BadNumber,
}

impl TaskHashError {
    pub fn code(&self) -> &'static str {
        match self {
            TaskHashError::BadStrength => "bad_strength",
            TaskHashError::BadHex(_) => "bad_hex",
            TaskHashError::BadName(_) => "bad_name",
            TaskHashError::BadNumber => "bad_number",
        }
    }
}
