//! 锁屏自动解锁（默认关闭，需显式配置密码文件）。
//!
//! 规则来自 toolkit 实测，之后按 iOS 26/27 的 iPhone、iPad 真机补充（以下是现行规则）：
//! 1. 先读 `/wda/locked`，没锁直接返回；WDA 不可用也直接返回（交给后面的调用去报错）
//! 2. 每轮固定：按 Home → 等 1.0s → 底部上滑 → 等 1.5s。不以「树里有键盘」判断：键盘收起时按键元素也常驻树中。
//!    有 Touch ID 的机型上滑后是「触控 ID」提示，先点「使用密码」再等 1.5s 调出键盘。
//!    数字键的位置排不成电话键盘（坐标换算错了）就当没有键盘，一位都不按
//! 3. 读密码栏「共N位数，已输入M位」：总位数和配置不符一位都不输；有残留输入先用右下角删除键逐位删到 0
//!    （iOS 27 上 Home 不会清空残留），删不掉就下一轮
//! 4. 每位一个独立 W3C 手势（按下 80ms），按完核对已输入位数正好 +1，不符立即停——确定没凑满整串的不算输错；
//!    多记一下凑满、密码栏归零或消失，按「可能已提交」去确认。多位合成一个手势会漏键；`/wda/tap` 在锁屏上每次约 2.9 秒，太慢
//! 5. 解锁期间 waitForIdleTimeout=1、animationCoolOffTimeout=0.2（0 会唤醒失败），结束后恢复
//! 6. 第一位没生效可以重来（最多 3 轮）。从第 2 位起先写停用标记，确认解开或确认没凑满才撤掉：整串输完仍锁着、
//!    中途出错、进程被杀都保持停用，直到人工清除。确认结果时核对出错一律当仍锁着
//! 7. 只支持纯数字；开机后首次解锁（BFU）永远需要人工

use std::path::PathBuf;
use std::time::Duration;

use ictl_wda::{IdleSettings, WdaClient, WdaError, elements, http::Transport, swipe_steps, tap_steps};

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("{}", ictl_l10n::tr!("手机已锁屏，且没有配置锁屏密码，需要人工解锁", "The phone is locked and no passcode is configured; unlock it by hand"))]
    NoPasscode,
    #[error("{}", ictl_l10n::tr!("自动解锁只支持纯数字锁屏密码", "Auto-unlock only supports numeric passcodes"))]
    NotDigits,
    #[error("{}", ictl_l10n::tr!("手机已锁屏；自动解锁之前失败过已停用，请人工解锁、核对密码后运行 iphone-control unlock-reset", "The phone is locked; auto-unlock was disabled after an earlier failure. Unlock it by hand, check the passcode, then run iphone-control unlock-reset"))]
    Disabled,
    #[error(
        "{}", ictl_l10n::tr!("调不出锁屏密码键盘（一位都没输进去），需要人工解锁；iPhone 横放充电时会进入待机显示，那个界面调不出键盘，可在「设置 → 待机显示」里关掉", "Couldn't bring up the passcode keypad (not a single digit was entered); unlock it by hand. An iPhone charging on its side shows StandBy, where the keypad can't be opened; turn it off in Settings → StandBy")
    )]
    NoKeypad,
    /// 输入不完整，不算输错
    #[error("{}", ictl_l10n::trf!("第 {0} 位没按上，已停止（输入不完整不算输错），稍后再试", "Digit {0} didn't register, stopped (an incomplete entry is not a wrong attempt); try again later", .0))]
    Incomplete(u32),
    #[error("{}", ictl_l10n::trf!("手机要求 {expected} 位密码，配置的是 {configured} 位，一位都没输", "The phone asks for a {expected}-digit passcode but {configured} digits are configured; nothing was entered"))]
    LengthMismatch { expected: u32, configured: u32 },
    /// 整串输完仍锁着：已写停用标记
    #[error("{}", ictl_l10n::tr!("整串密码输完手机仍锁着，自动解锁已停用（防止输错次数累计）", "The phone is still locked after entering the whole passcode; auto-unlock is now disabled (so wrong attempts don't pile up)"))]
    WrongPasscode,
    /// 第 2 位之后出错，不知道密码是否已提交：停用标记留着
    #[error("{}", ictl_l10n::trf!("输入密码途中出错（{0}），无法确认是否已提交，自动解锁已停用（防止输错次数累计）", "Error while entering the passcode ({0}); can't tell whether it was submitted, so auto-unlock is now disabled (so wrong attempts don't pile up)", .0))]
    Uncertain(String),
    #[error(transparent)]
    Wda(#[from] WdaError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, UnlockError>;

/// 各步等待时长。默认值是实测可靠的；单测里设为 0
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub wake_settle: Duration,
    pub swipe_settle: Duration,
    pub key_press_ms: u64,
    pub key_confirm: Duration,
    pub unlock_confirm: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            wake_settle: Duration::from_millis(1000),
            swipe_settle: Duration::from_millis(1500),
            key_press_ms: 80,
            key_confirm: Duration::from_millis(1500),
            unlock_confirm: Duration::from_secs(5),
            poll: Duration::from_millis(300),
        }
    }
}

pub const FIRST_DIGIT_ROUNDS: u32 = 3;

pub struct UnlockConfig {
    pub passcode_file: PathBuf,
    pub disabled_flag: PathBuf,
    pub timing: Timing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 本来就没锁，或 WDA 不可用（不处理）
    NotLocked,
    Unlocked,
}

impl UnlockConfig {
    pub fn enabled(&self) -> bool {
        self.passcode_file.is_file()
    }

    pub fn disabled_by_failure(&self) -> bool {
        self.disabled_flag.exists()
    }
}

/// 未锁屏直接返回；否则按规则解锁
pub async fn ensure_unlocked<T: Transport>(wda: &mut WdaClient<T>, cfg: &UnlockConfig) -> Result<Outcome> {
    // 先只问 WDA：确定没锁就直接返回，日常操作不多发请求
    let reported = match wda.wda_reports_locked().await {
        Ok(v) => v,
        Err(WdaError::Unavailable(_)) => return Ok(Outcome::NotLocked),
        Err(e) => return Err(e.into()),
    };
    if !reported && wda.lock_state_trusted() {
        return Ok(Outcome::NotLocked);
    }
    let code = passcode(cfg);
    // WDA 说锁了且能自动解锁：不先核对（熄屏锁屏时第一个无障碍查询要等约 8 秒），直接进解锁流程——第一步按 Home
    // 唤醒屏幕；若其实是通知中心（WDA 误报），Home 会收起它，随后核对为未锁屏，一位都不输。
    // 其余情况先核对，不动等待设置：不能自动解锁时免得把通知中心报成「需要人工解锁」，WDA 说没锁时确认一次就可信了。
    // 核对也拿不准（锁屏样式的界面，WDA 说没锁）时：能自动解锁就进解锁流程，理由同上；不能就按没锁处理
    if !(reported && code.is_ok()) {
        match wda.check_lock(Some(reported), false).await {
            Ok(c) if !c.locked && !(c.uncertain && code.is_ok()) => return Ok(Outcome::NotLocked),
            Ok(_) => {}
            Err(WdaError::Unavailable(_)) => return Ok(Outcome::NotLocked),
            Err(e) => return Err(e.into()),
        }
    }
    let code = code?;
    // 解锁期间等待上限调小（不能是 0：唤醒会失败），做完恢复成平时的值。
    // 上次异常退出没恢复的话读到的是调小的值，baseline 会按 WDA 默认值算
    let restore = wda.baseline().await?;
    wda.set_idle_settings(IdleSettings::QUICK).await?;
    let r = unlock(wda, cfg, code.as_bytes()).await;
    if let Err(e) = wda.set_idle_settings(restore).await {
        tracing::warn!(
            "{}",
            ictl_l10n::trf!("恢复 WDA 等待设置失败：{e}", "Restoring the WDA wait settings failed: {e}")
        );
    }
    r
}

/// 可用的密码；不能自动解锁时给出原因（停用、没配、不是纯数字）
fn passcode(cfg: &UnlockConfig) -> Result<String> {
    if cfg.disabled_by_failure() {
        return Err(UnlockError::Disabled);
    }
    let code = std::fs::read_to_string(&cfg.passcode_file).map_err(|_| UnlockError::NoPasscode)?.trim().to_owned();
    if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UnlockError::NotDigits);
    }
    Ok(code)
}

async fn press<T: Transport>(wda: &mut WdaClient<T>, at: (i64, i64), t: &Timing) -> Result<()> {
    Ok(wda.actions(&tap_steps(at.0 as f64, at.1 as f64, t.key_press_ms)).await?)
}

/// 用删除键把 `entered` 位残留删到 0；找不到删除键或某次没删掉返回 false（这轮作废，一位没输）
async fn clear_residual<T: Transport>(
    wda: &mut WdaClient<T>,
    delete: Option<(i64, i64)>,
    entered: u32,
    t: &Timing,
) -> Result<bool> {
    let Some(del) = delete else { return Ok(false) };
    for left in (0..entered).rev() {
        if tap_key(wda, del, left as i64 + 1, t).await? != left as i64 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 按一下键（数字或删除），等密码栏已输入位数离开 `before`；返回最后读到的位数（读不到密码栏为 -1）。
/// 只认「变了」不认方向：删除键是变少，数字键是变多，按键被记成两下时会多变一位
async fn tap_key<T: Transport>(wda: &mut WdaClient<T>, at: (i64, i64), before: i64, t: &Timing) -> Result<i64> {
    press(wda, at, t).await?;
    let deadline = tokio::time::Instant::now() + t.key_confirm;
    loop {
        let got = wda.passcode_field().await?.map(|f| f.1 as i64).unwrap_or(-1);
        // 读不到密码栏可能只是动画中，等到超时再算
        if (got >= 0 && got != before) || tokio::time::Instant::now() >= deadline {
            return Ok(got);
        }
        tokio::time::sleep(t.poll.min(Duration::from_millis(150))).await;
    }
}

/// 第 2 位起输入的结果
enum Verdict {
    Unlocked,
    /// 确定没有凑满整串（第 n 位被吞，或多记了一下但没满）
    NotSubmitted(u32),
    /// 凑满整串后仍锁着
    Wrong,
}

fn write_flag(cfg: &UnlockConfig, why: &str) -> std::io::Result<()> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    std::fs::write(&cfg.disabled_flag, format!("{now} {why}\n"))
}

async fn unlock<T: Transport>(wda: &mut WdaClient<T>, cfg: &UnlockConfig, code: &[u8]) -> Result<Outcome> {
    let t = &cfg.timing;
    let digit = |i: usize| (code[i] - b'0') as usize;
    let len = code.len() as u32;

    let mut keys = None;
    for round in 0..FIRST_DIGIT_ROUNDS {
        // Home 唤醒（键盘已打开时 iOS 27 只是把键收起，半截输入还在，下面用删除键处理）
        // 熄屏锁屏刚唤醒时上滑偶尔卡十几秒后报 `point.x != INFINITY`（WDA 还拿不到界面尺寸）：一位没输，算这轮落空重来。
        // XCTest 的 home 和 IOHID 的 Home 轮流用：iPhone 12 mini（iOS 26.4.1）上 XCTest 的 home 点不亮熄灭的屏幕，
        // IOHID 的能；iOS 27 上一直用的是 XCTest 的，第一轮还是它。
        // 唤醒后才问方向（iPad 横竖屏的上滑位置、键位换算都靠它）：熄屏时的第一个无障碍查询要等约 8 秒
        let woke = async {
            if round % 2 == 0 {
                wda.press_button("home").await?;
            } else {
                wda.press_home_hid().await?;
            }
            tokio::time::sleep(t.wake_settle).await;
            let view = wda.view().await?;
            let (cx, h) = (view.screen.width / 2.0, view.screen.height);
            wda.actions(&swipe_steps((cx, h - 8.0), (cx, h * 0.45), 250)).await?;
            Ok::<_, WdaError>(view)
        }
        .await;
        let view = match woke {
            Ok(v) => v,
            Err(e @ WdaError::Remote { .. }) => {
                tracing::info!(
                    "{}",
                    ictl_l10n::trf!("唤醒/上滑失败，重来一轮：{e}", "Wake/swipe up failed, trying another round: {e}")
                );
                continue;
            }
            Err(other) => return Err(other.into()),
        };
        tokio::time::sleep(t.swipe_settle).await;
        let check = wda.check_lock(None, false).await?;
        // 按过 Home、上滑之后还是认不出的锁屏样式界面：不算解开，接着找键盘（找不到就重来一轮）
        if !check.locked && !check.uncertain {
            return Ok(Outcome::Unlocked);
        }
        // 核对时已取到元素树就直接用，密码栏也从树里读。那棵树是上滑后马上取的，键盘有时还没进树：
        // 缺键盘或密码栏时再取一次（真机上这样省掉一整轮，约 4.5 秒）
        let mut els = match &check.tree {
            Some(xml) => elements::parse(xml, view),
            None => wda.elements().await?,
        };
        if check.tree.is_some() && (elements::keypad(&els).is_none() || elements::passcode_field(&els).is_none()) {
            els = wda.elements().await?;
        }
        // Touch ID 机型：点「使用密码」调出键盘。不是数字键，点了不会算输错
        if elements::keypad(&els).is_none()
            && let Some(at) = elements::use_passcode_button(&els)
        {
            press(wda, at, t).await?;
            tokio::time::sleep(t.swipe_settle).await;
            els = wda.elements().await?;
        }
        let field = match elements::passcode_field(&els) {
            Some(f) => Some(f),
            None => wda.passcode_field().await?,
        };
        let (Some(pad), Some((total, entered))) = (elements::keypad(&els), field) else {
            continue;
        };
        if total != len {
            return Err(UnlockError::LengthMismatch { expected: total, configured: len });
        }
        // 有残留输入：iOS 27 上 Home 不会清空，用右下角「删除」逐位删掉，每删一位核对一次
        if entered != 0 && !clear_residual(wda, elements::delete_key(&els, &pad), entered, t).await? {
            continue;
        }
        // 第一位不可能凑满整串（至少 4 位），出什么情况都不算输错
        match tap_key(wda, pad[digit(0)], 0, t).await? {
            1 => {
                keys = Some(pad);
                break;
            }
            n if n > 1 => return Err(UnlockError::Incomplete(1)),
            _ => continue,
        }
    }
    let pad = keys.ok_or(UnlockError::NoKeypad)?;

    // 从第 2 位起，任何一下都可能凑满整串让 iOS 判定一次（按键被记成两下、最后一位）。先写停用标记，
    // 确认解开或确认没凑满才撤掉；中途出错、进程被杀都会留下标记，绝不会把同一个错误密码再输一遍
    write_flag(
        cfg,
        ictl_l10n::tr!(
            "正在输入密码，未确认结果（若长期存在：上次输入中途出错或进程退出）",
            "entering the passcode, result not confirmed (if this persists: the last attempt failed midway or the process exited)"
        ),
    )?;
    let verdict = enter_rest(wda, cfg, &pad, code).await;
    let clear = || {
        if let Err(e) = std::fs::remove_file(&cfg.disabled_flag) {
            tracing::warn!(
                "{}",
                ictl_l10n::trf!(
                    "撤掉停用标记失败，自动解锁保持停用：{e}",
                    "Removing the disabled flag failed, auto-unlock stays disabled: {e}"
                )
            );
        }
    };
    match verdict {
        Ok(Verdict::Unlocked) => {
            clear();
            Ok(Outcome::Unlocked)
        }
        Ok(Verdict::NotSubmitted(n)) => {
            clear();
            Err(UnlockError::Incomplete(n))
        }
        Ok(Verdict::Wrong) => {
            let _ = write_flag(
                cfg,
                ictl_l10n::tr!("整串密码输完仍是锁屏状态", "still locked after entering the whole passcode"),
            );
            Err(UnlockError::WrongPasscode)
        }
        Err(e) => {
            let why = ictl_l10n::trf!(
                "输入过程中出错，无法确认密码是否已提交：{e}",
                "Error while typing, can't tell whether the passcode was submitted: {e}"
            );
            let _ = write_flag(cfg, &why);
            Err(UnlockError::Uncertain(e.to_string()))
        }
    }
}

/// 输第 2 位到最后一位并确认结果。出错直接返回（调用方保留停用标记）
async fn enter_rest<T: Transport>(
    wda: &mut WdaClient<T>,
    cfg: &UnlockConfig,
    pad: &[(i64, i64); 10],
    code: &[u8],
) -> Result<Verdict> {
    let t = &cfg.timing;
    let len = code.len() as u32;
    let key = |i: usize| pad[(code[i] - b'0') as usize];
    let mut last_pressed = true;
    for n in 2..len {
        let got = tap_key(wda, key(n as usize - 1), n as i64 - 1, t).await?;
        if got == n as i64 {
            continue;
        }
        // 被吞（还是 n-1 位）或多记一下但没满：确定没提交
        if got == n as i64 - 1 || (got > n as i64 && got < len as i64) {
            return Ok(Verdict::NotSubmitted(n));
        }
        // 归零、密码栏消失等：可能已经凑满提交了，按提交处理去确认
        tracing::warn!(
            "{}",
            ictl_l10n::trf!(
                "第 {n} 位按后密码栏变成 {got} 位，可能已提交，确认结果",
                "After digit {n} the passcode field shows {got} digits; it may have been submitted, checking the result"
            )
        );
        last_pressed = false;
        break;
    }
    if last_pressed {
        press(wda, key(code.len() - 1), t).await?;
    }
    let deadline = tokio::time::Instant::now() + t.unlock_confirm;
    loop {
        tokio::time::sleep(t.poll).await;
        // 严格判断：核对出错一律当仍锁着继续等，宁可多停用一次也不把错误密码当成解开
        if let Ok(c) = wda.check_lock(None, true).await
            && !c.locked
        {
            return Ok(Verdict::Unlocked);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    // 仍锁着。最后一位被吞时密码栏停在 len-1 位，说明根本没提交，不算输错
    if last_pressed && matches!(wda.passcode_field().await, Ok(Some((_, e))) if e == len - 1) {
        return Ok(Verdict::NotSubmitted(len));
    }
    Ok(Verdict::Wrong)
}

#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use super::*;
    use fake::FakeLockScreen;

    fn cfg(passcode: Option<&str>) -> (UnlockConfig, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-unlock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(p) = passcode {
            std::fs::write(dir.join("passcode"), p).unwrap();
        }
        let timing = Timing {
            wake_settle: Duration::ZERO,
            swipe_settle: Duration::ZERO,
            key_press_ms: 80,
            key_confirm: Duration::from_millis(20),
            unlock_confirm: Duration::from_millis(20),
            poll: Duration::from_millis(1),
        };
        (UnlockConfig { passcode_file: dir.join("passcode"), disabled_flag: dir.join("UNLOCK-DISABLED"), timing }, dir)
    }

    #[tokio::test]
    async fn correct_passcode_unlocks_and_restores_idle_settings() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("123456\n"));
        let mut wda = WdaClient::new(&fake);
        assert_eq!(ensure_unlocked(&mut wda, &cfg).await.unwrap(), Outcome::Unlocked);
        let s = fake.state();
        assert!(!s.locked);
        assert_eq!(s.idle, (10.0, 2.0), "结束后要恢复原来的等待设置");
        assert!(s.idle_during_unlock.iter().all(|i| *i == (1.0, 0.2)));
        assert_eq!(s.presses_per_action_max, 1, "每位一个独立手势");
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn not_locked_does_nothing() {
        let fake = FakeLockScreen::new("123456");
        fake.unlock_now();
        let (cfg, dir) = cfg(None);
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::NotLocked);
        assert_eq!(fake.state().requests_to_actions, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn wrong_passcode_counts_once_and_disables() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("654321"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::WrongPasscode)));
        assert_eq!(fake.state().failed_attempts, 1);
        assert!(cfg.disabled_flag.exists());
        // 停用后不再尝试
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Disabled)));
        assert_eq!(fake.state().failed_attempts, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn length_mismatch_types_nothing() {
        let fake = FakeLockScreen::new("123456");
        let (cfg, dir) = cfg(Some("1234"));
        let r = ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await;
        assert!(matches!(r, Err(UnlockError::LengthMismatch { expected: 6, configured: 4 })), "{r:?}");
        assert_eq!(fake.state().digits_typed, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 回归用例 #1：键盘收起时按键仍在树里。第一次上滑没调出键盘（按键落空），不算输错，下一轮重来
    #[tokio::test]
    async fn keypad_in_tree_but_not_shown_retries_without_counting() {
        let fake = FakeLockScreen::new("123456");
        fake.swipes_ignored(1);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().failed_attempts, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 键盘上留着别人按的半截（可能是错的）：先删干净再输，不能接在后面凑成一次输错
    #[tokio::test]
    async fn residual_input_is_deleted_before_typing() {
        let fake = FakeLockScreen::new("123456");
        fake.leave_residual(&[9, 9]);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().failed_attempts, 0);
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn swipe_error_right_after_wake_retries_round() {
        let fake = FakeLockScreen::new("123456");
        fake.swipe_errors(1);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().failed_attempts, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// iPhone 12 mini（iOS 26.4.1）：熄屏锁屏时 XCTest 的 home 点不亮屏幕，第二轮改用 IOHID 的 Home
    #[tokio::test]
    async fn dark_screen_is_woken_by_hid_home_when_xctest_home_is_dead() {
        let fake = FakeLockScreen::new("123456");
        fake.asleep(true);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        let s = fake.state();
        assert_eq!((s.failed_attempts, s.hid_events), (0, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// XCTest 的 home 能点亮屏幕时（iOS 27）第一轮就解开，不发 IOHID
    #[tokio::test]
    async fn dark_screen_woken_by_xctest_home_needs_no_hid() {
        let fake = FakeLockScreen::new("123456");
        fake.asleep(false);
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        assert_eq!(fake.state().hid_events, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// iPad mini 5（iPadOS 26.6.1）横屏：上滑后是「触控 ID」提示，点「使用密码」调出键盘；树是竖屏坐标，换算后按对键
    #[tokio::test]
    async fn ipad_touch_id_landscape_taps_use_passcode_then_unlocks() {
        let fake = FakeLockScreen::ipad_touch_id("123456", "LANDSCAPE");
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        let s = fake.state();
        assert_eq!((s.failed_attempts, s.digits_typed, s.stray_taps), (0, 6, 0));
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// iPad mini 5 真机：WDA 在锁屏时启动（一直报没锁），熄屏、有通知的锁屏上又没有锁的标记，认不出是锁屏。
    /// 以前会因此认定 WDA 可信、永远不自动解锁；现在能自动解锁就进解锁流程
    #[tokio::test]
    async fn unmarked_ipad_lock_screen_with_stale_wda_is_unlocked() {
        let fake = FakeLockScreen::ipad_lock_screen_without_hint("123456", "LANDSCAPE");
        fake.stale_wda_lock();
        let (cfg, dir) = cfg(Some("123456"));
        let mut wda = WdaClient::new(&fake);
        assert_eq!(ensure_unlocked(&mut wda, &cfg).await.unwrap(), Outcome::Unlocked);
        let s = fake.state();
        assert_eq!((s.locked, s.failed_attempts, s.digits_typed), (false, 0, 6));
        // 解开后在主屏上确认过，之后 WDA 说没锁就直接信
        assert!(wda.lock_state_trusted());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 同上但没配密码：拿不准时按没锁处理，不报「需要人工解锁」（可能只是通知中心），也不认定 WDA 可信
    #[tokio::test]
    async fn unmarked_lock_screen_without_passcode_is_not_reported_locked() {
        let fake = FakeLockScreen::ipad_lock_screen_without_hint("123456", "LANDSCAPE");
        fake.stale_wda_lock();
        let (cfg, dir) = cfg(None);
        let mut wda = WdaClient::new(&fake);
        assert_eq!(ensure_unlocked(&mut wda, &cfg).await.unwrap(), Outcome::NotLocked);
        assert!(!wda.lock_state_trusted());
        assert_eq!(fake.state().digits_typed, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 另一个横屏方向（Home 键在左）：WDA 照样报 `LANDSCAPE`，按状态栏的位置换算，按对键
    #[tokio::test]
    async fn ipad_touch_id_landscape_home_left_unlocks() {
        let fake = FakeLockScreen::ipad_touch_id_home_left("123456", "LANDSCAPE");
        let (cfg, dir) = cfg(Some("123456"));
        assert_eq!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await.unwrap(), Outcome::Unlocked);
        let s = fake.state();
        assert_eq!((s.failed_attempts, s.digits_typed, s.stray_taps), (0, 6, 0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 方向报错（树是竖屏坐标却当成不用转）：键位排不成电话键盘，一位数字都不按
    #[tokio::test]
    async fn ipad_landscape_with_wrong_orientation_types_nothing() {
        let fake = FakeLockScreen::ipad_touch_id("123456", "PORTRAIT");
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::NoKeypad)));
        let s = fake.state();
        assert_eq!((s.failed_attempts, s.digits_typed), (0, 0));
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn keypad_never_shows_is_no_keypad_not_wrong() {
        let fake = FakeLockScreen::new("123456");
        fake.swipes_ignored(99);
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::NoKeypad)));
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 回归用例 #4：中间某位被吞（键盘没接住），立即停止，不提交、不算输错
    #[tokio::test]
    async fn dropped_middle_digit_stops_without_submitting() {
        let fake = FakeLockScreen::new("123456");
        fake.drop_press(3);
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::Incomplete(3))));
        assert_eq!(fake.state().failed_attempts, 0);
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 最后一位被吞：没提交，不算输错，也不停用
    #[tokio::test]
    async fn dropped_last_digit_is_not_a_wrong_attempt() {
        let fake = FakeLockScreen::new("123456");
        fake.drop_press(6);
        let (cfg, dir) = cfg(Some("123456"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg).await, Err(UnlockError::Incomplete(6))));
        assert_eq!(fake.state().failed_attempts, 0);
        assert!(!cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 某一下被记成两下、凑满了整串：iOS 判了一次，必须当作输错停用
    #[tokio::test]
    async fn double_press_that_fills_the_entry_disables() {
        let fake = FakeLockScreen::new("123456");
        fake.double_press(5);
        let (cfg, dir) = cfg(Some("123456"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::WrongPasscode)));
        assert_eq!(fake.state().failed_attempts, 1);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Disabled)));
        assert_eq!(fake.state().failed_attempts, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 第 2 位之后出错：不知道是否提交了，停用标记必须留着，下一次不能再输
    #[tokio::test]
    async fn error_after_second_digit_keeps_auto_unlock_disabled() {
        let fake = FakeLockScreen::new("123456");
        fake.broken_after_press(3);
        let (cfg, dir) = cfg(Some("123456"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Uncertain(_))));
        assert!(cfg.disabled_flag.exists());
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::Disabled)));
        assert_eq!(fake.state().failed_attempts, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// WDA 在锁屏时启动（/wda/locked 一直报 false），输错后取元素树又失败：
    /// 确认阶段不能按 WDA 的 false 当成解开了
    #[tokio::test]
    async fn stale_wda_lock_state_cannot_turn_a_wrong_entry_into_unlocked() {
        let fake = FakeLockScreen::new("123456");
        fake.stale_wda_lock();
        fake.broken_after_press(6);
        let (cfg, dir) = cfg(Some("654321"));
        let mut wda = WdaClient::new(&fake);
        assert!(matches!(ensure_unlocked(&mut wda, &cfg).await, Err(UnlockError::WrongPasscode)));
        assert_eq!(fake.state().failed_attempts, 1);
        assert!(cfg.disabled_flag.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 上次异常退出没恢复等待设置：恢复成 WDA 默认值，而不是把解锁用的值当成原值
    #[tokio::test]
    async fn stuck_unlock_idle_settings_are_restored_to_defaults() {
        // 解锁用的值，或轮询、滑动用的值（之前漏判了这个）
        for stuck in [IdleSettings::QUICK, IdleSettings::NO_COOL_OFF] {
            let fake = FakeLockScreen::new("123456");
            let (cfg, dir) = cfg(Some("123456"));
            let mut wda = WdaClient::new(&fake);
            wda.set_idle_settings(stuck).await.unwrap();
            assert_eq!(ensure_unlocked(&mut wda, &cfg).await.unwrap(), Outcome::Unlocked);
            assert_eq!(fake.state().idle, (10.0, 2.0), "{stuck:?}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn missing_or_non_digit_passcode() {
        let fake = FakeLockScreen::new("123456");
        let (cfg_none, d1) = cfg(None);
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg_none).await, Err(UnlockError::NoPasscode)));
        let (cfg_alpha, d2) = cfg(Some("abc123"));
        assert!(matches!(ensure_unlocked(&mut WdaClient::new(&fake), &cfg_alpha).await, Err(UnlockError::NotDigits)));
        std::fs::remove_dir_all(d1).unwrap();
        std::fs::remove_dir_all(d2).unwrap();
    }
}
