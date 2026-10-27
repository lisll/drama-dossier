//! 卡密格式预校验：只拦明显输错的情况，最终以服务端结果为准。

/// 与服务端一致：32 个字符，去掉易混的 0/O/1/I。
pub const CARD_ALPHABET: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const CARD_LEN: usize = 16;
const PREFIX: &str = "DDK";

/// 用户输入 → 规范展示形式 `DDK-XXXX-XXXX-XXXX-XXXX`；格式不对返回 `None`。
pub fn normalize_card(raw: &str) -> Option<String> {
    if raw.len() > 64 {
        return None;
    }
    let mut text: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase();
    if text.len() == CARD_LEN + PREFIX.len() && text.starts_with(PREFIX) {
        text = text[PREFIX.len()..].to_string();
    }
    if text.len() != CARD_LEN || !text.chars().all(|c| CARD_ALPHABET.contains(c)) {
        return None;
    }
    let groups: Vec<&str> = (0..4).map(|i| &text[i * 4..i * 4 + 4]).collect();
    Some(format!("{PREFIX}-{}", groups.join("-")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_spellings() {
        let want = Some("DDK-ABCD-EFGH-JKLM-NPQR".to_string());
        assert_eq!(normalize_card("DDK-ABCD-EFGH-JKLM-NPQR"), want);
        assert_eq!(normalize_card("ddk abcd efgh jklm npqr"), want);
        assert_eq!(normalize_card("ABCDEFGHJKLMNPQR"), want);
        assert_eq!(normalize_card("  abcd-efgh-jklm-npqr \n"), want);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "DDK",
            "0000000000000000",
            "ABCDEFGHJKLMNPQ",
            "ABCDEFGHIJKLMNOP",
            "卡密卡密",
        ] {
            assert_eq!(normalize_card(bad), None, "{bad}");
        }
    }
}
