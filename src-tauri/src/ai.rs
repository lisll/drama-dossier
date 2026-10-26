//! AI 能力调用：OpenAI 兼容协议的文本（剧名/简介）与生图（海报）。
//!
//! 设计：
//! - 用户在「设置 → 转码设置」配置 base_url + api_key + model（DeepSeek/智谱/通义/Kimi/中转站均兼容）；
//! - 全部走 `reqwest::blocking`（在 tauri 的 spawn_blocking 中执行），TLS 用 rustls（本机 openssl 有存量坑）；
//! - 未配置时返回带操作指引的中文错误；API key 绝不写入日志；
//! - 响应解析函数独立出来做离线单测，网络层与解析层分离。

use std::path::{Path, PathBuf};
use std::time::Duration;

use dossier_core::AppSettings;
use serde_json::json;

/// 单次请求超时：生图可能较慢，放宽到 3 分钟。
const HTTP_TIMEOUT: Duration = Duration::from_secs(180);

/// 文本生成任务种类：剧名 / 简介。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Title,
    Synopsis,
}

/// 取出必填的非空配置项；未配置时返回带指引的提示。
fn required_field<'a>(
    value: &'a Option<String>,
    label: &str,
    setting_page: &str,
) -> Result<&'a str, String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("尚未配置{label}。请到 设置 → {setting_page} 填写后再试。"))
}

/// LLM 三项配置（base_url / api_key / model）。
fn llm_config(settings: &AppSettings) -> Result<(String, String, String), String> {
    let base = required_field(&settings.llm_base_url, "LLM API 地址", "转码设置")?;
    let key = required_field(&settings.llm_api_key, "LLM API 密钥", "转码设置")?;
    let model = required_field(&settings.llm_model, "LLM 模型名", "转码设置")?;
    Ok((
        base.trim_end_matches('/').to_string(),
        key.to_string(),
        model.to_string(),
    ))
}

/// 生图三项配置。
fn img_config(settings: &AppSettings) -> Result<(String, String, String), String> {
    let base = required_field(&settings.img_base_url, "生图 API 地址", "转码设置")?;
    let key = required_field(&settings.img_api_key, "生图 API 密钥", "转码设置")?;
    let model = required_field(&settings.img_model, "生图模型名", "转码设置")?;
    Ok((
        base.trim_end_matches('/').to_string(),
        key.to_string(),
        model.to_string(),
    ))
}

/// 组装文本生成的 system / user 提示词。
fn build_text_prompt(kind: TextKind, context: &str) -> (String, String) {
    let context = context.trim();
    match kind {
        TextKind::Title => (
            "你是短剧报审材料的剧名策划。只输出一个剧名，不要任何解释、引号或标点结尾。".to_string(),
            format!(
                "请根据以下剧情素材生成一个 8~12 字的竖屏短剧剧名，要求口语化、有钩子、适合平台报审：\n{context}"
            ),
        ),
        TextKind::Synopsis => (
            "你是短剧报审材料的简介撰写。只输出一段简介正文，不要标题、不要分点。".to_string(),
            format!(
                "请根据以下剧情素材写一段 200 字左右的短剧简介，\
                 要求交代主角设定与目标、核心冲突如何升级、关键转折与最终结局走向，\
                 叙事连贯有戏剧张力，不要罗列关键词：\n{context}"
            ),
        ),
    }
}

/// blocking 客户端（统一超时与 UA 标识）。
fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent("DramaDossier/0.2")
        .build()
        .map_err(|error| format!("初始化 HTTP 客户端失败：{error}"))
}

/// 调用 chat/completions 生成文本（blocking，须在 spawn_blocking 中调用）。
fn chat_blocking(
    base: &str,
    key: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let client = http_client()?;
    let response = client
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(&json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "temperature": 0.9
        }))
        .send()
        .map_err(|error| format!("连接 LLM API 失败：{error}。请检查网络与 API 地址。"))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| format!("读取 LLM 响应失败：{error}"))?;
    if !status.is_success() {
        // 状态码非 2xx 时提取 error.message，方便定位（不回显 key）。
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect::<String>());
        return Err(format!("LLM API 返回 {status}：{detail}"));
    }
    parse_chat_content(&body)
}

/// 解析 chat/completions 响应体，取出首个 content（离线单测覆盖）。
fn parse_chat_content(body: &str) -> Result<String, String> {
    // 有些兼容服务会把整个响应体包在代码围栏里，先剥一层再解析。
    let cleaned = body
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value =
        serde_json::from_str(cleaned).map_err(|error| format!("LLM 响应不是合法 JSON：{error}"))?;
    if let Some(message) = value["error"]["message"].as_str() {
        return Err(format!("LLM API 报错：{message}"));
    }
    let content = value["choices"][0]["message"]["content"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            format!(
                "LLM 响应缺少 content：{}",
                body.chars().take(200).collect::<String>()
            )
        })?;
    // 有些模型会把答案包在代码块里，去掉常见的围栏。
    Ok(content
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string())
}

/// 生图响应里取出图像数据：优先 b64_json，其次 url（url 由调用方下载）。
/// 返回 `Left(b64)` 或 `Right(url)`，用枚举避免引入 Either 依赖。
#[derive(Debug, PartialEq, Eq)]
pub enum ImagePayload {
    Base64(String),
    Url(String),
}

/// 解析 images/generations 响应体（离线单测覆盖）。
fn parse_image_payload(body: &str) -> Result<ImagePayload, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("生图 API 响应不是合法 JSON：{error}"))?;
    if let Some(message) = value["error"]["message"].as_str() {
        return Err(format!("生图 API 报错：{message}"));
    }
    let first = &value["data"][0];
    if let Some(b64) = first["b64_json"].as_str().filter(|text| !text.is_empty()) {
        return Ok(ImagePayload::Base64(b64.to_string()));
    }
    if let Some(url) = first["url"].as_str().filter(|text| !text.is_empty()) {
        return Ok(ImagePayload::Url(url.to_string()));
    }
    Err(format!(
        "生图 API 响应中既无 b64_json 也无 url：{}",
        body.chars().take(200).collect::<String>()
    ))
}

/// base64 解码为字节（离线单测覆盖）。
fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|error| format!("海报图片 base64 解码失败：{error}"))
}

/// 海报提示词：结合剧名与简介生成竖版海报。
fn build_poster_prompt(title: &str, synopsis: &str) -> String {
    let title = title.trim();
    let synopsis = synopsis.trim();
    let mut prompt =
        String::from("竖版短剧海报，9:16 构图，电影感布光，中文标题醒目排版，适合平台报审");
    if !title.is_empty() {
        prompt.push_str(&format!("；剧名《{title}》"));
    }
    if !synopsis.is_empty() {
        prompt.push_str(&format!(
            "；剧情：{}",
            synopsis.chars().take(60).collect::<String>()
        ));
    }
    prompt
}

/// 写入海报文件（父目录自动创建）。
fn save_poster_bytes(output_dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    let path = output_dir.join("剧目海报.png");
    std::fs::write(&path, bytes)
        .map_err(|error| format!("写入海报失败（{}）：{error}", path.display()))?;
    Ok(path)
}

/// 深度处理材料阶段的 blocking 生成入口：按设置生成剧名或简介（须在 spawn_blocking 中调用）。
pub(crate) fn generate_text_blocking(
    settings: &AppSettings,
    kind: TextKind,
    context: &str,
) -> Result<String, String> {
    let (base, key, model) = llm_config(settings)?;
    let (system, user) = build_text_prompt(kind, context);
    chat_blocking(&base, &key, &model, &system, &user)
}

/// 生成竖版海报并落盘（blocking，须在 spawn_blocking 中调用）。
///
/// 从命令入口抽出的公共实现：命令（手动按钮）与深度处理材料阶段共用，
/// 落点都是 `output_dir/剧目海报.png`。返回落盘路径。
pub(crate) fn generate_poster_blocking(
    settings: &AppSettings,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    let title = settings.drama_title.clone().unwrap_or_default();
    let synopsis = settings.drama_synopsis.clone().unwrap_or_default();
    poster_blocking(settings, output_dir, &title, &synopsis)
}

/// 按指定的剧名/简介生成竖版海报并落盘。
pub(crate) fn poster_blocking(
    settings: &AppSettings,
    output_dir: &Path,
    title: &str,
    synopsis: &str,
) -> Result<PathBuf, String> {
    let (base, key, model) = img_config(settings)?;
    let prompt = build_poster_prompt(title, synopsis);
    let client = http_client()?;
    let post = |payload: serde_json::Value| -> Result<String, String> {
        let response = client
            .post(format!("{base}/images/generations"))
            .bearer_auth(&key)
            .json(&payload)
            .send()
            .map_err(|error| format!("连接生图 API 失败：{error}。请检查网络与 API 地址。"))?;
        let status = response.status();
        let body = response
            .text()
            .map_err(|error| format!("读取生图响应失败：{error}"))?;
        if !status.is_success() {
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.chars().take(200).collect::<String>());
            return Err(format!("生图 API 返回 {status}：{detail}"));
        }
        Ok(body)
    };
    // 先按竖版尺寸请求；部分兼容服务不支持 size 参数，报错后去掉重试一次。
    // 不发 n：火山方舟图片接口未定义该参数（固定单图），OpenAI 规范缺省值也是 1。
    let with_size = json!({"model": model, "prompt": prompt, "size": "1024x1792"});
    let body = match post(with_size) {
        Ok(body) => body,
        Err(error) if error.contains("size") => post(json!({"model": model, "prompt": prompt}))?,
        Err(error) => return Err(error),
    };
    let bytes = match parse_image_payload(&body)? {
        ImagePayload::Base64(b64) => decode_base64(&b64)?,
        ImagePayload::Url(url) => client
            .get(url)
            .send()
            .map_err(|error| format!("下载海报失败：{error}"))?
            .error_for_status()
            .map_err(|error| format!("下载海报失败：{error}"))?
            .bytes()
            .map_err(|error| format!("读取海报数据失败：{error}"))?
            .to_vec(),
    };
    save_poster_bytes(output_dir, &bytes)
}

/// LLM 三项配置是否齐全（供材料阶段判断剧名/简介走 AI 还是沿用已有内容）。
pub(crate) fn storyboard_llm_ready(settings: &AppSettings) -> bool {
    llm_config(settings).is_ok()
}

/// 自动材料流水线用的组合文本结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterialIdentity {
    pub title: String,
    pub synopsis: String,
}

/// 根据视频内容证据一次性生成剧名与简介。
pub(crate) fn material_identity_blocking(
    settings: &AppSettings,
    content_evidence: &str,
) -> Result<MaterialIdentity, String> {
    let (base, key, model) = llm_config(settings)?;
    let system = "你是短剧报审材料策划。只输出一个 UTF-8 JSON 对象，不要 Markdown、解释或代码围栏。\
                  格式：{\"title\":\"8~12字剧名\",\"synopsis\":\"200字左右简介\"}。\
                  synopsis 要写成完整故事弧线：主角设定与目标、核心冲突、升级与转折、结局走向，叙事连贯。\
                  内容必须只依据用户提供的视频转写与画面文字证据，不得编造证据之外的关键情节。";
    let user = format!("以下是视频内容证据：\n{content_evidence}\n请生成报审材料。");
    let body = chat_blocking(&base, &key, &model, system, &user)?;
    parse_material_identity(&body)
}

/// 解析自动材料响应；模型偶尔包一层代码围栏，这里统一剥掉。
pub(crate) fn parse_material_identity(body: &str) -> Result<MaterialIdentity, String> {
    let cleaned = body
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    // 部分模型会在 JSON 前后加一句说明；只截第一个 `{` 到最后一个 `}`，比要求模型
    // 严格输出更稳，也不会把围栏外的文字当成字段。
    let json_text = match (cleaned.find('{'), cleaned.rfind('}')) {
        (Some(start), Some(end)) if end > start => &cleaned[start..=end],
        _ => cleaned,
    };
    let value: serde_json::Value =
        serde_json::from_str(json_text).map_err(|error| format!("材料 JSON 解析失败：{error}"))?;
    let required = |key: &str| -> Result<String, String> {
        value[key]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("材料 JSON 缺少 {key}"))
    };
    Ok(MaterialIdentity {
        title: required("title")?,
        synopsis: required("synopsis")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_content_reads_first_choice() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":" 风起南城 "}}]}"#;
        assert_eq!(parse_chat_content(body).unwrap(), "风起南城");
    }

    #[test]
    fn parse_chat_content_strips_code_fence() {
        // 兼容服务可能把整个响应体包在围栏里，应能剥掉并取出 content。
        let body = "```json\n{\"choices\":[{\"message\":{\"content\":\"重生之逆袭\"}}]}\n```";
        assert_eq!(parse_chat_content(body).unwrap(), "重生之逆袭");
    }

    #[test]
    fn parse_chat_content_surfaces_api_error() {
        let body = r#"{"error":{"message":"rate limited"}}"#;
        assert!(parse_chat_content(body)
            .unwrap_err()
            .contains("rate limited"));
    }

    #[test]
    fn parse_chat_content_rejects_empty_choices() {
        let body = r#"{"choices":[]}"#;
        assert!(parse_chat_content(body).is_err());
    }

    #[test]
    fn parse_image_payload_prefers_base64() {
        let body = r#"{"data":[{"b64_json":"QUJD","url":"http://x/y.png"}]}"#;
        assert_eq!(
            parse_image_payload(body).unwrap(),
            ImagePayload::Base64("QUJD".to_string())
        );
    }

    #[test]
    fn parse_image_payload_falls_back_to_url() {
        let body = r#"{"data":[{"url":"http://x/y.png"}]}"#;
        assert_eq!(
            parse_image_payload(body).unwrap(),
            ImagePayload::Url("http://x/y.png".to_string())
        );
    }

    #[test]
    fn parse_image_payload_surfaces_api_error() {
        let body = r#"{"error":{"message":"invalid size"}}"#;
        assert!(parse_image_payload(body)
            .unwrap_err()
            .contains("invalid size"));
    }

    #[test]
    fn decode_base64_roundtrip() {
        assert_eq!(decode_base64("QUJD").unwrap(), b"ABC");
        assert!(decode_base64("!!").is_err());
    }

    #[test]
    fn llm_config_requires_all_fields() {
        let mut settings = AppSettings::default();
        assert!(llm_config(&settings).is_err());
        settings.llm_base_url = Some("https://api.x.com/v1/".to_string());
        settings.llm_api_key = Some("sk-test".to_string());
        settings.llm_model = Some("demo".to_string());
        let (base, key, model) = llm_config(&settings).unwrap();
        // 尾部斜杠被归一化。
        assert_eq!(base, "https://api.x.com/v1");
        assert_eq!(key, "sk-test");
        assert_eq!(model, "demo");
    }

    #[test]
    fn poster_prompt_includes_title_and_synopsis() {
        let prompt = build_poster_prompt("风起南城", "她重生回到婚礼当天。");
        assert!(prompt.contains("风起南城"));
        assert!(prompt.contains("她重生回到婚礼当天"));
    }

    #[test]
    fn parse_material_identity_accepts_fenced_json() {
        let body = "```json\n{\"title\":\"雪夜进山\",\"synopsis\":\"医生被困山村。\"}\n```";
        let parsed = parse_material_identity(body).unwrap();
        assert_eq!(parsed.title, "雪夜进山");
        assert_eq!(parsed.synopsis, "医生被困山村。");
    }

    #[test]
    fn parse_material_identity_accepts_json_wrapped_in_prose() {
        let body = "已根据视频证据生成材料：\n\
                    {\"title\":\"雪夜进山\",\"synopsis\":\"医生被困山村。\"}\n\
                    以上内容仅供参考。";
        let parsed = parse_material_identity(body).unwrap();
        assert_eq!(parsed.title, "雪夜进山");
        assert_eq!(parsed.synopsis, "医生被困山村。");
    }

    #[test]
    fn parse_material_identity_requires_complete_fields() {
        assert!(parse_material_identity(r#"{"title":"只有标题"}"#).is_err());
        assert!(parse_material_identity(r#"{"title":"标题","synopsis":"  "}"#).is_err());
    }
}
