use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Language {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
    #[serde(rename = "en", other)]
    English,
}

impl Language {
    pub fn resolve(self) -> Self {
        self.resolve_with_ui_language(windows_ui_language())
    }

    fn resolve_with_ui_language(self, id: u16) -> Self {
        match self {
            // Mainland China and Singapore use Simplified Chinese.
            Self::Auto if matches!(id, 0x0804 | 0x1004 | 0x0004 | 0x7804) => {
                Self::SimplifiedChinese
            }
            Self::SimplifiedChinese => Self::SimplifiedChinese,
            _ => Self::English,
        }
    }

    pub fn text(self, key: Text) -> &'static str {
        let (english, chinese) = match key {
            Text::NoDevicesFound => ("No Logitech devices found", "未找到罗技设备"),
            Text::SelectedDeviceOffline => ("Selected device not connected", "所选设备未连接"),
            Text::SelectDevice => ("Select Device", "选择设备"),
            Text::Refresh => ("Refresh now", "立即刷新"),
            Text::TextMode => ("Show percentage as text", "以文字显示百分比"),
            Text::PollInterval => ("Poll interval", "轮询间隔"),
            Text::Notifications => ("Enable low-battery notifications", "启用低电量通知"),
            Text::Threshold => ("Low battery alert at", "低电量提醒阈值"),
            Text::Cooldown => ("Reminder interval", "提醒间隔"),
            Text::Autostart => ("Start at login", "开机自动启动"),
            Text::OpenConfig => ("Open config file…", "打开配置文件…"),
            Text::Exit => ("Exit", "退出"),
            Text::NoDevices => ("No devices", "无设备"),
            Text::Percent => ("%", "%"),
            Text::Charging => ("% (charging)", "%（充电中）"),
            Text::Language => ("Language", "语言"),
            Text::Automatic => ("Automatic (Windows language)", "自动（Windows 语言）"),
            Text::CheckForUpdates => ("Check for updates automatically", "自动检查更新"),
            // Prefix for "{prefix}{version}", hence the trailing separator.
            Text::UpdateAvailable => ("Update available: ", "有可用更新："),
            Text::UpdateToastBody => ("Click to open the download page", "点击前往下载页面"),
            Text::LowBattery => (
                "Battery low, plug in charger soon",
                "电量低，请及时连接充电器",
            ),
            Text::Seconds15 => ("15 seconds", "15 秒"),
            Text::Seconds30 => ("30 seconds", "30 秒"),
            Text::Minute1 => ("1 minute", "1 分钟"),
            Text::Minutes2 => ("2 minutes", "2 分钟"),
            Text::Minutes3 => ("3 minutes", "3 分钟"),
            Text::Minutes5 => ("5 minutes", "5 分钟"),
            Text::Minutes15 => ("15 minutes", "15 分钟"),
            Text::Minutes30 => ("30 minutes", "30 分钟"),
            Text::Hour1 => ("1 hour", "1 小时"),
            Text::Hours2 => ("2 hours", "2 小时"),
            Text::Hours4 => ("4 hours", "4 小时"),
            Text::Hours8 => ("8 hours", "8 小时"),
        };
        if self == Self::SimplifiedChinese {
            chinese
        } else {
            english
        }
    }
}

#[derive(Clone, Copy)]
pub enum Text {
    NoDevicesFound,
    SelectedDeviceOffline,
    SelectDevice,
    Refresh,
    TextMode,
    PollInterval,
    Notifications,
    Threshold,
    Cooldown,
    Autostart,
    OpenConfig,
    Exit,
    NoDevices,
    Percent,
    Charging,
    Language,
    Automatic,
    CheckForUpdates,
    UpdateAvailable,
    UpdateToastBody,
    LowBattery,
    Seconds15,
    Seconds30,
    Minute1,
    Minutes2,
    Minutes3,
    Minutes5,
    Minutes15,
    Minutes30,
    Hour1,
    Hours2,
    Hours4,
    Hours8,
}

#[cfg(windows)]
fn windows_ui_language() -> u16 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // This Win32 call takes no pointers and has no preconditions.
    unsafe { GetUserDefaultUILanguage() }
}

#[cfg(not(windows))]
fn windows_ui_language() -> u16 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_detection_and_overrides() {
        let ids = [
            0x0804, 0x1004, 0x0004, 0x7804, 0, 0x0409, 0x0407, 0x0404, 0x0c04, 0x1404,
        ];
        let actual: Vec<_> = ids
            .into_iter()
            .map(|id| {
                (
                    Language::Auto.resolve_with_ui_language(id),
                    Language::English.resolve_with_ui_language(id),
                    Language::SimplifiedChinese.resolve_with_ui_language(id),
                )
            })
            .collect();
        use Language::{English as En, SimplifiedChinese as Zh};
        let expected = [
            (Zh, En, Zh),
            (Zh, En, Zh),
            (Zh, En, Zh),
            (Zh, En, Zh),
            (En, En, Zh),
            (En, En, Zh),
            (En, En, Zh),
            (En, En, Zh),
            (En, En, Zh),
            (En, En, Zh),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn visible_text_uses_selected_language() {
        let actual: Vec<_> = [Language::English, Language::SimplifiedChinese]
            .into_iter()
            .map(|language| {
                [
                    Text::SelectDevice,
                    Text::Charging,
                    Text::LowBattery,
                    Text::Minutes30,
                ]
                .map(|key| language.text(key))
            })
            .collect();
        assert_eq!(
            actual,
            [
                [
                    "Select Device",
                    "% (charging)",
                    "Battery low, plug in charger soon",
                    "30 minutes"
                ],
                [
                    "选择设备",
                    "%（充电中）",
                    "电量低，请及时连接充电器",
                    "30 分钟"
                ],
            ]
        );
    }
}
