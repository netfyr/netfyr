// The worker enters its namespace before this transport opens sockets.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::fmt::Debug;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use futures_util::{FutureExt, Stream, StreamExt, TryStreamExt};
use indexmap::IndexMap;
use libc::{RTM_DELLINK, RTM_NEWADDR, RTM_NEWLINK};
use netfyr_state::plan::{
    AddressObservation, DeviceObservation, Diagnostic, InterfaceId, Ipv4Address, NamespaceId,
    Observation, OperationKind,
};
use netfyr_state::{Match, Source, State, Value};
use rtnetlink::packet_core::{
    DecodeError, Emitable, NLA_ALIGNTO, NLM_F_ACK, NLM_F_ACK_TLVS, NLM_F_CAPPED, NLM_F_CREATE,
    NLM_F_DUMP, NLM_F_DUMP_INTR, NLM_F_EXCL, NLM_F_REPLACE, NLM_F_REQUEST, NetlinkBuffer,
    NetlinkDeserializable, NetlinkHeader, NetlinkMessage, NetlinkPayload, NetlinkSerializable,
    NlasIterator, Parseable, ParseableParametrized,
};
use rtnetlink::packet_route::address::{AddressAttribute, AddressMessage, CacheInfo};
use rtnetlink::packet_route::link::{
    InfoKind, LinkAttribute, LinkFlags, LinkHeader, LinkInfo, LinkLayerType, LinkMessage,
};
use rtnetlink::packet_route::{AddressFamily, RouteNetlinkMessage};
use rtnetlink::proto::{ConnectionHandle, NetlinkCodec, NetlinkMessageCodec};
use rtnetlink::sys::{AsyncSocket, SocketAddr, TokioSocket};
use tokio::task::JoinHandle;

use crate::{BackendError, BackendErrorKind, ETHERNET};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OPTIONAL_TIMEOUT: Duration = Duration::from_secs(2);
const NLMSGERR_ATTR_MSG: u16 = 1;
const ETHTOOL_GDRVINFO: u32 = 3;

#[derive(Debug)]
struct RoutePacket {
    kind: u16,
    body: Vec<u8>,
}

impl From<RouteNetlinkMessage> for RoutePacket {
    fn from(message: RouteNetlinkMessage) -> Self {
        let mut body = vec![0; NetlinkSerializable::buffer_len(&message)];
        message.serialize(&mut body);
        Self {
            kind: message.message_type(),
            body,
        }
    }
}

impl NetlinkSerializable for RoutePacket {
    fn message_type(&self) -> u16 {
        self.kind
    }
    fn buffer_len(&self) -> usize {
        self.body.len()
    }
    fn serialize(&self, buffer: &mut [u8]) {
        buffer.copy_from_slice(&self.body);
    }
}

impl NetlinkDeserializable for RoutePacket {
    type Error = DecodeError;

    fn deserialize(header: &NetlinkHeader, body: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self {
            kind: header.message_type,
            body: body.to_vec(),
        })
    }
}

fn align(length: usize) -> Option<usize> {
    Some(length.checked_add(NLA_ALIGNTO - 1)? & !(NLA_ALIGNTO - 1))
}

// The upstream codec drops malformed packets and continues the dump. Inventory
// consumers must instead see a failed connection, never a shortened complete list.
struct StrictCodec;

impl NetlinkMessageCodec for StrictCodec {
    fn decode<T>(source: &mut BytesMut) -> io::Result<Option<NetlinkMessage<T>>>
    where
        T: NetlinkDeserializable + Debug,
    {
        if source.is_empty() {
            return Ok(None);
        }
        let size = NetlinkBuffer::new_checked(source.as_ref())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
            .length() as usize;
        let aligned_size = align(size).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "netlink message length overflow",
            )
        })?;
        let data = source.split_to(aligned_size.min(source.len()));
        let packet = NetlinkMessage::<T>::deserialize(&data[..size])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(Some(packet))
    }

    fn encode<T>(message: NetlinkMessage<T>, buffer: &mut BytesMut) -> io::Result<()>
    where
        T: NetlinkSerializable + Debug,
    {
        NetlinkCodec::encode(message, buffer)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exchange {
    Dump,
    Read,
    Write,
}

impl Exchange {
    fn flags(self) -> u16 {
        match self {
            Self::Dump => NLM_F_REQUEST | NLM_F_DUMP,
            Self::Read | Self::Write => NLM_F_REQUEST | NLM_F_ACK,
        }
    }

    fn rejected(self) -> BackendErrorKind {
        match self {
            Self::Dump | Self::Read => BackendErrorKind::QueryFailed,
            Self::Write => BackendErrorKind::ApplyFailed,
        }
    }

    fn unconfirmed(self) -> BackendErrorKind {
        match self {
            Self::Dump | Self::Read => BackendErrorKind::QueryFailed,
            Self::Write => BackendErrorKind::OutcomeUnknown,
        }
    }
}

type Notifications = Pin<Box<dyn Stream<Item = (NetlinkMessage<RoutePacket>, SocketAddr)> + Send>>;

pub(crate) struct LinuxTransport {
    namespace: NamespaceId,
    route: ConnectionHandle<RoutePacket>,
    notifications: Notifications,
    notifications_closed: bool,
    route_task: JoinHandle<()>,
    ethtool: Option<ethtool::EthtoolHandle>,
    ethtool_task: Option<JoinHandle<()>>,
    wireless: Option<wl_nl80211::Nl80211Handle>,
    wireless_task: Option<JoinHandle<()>>,
    ioctl_socket: OwnedFd,
    generations: HashMap<u32, u64>,
    deletion_epoch: u64,
    invalidated: Option<&'static str>,
    address_messages: HashMap<(u32, Ipv4Addr, u8), AddressMessage>,
    initialization_diagnostics: Vec<Diagnostic>,
}

impl Drop for LinuxTransport {
    fn drop(&mut self) {
        self.route_task.abort();
        if let Some(task) = &self.ethtool_task {
            task.abort();
        }
        if let Some(task) = &self.wireless_task {
            task.abort();
        }
    }
}

impl LinuxTransport {
    pub(crate) async fn new(namespace: NamespaceId) -> Result<Self, BackendError> {
        let failed = |error| BackendError::from_io(BackendErrorKind::QueryFailed, error);
        let (mut connection, route, notifications) =
            rtnetlink::proto::new_connection_with_codec::<RoutePacket, TokioSocket, StrictCodec>(
                rtnetlink::sys::protocols::NETLINK_ROUTE,
            )
            .map_err(failed)?;
        connection.set_forward_ack(true);
        connection.set_forward_done(true);
        let socket = connection.socket_mut().socket_mut();
        socket.bind(&SocketAddr::new(0, 1)).map_err(failed)?;
        socket.set_ext_ack(true).map_err(failed)?;
        socket.set_cap_ack(true).map_err(failed)?;
        // SAFETY: socket has no pointer arguments and its returned descriptor is owned here.
        let descriptor =
            unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if descriptor < 0 {
            return Err(failed(io::Error::last_os_error()));
        }
        // SAFETY: descriptor is a newly created, valid descriptor with one owner.
        let ioctl_socket = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let route_task = tokio::spawn(connection);
        let mut initialization_diagnostics = Vec::new();
        let (ethtool, ethtool_task) = match ethtool::new_connection() {
            Ok((connection, handle, _)) => (Some(handle), Some(tokio::spawn(connection))),
            Err(error) => {
                initialization_diagnostics.push(diagnostic(None, "ethernet", error.to_string()));
                (None, None)
            }
        };
        let (wireless, wireless_task) = match wl_nl80211::new_connection() {
            Ok((mut connection, handle, _)) => {
                connection.set_forward_done(true);
                (Some(handle), Some(tokio::spawn(connection)))
            }
            Err(error) => {
                initialization_diagnostics.push(diagnostic(None, "type", error.to_string()));
                (None, None)
            }
        };
        Ok(Self {
            namespace,
            route,
            notifications: Box::pin(notifications),
            notifications_closed: false,
            route_task,
            ethtool,
            ethtool_task,
            wireless,
            wireless_task,
            ioctl_socket,
            generations: HashMap::new(),
            deletion_epoch: 0,
            invalidated: None,
            address_messages: HashMap::new(),
            initialization_diagnostics,
        })
    }

    pub(crate) fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    fn invalidate(&mut self, reason: &'static str) {
        self.generations.clear();
        self.deletion_epoch = self.deletion_epoch.wrapping_add(1);
        self.invalidated.get_or_insert(reason);
    }

    fn record(&mut self, item: Option<(NetlinkMessage<RoutePacket>, SocketAddr)>) {
        let Some((message, _)) = item else {
            self.notifications_closed = true;
            self.invalidate("netlink connection closed");
            return;
        };
        match message.payload {
            NetlinkPayload::InnerMessage(packet) => match deleted_link(&packet) {
                Ok(Some(index)) => {
                    record_deletion(&mut self.generations, &mut self.deletion_epoch, index)
                }
                Ok(None) => {}
                Err(_) => self.invalidate("unparseable link deletion notification"),
            },
            NetlinkPayload::Overrun(_) => self.invalidate("link notification buffer overrun"),
            _ => {}
        }
    }

    // Consume idle notifications to avoid an unbounded backlog.
    pub(crate) async fn next_notification(&mut self) {
        if self.notifications_closed {
            return std::future::pending().await;
        }
        let item = self.notifications.next().await;
        self.record(item);
    }

    fn drain_notifications(&mut self) -> Result<(), BackendError> {
        while !self.notifications_closed {
            match self.notifications.next().now_or_never() {
                Some(item) => self.record(item),
                None => break,
            }
        }
        match self.invalidated.take() {
            Some(reason) => Err(BackendError::new(
                BackendErrorKind::QueryFailed,
                format!("{reason}; identities invalidated, query again"),
            )),
            None => Ok(()),
        }
    }

    async fn request(
        &mut self,
        message: RouteNetlinkMessage,
        exchange: Exchange,
        extra_flags: u16,
    ) -> Result<Vec<RoutePacket>, BackendError> {
        self.drain_notifications()?;
        let mut request = NetlinkMessage::new(
            NetlinkHeader::default(),
            NetlinkPayload::InnerMessage(message.into()),
        );
        request.header.flags = exchange.flags() | extra_flags;
        let mut stream = self
            .route
            .request(request, SocketAddr::new(0, 0))
            .map_err(|error| BackendError::new(exchange.rejected(), error.to_string()))?;
        tokio::time::timeout(REQUEST_TIMEOUT, collect_response(&mut stream, exchange))
            .await
            .map_err(|_| {
                BackendError::new(
                    exchange.unconfirmed(),
                    "netlink request timed out before confirmed completion",
                )
            })?
    }

    pub(crate) async fn inventory(
        &mut self,
        enrich: impl Fn(&Match) -> bool,
    ) -> Result<Observation, BackendError> {
        self.drain_notifications()?;
        let deletion_epoch = self.deletion_epoch;
        let links = self
            .request(
                RouteNetlinkMessage::GetLink(LinkMessage::default()),
                Exchange::Dump,
                0,
            )
            .await?;
        let mut address_request = AddressMessage::default();
        address_request.header.family = AddressFamily::Inet;
        let addresses = self
            .request(
                RouteNetlinkMessage::GetAddress(address_request),
                Exchange::Dump,
                0,
            )
            .await?;
        let observed_at = Instant::now();
        let mut diagnostics = self.initialization_diagnostics.clone();
        let wireless = match self.wireless_indices().await {
            Ok(indices) => Some(indices),
            Err(message) => {
                diagnostics.push(diagnostic(None, "type", message));
                None
            }
        };
        let socket = self.ioctl_socket.as_raw_fd();
        let mut devices = Vec::new();
        let mut link_mode_targets = Vec::new();
        for packet in links {
            if packet.kind != RTM_NEWLINK {
                return Err(BackendError::new(
                    BackendErrorKind::QueryFailed,
                    "unexpected response in link dump",
                ));
            }
            let parsed = parse_link(&packet.body)?;
            let index = parsed.message.header.index;
            let legacy = while_named(socket, &parsed.name, index, || {
                Ok(legacy_wireless(socket, &parsed.name))
            })
            .map_err(|error| {
                if error.kind() == io::ErrorKind::Interrupted {
                    renamed_during_inventory()
                } else {
                    BackendError::from_io(BackendErrorKind::QueryFailed, error)
                }
            })?;
            let device_type = if legacy {
                "wifi".into()
            } else {
                classify_link(&parsed, wireless.as_ref())
            };
            let generation = match self.generations.get(&index) {
                Some(generation) => *generation,
                None => {
                    let generation = fresh_generation()?;
                    self.generations.insert(index, generation);
                    generation
                }
            };
            let (mut state, mac) = link_state(&parsed, &device_type);
            let identity = InterfaceId {
                namespace: self.namespace,
                index,
                generation,
                name: parsed.name.clone(),
                device_type,
                mac,
            };
            for (field, message) in parsed.diagnostics {
                diagnostics.push(diagnostic(Some(identity.clone()), &field, message));
            }
            if enrich(&state.match_spec) {
                match while_named(socket, &identity.name, index, || {
                    driver_info(socket, &identity.name)
                }) {
                    Ok((driver, pci_path)) => {
                        if let Some(driver) = &driver {
                            state
                                .fields
                                .insert("driver".into(), Value::String(driver.clone()));
                        }
                        state.match_spec.driver = driver;
                        state.match_spec.pci_path = pci_path;
                    }
                    Err(error) if unsupported_ioctl(&error) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        return Err(renamed_during_inventory());
                    }
                    Err(error) => diagnostics.push(diagnostic(
                        Some(identity.clone()),
                        "driver",
                        error.to_string(),
                    )),
                }
                if identity.device_type == ETHERNET {
                    link_mode_targets.push((devices.len(), identity.name.clone(), index));
                }
            }
            let addresses_complete = identity.device_type == ETHERNET;
            devices.push(DeviceObservation {
                identity,
                state,
                addresses: Vec::new(),
                addresses_complete,
                observed_at,
            });
        }
        if let Some(handle) = &self.ethtool {
            let results =
                futures_util::future::join_all(link_mode_targets.iter().map(|(_, name, index)| {
                    ethernet_attributes(handle.clone(), name.clone(), *index)
                }))
                .await;
            for ((position, _, _), result) in link_mode_targets.iter().zip(results) {
                let (fields, messages) = result?;
                let device = &mut devices[*position];
                if !fields.is_empty() {
                    device
                        .state
                        .fields
                        .insert("ethernet".into(), Value::Map(fields));
                }
                for message in messages {
                    diagnostics.push(diagnostic(
                        Some(device.identity.clone()),
                        "ethernet",
                        message,
                    ));
                }
            }
        }
        self.generations
            .retain(|index, _| devices.iter().any(|device| device.identity.index == *index));
        let mut address_messages = HashMap::new();
        let mut duplicated = HashSet::new();
        for packet in addresses {
            if packet.kind != RTM_NEWADDR {
                return Err(BackendError::new(
                    BackendErrorKind::QueryFailed,
                    "unexpected response in address dump",
                ));
            }
            let message = AddressMessage::parse(&packet.body).map_err(|error| {
                BackendError::new(
                    BackendErrorKind::QueryFailed,
                    format!("incomplete IPv4 observation: {error}"),
                )
            })?;
            if message.header.family != AddressFamily::Inet {
                continue;
            }
            let index = message.header.index;
            let device = devices
                .iter_mut()
                .find(|device| device.identity.index == index)
                .ok_or_else(|| {
                    BackendError::new(
                        BackendErrorKind::QueryFailed,
                        format!(
                            "address inventory contains an unobserved interface {index}; query again"
                        ),
                    )
                })?;
            if device.identity.device_type != ETHERNET {
                continue;
            }
            let observed = parse_address(&message).map_err(|message| {
                BackendError::new(
                    BackendErrorKind::QueryFailed,
                    format!(
                        "{}: incomplete IPv4 observation: {message}",
                        device.identity.name
                    ),
                )
            })?;
            let key = (index, observed.address.ip, observed.address.prefix_len);
            if address_messages.insert(key, message).is_some() {
                // The kernel allows one local address with different peers; the
                // plan model identifies addresses by local IP and prefix only.
                device.addresses_complete = false;
                if duplicated.insert(index) {
                    diagnostics.push(diagnostic(
                        Some(device.identity.clone()),
                        "ipv4.addresses",
                        "local address assigned more than once with different peers; address operations are refused"
                            .into(),
                    ));
                }
                continue;
            }
            device.addresses.push(observed);
        }
        for device in &mut devices {
            if device.identity.device_type == ETHERNET {
                let values = device.addresses.iter().map(address_value).collect();
                device.state.fields.insert(
                    "ipv4".into(),
                    Value::Map(IndexMap::from([("addresses".into(), Value::List(values))])),
                );
            }
        }
        self.drain_notifications()?;
        validate_deletion_epoch(deletion_epoch, self.deletion_epoch)?;
        for device in &devices {
            if self.generations.get(&device.identity.index) != Some(&device.identity.generation) {
                return Err(BackendError::new(
                    BackendErrorKind::QueryFailed,
                    "interface disappeared during inventory; query again",
                ));
            }
        }
        self.address_messages = address_messages;
        Ok(Observation {
            devices,
            diagnostics,
        })
    }

    pub(crate) async fn execute(
        &mut self,
        target: &InterfaceId,
        operation: &OperationKind,
    ) -> Result<(), BackendError> {
        let (message, flags) = match operation {
            OperationKind::SetMtu(mtu) => {
                let mut message = LinkMessage::default();
                message.header.index = target.index;
                message.attributes.push(LinkAttribute::Mtu(*mtu));
                (RouteNetlinkMessage::SetLink(message), 0)
            }
            OperationKind::SetEnabled(enabled) => {
                let mut message = LinkMessage::default();
                message.header.index = target.index;
                message.header.change_mask = LinkFlags::Up;
                if *enabled {
                    message.header.flags = LinkFlags::Up;
                }
                (RouteNetlinkMessage::SetLink(message), 0)
            }
            OperationKind::AddIpv4(address) => (
                RouteNetlinkMessage::NewAddress(address_message(target.index, address, true)),
                NLM_F_CREATE | NLM_F_EXCL,
            ),
            OperationKind::ReplaceIpv4(address) => (
                RouteNetlinkMessage::NewAddress(replacement_message(
                    self.address_messages
                        .get(&(target.index, address.ip, address.prefix_len)),
                    target.index,
                    address,
                )),
                NLM_F_REPLACE,
            ),
            OperationKind::RemoveIpv4(address) => (
                RouteNetlinkMessage::DelAddress(removal_message(
                    self.address_messages
                        .get(&(target.index, address.ip, address.prefix_len)),
                    target.index,
                    address,
                )),
                0,
            ),
            _ => {
                return Err(BackendError::new(
                    BackendErrorKind::UnsupportedOperation,
                    "operation is not supported by the Linux transport",
                ));
            }
        };
        self.confirm_identity(target).await?;
        self.request(message, Exchange::Write, flags).await?;
        Ok(())
    }

    // Notifications arrive in socket order, so once this reply arrives every
    // earlier deletion has been recorded.
    async fn confirm_identity(&mut self, target: &InterfaceId) -> Result<(), BackendError> {
        let stale = || {
            BackendError::new(
                BackendErrorKind::StaleState,
                "target identity is no longer current",
            )
        };
        if target.namespace != self.namespace {
            return Err(stale());
        }
        let mut message = LinkMessage::default();
        message.header.index = target.index;
        let reply = self
            .request(RouteNetlinkMessage::GetLink(message), Exchange::Read, 0)
            .await?;
        self.drain_notifications()?;
        let name = match reply.as_slice() {
            [packet] if packet.kind == RTM_NEWLINK => parse_link(&packet.body)?.name,
            _ => {
                return Err(BackendError::new(
                    BackendErrorKind::QueryFailed,
                    "unexpected reply to a link query",
                ));
            }
        };
        if self.generations.get(&target.index) != Some(&target.generation) || name != target.name {
            return Err(stale());
        }
        Ok(())
    }

    async fn wireless_indices(&mut self) -> Result<HashSet<u32>, String> {
        let handle = self.wireless.as_mut().ok_or(
            "wireless identification unavailable; unclassified Ethernet link-layer devices are read-only",
        )?;
        tokio::time::timeout(OPTIONAL_TIMEOUT, async {
            use wl_nl80211::packet_generic::GenlMessage;
            use wl_nl80211::{Nl80211Attr, Nl80211Command, Nl80211Message};
            let payload = Nl80211Message {
                cmd: Nl80211Command::GetInterface,
                attributes: Vec::new(),
            };
            let mut message = NetlinkMessage::from(GenlMessage::from_payload(payload));
            message.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
            let mut responses = match handle.handle.request(message).await {
                Ok(responses) => responses,
                Err(genetlink::GenetlinkError::NetlinkError(error))
                    if error.raw_os_error() == Some(libc::ENOENT) =>
                {
                    return Ok(HashSet::new());
                }
                Err(error) => return Err(format!("wireless identification failed: {error}")),
            };
            let mut indices = HashSet::new();
            while let Some(response) = responses.next().await {
                let response =
                    response.map_err(|error| format!("invalid wireless inventory: {error}"))?;
                if response.header.flags & NLM_F_DUMP_INTR != 0 {
                    return Err("wireless inventory interrupted".into());
                }
                match response.payload {
                    NetlinkPayload::InnerMessage(message) => {
                        for attribute in message.payload.attributes {
                            if let Nl80211Attr::IfIndex(index) = attribute {
                                indices.insert(index);
                            }
                        }
                    }
                    NetlinkPayload::Done(done) if done.code == 0 => return Ok(indices),
                    NetlinkPayload::Error(error) if error.code.is_some() => {
                        return Err(format!("wireless inventory failed: {error}"));
                    }
                    NetlinkPayload::Noop => {}
                    _ => return Err("unexpected wireless inventory response".into()),
                }
            }
            Err("wireless inventory closed without completion".into())
        })
        .await
        .map_err(|_| "wireless identification timed out".to_string())?
    }
}

fn renamed_during_inventory() -> BackendError {
    BackendError::new(
        BackendErrorKind::QueryFailed,
        "interface renamed during inventory; query again",
    )
}

async fn ethernet_attributes(
    mut handle: ethtool::EthtoolHandle,
    name: String,
    index: u32,
) -> Result<(IndexMap<String, Value>, Vec<String>), BackendError> {
    use ethtool::{EthtoolAttr, EthtoolHeader, EthtoolLinkModeAttr};
    let mut fields = IndexMap::new();
    let mut diagnostics = Vec::new();
    let result = tokio::time::timeout(OPTIONAL_TIMEOUT, async {
        let mut stream = handle.link_mode().get(Some(&name)).execute().await?;
        let mut received = false;
        while let Some(message) = stream.try_next().await? {
            received = true;
            for attribute in message.payload.nlas {
                match attribute {
                    EthtoolAttr::LinkMode(EthtoolLinkModeAttr::Header(headers)) => {
                        let other_device = |header: &EthtoolHeader| {
                            matches!(header, EthtoolHeader::DevIndex(other) if *other != index)
                        };
                        if headers.iter().any(other_device) {
                            return Ok(None);
                        }
                    }
                    EthtoolAttr::LinkMode(attribute) => {
                        enrich_ethernet(attribute, &mut fields, &mut diagnostics);
                    }
                    _ => {}
                }
            }
        }
        Ok::<_, ethtool::EthtoolError>(Some(received))
    })
    .await;
    match result {
        Ok(Ok(Some(true))) => {}
        Ok(Ok(Some(false))) => diagnostics.push("ethtool returned no link-mode observation".into()),
        Ok(Ok(None)) => return Err(renamed_during_inventory()),
        Ok(Err(ethtool::EthtoolError::NetlinkError(error)))
            if matches!(
                error.raw_code().abs(),
                libc::EOPNOTSUPP | libc::ENODEV | libc::ENOENT
            ) => {}
        Ok(Err(error)) => diagnostics.push(format!("ethtool link-mode query failed: {error}")),
        Err(_) => diagnostics.push("ethtool link-mode query timed out".into()),
    }
    Ok((fields, diagnostics))
}

async fn collect_response<S>(
    stream: &mut S,
    exchange: Exchange,
) -> Result<Vec<RoutePacket>, BackendError>
where
    S: Stream<Item = NetlinkMessage<RoutePacket>> + Unpin,
{
    let is_dump = exchange == Exchange::Dump;
    let mut packets = Vec::new();
    while let Some(message) = stream.next().await {
        if message.header.flags & NLM_F_DUMP_INTR != 0 {
            return Err(BackendError::new(
                exchange.rejected(),
                "kernel interrupted the netlink dump",
            ));
        }
        match message.payload {
            NetlinkPayload::InnerMessage(packet) => packets.push(packet),
            NetlinkPayload::Error(error) => {
                if error.code.is_some() {
                    return Err(netlink_error(
                        exchange.rejected(),
                        &error,
                        message.header.flags,
                    ));
                }
                if !is_dump {
                    return Ok(packets);
                }
            }
            NetlinkPayload::Done(done) if is_dump => {
                if done.code != 0 {
                    let mut error = BackendError::from_io(
                        exchange.rejected(),
                        io::Error::from_raw_os_error(done.code.abs()),
                    );
                    error.extack = extack_message(&done.extended_ack);
                    return Err(error);
                }
                return Ok(packets);
            }
            NetlinkPayload::Noop => {}
            _ => {
                return Err(BackendError::new(
                    exchange.unconfirmed(),
                    "unexpected netlink response",
                ));
            }
        }
    }
    Err(BackendError::new(
        exchange.unconfirmed(),
        "netlink transport closed before confirmed completion",
    ))
}

struct ParsedLink {
    message: LinkMessage,
    name: String,
    kind_valid: bool,
    diagnostics: Vec<(String, String)>,
}

fn parse_link(body: &[u8]) -> Result<ParsedLink, BackendError> {
    let header = LinkHeader::parse(body).map_err(|error| {
        BackendError::new(
            BackendErrorKind::QueryFailed,
            format!("invalid link header: {error}"),
        )
    })?;
    let attributes = &body[header.buffer_len()..];
    let mut message = LinkMessage::default();
    message.header = header;
    let mut diagnostics = Vec::new();
    let mut kind_valid = true;
    for nla in NlasIterator::new(attributes) {
        let nla = nla.map_err(|error| {
            BackendError::new(
                BackendErrorKind::QueryFailed,
                format!("invalid link attribute framing: {error}"),
            )
        })?;
        match LinkAttribute::parse_with_param(&nla, message.header.interface_family) {
            Ok(LinkAttribute::Address(bytes))
                if matches!(
                    message.header.link_layer_type,
                    LinkLayerType::Ether | LinkLayerType::Loopback
                ) && bytes.len() != 6 =>
            {
                diagnostics.push((
                    "mac".into(),
                    format!("invalid Ethernet MAC length {}", bytes.len()),
                ))
            }
            Ok(LinkAttribute::Carrier(value)) if value > 1 => {
                diagnostics.push(("carrier".into(), format!("invalid carrier value {value}")))
            }
            Ok(attribute) => message.attributes.push(attribute),
            Err(error) => {
                if nla.kind() == libc::IFLA_LINKINFO {
                    kind_valid = false;
                }
                diagnostics.push((
                    format!("netlink.attribute.{}", nla.kind()),
                    error.to_string(),
                ));
            }
        }
    }
    let name = message
        .attributes
        .iter()
        .find_map(|attribute| {
            if let LinkAttribute::IfName(name) = attribute {
                Some(name.clone())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            BackendError::new(
                BackendErrorKind::QueryFailed,
                "link has no valid interface name",
            )
        })?;
    Ok(ParsedLink {
        message,
        name,
        kind_valid,
        diagnostics,
    })
}

fn classify_link(link: &ParsedLink, wireless: Option<&HashSet<u32>>) -> String {
    if !link.kind_valid {
        return "unknown".into();
    }
    for attribute in &link.message.attributes {
        if let LinkAttribute::LinkInfo(information) = attribute {
            for info in information {
                if let LinkInfo::Kind(kind) = info {
                    return match kind {
                        InfoKind::Veth => ETHERNET.into(),
                        kind => kind.to_string(),
                    };
                }
            }
        }
    }
    if link.message.header.link_layer_type == LinkLayerType::Loopback {
        return "loopback".into();
    }
    if wireless.is_some_and(|indices| indices.contains(&link.message.header.index)) {
        return "wifi".into();
    }
    if link.message.header.link_layer_type == LinkLayerType::Ether && wireless.is_some() {
        return ETHERNET.into();
    }
    "unknown".into()
}

fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn link_state(link: &ParsedLink, device_type: &str) -> (State, Option<String>) {
    let mut state = State::new(Source::Kernel);
    state.device_type = device_type.into();
    state.match_spec.name = Some(link.name.clone());
    state.match_spec.r#type = Some(device_type.into());
    state
        .fields
        .insert("name".into(), Value::String(link.name.clone()));
    state.fields.insert(
        "enabled".into(),
        Value::Bool(link.message.header.flags.contains(LinkFlags::Up)),
    );
    let mut mac = None;
    for attribute in &link.message.attributes {
        match attribute {
            LinkAttribute::Mtu(mtu) => {
                state
                    .fields
                    .insert("mtu".into(), Value::U64(u64::from(*mtu)));
            }
            LinkAttribute::Carrier(carrier) => {
                state
                    .fields
                    .insert("carrier".into(), Value::Bool(*carrier == 1));
            }
            LinkAttribute::Address(bytes) if !bytes.is_empty() => {
                let text = format_mac(bytes);
                state
                    .fields
                    .insert("mac".into(), Value::String(text.clone()));
                mac = Some(text);
            }
            _ => {}
        }
    }
    state.match_spec.mac = mac.clone();
    (state, mac)
}

fn parse_address(message: &AddressMessage) -> Result<AddressObservation, String> {
    if message.header.family != AddressFamily::Inet || message.header.prefix_len > 32 {
        return Err("invalid IPv4 family or prefix length".into());
    }
    let mut local = None;
    let mut address = None;
    let mut flags = u32::from(message.header.flags.bits());
    let mut cache = None;
    for attribute in &message.attributes {
        match attribute {
            AddressAttribute::Local(IpAddr::V4(ip)) => local = Some(*ip),
            AddressAttribute::Address(IpAddr::V4(ip)) => address = Some(*ip),
            AddressAttribute::Local(_) | AddressAttribute::Address(_) => {
                return Err("IPv6 attribute in IPv4 address record".into());
            }
            AddressAttribute::Flags(value) => flags = value.bits(),
            AddressAttribute::CacheInfo(value) => cache = Some(value),
            _ => {}
        }
    }
    let ip = local
        .or(address)
        .ok_or("address record has no assigned local address")?;
    let cache = cache.ok_or("address record has no lifetime observation")?;
    if cache.ifa_preferred > cache.ifa_valid {
        return Err("preferred lifetime exceeds valid lifetime".into());
    }
    Ok(AddressObservation {
        address: Ipv4Address {
            ip,
            prefix_len: message.header.prefix_len,
            valid_lft: Some(cache.ifa_valid),
            preferred_lft: Some(cache.ifa_preferred),
        },
        flags,
        scope: message.header.scope.into(),
        peer: address.filter(|address| *address != ip),
    })
}

fn cache_info(address: &Ipv4Address) -> AddressAttribute {
    let (valid, preferred) = address.lifetimes();
    let mut cache = CacheInfo::default();
    cache.ifa_valid = valid;
    cache.ifa_preferred = preferred;
    AddressAttribute::CacheInfo(cache)
}

fn address_message(index: u32, address: &Ipv4Address, include_lifetimes: bool) -> AddressMessage {
    let mut message = AddressMessage::default();
    message.header.index = index;
    message.header.family = AddressFamily::Inet;
    message.header.prefix_len = address.prefix_len;
    message
        .attributes
        .push(AddressAttribute::Local(IpAddr::V4(address.ip)));
    message
        .attributes
        .push(AddressAttribute::Address(IpAddr::V4(address.ip)));
    if include_lifetimes {
        message.attributes.push(cache_info(address));
    }
    message
}

fn replacement_message(
    observed: Option<&AddressMessage>,
    index: u32,
    address: &Ipv4Address,
) -> AddressMessage {
    let Some(observed) = observed else {
        return address_message(index, address, true);
    };
    let mut message = observed.clone();
    message
        .attributes
        .retain(|attribute| !matches!(attribute, AddressAttribute::CacheInfo(_)));
    message.attributes.push(cache_info(address));
    message
}

// The kernel matches the peer too, so remove by the observed record.
fn removal_message(
    observed: Option<&AddressMessage>,
    index: u32,
    address: &Ipv4Address,
) -> AddressMessage {
    let mut message = observed
        .cloned()
        .unwrap_or_else(|| address_message(index, address, false));
    message.attributes.retain(|attribute| {
        matches!(
            attribute,
            AddressAttribute::Local(_) | AddressAttribute::Address(_) | AddressAttribute::Label(_)
        )
    });
    message
}

fn address_value(observation: &AddressObservation) -> Value {
    let address = &observation.address;
    let mut fields = IndexMap::from([(
        "ip".into(),
        Value::IpNetwork((IpAddr::V4(address.ip), address.prefix_len)),
    )]);
    if let Some(lifetime) = address.valid_lft {
        fields.insert("valid_lft".into(), Value::U64(u64::from(lifetime)));
    }
    if let Some(lifetime) = address.preferred_lft {
        fields.insert("preferred_lft".into(), Value::U64(u64::from(lifetime)));
    }
    Value::Map(fields)
}

fn enrich_ethernet(
    attribute: ethtool::EthtoolLinkModeAttr,
    fields: &mut IndexMap<String, Value>,
    diagnostics: &mut Vec<String>,
) {
    use ethtool::{EthtoolLinkModeAttr as Attr, EthtoolLinkModeDuplex as Duplex};
    match attribute {
        Attr::Speed(speed) if speed != u32::MAX && speed != 0 => {
            fields.insert("speed".into(), Value::U64(u64::from(speed)));
        }
        Attr::Autoneg(enabled) => {
            fields.insert("autoneg".into(), Value::Bool(enabled));
        }
        Attr::Duplex(duplex) => {
            let value = match duplex {
                Duplex::Full => "full",
                Duplex::Half => "half",
                Duplex::Unknown => "unknown",
                Duplex::Other(value) => {
                    diagnostics.push(format!("invalid duplex value {value}"));
                    return;
                }
            };
            fields.insert("duplex".into(), Value::String(value.into()));
        }
        _ => {}
    }
}

fn diagnostic(target: Option<InterfaceId>, field: &str, message: String) -> Diagnostic {
    Diagnostic {
        target,
        field: Some(field.into()),
        message,
    }
}

fn fresh_generation() -> Result<u64, BackendError> {
    let mut bytes = [0u8; 8];
    // SAFETY: bytes is a writable buffer of the length passed.
    let written = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if written != bytes.len() as isize {
        return Err(BackendError::from_io(
            BackendErrorKind::Internal,
            io::Error::last_os_error(),
        ));
    }
    Ok(u64::from_ne_bytes(bytes))
}

fn deleted_link(packet: &RoutePacket) -> Result<Option<u32>, DecodeError> {
    if packet.kind != RTM_DELLINK {
        return Ok(None);
    }
    let header = LinkHeader::parse(&packet.body)?;
    // AF_BRIDGE RTM_DELLINK reports a port leaving its bridge, not a deletion.
    Ok((header.interface_family == AddressFamily::Unspec).then_some(header.index))
}

fn record_deletion(generations: &mut HashMap<u32, u64>, epoch: &mut u64, index: u32) {
    generations.remove(&index);
    *epoch = epoch.wrapping_add(1);
}

fn validate_deletion_epoch(before: u64, after: u64) -> Result<(), BackendError> {
    if before != after {
        return Err(BackendError::new(
            BackendErrorKind::QueryFailed,
            "interface deletion occurred during inventory; query again",
        ));
    }
    Ok(())
}

fn netlink_error(
    fallback: BackendErrorKind,
    error: &rtnetlink::packet_core::ErrorMessage,
    flags: u16,
) -> BackendError {
    let mut result = BackendError::from_io(fallback, error.to_io());
    if flags & NLM_F_ACK_TLVS != 0 {
        // A capped error omits the request payload after its 16-byte header.
        let offset = if flags & NLM_F_CAPPED != 0 {
            16
        } else {
            error
                .header
                .get(..4)
                .map(|bytes| u32::from_ne_bytes(bytes.try_into().expect("four bytes")) as usize)
                .unwrap_or(0)
        };
        if let Some(attributes) = align(offset).and_then(|start| error.header.get(start..)) {
            result.extack = extack_message(attributes);
        }
    }
    result
}

fn extack_message(attributes: &[u8]) -> Option<String> {
    NlasIterator::new(attributes)
        .map_while(Result::ok)
        .find(|attribute| attribute.kind() == NLMSGERR_ATTR_MSG)
        .and_then(|attribute| c_string(attribute.value()).ok().flatten())
}

#[repr(C)]
struct DriverInfo {
    command: u32,
    driver: [u8; 32],
    version: [u8; 32],
    firmware_version: [u8; 32],
    bus_info: [u8; 32],
    erom_version: [u8; 32],
    reserved: [u8; 12],
    private_flags: u32,
    statistics: u32,
    testinfo_length: u32,
    eeprom_length: u32,
    regdump_length: u32,
}

fn ifreq_for(name: &str) -> io::Result<libc::ifreq> {
    if name.len() >= libc::IFNAMSIZ || name.as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid interface name",
        ));
    }
    // SAFETY: ifreq consists of integers, byte arrays and a union of the same; all-zero is valid.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (destination, source) in request.ifr_name.iter_mut().zip(name.bytes()) {
        *destination = source as libc::c_char;
    }
    Ok(request)
}

fn interface_index(socket: libc::c_int, name: &str) -> io::Result<u32> {
    let mut request = ifreq_for(name)?;
    // SAFETY: request is a live, NUL-terminated ifreq; SIOCGIFINDEX writes only its union.
    if unsafe { libc::ioctl(socket, libc::SIOCGIFINDEX, &mut request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: SIOCGIFINDEX succeeded and set ifru_ifindex.
    Ok(unsafe { request.ifr_ifru.ifru_ifindex } as u32)
}

fn while_named<T>(
    socket: libc::c_int,
    name: &str,
    index: u32,
    call: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let check = || match interface_index(socket, name) {
        Ok(current) if current == index => Ok(()),
        Ok(_) => Err(io::ErrorKind::Interrupted.into()),
        Err(error) if error.raw_os_error() == Some(libc::ENODEV) => {
            Err(io::ErrorKind::Interrupted.into())
        }
        Err(error) => Err(error),
    };
    check()?;
    let result = call()?;
    check()?;
    Ok(result)
}

fn driver_info(socket: libc::c_int, name: &str) -> io::Result<(Option<String>, Option<String>)> {
    let mut request = ifreq_for(name)?;
    // SAFETY: DriverInfo consists entirely of integers and byte arrays.
    let mut info: DriverInfo = unsafe { std::mem::zeroed() };
    info.command = ETHTOOL_GDRVINFO;
    request.ifr_ifru.ifru_data = std::ptr::from_mut(&mut info).cast();
    // SAFETY: request points to a live ifreq and its data points to the correctly sized DriverInfo.
    if unsafe { libc::ioctl(socket, libc::SIOCETHTOOL, &mut request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let driver = c_string(&info.driver)?;
    let bus = c_string(&info.bus_info)?;
    let pci_path = bus.filter(|bus| is_pci_address(bus));
    Ok((driver, pci_path))
}

fn c_string(bytes: &[u8]) -> io::Result<Option<String>> {
    let text = CStr::from_bytes_until_nul(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .to_str()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

fn is_pci_address(value: &str) -> bool {
    value.len() == 12
        && value.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b':',
            10 => byte == b'.',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn unsupported_ioctl(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::ENODEV | libc::ENOTTY)
    )
}

fn legacy_wireless(socket: libc::c_int, name: &str) -> bool {
    let Ok(mut request) = ifreq_for(name) else {
        return false;
    };
    // SAFETY: SIOCGIWNAME writes only the interface-name and union storage of
    // request, which is at least as large as iwreq.
    unsafe { libc::ioctl(socket, libc::SIOCGIWNAME, &mut request) == 0 }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroI32;

    use rtnetlink::packet_core::{DoneMessage, ErrorMessage};
    use rtnetlink::packet_route::address::{AddressHeaderFlags, AddressScope};

    use super::*;

    fn link(kind: Option<InfoKind>) -> ParsedLink {
        let mut message = LinkMessage::default();
        message.header.index = 3;
        message.header.link_layer_type = LinkLayerType::Ether;
        message
            .attributes
            .push(LinkAttribute::IfName("test0".into()));
        if let Some(kind) = kind {
            message
                .attributes
                .push(LinkAttribute::LinkInfo(vec![LinkInfo::Kind(kind)]));
        }
        ParsedLink {
            message,
            name: "test0".into(),
            kind_valid: true,
            diagnostics: Vec::new(),
        }
    }

    fn emitted(message: &LinkMessage) -> Vec<u8> {
        let mut body = vec![0; message.buffer_len()];
        message.emit(&mut body);
        body
    }

    fn attribute(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(4 + payload.len() as u16).to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(payload);
        bytes.resize(align(bytes.len()).unwrap(), 0);
        bytes
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn stream(
        payloads: Vec<NetlinkPayload<RoutePacket>>,
    ) -> impl Stream<Item = NetlinkMessage<RoutePacket>> + Unpin {
        futures_util::stream::iter(
            payloads
                .into_iter()
                .map(|payload| NetlinkMessage::new(NetlinkHeader::default(), payload)),
        )
    }

    async fn collect(
        payloads: Vec<NetlinkPayload<RoutePacket>>,
        exchange: Exchange,
    ) -> Result<Vec<RoutePacket>, BackendError> {
        collect_response(&mut stream(payloads), exchange).await
    }

    fn packet(kind: u16) -> NetlinkPayload<RoutePacket> {
        NetlinkPayload::InnerMessage(RoutePacket {
            kind,
            body: Vec::new(),
        })
    }

    fn ack() -> NetlinkPayload<RoutePacket> {
        NetlinkPayload::Error(ErrorMessage::default())
    }

    fn ipv4(last: u8, prefix_len: u8) -> Ipv4Address {
        Ipv4Address {
            ip: Ipv4Addr::new(192, 0, 2, last),
            prefix_len,
            valid_lft: None,
            preferred_lft: None,
        }
    }

    #[test]
    fn classification_preserves_virtual_kinds_and_rejects_wireless() {
        let empty = HashSet::new();
        assert_eq!(classify_link(&link(Some(InfoKind::Veth)), None), ETHERNET);
        for (kind, expected) in [
            (InfoKind::Bridge, "bridge"),
            (InfoKind::Bond, "bond"),
            (InfoKind::Vlan, "vlan"),
            (InfoKind::Dummy, "dummy"),
        ] {
            assert_eq!(classify_link(&link(Some(kind)), Some(&empty)), expected);
        }
        assert_eq!(
            classify_link(&link(None), Some(&HashSet::from([3]))),
            "wifi"
        );
        assert_eq!(classify_link(&link(None), None), "unknown");
        assert_eq!(classify_link(&link(None), Some(&empty)), ETHERNET);
        let mut loopback = link(None);
        loopback.message.header.link_layer_type = LinkLayerType::Loopback;
        assert_eq!(classify_link(&loopback, Some(&empty)), "loopback");
        let mut invalid = link(Some(InfoKind::Veth));
        invalid.kind_valid = false;
        assert_eq!(classify_link(&invalid, Some(&empty)), "unknown");
    }

    #[test]
    fn link_state_reports_base_fields_and_formats_the_mac() {
        let mut parsed = link(Some(InfoKind::Veth));
        parsed.message.header.flags = LinkFlags::Up;
        parsed.message.attributes.extend([
            LinkAttribute::Mtu(1400),
            LinkAttribute::Carrier(0),
            LinkAttribute::Address(vec![0x02, 0, 0, 0, 0, 0x0a]),
        ]);
        let (state, mac) = link_state(&parsed, ETHERNET);
        assert_eq!(mac.as_deref(), Some("02:00:00:00:00:0a"));
        assert_eq!(state.match_spec.mac, mac);
        assert_eq!(state.match_spec.name.as_deref(), Some("test0"));
        assert_eq!(state.match_spec.r#type.as_deref(), Some(ETHERNET));
        assert_eq!(state.fields["mtu"], Value::U64(1400));
        assert_eq!(state.fields["enabled"], Value::Bool(true));
        assert_eq!(state.fields["carrier"], Value::Bool(false));
        assert_eq!(state.fields["name"], Value::String("test0".into()));

        let (state, mac) = link_state(&link(None), "unknown");
        assert_eq!(mac, None);
        assert_eq!(state.fields["enabled"], Value::Bool(false));
        assert!(!state.fields.contains_key("mtu"));
    }

    #[test]
    fn link_parsing_reports_invalid_attributes_and_rejects_unusable_links() {
        let mut message = link(None).message;
        message.attributes.extend([
            LinkAttribute::Address(vec![1, 2, 3, 4]),
            LinkAttribute::Carrier(2),
        ]);
        let parsed = parse_link(&emitted(&message)).unwrap();
        let fields: Vec<_> = parsed
            .diagnostics
            .iter()
            .map(|(field, _)| field.as_str())
            .collect();
        assert_eq!(fields, ["mac", "carrier"]);
        assert!(!parsed.message.attributes.iter().any(|attribute| matches!(
            attribute,
            LinkAttribute::Address(_) | LinkAttribute::Carrier(_)
        )));

        let mut body = emitted(&link(None).message);
        body.extend(attribute(libc::IFLA_LINKINFO, &[0xff, 0xff, 0, 0]));
        let parsed = parse_link(&body).unwrap();
        assert!(!parsed.kind_valid);
        assert_eq!(parsed.diagnostics[0].0, "netlink.attribute.18");
        assert_eq!(classify_link(&parsed, Some(&HashSet::new())), "unknown");

        assert_eq!(
            parse_link(&[1, 2, 3]).err().unwrap().kind,
            BackendErrorKind::QueryFailed
        );
        let mut unnamed = link(None).message;
        unnamed.attributes.clear();
        assert_eq!(
            parse_link(&emitted(&unnamed)).err().unwrap().kind,
            BackendErrorKind::QueryFailed
        );
        let mut truncated = emitted(&link(None).message);
        truncated.extend_from_slice(&[64, 0, 3, 0]);
        assert_eq!(
            parse_link(&truncated).err().unwrap().kind,
            BackendErrorKind::QueryFailed
        );
    }

    #[test]
    fn optional_invalid_attribute_preserves_link() {
        let mut body = emitted(&link(Some(InfoKind::Veth)).message);
        // IFLA_MTU with a one-byte payload.
        body.extend(attribute(4, &[1]));
        let parsed = parse_link(&body).unwrap();
        assert_eq!(parsed.name, "test0");
        assert_eq!(
            parsed
                .diagnostics
                .iter()
                .map(|(field, _)| field.as_str())
                .collect::<Vec<_>>(),
            ["netlink.attribute.4"]
        );
        assert_eq!(classify_link(&parsed, None), ETHERNET);
    }

    #[test]
    fn non_ethernet_hardware_address_is_preserved() {
        let mut message = link(None).message;
        message.header.link_layer_type = LinkLayerType::Infiniband;
        message.attributes.push(LinkAttribute::Address(vec![1; 20]));
        let parsed = parse_link(&emitted(&message)).unwrap();
        assert!(parsed.diagnostics.is_empty());
        assert!(parsed.message.attributes.iter().any(
            |attribute| matches!(attribute, LinkAttribute::Address(bytes) if bytes.len() == 20)
        ));
    }

    #[test]
    fn assigned_ipv4_keeps_secondary_lifetimes_and_uses_local() {
        let address = Ipv4Address {
            valid_lft: Some(300),
            preferred_lft: Some(120),
            ..ipv4(7, 16)
        };
        let mut message = address_message(3, &address, true);
        message.header.flags = AddressHeaderFlags::Secondary;
        message
            .attributes
            .retain(|attribute| !matches!(attribute, AddressAttribute::Address(_)));
        message.attributes.extend([
            AddressAttribute::Address(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8))),
            AddressAttribute::Broadcast(Ipv4Addr::new(192, 0, 255, 255)),
        ]);
        let observed = parse_address(&message).unwrap();
        assert_eq!(observed.address, address);
        assert_eq!(observed.flags, libc::IFA_F_SECONDARY);
        assert_eq!(observed.peer, Some(Ipv4Addr::new(192, 0, 2, 8)));
    }

    #[test]
    fn address_parsing_falls_back_to_the_address_attribute_and_omits_a_self_peer() {
        let mut message = address_message(3, &ipv4(7, 24), true);
        assert_eq!(parse_address(&message).unwrap().peer, None);
        message
            .attributes
            .retain(|attribute| !matches!(attribute, AddressAttribute::Local(_)));
        let observed = parse_address(&message).unwrap();
        assert_eq!(observed.address.ip, Ipv4Addr::new(192, 0, 2, 7));
        assert_eq!(observed.peer, None);
    }

    #[test]
    fn missing_or_invalid_address_data_is_not_an_empty_observation() {
        let mut message = AddressMessage::default();
        message.header.family = AddressFamily::Inet;
        assert!(parse_address(&message).is_err());
        message
            .attributes
            .push(AddressAttribute::Local(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(parse_address(&message).is_err());
        message
            .attributes
            .push(AddressAttribute::CacheInfo(CacheInfo::default()));
        assert!(parse_address(&message).is_ok());

        let mut long = message.clone();
        long.header.prefix_len = 33;
        let mut inet6 = message.clone();
        inet6.header.family = AddressFamily::Inet6;
        let mut mixed = message.clone();
        mixed.attributes.push(AddressAttribute::Address(IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST,
        )));
        let mut inverted = message.clone();
        let mut cache = CacheInfo::default();
        cache.ifa_valid = 10;
        cache.ifa_preferred = 11;
        inverted.attributes.push(AddressAttribute::CacheInfo(cache));
        for invalid in [long, inet6, mixed, inverted] {
            assert!(parse_address(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn address_messages_carry_resolved_lifetimes_only_when_requested() {
        let mut address = ipv4(1, 24);
        address.valid_lft = Some(300);
        let without = address_message(5, &address, false);
        assert!(
            !without
                .attributes
                .iter()
                .any(|attribute| matches!(attribute, AddressAttribute::CacheInfo(_)))
        );
        let with = address_message(5, &address, true);
        let observed = parse_address(&with).unwrap();
        assert_eq!(observed.address.valid_lft, Some(300));
        assert_eq!(observed.address.preferred_lft, Some(300));
        assert_eq!(with.header.index, 5);
        assert_eq!(replacement_message(None, 5, &address), with);
    }

    #[test]
    fn replacement_retains_observed_peer_scope_and_flags() {
        let address = Ipv4Address {
            valid_lft: Some(300),
            preferred_lft: Some(200),
            ..ipv4(1, 24)
        };
        let mut observed = address_message(7, &address, true);
        observed.header.scope = AddressScope::Link;
        observed.header.flags = AddressHeaderFlags::Secondary;
        observed
            .attributes
            .retain(|attribute| !matches!(attribute, AddressAttribute::Address(_)));
        observed
            .attributes
            .push(AddressAttribute::Address(IpAddr::V4(Ipv4Addr::new(
                192, 0, 2, 8,
            ))));
        let desired = Ipv4Address {
            valid_lft: Some(400),
            preferred_lft: Some(350),
            ..address
        };
        let parsed = parse_address(&replacement_message(Some(&observed), 7, &desired)).unwrap();
        assert_eq!(parsed.address, desired);
        assert_eq!(parsed.scope, libc::RT_SCOPE_LINK);
        assert_eq!(parsed.flags, libc::IFA_F_SECONDARY);
        assert_eq!(parsed.peer, Some(Ipv4Addr::new(192, 0, 2, 8)));
    }

    #[test]
    fn removal_names_the_observed_address_without_its_attributes() {
        let address = ipv4(1, 24);
        let mut observed = address_message(7, &address, true);
        observed.attributes.extend([
            AddressAttribute::Label("veth0:1".into()),
            AddressAttribute::Flags(rtnetlink::packet_route::address::AddressFlags::Permanent),
        ]);
        let removal = removal_message(Some(&observed), 7, &address);
        assert_eq!(
            removal.attributes,
            [
                AddressAttribute::Local(IpAddr::V4(address.ip)),
                AddressAttribute::Address(IpAddr::V4(address.ip)),
                AddressAttribute::Label("veth0:1".into()),
            ]
        );
        assert_eq!(
            removal_message(None, 7, &address),
            address_message(7, &address, false)
        );
    }

    #[test]
    fn unknown_ethtool_values_do_not_invent_speed_or_duplex() {
        let mut fields = IndexMap::new();
        let mut diagnostics = Vec::new();
        enrich_ethernet(
            ethtool::EthtoolLinkModeAttr::Speed(u32::MAX),
            &mut fields,
            &mut diagnostics,
        );
        enrich_ethernet(
            ethtool::EthtoolLinkModeAttr::Duplex(ethtool::EthtoolLinkModeDuplex::Other(7)),
            &mut fields,
            &mut diagnostics,
        );
        assert!(fields.is_empty());
        assert_eq!(diagnostics.len(), 1);
        enrich_ethernet(
            ethtool::EthtoolLinkModeAttr::Autoneg(false),
            &mut fields,
            &mut diagnostics,
        );
        enrich_ethernet(
            ethtool::EthtoolLinkModeAttr::Speed(10_000),
            &mut fields,
            &mut diagnostics,
        );
        assert_eq!(fields.get("autoneg"), Some(&Value::Bool(false)));
        assert_eq!(fields.get("speed"), Some(&Value::U64(10_000)));
    }

    #[test]
    fn strict_framing_rejects_truncated_datagrams() {
        assert!(StrictCodec::decode::<RoutePacket>(&mut BytesMut::from(&[1, 2, 3][..])).is_err());
        let mut oversized = vec![0; 16];
        oversized[..4].copy_from_slice(&64u32.to_ne_bytes());
        assert!(
            StrictCodec::decode::<RoutePacket>(&mut BytesMut::from(oversized.as_slice())).is_err()
        );
        assert_eq!(align(17), Some(20));
        assert_eq!(align(usize::MAX), None);
    }

    #[test]
    fn strict_framing_consumes_alignment_between_messages() {
        let mut first = NetlinkMessage::new(
            NetlinkHeader::default(),
            NetlinkPayload::InnerMessage(RoutePacket {
                kind: RTM_NEWLINK,
                body: vec![9],
            }),
        );
        first.finalize();
        let mut done = NetlinkMessage::<RoutePacket>::new(
            NetlinkHeader::default(),
            NetlinkPayload::Done(DoneMessage::default()),
        );
        done.finalize();
        let mut bytes = vec![0; 20 + done.buffer_len()];
        first.serialize(&mut bytes[..first.buffer_len()]);
        done.serialize(&mut bytes[20..]);
        let mut datagram = BytesMut::from(bytes.as_slice());
        assert!(matches!(
            StrictCodec::decode::<RoutePacket>(&mut datagram)
                .unwrap()
                .unwrap()
                .payload,
            NetlinkPayload::InnerMessage(_)
        ));
        assert!(matches!(
            StrictCodec::decode::<RoutePacket>(&mut datagram)
                .unwrap()
                .unwrap()
                .payload,
            NetlinkPayload::Done(_)
        ));
        assert!(datagram.is_empty());
        let mut unpadded = vec![0; first.buffer_len()];
        first.serialize(&mut unpadded);
        assert!(
            StrictCodec::decode::<RoutePacket>(&mut BytesMut::from(unpadded.as_slice()))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn os_errors_are_classified_by_errno() {
        for (errno, kind) in [
            (libc::EPERM, BackendErrorKind::PermissionDenied),
            (libc::EACCES, BackendErrorKind::PermissionDenied),
            (libc::ENODEV, BackendErrorKind::NotFound),
            (libc::ENXIO, BackendErrorKind::NotFound),
            (libc::ENOENT, BackendErrorKind::NotFound),
            (libc::EBUSY, BackendErrorKind::ApplyFailed),
        ] {
            let error = BackendError::from_io(
                BackendErrorKind::ApplyFailed,
                io::Error::from_raw_os_error(errno),
            );
            assert_eq!((error.kind, error.errno), (kind, Some(errno)));
        }
    }

    #[test]
    fn extack_is_found_after_capped_and_uncapped_request_headers() {
        let extack = attribute(NLMSGERR_ATTR_MSG, b"denied\0");
        assert_eq!(extack_message(&extack).as_deref(), Some("denied"));

        let mut capped = ErrorMessage::default();
        capped.code = NonZeroI32::new(-libc::EPERM);
        capped.header = vec![0; 16];
        capped.header.extend_from_slice(&extack);
        let error = netlink_error(
            BackendErrorKind::ApplyFailed,
            &capped,
            NLM_F_ACK_TLVS | NLM_F_CAPPED,
        );
        assert_eq!(error.kind, BackendErrorKind::PermissionDenied);
        assert_eq!(error.errno, Some(libc::EPERM));
        assert_eq!(error.extack.as_deref(), Some("denied"));

        let mut uncapped = capped.clone();
        uncapped.header = 21u32.to_ne_bytes().to_vec();
        uncapped.header.resize(24, 0);
        uncapped.header.extend_from_slice(&extack);
        let error = netlink_error(BackendErrorKind::ApplyFailed, &uncapped, NLM_F_ACK_TLVS);
        assert_eq!(error.extack.as_deref(), Some("denied"));

        let error = netlink_error(BackendErrorKind::ApplyFailed, &capped, NLM_F_CAPPED);
        assert_eq!(error.extack, None);
    }

    #[test]
    fn response_completion_requires_ack_or_done() {
        block_on(async {
            for (exchange, kind) in [
                (Exchange::Write, BackendErrorKind::OutcomeUnknown),
                (Exchange::Read, BackendErrorKind::QueryFailed),
                (Exchange::Dump, BackendErrorKind::QueryFailed),
            ] {
                let result = collect(vec![packet(RTM_NEWADDR)], exchange);
                assert_eq!(result.await.err().unwrap().kind, kind, "{exchange:?}");
            }
            let read = collect(
                vec![NetlinkPayload::Noop, packet(RTM_NEWLINK), ack()],
                Exchange::Read,
            );
            assert_eq!(read.await.unwrap().len(), 1);
            let dump = collect(
                vec![
                    packet(RTM_NEWLINK),
                    ack(),
                    packet(RTM_NEWLINK),
                    NetlinkPayload::Done(DoneMessage::default()),
                ],
                Exchange::Dump,
            );
            assert_eq!(dump.await.unwrap().len(), 2);
            let unexpected = collect(
                vec![NetlinkPayload::Done(DoneMessage::default())],
                Exchange::Write,
            );
            assert_eq!(
                unexpected.await.err().unwrap().kind,
                BackendErrorKind::OutcomeUnknown
            );
        });
    }

    #[test]
    fn kernel_errors_and_interrupted_dumps_fail_the_request() {
        block_on(async {
            let mut nack = ErrorMessage::default();
            nack.code = NonZeroI32::new(-libc::EBUSY);
            let rejected = collect(vec![NetlinkPayload::Error(nack)], Exchange::Write)
                .await
                .err()
                .unwrap();
            assert_eq!(
                (rejected.kind, rejected.errno),
                (BackendErrorKind::ApplyFailed, Some(libc::EBUSY))
            );

            let mut done = DoneMessage::default();
            done.code = -libc::ENOBUFS;
            let failed = collect(vec![NetlinkPayload::Done(done)], Exchange::Dump)
                .await
                .err()
                .unwrap();
            assert_eq!(
                (failed.kind, failed.errno),
                (BackendErrorKind::QueryFailed, Some(libc::ENOBUFS))
            );

            let mut header = NetlinkHeader::default();
            header.flags = NLM_F_DUMP_INTR;
            let interrupted =
                NetlinkMessage::new(header, NetlinkPayload::Done(DoneMessage::default()));
            let error = collect_response(
                &mut futures_util::stream::iter([interrupted]),
                Exchange::Dump,
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.kind, BackendErrorKind::QueryFailed);
        });
    }

    #[test]
    fn exchanges_request_acknowledgement_or_a_dump() {
        assert_eq!(Exchange::Dump.flags(), NLM_F_REQUEST | NLM_F_DUMP);
        assert_eq!(Exchange::Read.flags(), NLM_F_REQUEST | NLM_F_ACK);
        assert_eq!(Exchange::Write.flags(), NLM_F_REQUEST | NLM_F_ACK);
    }

    #[test]
    fn only_unspecified_family_link_deletions_are_deletions() {
        let deletion = |family| {
            let mut message = LinkMessage::default();
            message.header.index = 7;
            message.header.interface_family = family;
            RoutePacket {
                kind: RTM_DELLINK,
                body: emitted(&message),
            }
        };
        assert_eq!(
            deleted_link(&deletion(AddressFamily::Unspec)).unwrap(),
            Some(7)
        );
        assert_eq!(
            deleted_link(&deletion(AddressFamily::Bridge)).unwrap(),
            None
        );
        let created = RoutePacket {
            kind: RTM_NEWLINK,
            ..deletion(AddressFamily::Unspec)
        };
        assert_eq!(deleted_link(&created).unwrap(), None);
        assert!(
            deleted_link(&RoutePacket {
                kind: RTM_DELLINK,
                body: vec![1, 2, 3],
            })
            .is_err()
        );
    }

    #[test]
    fn deletion_invalidates_the_identity_and_any_inventory_in_progress() {
        let mut generations = HashMap::from([(7, 123), (8, 456)]);
        let mut deletion_epoch = 0;
        let inventory_epoch = deletion_epoch;
        record_deletion(&mut generations, &mut deletion_epoch, 7);
        assert_eq!(generations, HashMap::from([(8, 456)]));
        assert_eq!(
            validate_deletion_epoch(inventory_epoch, deletion_epoch)
                .unwrap_err()
                .kind,
            BackendErrorKind::QueryFailed
        );
        assert!(validate_deletion_epoch(deletion_epoch, deletion_epoch).is_ok());
    }

    #[test]
    fn device_text_helpers_accept_only_well_formed_values() {
        assert_eq!(c_string(b"veth\0junk").unwrap().as_deref(), Some("veth"));
        assert_eq!(c_string(b"\0").unwrap(), None);
        assert!(c_string(&[0xff, 0]).is_err());
        assert!(c_string(b"unterminated").is_err());
        assert!(is_pci_address("0000:00:1f.6"));
        for invalid in [
            "0000:00:1f",
            "zzzz:00:1f.6",
            "0000-00:1f.6",
            "0000:00:1f.6 ",
        ] {
            assert!(!is_pci_address(invalid), "{invalid}");
        }
        assert!(ifreq_for("a".repeat(libc::IFNAMSIZ).as_str()).is_err());
        assert!(ifreq_for("bad\0name").is_err());
        assert!(ifreq_for("veth0").is_ok());
        assert_eq!(format_mac(&[0xde, 0xad, 0, 1]), "de:ad:00:01");
    }

    #[test]
    fn generations_are_random() {
        assert_ne!(fresh_generation().unwrap(), fresh_generation().unwrap());
    }
}
