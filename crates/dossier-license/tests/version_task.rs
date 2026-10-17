//! version-task@1 任务哈希：和 Python 参考实现（cloud/tests/commerce/version_task.py）对拍。
//! 共享向量在 version_task_vectors.json（Python 生成、提交进仓库）；两边必须逐字节一致。

use serde_json::Value;

use dossier_license::version_task::{
    task_input_bytes, template_hash, version_task_hash, EpisodeInput, VersionTask,
};

/// 读共享向量：向量文件是 Python 生成后提交的，不许手写或删条目。
fn load_cases() -> Vec<Case> {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/version_task_vectors.json"
    ))
    .expect("读共享测试向量");
    serde_json::from_str(&raw).expect("共享测试向量格式")
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    episodes: Vec<(String, String)>,
    rights: String,
    template: Value,
    strength: String,
    task: String,
}

#[test]
fn every_vector_matches_python() {
    let cases = load_cases();
    assert!(cases.len() >= 3, "至少要有一条用例");
    for case in &cases {
        let task = VersionTask {
            episodes: case
                .episodes
                .iter()
                .map(|(name, sha256)| EpisodeInput {
                    name: name.clone(),
                    sha256: sha256.clone(),
                })
                .collect(),
            template: template_hash(&case.template).expect("模板规范化"),
            rights: case.rights.clone(),
            strength: case.strength.clone(),
        };
        let hash = version_task_hash(&task).expect("算哈希");
        assert_eq!(hash, case.task, "用例 {}：和 Python 结果不一致", case.name);
    }
}

#[test]
fn canonical_bytes_are_compact_and_sorted() {
    // 手工核对规范化字节：无空白、键按字母序（episodes < rights < schema < strength < template）。
    let task = VersionTask {
        episodes: vec![EpisodeInput {
            name: "第01集.mp4".into(),
            sha256: "1".repeat(64),
        }],
        template: template_hash(&serde_json::json!({})).unwrap(),
        rights: "3".repeat(64),
        strength: "medium".into(),
    };
    let bytes = task_input_bytes(&task).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.starts_with("{\"episodes\":[{\"name\":\"第01集.mp4\",\"sha256\":\""));
    assert!(text.contains("\"rights\":\""));
    assert!(text.contains("\"schema\":\"version-task@1\""));
    assert!(text.contains("\"strength\":\"medium\""));
    assert!(text.contains("\"template\":\""));
    assert!(!text.contains(' '), "紧凑 JSON 里不该有空格");
}

#[test]
fn template_hash_ignores_spacing_and_key_order() {
    let a: Value = serde_json::from_str(r#"{"b":1,"a":[2,3]}"#).unwrap();
    let b: Value = serde_json::from_str("{ \"a\" : [2,3] ,\n \"b\":1 }").unwrap();
    assert_eq!(template_hash(&a).unwrap(), template_hash(&b).unwrap());
    // 内容变了哈希就变。
    let c: Value = serde_json::from_str(r#"{"b":2,"a":[2,3]}"#).unwrap();
    assert_ne!(template_hash(&a).unwrap(), template_hash(&c).unwrap());
}

/// 浮点规范化后的模板哈希和 task_input_bytes 原始字节。
/// 期望值取自实际运行输出（Python 侧同一模板的 template_hash 与之相同），不是手算的。
#[test]
fn float_template_bytes_match_python() {
    let template = serde_json::json!({
        "零": 0, "零点五": 0.5, "一点零": 1.0, "负二点五": -2.5,
        "下界": 0.0001, "上界": 123456789.5, "小数": 1234.5678,
        "十分之一": 0.1, "整数": 100.0, "长尾": 0.30000000000000004,
    });
    // Python: json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False) 的输出，
    // 整数值带 .0（1.0、100.0）、最短往返表示（0.1、0.30000000000000004）。
    let template_digest = template_hash(&template).unwrap();
    assert_eq!(
        template_digest,
        "7ba5ecd1b7488ece678355c6cedfa77c71d6fea24a54716a03824c9beb12efbb"
    );

    let task = VersionTask {
        episodes: vec![EpisodeInput {
            name: "第01集.mp4".into(),
            sha256: "9a".repeat(32),
        }],
        template: template_digest,
        rights: "b1".repeat(32),
        strength: "light".into(),
    };
    let bytes = task_input_bytes(&task).unwrap();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "{\"episodes\":[{\"name\":\"第01集.mp4\",\"sha256\":\"9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a\"}],\
\"rights\":\"b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1\",\
\"schema\":\"version-task@1\",\"strength\":\"light\",\
\"template\":\"7ba5ecd1b7488ece678355c6cedfa77c71d6fea24a54716a03824c9beb12efbb\"}"
    );
}

#[test]
fn rejects_bad_inputs() {
    let template = template_hash(&serde_json::json!({})).unwrap();
    let bad_strength = VersionTask {
        episodes: vec![],
        template: template.clone(),
        rights: "3".repeat(64),
        strength: "extreme".into(),
    };
    assert_eq!(
        task_input_bytes(&bad_strength).unwrap_err().code(),
        "bad_strength"
    );

    let bad_rights = VersionTask {
        episodes: vec![],
        template: template.clone(),
        rights: "XYZ".into(),
        strength: "medium".into(),
    };
    assert_eq!(task_input_bytes(&bad_rights).unwrap_err().code(), "bad_hex");

    let bad_template = VersionTask {
        episodes: vec![],
        template: "not-hex".into(),
        rights: "3".repeat(64),
        strength: "medium".into(),
    };
    assert_eq!(
        task_input_bytes(&bad_template).unwrap_err().code(),
        "bad_hex"
    );

    let bad_name = VersionTask {
        episodes: vec![EpisodeInput {
            name: "../第01集.mp4".into(),
            sha256: "1".repeat(64),
        }],
        template: template.clone(),
        rights: "3".repeat(64),
        strength: "medium".into(),
    };
    assert_eq!(task_input_bytes(&bad_name).unwrap_err().code(), "bad_name");

    let bad_episode_hash = VersionTask {
        episodes: vec![EpisodeInput {
            name: "第01集.mp4".into(),
            sha256: "12".into(),
        }],
        template,
        rights: "3".repeat(64),
        strength: "medium".into(),
    };
    assert_eq!(
        task_input_bytes(&bad_episode_hash).unwrap_err().code(),
        "bad_hex"
    );
}

/// 向量文件是提交物：条数掉下去说明有人删了用例（或忘了用 UPDATE 重新生成）。
#[test]
fn vector_file_is_not_shrinking() {
    let cases = load_cases();
    assert!(
        cases.len() >= 13,
        "共享向量至少 13 条，实际 {} 条；用 UPDATE_VERSION_TASK_VECTORS=1 重新生成",
        cases.len()
    );
    for name in [
        "basic",
        "empty_episodes",
        "unicode_keys",
        "episode_name_spaces",
        "episode_name_quote",
        "episode_name_newline",
        "episode_name_four_byte_utf8",
        "episode_name_fullwidth_punct",
        "many_episodes",
        "template_rich_types",
        "template_floats",
        "template_key_order_a",
        "template_key_order_b",
    ] {
        assert!(
            cases.iter().any(|case| case.name == name),
            "共享向量缺用例 {name}"
        );
    }
}

/// 模板键顺序不影响任务哈希：两条用例只有键顺序不同，Python 算出的 task 必须相等。
#[test]
fn template_key_order_does_not_change_task() {
    let cases = load_cases();
    let a = cases
        .iter()
        .find(|case| case.name == "template_key_order_a")
        .expect("用例 template_key_order_a");
    let b = cases
        .iter()
        .find(|case| case.name == "template_key_order_b")
        .expect("用例 template_key_order_b");
    assert_eq!(
        a.task, b.task,
        "键顺序不同不该改变任务哈希：Python 生成的两条向量就已不相等，说明有一侧没排序"
    );
    assert_eq!(
        template_hash(&a.template).unwrap(),
        template_hash(&b.template).unwrap()
    );
    assert_eq!(
        version_task_hash(&build_task(a)).unwrap(),
        version_task_hash(&build_task(b)).unwrap()
    );
}

fn build_task(case: &Case) -> VersionTask {
    VersionTask {
        episodes: case
            .episodes
            .iter()
            .map(|(name, sha256)| EpisodeInput {
                name: name.clone(),
                sha256: sha256.clone(),
            })
            .collect(),
        template: template_hash(&case.template).expect("模板规范化"),
        rights: case.rights.clone(),
        strength: case.strength.clone(),
    }
}

/// `task_input_bytes` 会拒绝的分集名：空、含 `/`、含 `\\`、含 NUL。
/// 这些字符不进共享向量（Python 侧没有对应的拒绝逻辑），在这里逐个断言错误码。
#[test]
fn rejects_illegal_episode_names() {
    let template = template_hash(&serde_json::json!({})).unwrap();
    for name in [
        "",
        "../第01集.mp4",
        "第01集/02.mp4",
        "第01集\\02.mp4",
        "第01集\0.mp4",
    ] {
        let task = VersionTask {
            episodes: vec![EpisodeInput {
                name: name.into(),
                sha256: "1".repeat(64),
            }],
            template: template.clone(),
            rights: "3".repeat(64),
            strength: "medium".into(),
        };
        assert_eq!(
            task_input_bytes(&task).unwrap_err().code(),
            "bad_name",
            "分集名 {name:?} 应被拒绝"
        );
    }
    // 空格、半角引号、换行、制表符、四字节 UTF-8、全角标点都是合法文件名（共享向量里已覆盖）。
    for name in [
        "第 01 集.mp4",
        "第\"01\"集.mp4",
        "第01集\n修正.mp4",
        "第01集\t修正.mp4",
        "第01集𝄞🎬.mp4",
        "第01集（修）.mp4",
    ] {
        let task = VersionTask {
            episodes: vec![EpisodeInput {
                name: name.into(),
                sha256: "1".repeat(64),
            }],
            template: template.clone(),
            rights: "3".repeat(64),
            strength: "medium".into(),
        };
        assert!(
            task_input_bytes(&task).is_ok(),
            "分集名 {name:?} 不该被拒绝"
        );
    }
}

/// 浮点越界：Python 那边会切成科学计数法（1e-07），两边无法逐字节一致，所以 Rust 直接拒绝。
/// 边界内的写法（整数值带 .0、最短往返表示）由 `float_template_bytes_match_python` 逐字节核对。
#[test]
fn rejects_floats_outside_python_repr_range() {
    for value in ["1e-7", "1e16", "1e300", "-1e16"] {
        let template: Value = serde_json::from_str(&format!("{{\"k\":{value}}}")).unwrap();
        assert_eq!(
            template_hash(&template).unwrap_err().code(),
            "bad_number",
            "模板浮点 {value} 应被拒绝"
        );
    }
    for value in [
        "0.0001",
        "0.5",
        "1.0",
        "100.0",
        "123456789.5",
        "9999999999999998.0",
    ] {
        let template: Value = serde_json::from_str(&format!("{{\"k\":{value}}}")).unwrap();
        assert!(
            template_hash(&template).is_ok(),
            "模板浮点 {value} 在 1e-4 ≤ |v| < 1e16 内，不该被拒绝"
        );
    }
}
