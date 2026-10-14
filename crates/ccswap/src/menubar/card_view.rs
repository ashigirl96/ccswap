//! Draws an `AccountCard` inside its menu item. A menu item with a view
//! draws neither its highlight nor dispatches its action, so this view does
//! both.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSFont, NSFontWeightRegular,
    NSResponder, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};

use super::format::UsageRow;
use super::menu::{AccountCard, AccountStatus};

const WIDTH: f64 = 380.0;
const LEFT: f64 = 20.0;
const RIGHT: f64 = 16.0;
const TOP: f64 = 6.0;
const BOTTOM: f64 = 7.0;
const HEADER_H: f64 = 20.0;
const ROW_H: f64 = 18.0;
const NAME_W: f64 = 58.0;
const PCT_W: f64 = 40.0;
const COUNTDOWN_W: f64 = 56.0;
const GAP: f64 = 8.0;
const BAR_X: f64 = LEFT + NAME_W + GAP;
const BAR_TRAIL: f64 = GAP + PCT_W + GAP + COUNTDOWN_W + RIGHT;
const BAR_H: f64 = 6.0;

struct Bar {
    mid_y: f64,
    pct: f64,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CcswapAccountCardView"]
    #[ivars = Vec<Bar>]
    struct CardView;

    impl CardView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            if self.enclosingMenuItem().is_some_and(|item| item.isHighlighted()) {
                let inset = NSRect::new(
                    NSPoint::new(5.0, 0.0),
                    NSSize::new(bounds.size.width - 10.0, bounds.size.height),
                );
                NSColor::labelColor().colorWithAlphaComponent(0.1).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inset, 5.0, 5.0).fill();
            }
            let width = (bounds.size.width - BAR_X - BAR_TRAIL).max(0.0);
            for bar in self.ivars() {
                let y = bar.mid_y - BAR_H / 2.0;
                let track = NSRect::new(NSPoint::new(BAR_X, y), NSSize::new(width, BAR_H));
                track_color(bar.pct).setFill();
                capsule(track).fill();
                let filled = width * bar.pct.clamp(0.0, 100.0) / 100.0;
                if filled > 0.0 {
                    let fill = NSRect::new(NSPoint::new(BAR_X, y), NSSize::new(filled.max(BAR_H), BAR_H));
                    fill_color(bar.pct).setFill();
                    capsule(fill).fill();
                }
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &AnyObject) {
            let Some(item) = self.enclosingMenuItem() else {
                return;
            };
            if let Some(menu) = unsafe { item.menu() } {
                menu.cancelTracking();
            }
            let Some(action) = item.action() else {
                return;
            };
            unsafe {
                let target = item.target();
                NSApplication::sharedApplication(self.mtm()).sendAction_to_from(
                    action,
                    target.as_deref(),
                    Some(&item),
                );
            }
        }
    }
);

fn capsule(rect: NSRect) -> Retained<NSBezierPath> {
    let r = rect.size.height / 2.0;
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, r, r)
}

fn fill_color(pct: f64) -> Retained<NSColor> {
    if pct >= 90.0 {
        NSColor::systemRedColor()
    } else if pct >= 70.0 {
        NSColor::systemYellowColor()
    } else {
        NSColor::systemBlueColor()
    }
}

fn track_color(pct: f64) -> Retained<NSColor> {
    if pct <= 0.0 {
        NSColor::labelColor().colorWithAlphaComponent(0.1)
    } else {
        fill_color(pct).colorWithAlphaComponent(0.2)
    }
}

fn status_color(status: AccountStatus) -> Retained<NSColor> {
    match status {
        AccountStatus::Active => NSColor::systemBlueColor(),
        AccountStatus::LimitReached => NSColor::systemRedColor(),
        AccountStatus::Disabled | AccountStatus::Idle => NSColor::secondaryLabelColor(),
    }
}

fn pct_color(row: &UsageRow) -> Retained<NSColor> {
    if row.maxed() {
        NSColor::systemRedColor()
    } else if row.ahead {
        NSColor::systemOrangeColor()
    } else {
        NSColor::labelColor()
    }
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    font: &NSFont,
    color: &NSColor,
) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFont(Some(font));
    field.setTextColor(Some(color));
    field.sizeToFit();
    field
}

/// Places `field` at `x` on the line centred at `mid_y`, at most `max_w` wide.
fn place(parent: &NSView, field: &NSTextField, x: f64, mid_y: f64, max_w: f64) -> f64 {
    let size = field.frame().size;
    let w = size.width.min(max_w);
    field.setFrame(NSRect::new(
        NSPoint::new(x, mid_y - size.height / 2.0),
        NSSize::new(w, size.height),
    ));
    parent.addSubview(field);
    w
}

/// Right-aligned in a fixed column ending `right` points from the edge, so
/// the column follows a menu wider than the card.
fn place_right(parent: &NSView, field: &NSTextField, right: f64, mid_y: f64, w: f64) {
    let h = field.frame().size.height;
    field.setAlignment(NSTextAlignment::Right);
    field.setFrame(NSRect::new(
        NSPoint::new(WIDTH - right - w, mid_y - h / 2.0),
        NSSize::new(w, h),
    ));
    field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
    parent.addSubview(field);
}

pub fn new(mtm: MainThreadMarker, card: &AccountCard) -> Retained<NSView> {
    let rows: &[UsageRow] = card.usage.as_deref().unwrap_or_default();
    let body_h = if card.usage.is_ok() {
        ROW_H * rows.len() as f64
    } else {
        ROW_H
    };
    let height = TOP + HEADER_H + body_h + BOTTOM;
    let row_mid = |i: usize| TOP + HEADER_H + ROW_H * (i as f64 + 0.5);
    let bars = rows
        .iter()
        .enumerate()
        .map(|(i, r)| Bar {
            mid_y: row_mid(i),
            pct: r.pct,
        })
        .collect();

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height));
    let this = CardView::alloc(mtm).set_ivars(bars);
    let view: Retained<CardView> = unsafe { msg_send![super(this), initWithFrame: frame] };
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

    let small = NSFont::systemFontOfSize(12.0);
    let digits =
        NSFont::monospacedDigitSystemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular });
    let secondary = NSColor::secondaryLabelColor();
    let header_mid = TOP + HEADER_H / 2.0;

    let mut x = LEFT;
    let number = label(mtm, &card.number, &small, &NSColor::tertiaryLabelColor());
    x += place(&view, &number, x, header_mid, 20.0) + 6.0;
    let name = label(
        mtm,
        &card.name,
        &NSFont::boldSystemFontOfSize(13.0),
        &NSColor::labelColor(),
    );
    x += place(&view, &name, x, header_mid, WIDTH - x - RIGHT) + 8.0;
    if card.status != AccountStatus::Idle {
        let status = label(mtm, card.status.text(), &small, &status_color(card.status));
        x += place(&view, &status, x, header_mid, (WIDTH - x - RIGHT).max(0.0)) + 8.0;
    }
    if let Some(email) = &card.email {
        let email = label(
            mtm,
            email,
            &NSFont::systemFontOfSize(11.0),
            &NSColor::tertiaryLabelColor(),
        );
        let w = email.frame().size.width.min((WIDTH - x - RIGHT).max(0.0));
        place_right(&view, &email, RIGHT, header_mid, w);
    }

    match &card.usage {
        Ok(rows) => {
            for (i, row) in rows.iter().enumerate() {
                let mid = row_mid(i);
                let name = label(mtm, row.name(), &small, &NSColor::labelColor());
                place(&view, &name, LEFT, mid, NAME_W);
                let pct = label(mtm, &row.pct_text(), &digits, &pct_color(row));
                place_right(&view, &pct, RIGHT + COUNTDOWN_W + GAP, mid, PCT_W);
                if let Some(countdown) = &row.countdown {
                    let countdown = label(mtm, countdown, &digits, &secondary);
                    place_right(&view, &countdown, RIGHT, mid, COUNTDOWN_W);
                }
            }
        }
        Err(note) => {
            let note = label(mtm, note, &small, &secondary);
            place(&view, &note, LEFT, row_mid(0), WIDTH - LEFT - RIGHT);
        }
    }
    if card.status == AccountStatus::Disabled {
        view.setAlphaValue(0.5);
    }
    Retained::into_super(view)
}
