//! `render_documents` 的集成测试：它是纯函数，同样的输入必须给出逐字节相同的输出。
//!
//! 覆盖：确定性、分集清单里的 SHA256、成本表缺字段时的「（待填写）」、CSV 转义、
//! 分集「复制进包」和「只登记哈希」两种放法，以及各种被拒绝的输入。
//! 只走公开 API（`dossier_core::render_documents` 和 `package` 模块导出的类型）。

use dossier_core::package::{Block, RenderError, SealedDocument, MAX_EPISODES};
use dossier_core::{
    render_documents, CostSheetForm, EpisodeEntry, EpisodeSpec, LocalAsset, RenderRequest,
    RenderedDocuments,
};

const DRAMA: &str = "测试剧";
/// 源码里的占位说法（`package.rs` 的 `PENDING`）。
const PENDING: &str = "（待填写）";
const CSV_PATH: &str = "03_分集清单/分集清单.csv";
const COST_MD_PATH: &str = "04_成本配置表/成本配置表.md";
const COST_PDF_SUFFIX: &str = "成本配置表.pdf";

/// 64 位小写十六进制：同一个字符重复出来，肉眼能分清是哪一集。
fn hash_of(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// 两个明显不同的假哈希，用来验证清单里的哈希确实来自请求。
fn hash_a() -> String {
    hash_of('1')
}
fn hash_b() -> String {
    hash_of('2')
}

fn spec() -> EpisodeSpec {
    EpisodeSpec {
        duration_sec: 95.5,
        width: 1080,
        height: 1920,
        video_codec: Some("h264".into()),
        audio_codec: Some("aac".into()),
        fps: Some(30.0),
        bitrate_bps: Some(4_000_000),
    }
}

fn episode(file_name: &str, number: u32, sha256: &str) -> EpisodeEntry {
    EpisodeEntry {
        file_name: file_name.into(),
        episode_number: Some(number),
        size_bytes: 1024,
        sha256: sha256.into(),
        spec: spec(),
    }
}

fn asset(file_name: &str, sha256: &str) -> LocalAsset {
    LocalAsset {
        file_name: file_name.into(),
        size_bytes: 10,
        sha256: sha256.into(),
    }
}

/// 一份正常的请求：两集、简介、封面、成本表填满。
fn base_request() -> RenderRequest {
    RenderRequest {
        drama_name: DRAMA.into(),
        ai_generated: false,
        copy_episodes: false,
        cost_sheet: Some(CostSheetForm {
            company_name: "某某文化传媒".into(),
            total_investment_wan: Some(120.0),
            actor_ratio_percent: Some(35.0),
            contact: "13800000000".into(),
        }),
        synopsis: Some(asset("简介.txt", &hash_of('3'))),
        cover: Some(asset("封面.jpg", &hash_of('4'))),
        episodes: vec![
            episode("第01集.mp4", 1, &hash_a()),
            episode("第02集.mp4", 2, &hash_b()),
        ],
    }
}

fn text_file<'r>(rendered: &'r RenderedDocuments, path: &str) -> &'r str {
    rendered
        .text_files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("包里没有 {path}"))
        .content
        .as_str()
}

/// 按路径后缀找一份需要盖章的文档。
fn sealed_doc<'r>(rendered: &'r RenderedDocuments, suffix: &str) -> &'r SealedDocument {
    rendered
        .sealed
        .iter()
        .find(|doc| doc.path.ends_with(suffix))
        .unwrap_or_else(|| panic!("排版模型里没有以 {suffix} 结尾的文档"))
}

/// 成本表 PDF 里的那张两列表格。
fn cost_table_rows(rendered: &RenderedDocuments) -> &Vec<(String, String)> {
    sealed_doc(rendered, COST_PDF_SUFFIX)
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Table { rows } => Some(rows),
            _ => None,
        })
        .expect("成本表的排版模型里应该有一张表格")
}

/// 文档里所有露在外面的文字（段落、说明、表格两列）。
fn document_text(doc: &SealedDocument) -> Vec<String> {
    doc.blocks
        .iter()
        .flat_map(|block| match block {
            Block::Paragraph { text } | Block::Note { text } => vec![text.clone()],
            Block::Table { rows } => rows
                .iter()
                .flat_map(|(label, value)| [label.clone(), value.clone()])
                .collect(),
        })
        .collect()
}

/// 极简 CSV 解析：按 `csv_escape` 的规则（双引号包裹、内部双引号翻倍），够这些测试用。
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                '\r' => {}
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn expect_error(request: &RenderRequest) -> String {
    render_documents(request).expect_err("应当拒绝").to_string()
}

// ---------- 确定性 ----------

#[test]
fn same_input_gives_byte_identical_output() {
    let request = base_request();
    let first = render_documents(&request).unwrap();
    let second = render_documents(&request).unwrap();
    assert_eq!(first, second, "同样的输入，结果必须相等");
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap(),
        "序列化后要逐字节相同"
    );

    // 请求走一趟 JSON 再回来（桌面端→服务端就是这么传的），文档内容不变。
    let wire = serde_json::to_vec(&request).unwrap();
    let parsed: RenderRequest = serde_json::from_slice(&wire).unwrap();
    assert_eq!(render_documents(&parsed).unwrap(), first);
}

// ---------- 分集清单 ----------

#[test]
fn episode_csv_keeps_the_sha256_from_the_request() {
    let rendered = render_documents(&base_request()).unwrap();
    let rows = parse_csv(text_file(&rendered, CSV_PATH));
    assert_eq!(
        rows[0],
        [
            "集号",
            "文件名",
            "时长(秒)",
            "宽",
            "高",
            "视频编码",
            "音频编码",
            "码率(Mbps)",
            "SHA256"
        ]
    );
    assert_eq!(rows.len(), 3, "表头 + 两集");

    assert_eq!(rows[1][0], "1");
    assert_eq!(rows[1][1], "第01集.mp4");
    assert_eq!(rows[1][8], hash_a(), "第一集的哈希就是请求里给的");
    assert_eq!(rows[2][8], hash_b(), "第二集的哈希就是请求里给的");
    assert_ne!(rows[1][8], rows[2][8], "两集的哈希不能串行");
    assert_eq!(rows[1][8].len(), 64);

    // 规格也来自请求，不是编出来的。
    assert_eq!(rows[1][2], "95.5");
    assert_eq!(rows[1][3], "1080");
    assert_eq!(rows[1][4], "1920");
    assert_eq!(rows[1][5], "h264");
    assert_eq!(rows[1][6], "aac");
    assert_eq!(rows[1][7], "4.00");
}

// ---------- 成本表缺数据 ----------

#[test]
fn cost_sheet_without_data_says_pending() {
    // （说明：整份 none / 空表单 / 只有空白 / 只填了机构）→ 该有几行是占位。
    for (tag, form, pending_rows) in [
        ("整份没填", None, 4),
        ("表单为空", Some(CostSheetForm::default()), 4),
        (
            "只有空格",
            Some(CostSheetForm {
                company_name: "   ".into(),
                total_investment_wan: None,
                actor_ratio_percent: None,
                contact: " ".into(),
            }),
            4,
        ),
        (
            "只填了机构",
            Some(CostSheetForm {
                company_name: "某某文化传媒".into(),
                ..CostSheetForm::default()
            }),
            3,
        ),
    ] {
        let mut request = base_request();
        request.cost_sheet = form;
        let rendered = render_documents(&request).unwrap();

        let markdown = text_file(&rendered, COST_MD_PATH);
        assert!(markdown.contains(PENDING), "{tag}：Markdown 里要有占位");

        let rows = cost_table_rows(&rendered);
        assert_eq!(rows.len(), 4, "{tag}：成本表还是四项");
        let pending = rows.iter().filter(|(_, value)| value == PENDING).count();
        assert_eq!(pending, pending_rows, "{tag}：占位行数");
        assert!(
            document_text(sealed_doc(&rendered, COST_PDF_SUFFIX))
                .iter()
                .any(|text| text == PENDING),
            "{tag}：排版模型里也要有占位"
        );
    }

    // 一项都没填时，Markdown 里正好四个占位（四个表格行各一个）。
    let mut request = base_request();
    request.cost_sheet = None;
    let blank_sheet = render_documents(&request).unwrap();
    assert_eq!(
        text_file(&blank_sheet, COST_MD_PATH)
            .matches(PENDING)
            .count(),
        4
    );
    assert!(cost_table_rows(&blank_sheet)
        .iter()
        .all(|(label, _)| !label.is_empty()));

    // 填了的项正常显示，不再出现占位。
    let full = render_documents(&base_request()).unwrap();
    let markdown = text_file(&full, COST_MD_PATH);
    assert!(!markdown.contains(PENDING), "{markdown}");
    for text in ["某某文化传媒", "120 万元", "35 %", "13800000000"] {
        assert!(markdown.contains(text), "成本表里应有 {text}：{markdown}");
    }
}

// ---------- CSV 转义 ----------

#[test]
fn csv_escapes_commas_and_quotes() {
    // 文件名里有逗号：整个字段用双引号包起来，解析回来还是原样。
    let mut request = base_request();
    request.episodes[0].file_name = "第01集,上.mp4".into();
    // 编码里同时有逗号和双引号：双引号翻倍，外面再包一层。
    request.episodes[0].spec.video_codec = Some("H\"264,AVC".into());
    let rendered = render_documents(&request).unwrap();
    let csv = text_file(&rendered, CSV_PATH);
    assert!(csv.contains("\"第01集,上.mp4\""), "{csv}");
    assert!(csv.contains("\"H\"\"264,AVC\""), "{csv}");

    let rows = parse_csv(csv);
    assert_eq!(rows[1][1], "第01集,上.mp4", "解析回来和原名一致");
    assert_eq!(rows[1][5], "H\"264,AVC", "解析回来和原编码一致");
    assert_eq!(rows[1][8], hash_a(), "转义不影响后面的列");
    assert_eq!(rows[2][1], "第02集.mp4", "第二集不受影响");

    // 剧名里有逗号，不经过 CSV（只写进剧名.txt 和权属声明），保持原样。
    let mut request = base_request();
    request.drama_name = "测试剧,上".into();
    let rendered = render_documents(&request).unwrap();
    assert_eq!(text_file(&rendered, "01_剧集信息/剧名.txt"), "测试剧,上");
    assert_eq!(
        sealed_doc(&rendered, "内容权属声明.pdf").blocks[0],
        Block::Paragraph {
            text: "剧目名称：测试剧,上".into()
        }
    );

    // 双引号和换行进不了文件名：`validate_request` 先按安全文件名挡住了，
    // 所以带双引号/换行的转义只能由编码字段走到（上面那条）。
    for bad in ["第\"01\"集.mp4", "第01集\n.mp4", "第01集\r.mp4"] {
        let mut request = base_request();
        request.episodes[0].file_name = bad.into();
        assert!(render_documents(&request).is_err(), "{bad:?} 应被拒绝");
    }
}

// ---------- 分集的两种放法 ----------

#[test]
fn episodes_are_copied_or_only_recorded() {
    let mut request = base_request();

    request.copy_episodes = true;
    let copied = render_documents(&request).unwrap();
    let paths: Vec<&str> = copied
        .local_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert!(paths.contains(&"06_分集视频/第01集.mp4"), "{paths:?}");
    assert!(paths.contains(&"06_分集视频/第02集.mp4"), "{paths:?}");
    assert!(paths.contains(&"01_剧集信息/简介.txt"), "{paths:?}");
    assert!(paths.contains(&"02_封面海报/封面.jpg"), "{paths:?}");
    assert!(copied.references.is_empty(), "复制进包时不再登记为引用");
    let one = copied
        .local_files
        .iter()
        .find(|file| file.path == "06_分集视频/第01集.mp4")
        .unwrap();
    assert_eq!(one.sha256, hash_a(), "哈希原样带过去");
    assert_eq!(one.size_bytes, 1024);

    request.copy_episodes = false;
    let referenced = render_documents(&request).unwrap();
    assert!(
        !referenced
            .local_files
            .iter()
            .any(|file| file.path.starts_with("06_分集视频/")),
        "不复制时列表里没有分集"
    );
    assert_eq!(referenced.local_files.len(), 2, "只剩简介和封面");
    assert_eq!(
        referenced
            .references
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["第01集.mp4", "第02集.mp4"],
        "引用的路径是剧目目录里的文件名"
    );
    assert_eq!(referenced.references[1].sha256, hash_b());
    assert_eq!(referenced.references[1].size_bytes, 1024);
}

// ---------- 被拒绝的输入 ----------

#[test]
fn rejects_more_episodes_than_the_limit() {
    let mut request = base_request();
    request.episodes = (1..=MAX_EPISODES as u32)
        .map(|number| episode(&format!("第{number:03}集.mp4"), number, &hash_a()))
        .collect();
    assert_eq!(request.episodes.len(), 500);
    assert!(render_documents(&request).is_ok(), "正好 500 集是允许的");

    request
        .episodes
        .push(episode("第501集.mp4", 501, &hash_b()));
    let message = expect_error(&request);
    assert!(message.contains("最多 500 集"), "{message}");
}

#[test]
fn rejects_duplicate_episode_file_names() {
    let mut request = base_request();
    request.episodes[1].file_name = "第01集.mp4".into();
    assert!(expect_error(&request).contains("分集文件名重复"));

    // 只差大小写也算重复：在 Windows / macOS 上会落到同一个文件。
    let mut request = base_request();
    request.episodes[1].file_name = "第01集.MP4".into();
    assert!(expect_error(&request).contains("分集文件名重复"));
}

#[test]
fn rejects_empty_or_path_like_drama_name() {
    for bad in ["", "   ", "测试/剧", "../测试剧", "测试\\剧"] {
        let mut request = base_request();
        request.drama_name = bad.into();
        let message = expect_error(&request);
        assert!(
            message.contains("剧名不能为空") || message.contains("不能用作文件名"),
            "{bad:?} → {message}"
        );
    }
    // 超长和为空分开报：超长时不该说「不能为空」。
    let mut request = base_request();
    request.drama_name = "剧".repeat(201);
    let message = expect_error(&request);
    assert!(message.contains("最长 200 个字"), "{message}");
    assert!(!message.contains("不能为空"), "{message}");
    // 正好 200 个字可以（用 ASCII：200 个汉字是 600 字节，会先撞上路径的 512 字节上限）。
    request.drama_name = "a".repeat(200);
    render_documents(&request).expect("正好 200 个字可以");
    request.drama_name = "剧".repeat(200);
    assert!(expect_error(&request).contains("或者太长"));
    // 首尾空格会被去掉，不算错误。
    let mut request = base_request();
    request.drama_name = "  测试剧  ".into();
    let rendered = render_documents(&request).unwrap();
    assert_eq!(text_file(&rendered, "01_剧集信息/剧名.txt"), DRAMA);
}

#[test]
fn rejects_synopsis_named_after_the_drama_file() {
    // 简介文件正好叫「剧名.txt」：放进 01_剧集信息 时和服务端写的剧名.txt 撞名。
    let mut request = base_request();
    request.synopsis = Some(asset("剧名.txt", &hash_of('5')));
    let message = expect_error(&request);
    assert!(message.contains("同名"), "{message}");
    assert!(message.contains("01_剧集信息/剧名.txt"), "{message}");

    // 换一个不撞名的文件名就正常；碰撞只在路径完全相同时发生。
    let mut request = base_request();
    request.cover = Some(asset("封面.jpg", &hash_of('6')));
    assert!(render_documents(&request).is_ok());
}

#[test]
fn rejects_sha256_that_is_not_64_lowercase_hex() {
    let mut uppercase = "abcdef0123456789".repeat(4);
    assert_eq!(uppercase.len(), 64);
    uppercase.make_ascii_uppercase();
    let bad_values = [
        String::new(),
        "abc".to_owned(),
        "1".repeat(63),
        "1".repeat(65),
        format!("{}G", "1".repeat(63)),
        uppercase,
    ];
    for bad in bad_values {
        for target in ["episode", "synopsis", "cover"] {
            let mut request = base_request();
            match target {
                "episode" => request.episodes[0].sha256 = bad.clone(),
                "synopsis" => request.synopsis = Some(asset("简介.txt", &bad)),
                _ => request.cover = Some(asset("封面.jpg", &bad)),
            }
            let message = expect_error(&request);
            assert!(
                message.contains("校验和格式不对"),
                "{target} {bad:?} → {message}"
            );
        }
    }
    // 64 位小写十六进制就通过。
    let mut request = base_request();
    request.episodes[0].sha256 = "ab".repeat(32);
    assert_eq!(request.episodes[0].sha256.len(), 64);
    assert!(render_documents(&request).is_ok());
}

#[test]
fn rejects_actor_ratio_or_investment_out_of_range() {
    let mut request = base_request();
    request.cost_sheet.as_mut().unwrap().actor_ratio_percent = Some(100.5);
    assert!(expect_error(&request).contains("演员成本占比"));

    let mut request = base_request();
    request.cost_sheet.as_mut().unwrap().actor_ratio_percent = Some(-1.0);
    assert!(expect_error(&request).contains("演员成本占比"));

    // 0% 和 100% 都是允许的边界。
    for ratio in [0.0, 100.0] {
        let mut request = base_request();
        request.cost_sheet.as_mut().unwrap().actor_ratio_percent = Some(ratio);
        assert!(render_documents(&request).is_ok(), "{ratio}%");
    }

    let mut request = base_request();
    request.cost_sheet.as_mut().unwrap().total_investment_wan = Some(-1.0);
    assert!(expect_error(&request).contains("投资总额"));

    // 上限是 1e9：超了也拒绝（消息里只说了「不小于 0」，见最终报告）。
    let mut request = base_request();
    request.cost_sheet.as_mut().unwrap().total_investment_wan = Some(1e9 + 1.0);
    assert!(expect_error(&request).contains("投资总额"));
}

#[test]
fn rejects_company_name_longer_than_200_chars() {
    let mut request = base_request();
    let long_name = "公".repeat(200);
    request.cost_sheet.as_mut().unwrap().company_name = long_name.clone();
    let rendered = render_documents(&request).unwrap();
    assert!(
        text_file(&rendered, COST_MD_PATH).contains(&long_name),
        "200 字的长公司名要能正常生成"
    );

    request.cost_sheet.as_mut().unwrap().company_name = "公".repeat(300);
    let message = expect_error(&request);
    assert!(message.contains("制作机构"), "{message}");
    assert!(message.contains("200"), "{message}");
}

#[test]
fn render_error_carries_the_chinese_message() {
    let mut request = base_request();
    request.episodes = vec![episode("../逃出去.mp4", 1, &hash_a())];
    let error: RenderError = render_documents(&request).unwrap_err();
    // 消息是中文，服务端原样转给用户：Display 就是那句话本身。
    assert_eq!(error.to_string(), "文件名不合法：../逃出去.mp4");
    assert_eq!(error.0, error.to_string());
}
