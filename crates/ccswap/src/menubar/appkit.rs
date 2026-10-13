//! Main thread only.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSControlStateValueOff,
    NSControlStateValueOn, NSMenu, NSMenuItem, NSTextField,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, ns_string};
#[allow(deprecated)]
use objc2_foundation::{NSUserNotification, NSUserNotificationCenter};

use super::menu::{Command, Entry};

pub const NOTIFICATION_TITLE: &str = "claude-swap";

/// Item tags index into `commands`.
pub fn build_menu(
    mtm: MainThreadMarker,
    entries: &[Entry],
    target: &AnyObject,
    commands: &mut Vec<Command>,
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    for entry in entries {
        let item = match entry {
            Entry::Separator => NSMenuItem::separatorItem(mtm),
            Entry::Item {
                title,
                checked,
                command,
            } => {
                let item = new_item(mtm, title);
                if let Some(command) = command {
                    unsafe {
                        item.setAction(Some(sel!(menuAction:)));
                        item.setTarget(Some(target));
                    }
                    item.setTag(commands.len() as isize);
                    commands.push(command.clone());
                }
                item.setState(if *checked {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
                item
            }
            Entry::Submenu { title, entries } => {
                let item = new_item(mtm, title);
                let submenu = build_menu(mtm, entries, target, commands);
                submenu.setTitle(&NSString::from_str(title));
                item.setSubmenu(Some(&submenu));
                item
            }
        };
        menu.addItem(&item);
    }
    menu
}

fn new_item(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            ns_string!(""),
        )
    }
}

/// An accessory app is never frontmost; a modal window shown without
/// activating first can open behind others or render blank.
fn activate(mtm: MainThreadMarker) {
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn new_alert(
    mtm: MainThreadMarker,
    title: &str,
    message: &str,
    buttons: &[&str],
) -> Retained<NSAlert> {
    activate(mtm);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    for button in buttons {
        alert.addButtonWithTitle(&NSString::from_str(button));
    }
    alert
}

pub fn alert(mtm: MainThreadMarker, title: &str, message: &str) {
    new_alert(mtm, title, message, &["OK"]).runModal();
}

/// `true` when the first button was chosen.
pub fn confirm(mtm: MainThreadMarker, title: &str, message: &str, ok: &str, cancel: &str) -> bool {
    new_alert(mtm, title, message, &[ok, cancel]).runModal() == NSAlertFirstButtonReturn
}

/// A one-line text prompt; `None` when cancelled or left blank.
pub fn prompt(mtm: MainThreadMarker, title: &str, message: &str, ok: &str) -> Option<String> {
    let alert = new_alert(mtm, title, message, &[ok, "Cancel"]);
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 24.0));
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    alert.setAccessoryView(Some(&field));
    alert.window().setInitialFirstResponder(Some(&field));
    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let text = field.stringValue().to_string().trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Falls back to `osascript` when `NSUserNotificationCenter` is unavailable.
pub fn notify(subtitle: &str, message: &str) {
    #[allow(deprecated)]
    let center: Option<Retained<NSUserNotificationCenter>> = unsafe {
        msg_send![
            objc2::class!(NSUserNotificationCenter),
            defaultUserNotificationCenter
        ]
    };
    let Some(center) = center else {
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 3 of argv) with title (item 1 of argv) subtitle (item 2 of argv)",
                "-e",
                "end run",
                NOTIFICATION_TITLE,
                subtitle,
                message,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        return;
    };
    #[allow(deprecated)]
    {
        let note = NSUserNotification::new();
        note.setTitle(Some(&NSString::from_str(NOTIFICATION_TITLE)));
        note.setSubtitle(Some(&NSString::from_str(subtitle)));
        note.setInformativeText(Some(&NSString::from_str(message)));
        center.deliverNotification(&note);
    }
}
