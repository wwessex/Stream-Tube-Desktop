use crate::ui::app::{Route, navigate_in_current_tab, restore_window};
use crate::ui::pages::settings::Section;
use gpui_kit::*;
use objc2::rc::Retained;
use objc2_app_kit::{NSView, NSWindow, NSWindowButton, NSWindowStyleMask};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::OsString;
use std::path::Path;

pub fn homebrew_prefix() -> &'static Path {
    Path::new(if cfg!(target_arch = "aarch64") { "/opt/homebrew" } else { "/usr/local" })
}

/// Finder launches do not inherit the shell's Homebrew paths.
pub fn path_with_homebrew() -> Option<OsString> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<_> = std::env::split_paths(&path).filter(|dir| !dir.as_os_str().is_empty()).collect();
    let bin = homebrew_prefix().join("bin");
    if paths.contains(&bin) || !bin.is_dir() {
        return None;
    }
    paths.push(bin);
    std::env::join_paths(paths).ok()
}

pub fn native_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
    // SAFETY: GPUI's live AppKit window handle points to its NSView on the main thread.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

pub fn configure_pip_window(window: &Window) {
    if let Some(window) = native_window(window) {
        // GPUI omits the resize style for titlebar-less macOS windows.
        window.setStyleMask(window.styleMask() | NSWindowStyleMask::Resizable);
        if let Some(zoom) = window.standardWindowButton(NSWindowButton::ZoomButton) {
            zoom.setHidden(true);
        }
        window.setHidesOnDeactivate(false);
    }
}

/// Whether "Swipe between pages" is set to two fingers (System Settings → Trackpad →
/// More Gestures), as browsers check before going back and forward on a sideways swipe.
pub fn swipe_between_pages() -> bool {
    unsafe { objc2::msg_send![objc2::class!(NSEvent), isSwipeTrackingFromScrollEventsEnabled] }
}

/// Whether scrolling is "natural" (the content follows the fingers), the default.
pub fn natural_scrolling() -> bool {
    use objc2::runtime::AnyObject;
    let key = objc2_foundation::NSString::from_str("com.apple.swipescrolldirection");
    unsafe {
        let defaults: Retained<AnyObject> = objc2::msg_send![objc2::class!(NSUserDefaults), standardUserDefaults];
        let set: Option<Retained<AnyObject>> = objc2::msg_send![&defaults, objectForKey: &*key];
        set.is_none() || objc2::msg_send![&defaults, boolForKey: &*key]
    }
}

actions!(streamtube, [About, Settings, Quit]);

pub fn init_menu(main_window: AnyWindowHandle, cx: &mut App) {
    cx.on_action(move |_: &About, cx| {
        open_settings(main_window, Route::Settings(Section::About), cx)
    });
    cx.on_action(move |_: &Settings, cx| open_settings(main_window, Route::settings(), cx));
    cx.on_action(move |_: &Quit, cx| {
        let _ = main_window.update(cx, |_, window, _| crate::save_window_bounds(window));
        crate::request_quit(cx);
    });
    cx.bind_keys([
        KeyBinding::new("cmd-,", Settings, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.set_menus([
        Menu::new("Stream Tube").items([
            MenuItem::action("About", About),
            MenuItem::separator(),
            MenuItem::action("Settings", Settings),
            MenuItem::separator(),
            MenuItem::action("Quit Stream Tube", Quit),
        ]),
        Menu::new("View").items([MenuItem::action("Reload Page", crate::ui::app::Reload)]),
        // AppKit fills the windows menu itself (Fill, Center, Move & Resize…). The tiling
        // shortcuts and window managers such as Swish tile through those items, with the
        // system's animation; without them the window only jumps to its new frame
        Menu::new("Window").items([]),
    ]);
}

fn open_settings(main_window: AnyWindowHandle, route: Route, cx: &mut App) {
    // A menu action can be dispatched while its originating window is being updated.
    cx.defer(move |cx| {
        let _ = main_window.update(cx, |_, window, cx| {
            restore_window(window);
            window.activate_window();
            navigate_in_current_tab(route, window, cx);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{About, Settings, init_menu};
    use gpui::{AsKeystroke, Empty, Keystroke, OwnedMenuItem, TestAppContext};

    #[gpui::test]
    fn stream_tube_menu_registers_about_and_settings(cx: &mut TestAppContext) {
        let main_window = cx.add_window(|_, _| Empty);
        cx.update(|cx| init_menu(main_window.into(), cx));

        let menus = cx.read(|cx| cx.get_menus().expect("application menus"));
        let menu = menus
            .iter()
            .find(|menu| menu.name == "Stream Tube")
            .expect("Stream Tube menu");
        assert!(menu.items.iter().any(|item| matches!(item,
            OwnedMenuItem::Action { name, action, disabled: false, .. }
            if name == "About" && action.as_any().is::<About>()
        )));
        assert!(menu.items.iter().any(|item| matches!(item,
            OwnedMenuItem::Action { name, action, disabled: false, .. }
            if name == "Settings" && action.as_any().is::<Settings>()
        )));

        main_window
            .update(cx, |_, window, _| {
                let bindings = window.bindings_for_action(&Settings);
                let [binding] = bindings.as_slice() else {
                    panic!("expected one Settings binding")
                };
                let [shortcut] = binding.keystrokes() else {
                    panic!("expected a single keystroke")
                };
                assert_eq!(shortcut.as_keystroke(), &Keystroke::parse("cmd-,").unwrap());
            })
            .unwrap();
    }
}
