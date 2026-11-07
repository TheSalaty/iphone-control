//! Language of the text that people and agents see: tool descriptions and results, CLI output,
//! `status` / `doctor`, logs and notifications.
//!
//! Every message is written in place as a pair, `tr!("中文", "English")`, so both versions stay next to
//! each other and can be reviewed together; there is no separate catalog to keep in sync.
//! `trf!` is the `format!` counterpart (same placeholders in both literals).
//!
//! The language is read once from `IPHONE_CONTROL_LANG` (`en`/`english`/`en_US…` → English,
//! `zh`/`chinese`/`zh_CN…` → Chinese). Unset or unknown keeps Simplified Chinese, the language the
//! project started with, so existing setups don't change.
//!
//! The client decides: the MCP server and the CLI send their language with every request, and the daemon
//! answers that request in it ([`scope`]). Requests from older clients carry no language and get Chinese.
//! The daemon's own setting only applies to what it writes by itself (event log, notifications).

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// 0 = not decided yet, 1 = Chinese, 2 = English
static LANG: AtomicU8 = AtomicU8::new(0);

/// Environment variable that selects the language
pub const ENV: &str = "IPHONE_CONTROL_LANG";

/// Parse a language name; `None` for anything unrecognised
pub fn parse(value: &str) -> Option<Lang> {
    let v = value.trim().to_ascii_lowercase();
    let v = v.split(['_', '-', '.']).next().unwrap_or_default();
    match v {
        "en" | "english" => Some(Lang::En),
        "zh" | "chinese" | "cn" => Some(Lang::Zh),
        _ => None,
    }
}

thread_local! {
    /// Per-thread override for [`with_lang`]: 0 = none
    static OVERRIDE: Cell<u8> = const { Cell::new(0) };
}

fn decode(v: u8) -> Option<Lang> {
    match v {
        1 => Some(Lang::Zh),
        2 => Some(Lang::En),
        _ => None,
    }
}

fn encode(l: Lang) -> u8 {
    if l == Lang::En { 2 } else { 1 }
}

tokio::task_local! {
    /// Language of the request being handled (see [`scope`])
    static TASK_LANG: Lang;
}

/// Run `f` (one request) in language `l`: every text it produces, across `.await`s, uses `l`.
/// Work moved to another thread (`spawn_blocking`) has to take [`lang`] along and use [`with_lang`]
pub async fn scope<F: std::future::Future>(l: Lang, f: F) -> F::Output {
    TASK_LANG.scope(l, f).await
}

/// The current language: a [`with_lang`] override, else the request's ([`scope`]), else the process
/// setting (decided on first use)
pub fn lang() -> Lang {
    if let Some(l) = decode(OVERRIDE.with(Cell::get)) {
        return l;
    }
    if let Ok(l) = TASK_LANG.try_with(|l| *l) {
        return l;
    }
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::Zh,
        2 => Lang::En,
        _ => {
            let l = std::env::var(ENV).ok().and_then(|v| parse(&v)).unwrap_or(Lang::Zh);
            set(l);
            l
        }
    }
}

/// Override the language for this process
pub fn set(l: Lang) {
    LANG.store(encode(l), Ordering::Relaxed);
}

/// Run `f` with the language set to `l` on this thread only (tests run in parallel and must not
/// change each other's language)
pub fn with_lang<T>(l: Lang, f: impl FnOnce() -> T) -> T {
    let prev = OVERRIDE.with(|o| o.replace(encode(l)));
    let r = f();
    OVERRIDE.with(|o| o.set(prev));
    r
}

pub fn is_en() -> bool {
    lang() == Lang::En
}

impl Lang {
    /// Short name on the wire: `zh` / `en`
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
}

/// Pick the Chinese or English version of a fixed text: `tr!("设备未连接", "Device not connected")`
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(,)?) => {
        if $crate::is_en() { $en } else { $zh }
    };
}

/// `format!` in the current language: `trf!("第 {n} 步", "step {n}")`, extra arguments as in `format!`
#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::is_en() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_language_names() {
        assert_eq!(parse("en"), Some(Lang::En));
        assert_eq!(parse("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(parse(" English "), Some(Lang::En));
        assert_eq!(parse("zh_CN"), Some(Lang::Zh));
        assert_eq!(parse("zh-Hans"), Some(Lang::Zh));
        assert_eq!(parse("de"), None);
        assert_eq!(parse(""), None);
    }

    #[tokio::test]
    async fn scope_sets_the_language_across_awaits() {
        let inner = scope(Lang::En, async {
            tokio::task::yield_now().await;
            tr!("中", "en")
        })
        .await;
        assert_eq!(inner, "en");
        let inner = scope(Lang::Zh, async { with_lang(Lang::En, || tr!("中", "en")) }).await;
        assert_eq!(inner, "en", "with_lang wins over scope");
    }

    #[test]
    fn macros_follow_the_language() {
        let n = 3;
        with_lang(Lang::En, || {
            assert_eq!(tr!("设备未连接", "Device not connected"), "Device not connected");
            assert_eq!(trf!("第 {n} 步", "step {n}"), "step 3");
            assert_eq!(trf!("{} 秒", "{} s", 1.5), "1.5 s");
        });
        with_lang(Lang::Zh, || {
            assert_eq!(tr!("设备未连接", "Device not connected"), "设备未连接");
            assert_eq!(trf!("第 {n} 步", "step {n}"), "第 3 步");
        });
    }
}
