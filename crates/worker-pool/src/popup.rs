//! Whether a click's popup is still a page once its document has stopped changing.
//! Each engine adapter only reports how it bound the new page.

use types::{Evidence, PageId};

/// The popup after the adapter has bound it, or learned that it closed.
pub enum PopupFate {
    Open {
        page_id: PageId,
        url: String,
        title: String,
    },
    Closed {
        page_id: PageId,
    },
}

/// Evidence for one popup: the page when it stayed, or [`Evidence::PopupClosed`]
/// when it died before the document settled. `click` is the click that opened it.
pub fn popup_evidence(
    opener_page_id: PageId,
    fate: PopupFate,
    click: impl IntoIterator<Item = Evidence>,
) -> Vec<Evidence> {
    let head = match fate {
        PopupFate::Open {
            page_id,
            url,
            title,
        } => Evidence::Popup {
            opener_page_id,
            page_id,
            url,
            title,
        },
        PopupFate::Closed { page_id } => Evidence::PopupClosed {
            opener_page_id,
            popup_page_id: page_id,
        },
    };
    let mut evidence = vec![head];
    evidence.extend(click);
    evidence
}
