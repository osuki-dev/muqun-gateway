//! Short bilingual UI copy, and Traditional Chinese (Taiwan) derived from it.
//!
//! Views write every string twice, English and Simplified Chinese
//! (`tr(language, en, zh)`). Traditional Chinese is not a third copy: it is
//! derived from the Simplified one offline, in two steps. First a short
//! table of words whose Taiwan usage differs from a character-by-character
//! conversion (`文件` is `檔案`, `设置` is `設定`, `默认` is `預設` …; mirrors
//! the App's zh-TW catalogue). Then OpenCC's `s2tw` character table
//! (`assets/i18n/s2tw-chars.txt`, Apache-2.0, see the NOTICE beside it) for
//! the rest. Converted strings are interned, so `tr_ref` keeps handing out
//! `&'static str` and a string converts once per process.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::config::Language;

/// Pick the owner's language for a string written in English and Simplified Chinese.
pub fn tr(language: Language, en: &str, zh: &str) -> String {
    match language {
        Language::En => en.to_string(),
        Language::ZhCn => crate::ui::i18n::lookup(Language::ZhCn, en)
            .unwrap_or(zh)
            .to_string(),
        Language::ZhTw => crate::ui::i18n::lookup(Language::ZhTw, en)
            .map(|s| s.to_string())
            .unwrap_or_else(|| zh_tw(zh)),
        other => crate::ui::i18n::lookup(other, en)
            .map(|s| s.to_string())
            .unwrap_or_else(|| en.to_string()),
    }
}

pub fn tr_ref(language: Language, en: &'static str, zh: &'static str) -> &'static str {
    match language {
        Language::En => en,
        Language::ZhCn => crate::ui::i18n::lookup(Language::ZhCn, en).unwrap_or(zh),
        Language::ZhTw => {
            crate::ui::i18n::lookup(Language::ZhTw, en).unwrap_or_else(|| zh_tw_ref(zh))
        }
        other => crate::ui::i18n::lookup(other, en).unwrap_or(en),
    }
}

/// Words where Taiwan usage differs from the character conversion, longest
/// first so `文件夹` wins over `文件`. Simplified on the left, as written in
/// the views; the right side is the final Traditional form.
const WORDS: &[(&str, &str)] = &[
    ("文件系统", "檔案系統"),
    ("分屏", "分割畫面"),
    ("会话", "工作階段"),
    ("文件夹", "資料夾"),
    ("收件箱", "收件匣"),
    ("快捷键", "快速鍵"),
    ("服务器", "伺服器"),
    ("用户主目录", "使用者家目錄"),
    ("主目录", "家目錄"),
    ("重命名", "重新命名"),
    ("文件", "檔案"),
    ("设置", "設定"),
    ("默认", "預設"),
    ("程序", "程式"),
    ("进程", "程序"),
    ("消息", "訊息"),
    ("信息", "訊息"),
    ("打开", "開啟"),
    ("刷新", "重新整理"),
    ("发送", "傳送"),
    ("搜索", "搜尋"),
    ("查找", "搜尋"),
    ("链接", "連結"),
    ("窗口", "視窗"),
    ("连接", "連線"),
    ("用户", "使用者"),
    ("保存", "儲存"),
    ("加载", "載入"),
    ("导入", "匯入"),
    ("导出", "匯出"),
    ("运行", "執行"),
    ("点击", "點選"),
    ("源码", "原始碼"),
    ("代码", "程式碼"),
    ("编程", "程式設計"),
    ("软件", "軟體"),
    ("硬件", "硬體"),
    ("磁盘", "磁碟"),
    ("设备", "裝置"),
    ("脚本", "指令碼"),
    ("字段", "欄位"),
    ("数据", "資料"),
    ("查看", "檢視"),
    ("界面", "介面"),
    ("添加", "新增"),
    ("在线", "線上"),
    ("离线", "離線"),
    ("内存", "記憶體"),
    ("网络", "網路"),
    ("粘贴", "貼上"),
    ("剪切", "剪下"),
    ("鼠标", "滑鼠"),
    ("支持", "支援"),
    ("视频", "影片"),
    ("音频", "音訊"),
    ("图标", "圖示"),
    ("项目", "專案"),
    ("终端", "終端機"),
    ("登录", "登入"),
    ("注销", "登出"),
    ("缓存", "快取"),
    ("高级", "進階"),
    ("字体", "字型"),
    ("字号", "字級"),
    ("文本", "文字"),
    ("光标", "游標"),
    ("滚动", "捲動"),
    ("实时", "即時"),
    ("智能", "智慧"),
    ("优化", "最佳化"),
    ("兼容", "相容"),
    ("构建", "建置"),
    ("调试", "除錯"),
    ("日志", "日誌"),
    ("快捷", "快速"),
];

fn chars() -> &'static HashMap<char, char> {
    static TABLE: OnceLock<HashMap<char, char>> = OnceLock::new();
    TABLE.get_or_init(|| {
        include_str!("../../assets/i18n/s2tw-chars.txt")
            .lines()
            .filter_map(|line| {
                let mut it = line.chars();
                let from = it.next()?;
                it.next().filter(|c| *c == '\t')?;
                let to = it.next()?;
                Some((from, to))
            })
            .collect()
    })
}

/// A string already in the owner's language, except that Simplified
/// Chinese built at runtime (counts, names) still needs the Taiwan form.
pub fn localize(language: Language, text: String) -> String {
    match language {
        Language::ZhTw => zh_tw(&text),
        _ => text,
    }
}

/// `zh` (Simplified) as Traditional Chinese, Taiwan usage.
pub fn zh_tw(zh: &str) -> String {
    let mut text = zh.to_string();
    for (from, to) in WORDS {
        if text.contains(from) {
            text = text.replace(from, to);
        }
    }
    let table = chars();
    text.chars()
        .map(|c| table.get(&c).copied().unwrap_or(c))
        .collect()
}

/// [`zh_tw`] interned: one conversion per distinct string for the process.
pub fn zh_tw_ref(zh: &'static str) -> &'static str {
    static INTERN: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let cache = INTERN.get_or_init(Default::default);
    if let Some(done) = cache.lock().unwrap().get(zh) {
        return done;
    }
    let converted: &'static str = Box::leak(zh_tw(zh).into_boxed_str());
    cache.lock().unwrap().insert(zh, converted);
    converted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taiwan_words_win_over_character_conversion() {
        assert_eq!(zh_tw("搜索会话"), "搜尋工作階段");
        assert_eq!(zh_tw("默认文件夹"), "預設資料夾");
        assert_eq!(zh_tw("设置"), "設定");
        assert_eq!(
            zh_tw("发消息…（/ 命令，$ 技能，@ 文件）"),
            "發訊息…（/ 命令，$ 技能，@ 檔案）"
        );
        // Plain characters still convert, and non-Chinese text is untouched.
        assert_eq!(zh_tw("后台 shell"), "後臺 shell");
        assert_eq!(zh_tw("Agent · now"), "Agent · now");
    }

    #[test]
    fn tr_ref_interns_once() {
        let a = tr_ref(Language::ZhTw, "Settings", "设置");
        let b = tr_ref(Language::ZhTw, "Settings", "设置");
        assert_eq!(a, "設定");
        assert!(std::ptr::eq(a, b));
    }
}
