//! Protocol-neutral wrappers for ext-data-control and WLR data-control.

use std::os::fd::BorrowedFd;

use wayland_client::globals::{self, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

use super::state::WaylandState;
use crate::clipboard::{
    backend::BackendError,
    types::{DataControlProtocol, ProbeResult},
};

// Interface names as they appear in the Wayland global registry.
const EXT_DATA_CONTROL_MANAGER: &str = "ext_data_control_manager_v1";
const WLR_DATA_CONTROL_MANAGER: &str = "zwlr_data_control_manager_v1";
const WL_SEAT: &str = "wl_seat";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct SeatToken(u32);

#[derive(Clone, Debug)]
pub(super) enum DataControlManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Clone, Debug)]
pub(super) enum DataControlDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone, Debug)]
pub(super) enum DataControlSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum DataControlOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl DataControlManager {
    pub(super) fn get_data_device(
        &self,
        seat: &WlSeat,
        qh: &QueueHandle<WaylandState>,
        token: SeatToken,
    ) -> DataControlDevice {
        match self {
            Self::Ext(manager) => DataControlDevice::Ext(manager.get_data_device(seat, qh, token)),
            Self::Wlr(manager) => DataControlDevice::Wlr(manager.get_data_device(seat, qh, token)),
        }
    }

    pub(super) fn create_data_source(&self, qh: &QueueHandle<WaylandState>) -> DataControlSource {
        match self {
            Self::Ext(manager) => DataControlSource::Ext(manager.create_data_source(qh, ())),
            Self::Wlr(manager) => DataControlSource::Wlr(manager.create_data_source(qh, ())),
        }
    }
}

impl DataControlDevice {
    pub(super) fn set_selection(&self, source: Option<&DataControlSource>) {
        match self {
            Self::Ext(device) => {
                device.set_selection(source.and_then(DataControlSource::as_ext));
            }
            Self::Wlr(device) => {
                device.set_selection(source.and_then(DataControlSource::as_wlr));
            }
        }
    }

    pub(super) fn destroy(&self) {
        match self {
            Self::Ext(device) => device.destroy(),
            Self::Wlr(device) => device.destroy(),
        }
    }
}

impl DataControlSource {
    pub(super) fn offer(&self, mime_type: String) {
        match self {
            Self::Ext(source) => source.offer(mime_type),
            Self::Wlr(source) => source.offer(mime_type),
        }
    }

    pub(super) fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }

    pub(super) fn as_ext(&self) -> Option<&ExtDataControlSourceV1> {
        match self {
            Self::Ext(source) => Some(source),
            Self::Wlr(_) => None,
        }
    }

    pub(super) fn as_wlr(&self) -> Option<&ZwlrDataControlSourceV1> {
        match self {
            Self::Wlr(source) => Some(source),
            Self::Ext(_) => None,
        }
    }

    pub(super) fn same_proxy(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ext(left), Self::Ext(right)) => left.id() == right.id(),
            (Self::Wlr(left), Self::Wlr(right)) => left.id() == right.id(),
            _ => false,
        }
    }
}

impl DataControlOffer {
    pub(super) fn receive(&self, mime_type: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime_type, fd),
            Self::Wlr(offer) => offer.receive(mime_type, fd),
        }
    }

    pub(super) fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

pub(super) struct SeatBinding {
    pub(super) token: SeatToken,
    pub(super) seat: WlSeat,
}

pub(super) fn bind_data_control_manager(
    globals: &GlobalList,
    qh: &QueueHandle<WaylandState>,
) -> Result<(DataControlProtocol, DataControlManager), BackendError> {
    if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(qh, 1..=1, ()) {
        return Ok((DataControlProtocol::Ext, DataControlManager::Ext(manager)));
    }

    if let Ok(manager) = globals.bind::<ZwlrDataControlManagerV1, _, _>(qh, 1..=1, ()) {
        return Ok((DataControlProtocol::Wlr, DataControlManager::Wlr(manager)));
    }

    Err(BackendError::NoDataControl)
}

pub(super) fn bind_seats(globals: &GlobalList, qh: &QueueHandle<WaylandState>) -> Vec<SeatBinding> {
    let registry = globals.registry();
    globals.contents().with_list(|items| {
        items
            .iter()
            .filter(|global| global.interface == WlSeat::interface().name && global.version >= 1)
            .enumerate()
            .map(|(index, global)| {
                let token = SeatToken(u32::try_from(index).unwrap_or(u32::MAX));
                SeatBinding {
                    token,
                    seat: registry.bind(global.name, 1, qh, ()),
                }
            })
            .collect::<Vec<_>>()
    })
}

/// Minimal dispatch state used only for the capability probe.
///
/// We only need the `wl_registry` dispatch to satisfy `registry_queue_init`;
/// the actual protocol objects are never bound during the probe.
struct ProbeState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The GlobalList machinery handles registry events internally.
        // We have nothing extra to do here.
    }
}

/// Performs a single Wayland round-trip to discover data-control globals.
///
/// This is intentionally synchronous; it is called from within
/// `spawn_blocking`.
pub(super) fn probe_wayland_globals() -> Result<ProbeResult, BackendError> {
    let conn = Connection::connect_to_env().map_err(|e| BackendError::Connection(e.to_string()))?;

    let (global_list, _queue) = globals::registry_queue_init::<ProbeState>(&conn)
        .map_err(|e| BackendError::Connection(e.to_string()))?;

    let contents = global_list.contents();

    let mut has_ext = false;
    let mut has_wlr = false;
    let mut has_seat = false;

    contents.with_list(|globals| {
        for global in globals {
            match global.interface.as_str() {
                EXT_DATA_CONTROL_MANAGER => has_ext = true,
                WLR_DATA_CONTROL_MANAGER => has_wlr = true,
                WL_SEAT => has_seat = true,
                _ => {}
            }
        }
    });

    let protocol = if has_ext {
        Some(DataControlProtocol::Ext)
    } else if has_wlr {
        Some(DataControlProtocol::Wlr)
    } else {
        None
    };

    Ok(ProbeResult { protocol, has_seat })
}
