//! Home's wording, in the app's two languages (the same `Language` switch as
//! `ui/strings.rs`; kept beside the page so its sentences with numbers stay
//! next to the logic that fills them).

use super::facts::{DayPart, LocalTime, Stats};
use crate::config::Language;

#[derive(Debug, Clone, Copy)]
pub struct Words {
    pub language: Language,
}

macro_rules! copy {
    ($($name:ident => $en:literal, $zh:literal;)*) => {
        impl Words {
            $(pub fn $name(&self) -> &'static str {
                match self.language {
                    Language::En => $en,
                    Language::ZhCn => $zh,
                }
            })*
        }
    };
}

copy! {
    new_card => "New session", "新建会话";
    agents => "Agents", "Agents";
    allow => "Allow", "允许";
    deny => "Deny", "拒绝";
    welcome => "Welcome to Muqun Desktop", "欢迎使用 Muqun Desktop";
    first_session => "Create your first session", "创建第一个会话";
    first_session_hint => "Pick an agent and a folder; Enter starts it with the defaults.", "选一个 agent 和文件夹；回车按默认值直接开始。";
    attention_empty => "Background sessions that need you appear here.", "后台会话需要你时会出现在这里。";
    launch_empty => "Agents you can start appear here once one is installed and enabled — see Agents below.", "装好并启用一个 agent 后，它会出现在这里 —— 状态见下方 Agents。";
    agents_checking => "Checking the agents on this computer…", "正在检查这台电脑上的 agent…";
    today_usage => "Today", "今天";
    choose_folder => "Choose folder…", "选择文件夹…";
    more_agents => "More agents", "更多 agent";
}

impl Words {
    pub fn greeting(&self, part: DayPart) -> &'static str {
        match (self.language, part) {
            (Language::En, DayPart::Morning) => "Good morning",
            (Language::En, DayPart::Afternoon) => "Good afternoon",
            (Language::En, DayPart::Evening) => "Good evening",
            (Language::En, DayPart::Night) => "Working late",
            (Language::ZhCn, DayPart::Morning) => "早上好",
            (Language::ZhCn, DayPart::Afternoon) => "下午好",
            (Language::ZhCn, DayPart::Evening) => "晚上好",
            (Language::ZhCn, DayPart::Night) => "夜深了",
        }
    }

    /// "Monday, November 2" / "11 月 2 日 · 周一".
    pub fn date(&self, t: LocalTime) -> String {
        const EN_DAYS: [&str; 7] = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ];
        const EN_MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        const ZH_DAYS: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
        let weekday = t.weekday as usize % 7;
        match self.language {
            Language::En => format!(
                "{}, {} {}",
                EN_DAYS[weekday],
                EN_MONTHS[(t.month as usize + 11) % 12],
                t.day
            ),
            Language::ZhCn => format!("{} 月 {} 日 · 周{}", t.month, t.day, ZH_DAYS[weekday]),
        }
    }

    /// "3 agents working · 2 waiting for you · 7 sessions today", leaving
    /// out the parts that are zero; `None` when every part is.
    pub fn stats(&self, stats: Stats) -> Option<String> {
        let mut parts = Vec::new();
        let en = self.language == Language::En;
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                one.to_string()
            } else {
                format!("{n} {many}")
            }
        };
        if stats.working > 0 {
            parts.push(if en {
                plural(stats.working, "1 agent working", "agents working")
            } else {
                format!("{} 个 agent 在工作", stats.working)
            });
        }
        if stats.waiting > 0 {
            parts.push(if en {
                format!("{} waiting for you", stats.waiting)
            } else {
                format!("{} 个在等你", stats.waiting)
            });
        }
        if stats.today > 0 {
            parts.push(if en {
                plural(stats.today, "1 session today", "sessions today")
            } else {
                format!("今天 {} 个会话", stats.today)
            });
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// The first-run subtitle: "Found 7 agents · 3 need setup".
    pub fn detected(&self, found: usize, need_setup: usize) -> String {
        match self.language {
            Language::En => {
                let mut s = if found == 1 {
                    "Found 1 agent on this computer".to_string()
                } else {
                    format!("Found {found} agents on this computer")
                };
                if need_setup > 0 {
                    s.push_str(&format!(" · {need_setup} need setup"));
                }
                s
            }
            Language::ZhCn => {
                let mut s = format!("检测到 {found} 个 agent");
                if need_setup > 0 {
                    s.push_str(&format!("，{need_setup} 个需要设置"));
                }
                s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: Words = Words {
        language: Language::En,
    };
    const ZH: Words = Words {
        language: Language::ZhCn,
    };

    #[test]
    fn dates_read_naturally_in_both_languages() {
        let t = LocalTime::at(1_793_613_600_000, 8 * 3600);
        assert_eq!(EN.date(t), "Monday, November 2");
        assert_eq!(ZH.date(t), "11 月 2 日 · 周一");
    }

    #[test]
    fn the_stats_line_leaves_out_zeros() {
        let stats = Stats {
            working: 3,
            waiting: 0,
            today: 1,
        };
        assert_eq!(
            EN.stats(stats).unwrap(),
            "3 agents working · 1 session today"
        );
        assert_eq!(
            ZH.stats(stats).unwrap(),
            "3 个 agent 在工作 · 今天 1 个会话"
        );
        assert_eq!(EN.stats(Stats::default()), None);
    }

    #[test]
    fn first_run_reports_what_was_found() {
        assert_eq!(
            EN.detected(7, 3),
            "Found 7 agents on this computer · 3 need setup"
        );
        assert_eq!(ZH.detected(1, 0), "检测到 1 个 agent");
    }
}
