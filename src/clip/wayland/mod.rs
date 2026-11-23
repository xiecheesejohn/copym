//! Wayland backends based on the data-control protocols, which let a clipboard
//! manager watch and set the clipboard without ever having keyboard focus.
//!
//! * `ext-data-control-v1` - the standard one (KDE Plasma 6.x, recent wlroots,
//!   Hyprland, niri, COSMIC, ...)
//! * `wlr-data-control-unstable-v1` - legacy fallback for older compositors

use super::{ClipEvent, Monitor, MonitorOptions};
use std::sync::mpsc::Sender;

mod ext {
    use wayland_protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1::{self as dev, ExtDataControlDeviceV1 as Device},
        ext_data_control_manager_v1::ExtDataControlManagerV1 as Manager,
        ext_data_control_offer_v1::{self as offer, ExtDataControlOfferV1 as Offer},
        ext_data_control_source_v1::{self as src, ExtDataControlSourceV1 as Source},
    };
    const MANAGER_IFACE: &str = "ext_data_control_manager_v1";
    const BACKEND: &str = "wayland (ext-data-control)";
    include!("imp.rs");
}

mod wlr {
    use wayland_protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1::{self as dev, ZwlrDataControlDeviceV1 as Device},
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 as Manager,
        zwlr_data_control_offer_v1::{self as offer, ZwlrDataControlOfferV1 as Offer},
        zwlr_data_control_source_v1::{self as src, ZwlrDataControlSourceV1 as Source},
    };
    const MANAGER_IFACE: &str = "zwlr_data_control_manager_v1";
    const BACKEND: &str = "wayland (wlr-data-control)";
    include!("imp.rs");
}

pub fn start(tx: Sender<ClipEvent>, opts: MonitorOptions) -> Result<Monitor, String> {
    match ext::start(tx.clone(), opts.clone()) {
        Ok(m) => Ok(m),
        Err(e1) => wlr::start(tx, opts).map_err(|e2| format!("{e1}; {e2}")),
    }
}
