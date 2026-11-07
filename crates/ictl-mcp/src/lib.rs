//! MCP stdio 服务。工具名、参数、返回文本与 iphone-control-toolkit 一致（docs/decisions.md D10），
//! 切换时 agent 侧不用改。本 crate 不直接碰设备，全部经守护进程控制接口（Unix socket）。
//!
//! 手写 `ServerHandler`（不用宏）：参数 schema 直接写 JSON，省掉 schemars；
//! 参数到请求的转换（[`to_request`]）是纯函数，可单测。

use std::path::PathBuf;
use std::time::Duration;

use ictl_protocol::{self as protocol, Request, Response};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

/// 服务说明（发给客户端的 instructions）：toolkit 的那段，加上自动解锁范围、多设备的说明
pub fn instructions() -> &'static str {
    ictl_l10n::tr!(
        "控制一台通过 USB 连接的真实 iPhone(或 iPad)。坐标单位是点(pt),与 iphone_screenshot 图像像素一致。\
操作前先用 iphone_screenshot 或 iphone_list_elements 看清屏幕;能按名字点就优先用 iphone_tap_element。\
游戏、地图等界面没有元素树,只能看截图按坐标操作;小图标、小字看不清时用 iphone_screenshot 的区域放大。\
配置了锁屏密码时,截图、读元素在内的各工具都会先自动解锁。每次操作后再截图确认结果。\
涉及付款、删除、发送消息、修改账号或隐私设置的操作,先向用户确认。\
省时间:操作类工具可带 then=elements 或 then=screenshot,等界面稳定后顺带返回操作后的界面,不用再单独调一次;\
几步确定的连续操作(按键、逐级点菜单、填表)用 iphone_batch 一次发完;等页面加载用 iphone_wait_for,不要反复截图。\
接着多台 iPhone/iPad 时同一时间只控制一台:用 iphone_list_devices 查看,iphone_select_device 切换。",
        "Controls a real iPhone (or iPad) connected over USB. Coordinates are in points (pt), the same as the pixels of an iphone_screenshot image. Before acting, look at the screen with iphone_screenshot or iphone_list_elements; prefer iphone_tap_element whenever the target has text. Games, maps and similar screens have no element tree: work from screenshots and coordinates, and zoom into small icons or text with an iphone_screenshot region. If a passcode is configured, the tools (including screenshots and reading elements) unlock the phone first. Check the result with a screenshot after each action. Ask the user before anything that pays, deletes, sends a message, or changes account or privacy settings. Save time: action tools accept then=elements or then=screenshot to return the screen once it has settled, without a separate call; send a few predictable steps in a row (keys, menu levels, forms) as one iphone_batch; wait for a page with iphone_wait_for instead of repeated screenshots. With several iPhones/iPads connected only one is controlled at a time: see them with iphone_list_devices, switch with iphone_select_device."
    )
}

fn num(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}

fn schema(props: Value, required: &[&str]) -> Map<String, Value> {
    let v = json!({"type": "object", "properties": props, "required": required});
    v.as_object().cloned().unwrap_or_default()
}

/// 操作后顺带返回界面时最多列多少个元素（每步都带太多会占满上下文）
pub const THEN_LIMIT: usize = 80;
/// 顺带返回界面前等界面稳定的上限
pub const THEN_SETTLE_MS: u64 = 1500;
/// 一次批量最多几步
pub const BATCH_MAX_STEPS: usize = 20;
/// reuse_elements 时按名字点击复用多旧的元素树（毫秒）
pub const BATCH_REUSE_MS: u64 = 10_000;
/// 批量里能用的工具：改变界面的操作和等待；看界面用 then
pub const BATCH_TOOLS: [&str; 11] = [
    "iphone_tap",
    "iphone_tap_element",
    "iphone_long_press",
    "iphone_swipe",
    "iphone_type_text",
    "iphone_press_button",
    "iphone_launch_app",
    "iphone_terminate_app",
    "iphone_open_url",
    "iphone_wait_for",
    "wait",
];

/// 操作类工具共有的可选参数：操作后顺带返回界面
fn then_props() -> Value {
    json!({
        "then": {"type": "string", "enum": ["none", "elements", "screenshot"], "default": "none",
                 "description": ictl_l10n::tr!("操作后等界面稳定,再顺带返回界面:elements=元素列表(最多 80 个),screenshot=截图。省一次单独的调用", "After the action, wait for the screen to settle and return it: elements = element list (up to 80), screenshot = screenshot. Saves a separate call")},
        "then_filter": {"type": "string", "default": "", "description": ictl_l10n::tr!("then=elements 时只列包含这段文字的元素", "With then=elements: only list elements containing this text")},
        "then_region": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4,
                        "description": ictl_l10n::tr!("then=screenshot 时只返回这块区域的放大图 [x, y, width, height](点),例如游戏棋盘", "With then=screenshot: only return this region, enlarged, as [x, y, width, height] (points), e.g. a game board")},
        "then_settle": {"type": "number", "default": 1.5,
                        "description": ictl_l10n::tr!("顺带返回界面前最多等几秒让界面稳定(不超过 10)。游戏、地图按画面是否静止判断,消除连锁等长动画可调大,画面一静止就返回", "Maximum seconds to wait for the screen to settle before returning it (up to 10). Games and maps are judged by whether the picture is still; raise it for long animations such as chain reactions, it returns as soon as the picture is still")}
    })
}

/// 在参数 schema 里加上 then / then_filter
fn with_then(props: Value) -> Value {
    let mut p = props;
    if let (Some(o), Value::Object(extra)) = (p.as_object_mut(), then_props()) {
        o.extend(extra);
    }
    p
}

/// 操作类工具描述的共同结尾：返回什么、then 能做什么、自动解锁。
/// `alert`：不带 then 时会不会查系统弹窗（见 [`wants_alert_check`]）
fn action_desc(body: &str, example: &str, alert: bool) -> String {
    let mut s = ictl_l10n::trf!(
        "{body}\n\n返回:一行结果文字,例如「{example}」。",
        "{body}\n\nReturns: one line of result text, e.g. “{example}”."
    );
    if alert {
        s += ictl_l10n::tr!(
            "不带 then 时,如果系统弹窗挡着屏幕,结果里会提醒(这时操作落在了弹窗上)。",
            " Without then, the result warns if a system alert covers the screen (the action then landed on the alert)."
        );
    }
    s += ictl_l10n::tr!(
        "传 then=elements 或 then=screenshot 会等界面稳定后附上操作后的元素列表或截图,省一次单独的调用。\
配置了锁屏密码时会先自动解锁。",
        " With then=elements or then=screenshot it waits for the screen to settle and attaches the element list or screenshot after the action, saving a separate call. If a passcode is configured, the phone is unlocked first."
    );
    s
}

/// 只看不改的工具
fn looking(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title).read_only(true).open_world(false)
}

/// 改变手机状态的工具。open_world：点击、输入可能经 App 触达外部（发消息、下单），所以都标 true
fn acting(title: &str, destructive: bool, idempotent: bool) -> ToolAnnotations {
    ToolAnnotations::with_title(title).read_only(false).destructive(destructive).idempotent(idempotent).open_world(true)
}

/// 17 个工具的定义（toolkit 的 13 个 + iphone_wait_for、iphone_batch、iphone_list_devices、iphone_select_device）。
/// 描述可以改写，但工具名、参数、默认返回不改（docs/decisions.md D10）
pub fn tools() -> Vec<Tool> {
    let t = |name: &'static str, desc: String, s: Map<String, Value>, a: ToolAnnotations| {
        Tool::new(name, desc, s).with_annotations(a)
    };
    let app = || {
        json!({"app": {"type": "string",
            "description": ictl_l10n::tr!("Bundle ID(如 com.apple.Preferences)或显示名(如「设置」「微信」)", "Bundle ID (e.g. com.apple.Preferences) or display name (e.g. “Settings”)")}})
    };
    vec![
        t(
            "iphone_status",
            ictl_l10n::tr!("查看手机和服务的状态:WDA(WebDriverAgent)是否就绪、是否锁屏、能否自动解锁、当前前台 App 的 Bundle ID、\
屏幕尺寸(点)、守护进程状态。\n\n何时用:开始操作前确认手机可用;操作报错、怀疑 WDA 掉线或手机锁屏时排查。\
WDA 不可用时会说明哪些功能受影响(读元素、按名字点击、启动 App、输入中文)。只读,不改变手机状态。", "Show the state of the phone and the service: whether WDA (WebDriverAgent) is ready, whether the phone is locked and can be auto-unlocked, the bundle ID of the foreground app, the screen size (points) and the daemon state.\n\nWhen to use: to check that the phone is usable before starting; to investigate when actions fail or WDA or the lock screen seem to be in the way. If WDA is unavailable it says which features are affected (reading elements, tapping by text, launching apps, typing non-ASCII text). Read-only, doesn't change the phone.")
                .into(),
            schema(json!({}), &[]),
            looking(ictl_l10n::tr!("查看状态", "Show status")),
        ),
        t(
            "iphone_screenshot",
            ictl_l10n::tr!("截取当前屏幕,返回 JPEG 图像和一行说明。图像已缩放到点尺寸,图中像素坐标可直接用于 iphone_tap、iphone_swipe。\n\n\
看不清小图标、小字时,传 x/y/width/height(点,四个要一起给)放大这块区域:从原始高清截图裁剪,小区域再额外放大。\
返回文字里有放大倍数,图中坐标换算回屏幕点:x + 像素/倍数。\n\n\
何时用:游戏、地图、图片等没有元素树的界面,或要确认画面效果时。只想知道有哪些按钮、文字,iphone_list_elements 更快更省上下文。\
操作后要看结果时,优先给操作工具带 then=screenshot。WDA 不可用时改用 CoreDevice 截图(iOS 27 起,说明里注明「HID」)。\
只读:不改变界面,但配置了锁屏密码时会先解锁。", "Capture the current screen and return a JPEG image plus a line of explanation. The image is scaled to points, so its pixel coordinates can be used directly with iphone_tap and iphone_swipe.\n\nTo read small icons or text, pass x/y/width/height (points, all four together) to zoom into that region: it is cropped from the full-resolution capture, and small regions are enlarged further. The text gives the zoom factor; convert image coordinates back to screen points with x + pixel / factor.\n\nWhen to use: screens without an element tree (games, maps, images), or to check how something looks. To find out which buttons and texts are there, iphone_list_elements is faster and uses less context. To see the result of an action, prefer then=screenshot on the action tool. If WDA is unavailable it uses a CoreDevice screenshot instead (iOS 27+, marked “HID”). Read-only: doesn't change the screen, but unlocks the phone first if a passcode is configured.")
                .into(),
            schema(
                json!({"x": num(ictl_l10n::tr!("区域左上角 x(点)", "left edge x of the region (points)")), "y": num(ictl_l10n::tr!("区域左上角 y(点)", "top edge y of the region (points)")), "width": num(ictl_l10n::tr!("区域宽(点)", "region width (points)")), "height": num(ictl_l10n::tr!("区域高(点)", "region height (points)"))}),
                &[],
            ),
            looking(ictl_l10n::tr!("截图", "Screenshot")),
        ),
        t(
            "iphone_list_elements",
            ictl_l10n::tr!("列出屏幕上带文字的界面元素,每行一个:类型、标签、名字、值、中心点坐标、尺寸(点)。\n\n\
何时用:找按钮、输入框、列表项的文字和位置,比截图快、省上下文;找到后用 iphone_tap_element 按文字点。\
游戏、地图等没有元素树的界面这里是空的,改用 iphone_screenshot。\n\n\
注意:按坐标判断是否在屏幕内,被弹窗遮住的元素也可能列出来,不确定时截图核对。需要 WDA。\
只读:不改变界面,但配置了锁屏密码时会先解锁。", "List the UI elements on screen that carry text, one per line: type, label, name, value, center point and size (points).\n\nWhen to use: to find buttons, text fields and list items with their text and position; faster than a screenshot and uses less context. Then tap one by its text with iphone_tap_element. Games, maps and other screens without an element tree return nothing here; use iphone_screenshot.\n\nNote: visibility is judged by position, so elements hidden behind an alert can still be listed; check with a screenshot when in doubt. Needs WDA. Read-only: doesn't change the screen, but unlocks the phone first if a passcode is configured.")
                .into(),
            schema(
                json!({
                    "filter": {"type": "string", "default": "",
                        "description": ictl_l10n::tr!("只返回类型/标签/名字/值里包含这段文字的元素(不区分大小写),例如 SearchField、Button、登录", "Only return elements whose type/label/name/value contains this text (case-insensitive), e.g. SearchField, Button, Login")},
                    "limit": {"type": "integer", "default": 150, "description": ictl_l10n::tr!("最多列几个,多出的只报数量", "Maximum number of elements to list; the rest are only counted")}
                }),
                &[],
            ),
            looking(ictl_l10n::tr!("列出界面元素", "List UI elements")),
        ),
        t(
            "iphone_tap",
            action_desc(
                ictl_l10n::tr!("点击屏幕上的一个坐标(单位是点,与 iphone_screenshot 全屏图的像素一致,左上角为原点)。\n\n\
何时用:目标没有文字(图标、图片)、在游戏或地图里,或 iphone_list_elements 里找不到它时。\
带文字的按钮、列表项优先用 iphone_tap_element,界面布局变了也点得准。按住用 iphone_long_press,滚动用 iphone_swipe。\n\n\
行为:单指按下约 0.05 秒后抬起,点一次。不检查坐标上是什么,点之前先截图确认位置。\
WDA 不可用时改用 CoreDevice HID(iOS 27 起;结果里注明「经 HID」)。", "Tap a point on the screen (in points, the same as the pixels of a full-screen iphone_screenshot, origin at the top left).\n\nWhen to use: the target has no text (icon, image), is in a game or map, or can't be found with iphone_list_elements. For buttons and list items with text prefer iphone_tap_element, which keeps working when the layout changes. Use iphone_long_press to hold and iphone_swipe to scroll.\n\nBehavior: one finger down for about 0.05 s, then up; a single tap. It doesn't check what is at that point, so look at a screenshot first. If WDA is unavailable it uses CoreDevice HID instead (iOS 27+; the result says “via HID”)."),
                ictl_l10n::tr!("已点击 (120,300)", "Tapped (120,300)"),
                true,
            ),
            schema(with_then(json!({"x": num(ictl_l10n::tr!("x(点)", "x (points)")), "y": num(ictl_l10n::tr!("y(点)", "y (points)"))})), &["x", "y"]),
            acting(ictl_l10n::tr!("点击坐标", "Tap point"), true, false),
        ),
        t(
            "iphone_tap_element",
            action_desc(
                ictl_l10n::tr!("按文字点击界面元素:先找标签/名字与 text 完全相同的,找不到再找包含 text 的。\
中心被悬浮栏、键盘等盖住时改点没被盖住的部分,整个被盖住就不点。\n\n\
何时用:按钮、列表项、标签页等带文字的元素,优先用它而不是按坐标点。不确定文字怎么写时先 iphone_list_elements。\
没有文字的图标、游戏、地图用 iphone_tap。\n\n\
行为:没找到、index 超出匹配个数或整个被盖住时不点,返回原因(不算出错;在 iphone_batch 里会让批量停下)。需要 WDA。", "Tap a UI element by its text: first an exact match of label or name with text, otherwise one that contains text. If its center is covered by a floating bar, the keyboard or similar, it taps an uncovered part; if it is completely covered, it doesn't tap.\n\nWhen to use: buttons, list items, tabs and other elements with text; prefer this over coordinates. If unsure how the text is written, use iphone_list_elements first. For icons without text, games and maps use iphone_tap.\n\nBehavior: when nothing matches, index is beyond the number of matches, or the element is completely covered, it doesn't tap and says why (not an error; inside iphone_batch it stops the batch). Needs WDA."),
                ictl_l10n::tr!("已点击 Button \"通用\"", "Tapped Button \"General\""),
                true,
            ),
            schema(
                with_then(json!({
                    "text": {"type": "string", "description": ictl_l10n::tr!("元素的文字(标签或名字),先精确匹配再按包含匹配", "the element's text (label or name); exact match first, then substring")},
                    "index": {"type": "integer", "default": 0,
                        "description": ictl_l10n::tr!("匹配到多个时点第几个(从 0 开始,顺序同 iphone_list_elements)", "which match to tap if there are several (from 0, same order as iphone_list_elements)")}
                })),
                &["text"],
            ),
            acting(ictl_l10n::tr!("按文字点击", "Tap by text"), true, false),
        ),
        t(
            "iphone_long_press",
            action_desc(
                ictl_l10n::tr!("在屏幕坐标(点)上单指按住 seconds 秒再抬起,手指不移动。\n\n\
何时用:要长按才出现的操作,例如主屏图标的快捷菜单、消息或图片的上下文菜单、选中文字。\
普通点击用 iphone_tap 或 iphone_tap_element;要移动手指(滚动、拖滑块)用 iphone_swipe。\n\n\
行为:只按一次。按得太短(不到约 0.5 秒)系统可能当成普通点击。长按弹出的菜单不会自己消失,\
下一步先看屏幕(或直接带 then=screenshot)。需要 WDA。", "Hold one finger still at a screen point (points) for seconds, then lift.\n\nWhen to use: actions that need a long press, such as home-screen quick actions, context menus on messages or images, or selecting text. For normal taps use iphone_tap or iphone_tap_element; to move the finger (scroll, drag a slider) use iphone_swipe.\n\nBehavior: a single press. Too short a press (under about 0.5 s) may count as a normal tap. Menus opened by a long press stay open, so look at the screen next (or pass then=screenshot). Needs WDA."),
                ictl_l10n::tr!("已长按 (200,400) 1 秒", "Long-pressed (200,400) for 1 s"),
                true,
            ),
            schema(
                with_then(json!({
                    "x": num(ictl_l10n::tr!("x(点)", "x (points)")),
                    "y": num(ictl_l10n::tr!("y(点)", "y (points)")),
                    "seconds": {"type": "number", "default": 1.0, "description": ictl_l10n::tr!("按住几秒,默认 1", "seconds to hold, default 1")}
                })),
                &["x", "y"],
            ),
            acting(ictl_l10n::tr!("长按", "Long press"), true, false),
        ),
        t(
            "iphone_swipe",
            action_desc(
                ictl_l10n::tr!("单指从 (from_x, from_y) 按下,用 seconds 秒移到 (to_x, to_y) 后抬起(坐标单位是点)。\n\n\
何时用:滚动列表或页面、左右翻页、拖动滑块。向上滑看下面的内容:from_y 大、to_y 小(例如 y 从 700 滑到 300);\
向左滑看右边的内容:from_x 大、to_x 小。点击用 iphone_tap,原地按住用 iphone_long_press。\n\n\
行为:seconds 越小滑得越快,快滑会带惯性多滚一段;要精确移动(滑块)把 seconds 调到 1 秒左右。\
滑完会等列表停止滚动(最多 2.5 秒)再返回,还在滚就在结果里说明。WDA 不可用时改用 CoreDevice HID(iOS 27 起)。", "One finger down at (from_x, from_y), moving to (to_x, to_y) over seconds, then up (coordinates in points).\n\nWhen to use: scroll a list or page, page left or right, drag a slider. To see content further down, swipe up: from_y large, to_y small (e.g. y from 700 to 300); to see content to the right, swipe left: from_x large, to_x small. Use iphone_tap to tap and iphone_long_press to hold in place.\n\nBehavior: the smaller seconds, the faster the swipe; a fast swipe keeps scrolling with momentum. For precise moves (sliders) use about 1 second. It waits until the list stops scrolling (up to 2.5 s) before returning and says so if it is still moving. If WDA is unavailable it uses CoreDevice HID instead (iOS 27+)."),
                ictl_l10n::tr!("已滑动 (215,700) → (215,300)", "Swiped (215,700) → (215,300)"),
                true,
            ),
            schema(
                with_then(json!({
                    "from_x": num(ictl_l10n::tr!("起点 x(点)", "start x (points)")),
                    "from_y": num(ictl_l10n::tr!("起点 y(点)", "start y (points)")),
                    "to_x": num(ictl_l10n::tr!("终点 x(点)", "end x (points)")),
                    "to_y": num(ictl_l10n::tr!("终点 y(点)", "end y (points)")),
                    "seconds": {"type": "number", "default": 0.3, "description": ictl_l10n::tr!("滑动用时(秒),默认 0.3;越小越快、惯性越大", "swipe duration (seconds), default 0.3; smaller is faster with more momentum")}
                })),
                &["from_x", "from_y", "to_x", "to_y"],
            ),
            acting(ictl_l10n::tr!("滑动", "Swipe"), false, false),
        ),
        t(
            "iphone_type_text",
            action_desc(
                ictl_l10n::tr!("在当前获得焦点的输入框里输入文字(支持中文),从光标处输入,不会清空原有内容。submit=true 时末尾按回车。\n\n\
何时用:搜索框、表单、聊天框等。要先点中输入框让键盘弹出;屏幕上没有键盘时报错,什么都不输。\
回车可能会发送消息或提交表单,发送前先向用户确认。\n\n\
行为:输完会读回输入框核对,少了结尾几个字会自动补上;对不上就报错,也不按回车。\
WDA 不可用时改用 HID(iOS 27 起),只能输入美式键盘上的英文和符号。", "Type text into the focused text field (any language, including Chinese) at the cursor, without clearing what is already there. With submit=true it presses Return at the end.\n\nWhen to use: search boxes, forms, chat fields. Tap the field first so the keyboard comes up; with no keyboard on screen it fails and types nothing. Return may send a message or submit a form, so ask the user before sending.\n\nBehavior: it reads the field back afterwards; dropped trailing characters are typed again. If it still doesn't match, it reports an error and doesn't press Return. If WDA is unavailable it uses HID instead (iOS 27+), which can only type English letters and symbols of a US keyboard."),
                ictl_l10n::tr!("已输入 4 个字符,并按了回车", "Typed 4 characters and pressed Return"),
                true,
            ),
            schema(
                with_then(json!({
                    "text": {"type": "string", "description": ictl_l10n::tr!("要输入的文字", "the text to type")},
                    "submit": {"type": "boolean", "default": false, "description": ictl_l10n::tr!("输完后按回车(搜索、发送、提交)", "press Return afterwards (search, send, submit)")}
                })),
                &["text"],
            ),
            acting(ictl_l10n::tr!("输入文字", "Type text"), false, false),
        ),
        t(
            "iphone_press_button",
            action_desc(
                ictl_l10n::tr!("按硬件键:home(回主屏幕)、volume_up、volume_down、lock(锁屏)。\n\n\
何时用:离开当前 App 回主屏幕用 home(App 留在后台,要彻底关掉用 iphone_terminate_app);调音量;锁屏。\
锁屏后配置了密码的话,下一个操作会先自动解锁。\n\n\
行为:系统弹窗挡着时按 home 不会生效,这时不按,并提示先点弹窗上的按钮。按 home 会等主屏真的到前台再返回。", "Press a hardware button: home (go to the home screen), volume_up, volume_down, lock (lock the screen).\n\nWhen to use: leave the current app with home (the app stays in the background; use iphone_terminate_app to quit it); change the volume; lock the phone. After locking, if a passcode is configured, the next action unlocks first.\n\nBehavior: while a system alert covers the screen home has no effect, so it isn't pressed and the result says to tap a button on the alert first. home waits until the home screen is really in front before returning."),
                ictl_l10n::tr!("已按 home", "Pressed home"),
                false,
            ),
            schema(
                with_then(json!({"button": {"type": "string", "enum": ["home", "volume_up", "volume_down", "lock"],
                    "description": ictl_l10n::tr!("home、volume_up、volume_down 或 lock", "home, volume_up, volume_down or lock")}})),
                &["button"],
            ),
            acting(ictl_l10n::tr!("按硬件键", "Press button"), false, false),
        ),
        t(
            "iphone_list_apps",
            ictl_l10n::tr!("列出已安装的 App,每行一个:显示名和 Bundle ID。\n\n\
何时用:iphone_launch_app、iphone_terminate_app 报「找不到名为…的 App」时,查准确的名字或 Bundle ID。\
默认只列用户装的 App;设置、相机等系统 App 要 include_system=true(要多花几秒)。结果有缓存。只读。", "List installed apps, one per line: display name and bundle ID.\n\nWhen to use: when iphone_launch_app or iphone_terminate_app report “No app named …”, to look up the exact name or bundle ID. By default only user-installed apps; system apps such as Settings or Camera need include_system=true (takes a few seconds longer). Results are cached. Read-only.")
                .into(),
            schema(
                json!({"include_system": {"type": "boolean", "default": false, "description": ictl_l10n::tr!("同时列出系统 App", "also list system apps")}}),
                &[],
            ),
            looking(ictl_l10n::tr!("列出 App", "List apps")),
        ),
        t(
            "iphone_launch_app",
            action_desc(
                ictl_l10n::tr!("启动 App 并切到前台。app 是显示名时按已安装的 App 查找,找不到会报错,提示用 iphone_list_apps 查看。\n\n\
何时用:打开某个 App。要直接进网页或 App 里的某个页面用 iphone_open_url。\
想从首页重新开始(而不是回到上次的页面),先用 iphone_terminate_app 结束它再启动。\n\n\
行为:App 已在后台运行时只切回前台,停在上次的页面;没在运行就冷启动。等 App 真的到前台才返回(最多约 8 秒);\
被系统弹窗(如权限请求)挡住或启动慢时,结果里会说明。需要 WDA。", "Launch an app and bring it to the foreground. A display name is looked up among the installed apps; if not found it fails and suggests iphone_list_apps.\n\nWhen to use: to open an app. To go straight to a web page or a screen inside an app, use iphone_open_url. To start from the app's first screen (instead of where it was), quit it with iphone_terminate_app first.\n\nBehavior: an app already running in the background is only brought back to the front, on the screen it was on; otherwise it cold-starts. It returns once the app is really in front (up to about 8 s); if a system alert (such as a permission prompt) blocks it or it starts slowly, the result says so. Needs WDA."),
                ictl_l10n::tr!("已启动 com.apple.Preferences", "Launched com.apple.Preferences"),
                true,
            ),
            schema(with_then(app()), &["app"]),
            acting(ictl_l10n::tr!("启动 App", "Launch app"), false, true),
        ),
        t(
            "iphone_terminate_app",
            action_desc(
                ictl_l10n::tr!("强制结束 App 进程,相当于在多任务界面里把它划掉。app 是显示名时按已安装的 App 查找,找不到会报错。\n\n\
何时用:App 卡住或状态不对,或者想让下次 iphone_launch_app 从首页冷启动。只想离开 App 用 iphone_press_button 按 home,App 留在后台。\n\n\
行为:App 里没保存的输入、正在播放的内容会丢失。App 本来没在运行也不报错。结束的是前台 App 时会回到主屏幕。需要 WDA。", "Force-quit an app, like swiping it away in the app switcher. A display name is looked up among the installed apps; if not found it fails.\n\nWhen to use: the app is stuck or in a wrong state, or the next iphone_launch_app should cold-start it on its first screen. To just leave the app, press home with iphone_press_button; the app stays in the background.\n\nBehavior: unsaved input and anything playing is lost. No error if the app wasn't running. Quitting the foreground app returns to the home screen. Needs WDA."),
                ictl_l10n::tr!("已结束 com.apple.Preferences", "Terminated com.apple.Preferences"),
                false,
            ),
            schema(with_then(app()), &["app"]),
            acting(ictl_l10n::tr!("结束 App", "Quit app"), true, true),
        ),
        t(
            "iphone_open_url",
            action_desc(
                ictl_l10n::tr!("用系统默认方式打开网址或 URL Scheme:http/https 网页在 Safari 打开,URL Scheme 打开对应的 App。\n\n\
何时用:直接进入某个网页或 App 里的某个页面,比启动 App 再一级级点快。只想打开 App 用 iphone_launch_app。\n\n\
行为:有的 URL Scheme 系统会先弹窗问要不要打开,带 then=screenshot 确认。需要 WDA。", "Open a web address or URL scheme the way the system does: http/https pages open in the default browser, URL schemes open their app.\n\nWhen to use: to go straight to a web page or a screen inside an app, faster than launching the app and tapping through. To just open an app use iphone_launch_app.\n\nBehavior: for some URL schemes the system first asks whether to open them; check with then=screenshot. Needs WDA."),
                ictl_l10n::tr!("已打开 https://example.com", "Opened https://example.com"),
                true,
            ),
            schema(
                with_then(json!({"url": {"type": "string", "description": ictl_l10n::tr!("完整网址(含 https://)或 URL Scheme", "full address (including https://) or URL scheme")}})),
                &["url"],
            ),
            acting(ictl_l10n::tr!("打开链接", "Open URL"), false, false),
        ),
        t(
            "iphone_wait_for",
            ictl_l10n::tr!("等包含某段文字的界面元素出现(例如页面加载完、搜索结果出来、弹窗出现),出现就立即返回找到的元素和用时;超时报错。\n\n\
何时用:操作后要等加载时代替反复截图轮询,每 0.3 秒查一次,不占上下文。输入框里的字不算(刚输入的搜索词不会被当成结果)。\
需要 WDA。只读:不改变界面,但配置了锁屏密码时会先解锁。", "Wait until a UI element containing some text appears (a page finished loading, search results arrived, an alert appeared); returns the element and the time taken as soon as it appears, or an error on timeout.\n\nWhen to use: after an action, to wait for loading instead of polling with screenshots; it checks every 0.3 s and uses no context meanwhile. Text inside input fields doesn't count (a search term just typed isn't taken for a result). Needs WDA. Read-only: doesn't change the screen, but unlocks the phone first if a passcode is configured.")
                .into(),
            schema(
                json!({
                    "text": {"type": "string", "description": ictl_l10n::tr!("要等的文字,元素的标签/名字/值包含它就算出现", "the text to wait for; it counts as appeared when an element's label/name/value contains it")},
                    "timeout": {"type": "number", "default": 5, "description": ictl_l10n::tr!("最多等几秒(不超过 30)", "maximum seconds to wait (up to 30)")}
                }),
                &["text"],
            ),
            looking(ictl_l10n::tr!("等文字出现", "Wait for text")),
        ),
        t(
            "iphone_list_devices",
            ictl_l10n::tr!("列出 USB 上接着的全部 iPhone/iPad,每行一台:设备名、型号、iOS 版本、UDID,标出正在控制哪台、有没有设自动解锁密码。\n\n\
何时用:接着多台设备、要确认操作落在哪台,或切换前查设备名和 UDID。同一时间只控制一台,其他工具都作用在正在控制的那台上。只读。", "List all iPhones/iPads connected over USB, one per line: device name, model, iOS version, UDID; marks the one being controlled and whether it has an auto-unlock passcode.\n\nWhen to use: with several devices connected, to check which one actions go to, or to look up names and UDIDs before switching. Only one device is controlled at a time; all other tools act on it. Read-only.")
                .into(),
            schema(json!({}), &[]),
            looking(ictl_l10n::tr!("列出设备", "List devices")),
        ),
        t(
            "iphone_select_device",
            ictl_l10n::tr!("改为控制另一台设备,之后所有工具都作用在它上面,守护进程重启后还记得。device 是 UDID 或设备名(设备名包含这段文字且只有一台也行),\
auto 表示取消选定、改回自动选(正在控制的那台断开才换)。\n\n\
何时用:接着多台设备、要操作的不是正在控制的那台。先用 iphone_list_devices 看有哪几台。\n\n\
行为:切换后要在新设备上拉起 WDA(10~60 秒),期间读界面、按名字点击等用不了,先用 iphone_status 确认就绪;\
这台从没装过 WDA 时起不来,要请用户运行 iphone-control setup。\
自动解锁按设备分开存密码,新设备没设就不会自动解锁。设了 IPHONE_UDID 时不能切换。\n\n\
返回:一行结果文字,例如「已切换到 Derrors 的 iPad(iPad11,1 iOS 26.6.1,00008020-…)」。", "Control another device from now on: all tools act on it, and the daemon remembers it across restarts. device is a UDID or device name (part of the name works if only one device matches); auto clears the selection and goes back to automatic (switches only when the controlled device disconnects).\n\nWhen to use: with several devices connected, when the one to operate isn't the one being controlled. Look at them with iphone_list_devices first.\n\nBehavior: after switching, WDA has to start on the new device (10-60 s); until then reading the screen, tapping by text etc. don't work, so check readiness with iphone_status. If WDA was never installed on that device it won't start; ask the user to run iphone-control setup. Auto-unlock passcodes are stored per device, so a new device without one isn't auto-unlocked. Not possible while IPHONE_UDID is set.\n\nReturns: one line of result text, e.g. “Switched to Derrors's iPad (iPad11,1 iOS 26.6.1, 00008020-…)”.")
                .into(),
            schema(
                json!({"device": {"type": "string", "description": ictl_l10n::tr!("UDID、设备名,或 auto(改回自动选)", "UDID, device name, or auto (back to automatic)")}}),
                &["device"],
            ),
            ToolAnnotations::with_title(ictl_l10n::tr!("切换设备", "Switch device")).read_only(false).destructive(false).idempotent(true).open_world(false),
        ),
        t(
            "iphone_batch",
            action_desc(
                ictl_l10n::tr!("按顺序执行几步操作,一次调用完成(最多 20 步),某一步出错就停下,说明停在第几步、后面几步没执行。\n\n\
steps 每项是 {\"tool\": 工具名, 其余是该工具的参数},可用:iphone_tap、iphone_tap_element、iphone_long_press、\
iphone_swipe、iphone_type_text、iphone_press_button、iphone_launch_app、iphone_terminate_app、iphone_open_url、\
iphone_wait_for,以及 {\"tool\": \"wait\", \"seconds\": 秒数}(不超过 5)。iphone_tap_element 没点到也会停下。\
执行前先检查全部参数,有错一步都不执行。\n\n\
何时用:结果确定的连续操作(按键、逐级点菜单、填表),每步省一轮往返。每步的结果不确定(点了以后不知道会跳到哪)时,还是一步步来。\
then 在全部做完(或停下)后返回一次界面。", "Run several steps in order in one call (up to 20); stops at the first error and says which step it stopped at and how many didn't run.\n\nEach item of steps is {\"tool\": tool name, plus that tool's parameters}; allowed: iphone_tap, iphone_tap_element, iphone_long_press, iphone_swipe, iphone_type_text, iphone_press_button, iphone_launch_app, iphone_terminate_app, iphone_open_url, iphone_wait_for, and {\"tool\": \"wait\", \"seconds\": n} (up to 5). An iphone_tap_element that taps nothing also stops it. All parameters are checked first; if any is wrong, no step runs.\n\nWhen to use: sequences with predictable results (keys, menu levels, forms), saving a round trip per step. When the outcome of a step is uncertain (you don't know where a tap leads), go step by step. then returns the screen once, after the last step (or where it stopped)."),
                ictl_l10n::tr!("1. iphone_launch_app → 已启动 com.apple.Preferences(每步一行)", "1. iphone_launch_app → Launched com.apple.Preferences (one line per step)"),
                true,
            ),
            schema(
                with_then(json!({
                    "steps": {"type": "array", "items": {"type": "object"},
                        "description": ictl_l10n::tr!("要执行的步骤,1 到 20 个,例如 [{\"tool\": \"iphone_tap_element\", \"text\": \"通用\"}]", "the steps to run, 1 to 20, e.g. [{\"tool\": \"iphone_tap_element\", \"text\": \"General\"}]")},
                    "reuse_elements": {"type": "boolean", "default": false,
                        "description": ictl_l10n::tr!("这批操作不改变界面布局时(例如在同一个键盘上连按)设为 true:按名字点击复用最近读到的元素树,每步省 0.3~0.8 秒。会跳转页面的不要用,否则可能点到旧位置", "Set to true when the steps don't change the layout (e.g. pressing keys on the same keypad): tapping by text reuses the most recent element tree, saving 0.3-0.8 s per step. Don't use it for steps that navigate, or it may tap where things used to be")}
                })),
                &["steps"],
            ),
            acting(ictl_l10n::tr!("批量操作", "Batch"), true, false),
        ),
    ]
}

struct Args<'a>(&'a Map<String, Value>);

/// 数字，或写成字符串的数字（toolkit 的 pydantic 也接受 "3"）
fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()).filter(|x: &f64| x.is_finite())
}

/// 参数给了但类型不对就报错，不再悄悄用默认值（例如 index: -1 原来会点第 0 个）
impl Args<'_> {
    fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }
    fn f(&self, k: &str) -> Result<f64, String> {
        self.opt_f(k)?.ok_or_else(|| ictl_l10n::trf!("缺少数字参数 {k}", "missing number parameter {k}"))
    }
    fn f_or(&self, k: &str, d: f64) -> Result<f64, String> {
        Ok(self.opt_f(k)?.unwrap_or(d))
    }
    fn opt_f(&self, k: &str) -> Result<Option<f64>, String> {
        self.get(k)
            .map(|v| number(v).ok_or_else(|| ictl_l10n::trf!("参数 {k} 要是数字", "parameter {k} must be a number")))
            .transpose()
    }
    fn s(&self, k: &str) -> Result<String, String> {
        self.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ictl_l10n::trf!("缺少文字参数 {k}", "missing text parameter {k}"))
    }
    fn s_or(&self, k: &str, d: &str) -> Result<String, String> {
        match self.get(k) {
            None => Ok(d.to_owned()),
            Some(v) => v
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ictl_l10n::trf!("参数 {k} 要是文字", "parameter {k} must be text")),
        }
    }
    fn u_or(&self, k: &str, d: usize) -> Result<usize, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => number(v).filter(|x| *x >= 0.0 && x.fract() == 0.0).map(|x| x as usize).ok_or_else(|| {
                ictl_l10n::trf!("参数 {k} 要是非负整数", "parameter {k} must be a non-negative integer")
            }),
        }
    }
    fn b_or(&self, k: &str, d: bool) -> Result<bool, String> {
        match self.get(k) {
            None => Ok(d),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(false),
            Some(_) => Err(ictl_l10n::trf!("参数 {k} 要是 true 或 false", "parameter {k} must be true or false")),
        }
    }
}

/// 工具调用 → 守护进程请求
pub fn to_request(name: &str, args: &Map<String, Value>) -> Result<Request, String> {
    let a = Args(args);
    Ok(match name {
        "iphone_status" => Request::Status,
        "iphone_screenshot" => {
            let r = [a.opt_f("x")?, a.opt_f("y")?, a.opt_f("width")?, a.opt_f("height")?];
            let region = match r {
                [None, None, None, None] => None,
                [Some(x), Some(y), Some(w), Some(h)] => Some((x, y, w, h)),
                _ => {
                    return Err(ictl_l10n::tr!(
                        "放大区域要同时给出 x、y、width、height",
                        "a zoom region needs all of x, y, width and height"
                    )
                    .into());
                }
            };
            Request::Screenshot { region }
        }
        "iphone_list_elements" => {
            Request::ListElements { filter: a.s_or("filter", "")?, limit: a.u_or("limit", 150)?, max_age_ms: 0 }
        }
        "iphone_tap" => Request::Tap { x: a.f("x")?, y: a.f("y")? },
        "iphone_tap_element" => Request::TapElement { text: a.s("text")?, index: a.u_or("index", 0)?, max_age_ms: 0 },
        "iphone_long_press" => Request::LongPress { x: a.f("x")?, y: a.f("y")?, seconds: a.f_or("seconds", 1.0)? },
        "iphone_swipe" => Request::Swipe {
            from: (a.f("from_x")?, a.f("from_y")?),
            to: (a.f("to_x")?, a.f("to_y")?),
            seconds: a.f_or("seconds", 0.3)?,
        },
        "iphone_type_text" => Request::TypeText { text: a.s("text")?, submit: a.b_or("submit", false)? },
        "iphone_press_button" => Request::PressButton { button: a.s("button")? },
        "iphone_list_apps" => Request::ListApps { include_system: a.b_or("include_system", false)? },
        "iphone_launch_app" => Request::LaunchApp { app: a.s("app")? },
        "iphone_terminate_app" => Request::TerminateApp { app: a.s("app")? },
        "iphone_open_url" => Request::OpenUrl { url: a.s("url")? },
        "iphone_list_devices" => Request::ListDevices,
        "iphone_select_device" => Request::SelectDevice { device: a.s("device")? },
        "iphone_wait_for" => {
            let secs = a.f_or("timeout", 5.0)?;
            if !(0.0..=30.0).contains(&secs) {
                return Err(
                    ictl_l10n::tr!("timeout 要在 0 到 30 秒之间", "timeout must be between 0 and 30 seconds").into()
                );
            }
            Request::WaitFor { text: a.s("text")?, timeout_ms: (secs * 1000.0) as u64 }
        }
        other => return Err(ictl_l10n::trf!("未知工具 {other}", "unknown tool {other}")),
    })
}

/// 操作后顺带返回什么
#[derive(Debug, Clone, PartialEq)]
pub enum Then {
    None,
    Elements(String),
    Screenshot(Option<(f64, f64, f64, f64)>),
}

/// then 带的选项：返回什么 + 最多等多久让界面稳定
#[derive(Debug, Clone, PartialEq)]
pub struct ThenOpts {
    pub then: Then,
    pub settle_ms: u64,
}

pub fn parse_then(args: &Map<String, Value>) -> Result<ThenOpts, String> {
    let a = Args(args);
    let then = match a.s_or("then", "none")?.as_str() {
        "none" | "" => Then::None,
        "elements" => Then::Elements(a.s_or("then_filter", "")?),
        "screenshot" => {
            let region =
                match a.get("then_region") {
                    None => None,
                    Some(v) => {
                        let xs: Option<Vec<f64>> = v.as_array().map(|arr| arr.iter().filter_map(number).collect());
                        match xs.as_deref() {
                            Some([x, y, w, h]) if *w > 0.0 && *h > 0.0 => Some((*x, *y, *w, *h)),
                            _ => return Err(ictl_l10n::tr!(
                                "then_region 要是 [x, y, width, height] 四个数,宽高大于 0",
                                "then_region must be four numbers [x, y, width, height], with width and height above 0"
                            )
                            .into()),
                        }
                    }
                };
            Then::Screenshot(region)
        }
        other => {
            return Err(ictl_l10n::trf!(
                "then 只能是 none / elements / screenshot,不是 {other}",
                "then must be none / elements / screenshot, not {other}"
            ));
        }
    };
    let secs = a.f_or("then_settle", THEN_SETTLE_MS as f64 / 1000.0)?;
    if !(0.0..=10.0).contains(&secs) {
        return Err(
            ictl_l10n::tr!("then_settle 要在 0 到 10 秒之间", "then_settle must be between 0 and 10 seconds").into()
        );
    }
    Ok(ThenOpts { then, settle_ms: (secs * 1000.0) as u64 })
}

/// 批量里的一步
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Request { tool: String, req: Request },
    Wait(std::time::Duration),
}

/// 检查并转换批量步骤（一步都不执行就先把参数错误全挡掉）
pub fn batch_steps(args: &Map<String, Value>) -> Result<Vec<Step>, String> {
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or(ictl_l10n::tr!("缺少 steps(数组)", "missing steps (array)"))?;
    let reuse = Args(args).b_or("reuse_elements", false)?;
    if steps.is_empty() || steps.len() > BATCH_MAX_STEPS {
        return Err(ictl_l10n::trf!(
            "steps 要有 1 到 {BATCH_MAX_STEPS} 步",
            "steps must have 1 to {BATCH_MAX_STEPS} steps"
        ));
    }
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let n = i + 1;
            let obj = s.as_object().ok_or_else(|| ictl_l10n::trf!("第 {n} 步不是对象", "step {n} is not an object"))?;
            let tool = obj
                .get("tool")
                .and_then(Value::as_str)
                .ok_or_else(|| ictl_l10n::trf!("第 {n} 步缺少 tool", "step {n} has no tool"))?;
            if !BATCH_TOOLS.contains(&tool) {
                return Err(ictl_l10n::trf!(
                    "第 {n} 步:批量里不能用 {tool},可用:{}",
                    "step {n}: {tool} can't be used in a batch; allowed: {}",
                    BATCH_TOOLS.join(ictl_l10n::tr!("、", ", "))
                ));
            }
            if tool == "wait" {
                let secs = Args(obj).f("seconds").map_err(|e| ictl_l10n::trf!("第 {n} 步:{e}", "step {n}: {e}"))?;
                if !(0.0..=5.0).contains(&secs) {
                    return Err(ictl_l10n::trf!(
                        "第 {n} 步:wait 的 seconds 要在 0 到 5 之间",
                        "step {n}: wait's seconds must be between 0 and 5"
                    ));
                }
                return Ok(Step::Wait(std::time::Duration::from_secs_f64(secs)));
            }
            let req = match to_request(tool, obj)
                .map_err(|e| ictl_l10n::trf!("第 {n} 步({tool}):{e}", "step {n} ({tool}): {e}"))?
            {
                Request::TapElement { text, index, .. } if reuse => {
                    Request::TapElement { text, index, max_age_ms: BATCH_REUSE_MS }
                }
                other => other,
            };
            Ok(Step::Request { tool: tool.to_owned(), req })
        })
        .collect()
}

/// tap_element 的结果是不是「没点到」（没找到、index 超出匹配个数、或整个被别的东西盖住）
pub fn tap_missed(text: &str) -> bool {
    protocol::is_tap_missed(text)
}

/// 不带 then 的操作做完后要不要查系统弹窗（弹窗挡着时操作都落在弹窗上，WDA 照样报成功）。
/// 按键自己处理（Home 先查弹窗；锁屏后不用查），结束 App 后前台是主屏，不会冒出新弹窗
pub fn wants_alert_check(req: &Request) -> bool {
    // 切换设备后新设备的 WDA 还没起来，查不了弹窗
    req.is_device_action()
        && !matches!(req, Request::PressButton { .. } | Request::TerminateApp { .. } | Request::SelectDevice { .. })
}

/// 守护进程响应 → MCP 结果。错误走工具错误（is_error），文字原样给客户端
pub fn to_result(r: Response) -> CallToolResult {
    match r {
        Response::Text { text } => CallToolResult::success(vec![ContentBlock::text(text)]),
        Response::Image { jpeg_base64, caption } => {
            CallToolResult::success(vec![ContentBlock::image(jpeg_base64, "image/jpeg"), ContentBlock::text(caption)])
        }
        Response::Error { message } => CallToolResult::error(vec![ContentBlock::text(message)]),
    }
}

#[derive(Debug, Clone)]
pub struct McpServer {
    pub socket: PathBuf,
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // rmcp 收到取消通知只是置这个令牌，不会中止处理：自己停下。丢掉进行中的调用会断开到守护进程的连接，
        // 守护进程那边还在排队的请求随之放弃（protocol::call），iphone_batch 剩下的步骤也不再执行
        tokio::select! {
            r = self.call_tool_inner(request) => r,
            _ = context.ct.cancelled() => Ok(to_result(Response::error(ictl_l10n::tr!("调用已取消", "Call cancelled"))).into()),
        }
    }
}

impl McpServer {
    async fn call_tool_inner(&self, request: CallToolRequestParams) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let then = match parse_then(&args) {
            Ok(t) => t,
            Err(e) => return Ok(to_result(Response::error(e)).into()),
        };
        if request.name == "iphone_batch" {
            return Ok(self.batch(&args, then).await.into());
        }
        let req = match to_request(&request.name, &args) {
            Ok(r) => r,
            Err(e) => return Ok(to_result(Response::error(e)).into()),
        };
        // then 只对操作类工具有意义；看界面的工具本身就是返回界面
        let resp = self.call(&req).await;
        Ok(match resp {
            Response::Text { text } if req.is_device_action() => {
                self.finish(text, false, then, wants_alert_check(&req)).await
            }
            resp => to_result(resp),
        }
        .into())
    }

    async fn call(&self, req: &Request) -> Response {
        self.call_on(req, None).await
    }

    /// `expect_udid`：守护进程正在控制的不是这台就不执行（见 [`protocol::EXPECT_UDID`]）
    async fn call_on(&self, req: &Request, expect_udid: Option<&str>) -> Response {
        // 守护进程正忙（例如在自动解锁）时会排队；最多等 150 秒，免得 agent 一直挂着
        protocol::call_expecting(&self.socket, req, Some(Duration::from_secs(150)), expect_udid).await.unwrap_or_else(
            |e| {
                Response::error(ictl_l10n::trf!("连不上守护进程({}):{e};确认守护进程在运行(容器部署看 iphone-control 容器)", "Can't reach the daemon ({}): {e}; make sure the daemon is running (for a container deployment, check the iphone-control container)",
                    self.socket.display()
                ))
            },
        )
    }

    /// 操作完成后：按 then 等界面稳定，再附上元素列表或截图（等稳定时顺带提醒弹窗）。
    /// 不带 then 时，`alert_check` 为真就查一下有没有系统弹窗挡着
    async fn finish(&self, mut text: String, failed: bool, opts: ThenOpts, alert_check: bool) -> CallToolResult {
        let done = |blocks: Vec<ContentBlock>| {
            if failed { CallToolResult::error(blocks) } else { CallToolResult::success(blocks) }
        };
        let then = opts.then;
        if then == Then::None {
            // 结果里已经说了有弹窗（启动 App 被挡）就不再查
            if alert_check
                && !protocol::reports_alert(&text)
                && let Response::Text { text: t } = self.call(&Request::AlertCheck).await
            {
                text += &t;
            }
            return done(vec![ContentBlock::text(text)]);
        }
        match self.call(&Request::Settle { max_ms: opts.settle_ms }).await {
            Response::Text { text: t } => text += &format!("\n{t}"),
            Response::Error { message } => {
                text += &ictl_l10n::trf!(
                    "\n(等界面稳定失败:{message})",
                    "\n(waiting for the screen to settle failed: {message})"
                )
            }
            Response::Image { .. } => {}
        }
        match then {
            Then::Elements(filter) => {
                // Settle 刚读过元素树，直接用它
                match self.call(&Request::ListElements { filter, limit: THEN_LIMIT, max_age_ms: THEN_SETTLE_MS }).await
                {
                    Response::Text { text: t } => {
                        text +=
                            &ictl_l10n::trf!("\n--- 操作后的界面 ---\n{t}", "\n--- Screen after the action ---\n{t}")
                    }
                    Response::Error { message } => {
                        text += &ictl_l10n::trf!("\n(读界面失败:{message})", "\n(reading the screen failed: {message})")
                    }
                    Response::Image { .. } => {}
                }
                done(vec![ContentBlock::text(text)])
            }
            Then::Screenshot(region) => match self.call(&Request::Screenshot { region }).await {
                Response::Image { jpeg_base64, caption } => done(vec![
                    ContentBlock::text(text),
                    ContentBlock::image(jpeg_base64, "image/jpeg"),
                    ContentBlock::text(caption),
                ]),
                Response::Error { message } => done(vec![ContentBlock::text(ictl_l10n::trf!(
                    "{text}\n(截图失败:{message})",
                    "{text}\n(screenshot failed: {message})"
                ))]),
                Response::Text { text: t } => done(vec![ContentBlock::text(format!("{text}\n{t}"))]),
            },
            Then::None => unreachable!("上面已返回"),
        }
    }

    /// 守护进程正在控制的设备的 UDID；读不到（没接设备、连不上）返回 None
    async fn controlled_udid(&self) -> Option<String> {
        let Response::Text { text } = self.call(&Request::Daemon).await else { return None };
        let v: Value = serde_json::from_str(&text).ok()?;
        v["device"]["udid"].as_str().filter(|u| !u.is_empty()).map(str::to_owned)
    }

    /// 按顺序执行；某一步出错就停，说明是第几步、后面几步没执行。出错时也按 then 附上当前界面，方便看停在哪
    async fn batch(&self, args: &Map<String, Value>, then: ThenOpts) -> CallToolResult {
        let steps = match batch_steps(args) {
            Ok(s) => s,
            Err(e) => return to_result(Response::error(e)),
        };
        let total = steps.len();
        // 每一步都要求还是开始时控制的那台：自动选设备时拔掉它会改控另一台，剩下的步骤不能落到那台上
        let udid = self.controlled_udid().await;
        let mut lines = Vec::new();
        // 最后一步是不是需要查弹窗的操作
        let mut check = false;
        for (i, step) in steps.into_iter().enumerate() {
            let n = i + 1;
            match step {
                Step::Wait(d) => {
                    tokio::time::sleep(d).await;
                    lines.push(ictl_l10n::trf!("{n}. wait → 等了 {} 秒", "{n}. wait → waited {} s", d.as_secs_f64()));
                }
                Step::Request { tool, req } => match self.call_on(&req, udid.as_deref()).await {
                    // 按名字没点到（toolkit 兼容：单独调用时是普通结果）在批量里要停下：后面的步骤都建立在它点中的前提上
                    Response::Text { text } if tool == "iphone_tap_element" && tap_missed(&text) => {
                        lines.push(format!("{n}. {tool} → {text}"));
                        if n < total {
                            lines.push(ictl_l10n::trf!(
                                "第 {n} 步没点到,后面 {} 步没有执行",
                                "Step {n} tapped nothing, the remaining {} step(s) didn't run",
                                total - n
                            ));
                        }
                        return self.finish(lines.join("\n"), true, then, false).await;
                    }
                    Response::Text { text } => {
                        lines.push(format!("{n}. {tool} → {text}"));
                        check = wants_alert_check(&req);
                    }
                    Response::Image { caption, .. } => lines.push(format!("{n}. {tool} → {caption}")),
                    Response::Error { message } => {
                        lines.push(ictl_l10n::trf!("{n}. {tool} → 出错:{message}", "{n}. {tool} → error: {message}"));
                        if n < total {
                            lines.push(ictl_l10n::trf!(
                                "第 {n} 步出错,后面 {} 步没有执行",
                                "Step {n} failed, the remaining {} step(s) didn't run",
                                total - n
                            ));
                        }
                        return self.finish(lines.join("\n"), true, then, true).await;
                    }
                },
            }
        }
        self.finish(lines.join("\n"), false, then, check).await
    }
}

/// 在 stdin/stdout 上运行 MCP 服务直到客户端断开
pub async fn serve_stdio(socket: PathBuf) -> anyhow::Result<()> {
    let running = McpServer { socket }.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| matches!(c, '\u{3000}'..='\u{303f}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'))
    }

    /// With IPHONE_CONTROL_LANG=en nothing the agent reads up front is Chinese: instructions, tool
    /// descriptions, titles and parameter docs (only examples of Chinese input may stay)
    #[test]
    fn english_tool_definitions_have_no_chinese() {
        use ictl_l10n::{Lang, with_lang};
        with_lang(Lang::En, || {
            assert!(!has_cjk(instructions()), "{}", instructions());
            for t in tools() {
                let json = serde_json::to_string(&t).unwrap();
                assert!(!has_cjk(&json), "{}: {json}", t.name);
            }
        });
        with_lang(Lang::Zh, || assert!(has_cjk(instructions())));
    }

    /// Daemon and MCP server may run with different languages: missed taps and alerts are recognised in both
    #[test]
    fn missed_taps_and_alerts_are_recognised_in_both_languages() {
        assert!(protocol::is_tap_missed("没找到包含 \"x\" 的元素"));
        assert!(protocol::is_tap_missed("Not found: no element contains \"x\""));
        assert!(protocol::is_tap_missed("Covered: Button \"OK\" is completely covered"));
        assert!(!protocol::is_tap_missed("Tapped Button \"Not found\""));
        assert!(protocol::reports_alert("Launched x, but a system alert is on screen: “Allow?”"));
        assert!(protocol::reports_alert("已启动 x,但屏幕上有系统弹窗「允许?」挡着"));
        assert!(!protocol::reports_alert("Launched x"));
    }

    #[test]
    fn alert_check_after_actions_but_not_buttons_or_reads() {
        assert!(wants_alert_check(&Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: 0.3 }));
        assert!(wants_alert_check(&Request::LaunchApp { app: "微信".into() }));
        assert!(!wants_alert_check(&Request::PressButton { button: "home".into() }));
        assert!(!wants_alert_check(&Request::TerminateApp { app: "微信".into() }));
        assert!(!wants_alert_check(&Request::Status));
    }

    #[test]
    fn toolkit_tools_plus_wait_for_batch_and_devices() {
        let names: Vec<_> = tools().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names.len(), 17);
        for n in [
            "iphone_status",
            "iphone_screenshot",
            "iphone_tap_element",
            "iphone_open_url",
            "iphone_list_devices",
            "iphone_select_device",
        ] {
            assert!(names.contains(&n.to_owned()), "{n}");
        }
        for t in tools().into_iter().filter(|t| t.name != "iphone_batch") {
            let no_required = t.input_schema.get("required").unwrap().as_array().unwrap().is_empty();
            assert_eq!(to_request(&t.name, &Map::new()).is_ok(), no_required, "{}", t.name);
        }
    }

    /// 注解与工具实际行为一致：看界面的只读，改界面的不是；每个参数都有说明
    #[test]
    fn annotations_and_param_docs() {
        for t in tools() {
            let a = t.annotations.as_ref().unwrap_or_else(|| panic!("{} 没有注解", t.name));
            assert!(a.title.is_some(), "{}", t.name);
            let probe = || {
                let props = t.input_schema["properties"].as_object().unwrap();
                let mut m = Map::new();
                for k in t.input_schema["required"].as_array().unwrap() {
                    let k = k.as_str().unwrap();
                    let v = match props[k]["type"].as_str() {
                        Some("number") => json!(1),
                        Some("array") => json!([{"tool": "wait", "seconds": 0}]),
                        _ if k == "button" => json!("home"),
                        _ => json!("x"),
                    };
                    m.insert(k.to_owned(), v);
                }
                m
            };
            let read_only = match to_request(&t.name, &probe()) {
                Ok(r) => !r.is_device_action(),
                Err(_) => false, // iphone_batch
            };
            assert_eq!(a.read_only_hint, Some(read_only), "{}", t.name);
            for (k, v) in t.input_schema["properties"].as_object().unwrap() {
                let d = v["description"].as_str().unwrap_or_default();
                assert!(!d.is_empty(), "{}.{k} 缺少说明", t.name);
            }
        }
    }

    #[test]
    fn defaults_match_toolkit() {
        assert_eq!(
            to_request("iphone_list_elements", &Map::new()).unwrap(),
            Request::ListElements { filter: String::new(), limit: 150, max_age_ms: 0 }
        );
        assert_eq!(
            to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}))).unwrap(),
            Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3 }
        );
        assert_eq!(
            to_request("iphone_long_press", &args(json!({"x": 1, "y": 2}))).unwrap(),
            Request::LongPress { x: 1.0, y: 2.0, seconds: 1.0 }
        );
        assert_eq!(
            to_request("iphone_type_text", &args(json!({"text": "设置"}))).unwrap(),
            Request::TypeText { text: "设置".into(), submit: false }
        );
    }

    #[test]
    fn then_is_offered_on_actions_only() {
        let has_then = |name: &str| {
            let t = tools().into_iter().find(|t| t.name == name).unwrap();
            t.input_schema["properties"].get("then").is_some()
        };
        for n in [
            "iphone_tap",
            "iphone_tap_element",
            "iphone_swipe",
            "iphone_type_text",
            "iphone_launch_app",
            "iphone_batch",
        ] {
            assert!(has_then(n), "{n}");
        }
        for n in ["iphone_status", "iphone_screenshot", "iphone_list_elements", "iphone_wait_for"] {
            assert!(!has_then(n), "{n}");
        }
        let then = |v: Value| parse_then(&args(v)).map(|o| o.then);
        assert_eq!(parse_then(&Map::new()).unwrap(), ThenOpts { then: Then::None, settle_ms: THEN_SETTLE_MS });
        assert_eq!(then(json!({"then": "elements", "then_filter": "设置"})).unwrap(), Then::Elements("设置".into()));
        assert_eq!(then(json!({"then": "screenshot"})).unwrap(), Then::Screenshot(None));
        assert_eq!(
            then(json!({"then": "screenshot", "then_region": [0, 295, 430, 460]})).unwrap(),
            Then::Screenshot(Some((0.0, 295.0, 430.0, 460.0)))
        );
        assert!(then(json!({"then": "screenshot", "then_region": [0, 1, 2]})).is_err());
        assert!(then(json!({"then": "screenshot", "then_region": [0, 1, 0, 5]})).is_err());
        assert!(then(json!({"then": "video"})).is_err());
        assert_eq!(parse_then(&args(json!({"then": "screenshot", "then_settle": 5}))).unwrap().settle_ms, 5000);
        assert!(parse_then(&args(json!({"then_settle": 30}))).is_err());
    }

    #[test]
    fn batch_steps_are_checked_before_anything_runs() {
        let steps = batch_steps(&args(json!({"steps": [
            {"tool": "iphone_tap_element", "text": "One"},
            {"tool": "wait", "seconds": 0.5},
            {"tool": "iphone_tap", "x": 1, "y": 2},
            {"tool": "iphone_wait_for", "text": "完成", "timeout": 3}
        ]})))
        .unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[1], Step::Wait(std::time::Duration::from_millis(500)));
        assert_eq!(
            steps[3],
            Step::Request {
                tool: "iphone_wait_for".into(),
                req: Request::WaitFor { text: "完成".into(), timeout_ms: 3000 }
            }
        );
        // 第 2 步参数错：一步都不执行，报出是第几步
        let e = batch_steps(&args(
            json!({"steps": [{"tool": "iphone_tap", "x": 1, "y": 2}, {"tool": "iphone_tap", "x": 1}]}),
        ))
        .unwrap_err();
        assert!(e.contains("第 2 步"), "{e}");
        // 看界面的工具、未知工具、批量套批量都不行
        for bad in ["iphone_screenshot", "iphone_batch", "rm"] {
            assert!(batch_steps(&args(json!({"steps": [{"tool": bad}]}))).is_err(), "{bad}");
        }
        assert!(batch_steps(&args(json!({"steps": [{"tool": "wait", "seconds": 9}]}))).is_err());
        assert!(batch_steps(&args(json!({"steps": []}))).is_err());
        let many: Vec<Value> = (0..21).map(|_| json!({"tool": "wait", "seconds": 0})).collect();
        assert!(batch_steps(&args(json!({"steps": many}))).is_err());
    }

    #[test]
    fn reuse_elements_only_affects_tap_element_steps() {
        let steps = |reuse: bool| {
            batch_steps(&args(json!({"reuse_elements": reuse, "steps": [
                {"tool": "iphone_tap_element", "text": "One"},
                {"tool": "iphone_tap", "x": 1, "y": 2}
            ]})))
            .unwrap()
        };
        assert_eq!(
            steps(true)[0],
            Step::Request {
                tool: "iphone_tap_element".into(),
                req: Request::TapElement { text: "One".into(), index: 0, max_age_ms: BATCH_REUSE_MS }
            }
        );
        assert!(matches!(&steps(false)[0], Step::Request { req: Request::TapElement { max_age_ms: 0, .. }, .. }));
        assert_eq!(steps(true)[1], steps(false)[1]);
    }

    #[test]
    fn missed_tap_texts_are_recognised() {
        assert!(tap_missed("没找到包含 \"x\" 的元素,先用 iphone_list_elements 或截图看看屏幕"));
        assert!(tap_missed("只匹配到 1 个:\nButton \"x\""));
        assert!(!tap_missed("已点击 Button \"x\""));
        assert!(tap_missed("被挡住:Button \"x\" 整个被「浮层」盖住,没有点"));
        assert!(!tap_missed("已点击 Button \"x\";它的中心被「搜索」盖住,改点没被盖住的 (215,848)"));
    }

    #[test]
    fn wait_for_timeout_is_bounded() {
        assert_eq!(
            to_request("iphone_wait_for", &args(json!({"text": "x"}))).unwrap(),
            Request::WaitFor { text: "x".into(), timeout_ms: 5000 }
        );
        assert!(to_request("iphone_wait_for", &args(json!({"text": "x", "timeout": 60}))).is_err());
    }

    #[test]
    fn wrong_typed_parameters_are_errors_not_defaults() {
        let args = |v: Value| v.as_object().unwrap().clone();
        assert!(to_request("iphone_tap_element", &args(json!({"text": "设置", "index": -1}))).is_err());
        assert!(to_request("iphone_list_elements", &args(json!({"limit": 1.5}))).is_err());
        assert!(to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": "x", "to_y": 4}))).is_err());
        // 写成字符串的数字照收（toolkit 的 pydantic 也收）
        assert_eq!(
            to_request("iphone_tap_element", &args(json!({"text": "设置", "index": "2"}))).unwrap(),
            Request::TapElement { text: "设置".into(), index: 2, max_age_ms: 0 }
        );
        assert_eq!(
            to_request("iphone_list_elements", &args(json!({"limit": null}))).unwrap(),
            Request::ListElements { filter: String::new(), limit: 150, max_age_ms: 0 }
        );
    }

    #[test]
    fn screenshot_region_needs_all_four() {
        assert_eq!(to_request("iphone_screenshot", &Map::new()).unwrap(), Request::Screenshot { region: None });
        assert_eq!(
            to_request("iphone_screenshot", &args(json!({"x": 0, "y": 55, "width": 430, "height": 40}))).unwrap(),
            Request::Screenshot { region: Some((0.0, 55.0, 430.0, 40.0)) }
        );
        assert_eq!(
            to_request("iphone_screenshot", &args(json!({"x": 10, "y": 10}))).unwrap_err(),
            "放大区域要同时给出 x、y、width、height"
        );
    }

    /// 回归用例 #17：预期内的错误要作为工具错误、带可读文字返回
    #[test]
    fn errors_become_tool_errors_with_text() {
        let r = to_result(Response::error("button 只能是 home / volume_up / volume_down / lock"));
        assert_eq!(r.is_error, Some(true));
        assert!(matches!(&r.content[0], ContentBlock::Text(t) if t.text.contains("button 只能是")));
        let img = to_result(Response::Image { jpeg_base64: "AAAA".into(), caption: "430x932 点".into() });
        assert_eq!(img.content.len(), 2);
        assert!(to_request("iphone_nope", &Map::new()).is_err());
    }
}
