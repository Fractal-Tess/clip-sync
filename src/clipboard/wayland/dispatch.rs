//! Wayland event dispatch for both data-control protocol variants.

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use super::{
    protocol::{DataControlOffer, DataControlSource, SeatToken},
    state::WaylandState,
};
use crate::clipboard::backend::ClipboardEvent;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: ext_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: zwlr_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, SeatToken> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &SeatToken,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.handle_data_offer(DataControlOffer::Ext(id));
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.handle_selection(id.map(DataControlOffer::Ext));
            }
            ext_data_control_device_v1::Event::Finished => {
                state.finished = true;
                state.emit(ClipboardEvent::Finished);
            }
            ext_data_control_device_v1::Event::PrimarySelection { .. } => {
                tracing::trace!("ignoring ext-data-control primary selection event");
            }
            _ => {}
        }
    }

    event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, SeatToken> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &SeatToken,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.handle_data_offer(DataControlOffer::Wlr(id));
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.handle_selection(id.map(DataControlOffer::Wlr));
            }
            zwlr_data_control_device_v1::Event::Finished => {
                state.finished = true;
                state.emit(ClipboardEvent::Finished);
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {
                tracing::trace!("ignoring wlr-data-control primary selection event");
            }
            _ => {}
        }
    }

    event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.handle_offer_mime(DataControlOffer::Ext(proxy.clone()), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.handle_offer_mime(DataControlOffer::Wlr(proxy.clone()), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let source = DataControlSource::Ext(proxy.clone());
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.handle_source_send(&source, mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.handle_source_cancelled(&source);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let source = DataControlSource::Wlr(proxy.clone());
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.handle_source_send(&source, mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.handle_source_cancelled(&source);
            }
            _ => {}
        }
    }
}
