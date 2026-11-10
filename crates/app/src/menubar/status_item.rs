//! macOS 原生部分：`NSStatusItem`（图标 + target-action）、弹窗外的全局点击监听、按 gpui 窗口拿 `NSWindow`。
//!
//! gpui 没有状态栏 API，这里直接用 objc2（与 gpui-pre-macos 同版本）。AppKit 回调都在主线程，
//! 但拿不到 gpui 的 `App`，所以只把事件塞进 channel，由前台任务处理。

use std::ptr::NonNull;

use block2::RcBlock;
use futures::channel::mpsc::UnboundedSender;
use gpui_kit::Window;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{AnyThread as _, DefinedClass as _, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSEvent, NSEventMask, NSImage, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSView,
    NSWindow,
};
use objc2_foundation::{NSData, NSSize, NSString, ns_string};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::ScreenRect;

/// 菜单栏图标：logo「三住户同屋檐」的像素剪影，22×16 pt，@2x 位图（44×32 px）。
/// 只用 alpha 通道，作为 template 图由系统按明暗着色。
const ICON_PNG: &[u8] = include_bytes!("../../assets/menubar/status-icon@2x.png");
const ICON_SIZE: NSSize = NSSize::new(22.0, 16.0);

/// AppKit 送回前台的事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// 点了菜单栏图标。
    Clicked,
    /// 弹窗开着时点了别的 app（全局监听只收得到别的进程的点击）。
    OutsideClick,
}

struct TargetIvars {
    sender: UnboundedSender<TrayEvent>,
}

define_class!(
    // SAFETY: `NSObject` 没有子类化要求，`StatusItemTarget` 不实现 `Drop`。
    #[unsafe(super(NSObject))]
    #[ivars = TargetIvars]
    struct StatusItemTarget;

    unsafe impl NSObjectProtocol for StatusItemTarget {}

    impl StatusItemTarget {
        /// 状态栏按钮的 action（主线程）。
        #[unsafe(method(statusItemClicked:))]
        fn clicked(&self, _sender: Option<&AnyObject>) {
            self.ivars().sender.unbounded_send(TrayEvent::Clicked).ok();
        }
    }
);

impl StatusItemTarget {
    fn new(sender: UnboundedSender<TrayEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(TargetIvars { sender });
        // SAFETY: `NSObject` 的 `init` 是它的指定初始化方法。
        unsafe { msg_send![super(this), init] }
    }
}

/// 状态项在屏幕上的位置。
pub struct Anchor {
    /// 状态项按钮所在窗口的 frame（Cocoa 全局坐标）。
    pub item: ScreenRect,
    /// 所在屏幕的 frame（Cocoa 全局坐标）。
    pub screen: ScreenRect,
    /// `NSScreenNumber`，即 CGDirectDisplayID，与 gpui macOS 的 `DisplayId` 同值。
    pub display_id: u32,
}

/// 菜单栏状态项。drop 时从状态栏移除。
pub struct StatusItem {
    item: Retained<NSStatusItem>,
    sender: UnboundedSender<TrayEvent>,
    _target: Retained<StatusItemTarget>,
    monitor: Option<Retained<AnyObject>>,
}

fn rect(frame: objc2_foundation::NSRect) -> ScreenRect {
    ScreenRect {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    }
}

impl StatusItem {
    /// 在系统状态栏加一个图标；不在主线程或拿不到按钮时返回 None。
    pub fn install(sender: UnboundedSender<TrayEvent>, tooltip: &str) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let button = item.button(mtm)?;
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICON_PNG))?;
        image.setSize(ICON_SIZE);
        image.setTemplate(true);
        button.setImage(Some(&image));
        button.setToolTip(Some(&NSString::from_str(tooltip)));
        let target = StatusItemTarget::new(sender.clone());
        // SAFETY: target 由本结构体持有，活得比按钮的弱引用久；selector 在上面的类里定义。
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(statusItemClicked:)));
        }
        Some(Self {
            item,
            sender,
            _target: target,
            monitor: None,
        })
    }

    /// 状态项与所在屏幕的位置；状态项被系统隐藏（菜单栏放不下）时返回 None。
    pub fn anchor(&self) -> Option<Anchor> {
        let mtm = MainThreadMarker::new()?;
        let window = self.item.button(mtm)?.window()?;
        let screen = window.screen()?;
        let number = screen
            .deviceDescription()
            .objectForKey(ns_string!("NSScreenNumber"))?;
        // SAFETY: `NSScreenNumber` 的值是 NSNumber。
        let display_id: u32 = unsafe { msg_send![&*number, unsignedIntValue] };
        Some(Anchor {
            item: rect(window.frame()),
            screen: rect(screen.frame()),
            display_id,
        })
    }

    /// 弹窗开着时让图标保持按下的高亮。
    pub fn set_highlighted(&self, highlighted: bool) {
        if let Some(button) = MainThreadMarker::new().and_then(|mtm| self.item.button(mtm)) {
            button.highlight(highlighted);
        }
    }

    /// 开 / 关「点了别的 app」的全局监听（只在弹窗开着时监听）。
    pub fn watch_outside_clicks(&mut self, watch: bool) {
        if !watch {
            if let Some(monitor) = self.monitor.take() {
                // SAFETY: monitor 是 addGlobalMonitor 返回的对象，只移除一次。
                unsafe { NSEvent::removeMonitor(&monitor) };
            }
            return;
        }
        if self.monitor.is_some() {
            return;
        }
        let sender = self.sender.clone();
        let block = RcBlock::new(move |_event: NonNull<NSEvent>| {
            sender.unbounded_send(TrayEvent::OutsideClick).ok();
        });
        self.monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
            &block,
        );
    }
}

impl Drop for StatusItem {
    fn drop(&mut self) {
        self.watch_outside_clicks(false);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

/// gpui 窗口对应的 `NSWindow`。
fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    // 用完全限定语法：`Window` 自己也有个返回 gpui 句柄的 `window_handle()`。
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    // SAFETY: gpui macOS 的句柄指向活着的 GPUIView（NSView 子类），调用方在主线程。
    let view = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

/// 隐藏窗口但不关闭（外壳实体、同步引擎都留着）。
pub fn order_out(window: &Window) {
    if let Some(native) = ns_window(window) {
        native.orderOut(None);
    }
}

/// 把隐藏的窗口重新摆到最前。
pub fn order_front(window: &Window) {
    if let Some(native) = ns_window(window) {
        native.makeKeyAndOrderFront(None);
    }
}
