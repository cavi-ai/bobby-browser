//! One vision click: screenshot pixels become one CSS point, then a near miss
//! snaps onto a control or a point on a page control is refused.

use types::PageId;

use super::IntentBrowser;
use crate::VisionAction;

/// Roles that count as a page control: a password-manager click must never
/// land on one, and a vision click near one snaps onto it.
const PAGE_CONTROL_ROLES: &[&str] = &[
    "textbox",
    "searchbox",
    "combobox",
    "spinbutton",
    "listbox",
    "button",
    "link",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "option",
    "menuitem",
    "tab",
];

const GROUNDING_RADII: [f64; 2] = [4.0, 8.0];
const GROUNDING_DIRECTIONS: [(f64, f64); 8] = [
    (1.0, 0.0),
    (-1.0, 0.0),
    (0.0, 1.0),
    (0.0, -1.0),
    (DIAGONAL, DIAGONAL),
    (-DIAGONAL, DIAGONAL),
    (DIAGONAL, -DIAGONAL),
    (-DIAGONAL, -DIAGONAL),
];
const DIAGONAL: f64 = std::f64::consts::FRAC_1_SQRT_2;

pub(super) fn is_page_control(role: &str) -> bool {
    PAGE_CONTROL_ROLES.contains(&role)
}

/// How a proposed point becomes the click the intent performs.
pub(super) enum PlacePolicy {
    /// Snap a near miss onto the only nearby control. Several nearby controls refuse the click.
    Snap,
    /// A point on a page control is refused. No snap.
    ClearOfPageControls,
}

pub(super) enum PlacedClick {
    Click {
        x: f64,
        y: f64,
    },
    /// Scaled action that is not a point click.
    Other(VisionAction),
    Ambiguous,
    OnPageControl {
        role: String,
    },
    Unchecked {
        message: String,
    },
}

/// `action` with a click read off a viewport screenshot `frame_width` pixels
/// wide moved to CSS pixels: screenshots are device pixels, clicks CSS pixels.
pub(super) async fn scale_action(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    frame_width: Option<u32>,
    action: &VisionAction,
) -> VisionAction {
    let &VisionAction::Click { x, y } = action else {
        return action.clone();
    };
    let ratio = match (frame_width, browser.viewport_size(page_id).await) {
        (Some(frame_width), Some((css_width, _))) if css_width > 0.0 => {
            f64::from(frame_width) / css_width
        }
        _ => 1.0,
    };
    VisionAction::Click {
        x: x / ratio,
        y: y / ratio,
    }
}

/// A click on no control snaps to the only control within a few pixels and is
/// declined when several are; lookup errors leave the click unchanged.
async fn ground_click(page_id: &PageId, browser: &dyn IntentBrowser, x: f64, y: f64) -> Grounded {
    match browser.element_at_point(page_id, x, y).await {
        Ok(hit) if !hit.as_ref().is_some_and(|(role, _)| is_page_control(role)) => {}
        _ => return Grounded::Unchanged,
    }
    let mut found: Vec<((String, String), (f64, f64))> = Vec::new();
    for radius in GROUNDING_RADII {
        for (dx, dy) in GROUNDING_DIRECTIONS {
            let (px, py) = (x + dx * radius, y + dy * radius);
            let hit = match browser.element_at_point(page_id, px, py).await {
                Ok(hit) => hit,
                Err(_) => return Grounded::Unchanged,
            };
            if let (true, Some(pair)) = (is_page_control_hit(&hit), hit) {
                if !found.iter().any(|(seen, _)| *seen == pair) {
                    found.push((pair, (px, py)));
                }
            }
        }
    }
    match found.as_slice() {
        [] => Grounded::Unchanged,
        [(_, (x, y))] => Grounded::Snapped { x: *x, y: *y },
        _ => Grounded::Ambiguous,
    }
}

fn is_page_control_hit(hit: &Option<(String, String)>) -> bool {
    hit.as_ref().is_some_and(|(role, _)| is_page_control(role))
}

enum Grounded {
    Unchanged,
    Snapped { x: f64, y: f64 },
    Ambiguous,
}

pub(super) async fn place_click(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    frame_width: Option<u32>,
    action: &VisionAction,
    policy: PlacePolicy,
) -> PlacedClick {
    let scaled = scale_action(page_id, browser, frame_width, action).await;
    let VisionAction::Click { x, y } = scaled else {
        return PlacedClick::Other(scaled);
    };
    match policy {
        PlacePolicy::Snap => match ground_click(page_id, browser, x, y).await {
            Grounded::Unchanged => PlacedClick::Click { x, y },
            Grounded::Snapped { x, y } => PlacedClick::Click { x, y },
            Grounded::Ambiguous => PlacedClick::Ambiguous,
        },
        PlacePolicy::ClearOfPageControls => match browser.element_at_point(page_id, x, y).await {
            Ok(None) => PlacedClick::Click { x, y },
            Ok(Some((role, _))) if !is_page_control(&role) => PlacedClick::Click { x, y },
            Ok(Some((role, _))) => PlacedClick::OnPageControl { role },
            Err(error) => PlacedClick::Unchecked {
                message: format!(
                    "cannot confirm the proposed point is clear of page controls: {}",
                    error.message
                ),
            },
        },
    }
}
