//! Shrinks tabs to fit their bar, like browser tabs.
//!
//! `TabBar` keeps every tab at its natural width and scrolls when they overflow. To make tabs
//! share the space instead, the bar's width is measured each frame and handed back as the
//! bar's per-tab `max_width`, which truncates long titles with an ellipsis. Below
//! [`MIN_TAB_WIDTH`] tabs stop shrinking and the bar falls back to scrolling and its overflow
//! menu.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{EntityId, IntoElement, Pixels, Styled as _, canvas, px};

/// Narrowest a tab gets: enough for a few characters, the ellipsis and the close button.
pub const MIN_TAB_WIDTH: f32 = 90.;
/// Widest a tab gets when there is plenty of room.
pub const MAX_TAB_WIDTH: f32 = 220.;
/// Space kept free in a tab bar for the "+" button and the overflow menu button.
pub const TAB_BAR_RESERVED: f32 = 72.;

/// Measured width of a tab bar, shared between the view and the element that measures it.
#[derive(Clone, Default)]
pub struct TabFit {
    width: Rc<Cell<Pixels>>,
}

impl TabFit {
    /// Width each of `count` tabs may take, after `reserved` pixels for the bar's own buttons.
    pub fn tab_width(&self, count: usize, reserved: Pixels) -> Pixels {
        fit(self.width.get(), count, reserved)
    }

    /// An invisible element that fills its (relative) parent and records its width. When the
    /// width changes it re-renders `owner`, so the next frame uses the new tab width.
    pub fn measure(&self, owner: EntityId) -> impl IntoElement {
        let width = self.width.clone();
        canvas(
            move |bounds, _, cx| {
                if width.get() != bounds.size.width {
                    width.set(bounds.size.width);
                    cx.notify(owner);
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full()
    }
}

#[allow(clippy::cast_precision_loss)]
fn fit(available: Pixels, count: usize, reserved: Pixels) -> Pixels {
    if count == 0 || available <= px(0.) {
        return px(MAX_TAB_WIDTH);
    }
    let per_tab = (available - reserved) / count as f32;
    per_tab.clamp(px(MIN_TAB_WIDTH), px(MAX_TAB_WIDTH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_share_space_within_bounds() {
        // Plenty of room: capped at the maximum.
        assert_eq!(fit(px(1000.), 2, px(40.)), px(MAX_TAB_WIDTH));
        // Crowded: shrink to share the bar.
        assert_eq!(fit(px(840.), 8, px(40.)), px(100.));
        // Very crowded: stop at the minimum and let the bar scroll.
        assert_eq!(fit(px(500.), 20, px(40.)), px(MIN_TAB_WIDTH));
        // Not measured yet.
        assert_eq!(fit(px(0.), 3, px(40.)), px(MAX_TAB_WIDTH));
    }
}
