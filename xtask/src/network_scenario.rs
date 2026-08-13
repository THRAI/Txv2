//! Versioned, external network scenarios for QEMU and real-board runners.
//!
//! The scenario is deliberately parsed through `toml::Value` instead of a
//! derived deserializer.  That keeps the accepted surface explicit: unknown
//! and missing fields are errors, so a typo cannot silently select a default
//! NIC, subnet, or placement.

use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;

use toml::value::Table;
use toml::Value;

pub(crate) const NETWORK_SCENARIO_SCHEMA_V1: &str = "tx.network-scenario.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScenarioTarget {
    QemuRv64,
    QemuLa64,
    BoardRv64,
    BoardLa64,
}

impl ScenarioTarget {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::QemuRv64 => "qemu-rv64",
            Self::QemuLa64 => "qemu-la64",
            Self::BoardRv64 => "board-rv64",
            Self::BoardLa64 => "board-la64",
        }
    }

    fn parse(value: &str) -> ScenarioResult<Self> {
        match value {
            "qemu-rv64" => Ok(Self::QemuRv64),
            "qemu-la64" => Ok(Self::QemuLa64),
            "board-rv64" => Ok(Self::BoardRv64),
            "board-la64" => Ok(Self::BoardLa64),
            _ => Err(ScenarioError::new(format!(
                "target: unsupported value {value:?}"
            ))),
        }
    }

    const fn is_qemu(self) -> bool {
        matches!(self, Self::QemuRv64 | Self::QemuLa64)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportKind {
    VirtioMmio,
    VirtioPci,
    Dwmac,
    BoardHook,
}

impl TransportKind {
    fn parse(value: &str) -> ScenarioResult<Self> {
        match value {
            "virtio-mmio" => Ok(Self::VirtioMmio),
            "virtio-pci" => Ok(Self::VirtioPci),
            "dwmac" => Ok(Self::Dwmac),
            "board-hook" => Ok(Self::BoardHook),
            _ => Err(ScenarioError::new(format!(
                "device.transport: unsupported value {value:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DeviceSelector {
    DeviceId(String),
    FirmwarePath(String),
    PciBdf(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NicPlacement {
    VirtioMmio { bus: String },
    VirtioPci { slot: u8, function: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryDeviceKind {
    VirtioRngMmio,
    VirtioRngPci,
    VirtioNetMmio,
    VirtioNetPci,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxiliaryDeviceOrder {
    BeforeNetwork,
    AfterNetwork,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuxiliaryDevice {
    pub(crate) id: String,
    pub(crate) kind: AuxiliaryDeviceKind,
    pub(crate) order: AuxiliaryDeviceOrder,
    pub(crate) placement: NicPlacement,
    pub(crate) backend: Option<NetworkBackend>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4Cidr {
    network: Ipv4Addr,
    prefix_len: u8,
}

impl Ipv4Cidr {
    pub(crate) const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    pub(crate) fn contains(self, address: Ipv4Addr) -> bool {
        let mask = prefix_mask(self.prefix_len);
        u32::from(address) & mask == u32::from(self.network)
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.network, self.prefix_len)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NetworkBackend {
    None,
    QemuUser {
        id: String,
        network: Ipv4Cidr,
        dhcp_start: Ipv4Addr,
        gateway: Ipv4Addr,
        dns: Ipv4Addr,
    },
    Tap {
        id: String,
        ifname: String,
    },
    Bridge {
        id: String,
        bridge: String,
    },
    BoardLink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GuestConfig {
    KernelDefault,
    Dhcp,
    Static {
        address: Ipv4Addr,
        prefix_len: u8,
        gateway: Option<Ipv4Addr>,
        dns: Option<Ipv4Addr>,
    },
    Userspace,
    Unconfigured,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetworkScenario {
    pub(crate) name: String,
    pub(crate) target: ScenarioTarget,
    pub(crate) selector: DeviceSelector,
    pub(crate) transport: TransportKind,
    pub(crate) placement: Option<NicPlacement>,
    pub(crate) auxiliary: Vec<AuxiliaryDevice>,
    pub(crate) backend: NetworkBackend,
    pub(crate) guest: GuestConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScenarioExpectations {
    pub(crate) address: Option<(Ipv4Addr, u8)>,
    pub(crate) gateway: Option<Ipv4Addr>,
    pub(crate) dns: Option<Ipv4Addr>,
}

impl NetworkScenario {
    pub(crate) fn from_path(path: &Path) -> ScenarioResult<Self> {
        let input = fs::read_to_string(path).map_err(|error| {
            ScenarioError::new(format!(
                "failed to read network scenario {}: {error}",
                path.display()
            ))
        })?;
        Self::from_toml_str(&input).map_err(|error| {
            ScenarioError::new(format!("network scenario {}: {error}", path.display()))
        })
    }

    pub(crate) fn default_qemu_user(target: ScenarioTarget) -> ScenarioResult<Self> {
        let source = match target {
            ScenarioTarget::QemuRv64 => {
                include_str!("../../tools/network-scenarios/default-qemu-user-rv64.toml")
            }
            ScenarioTarget::QemuLa64 => {
                include_str!("../../tools/network-scenarios/default-qemu-user-la64.toml")
            }
            ScenarioTarget::BoardRv64 | ScenarioTarget::BoardLa64 => {
                return Err(ScenarioError::new(format!(
                    "target {} has no QEMU user-network scenario",
                    target.as_str()
                )));
            }
        };
        Self::from_toml_str(source)
    }

    pub(crate) fn from_toml_str(input: &str) -> ScenarioResult<Self> {
        let value = toml::from_str::<Value>(input)
            .map_err(|error| ScenarioError::new(format!("scenario TOML: {error}")))?;
        let root = table(&value, "scenario")?;
        reject_unknown(
            root,
            &[
                "schema",
                "name",
                "target",
                "device",
                "auxiliary",
                "backend",
                "guest",
            ],
            "scenario",
        )?;

        let schema = required_str(root, "schema", "scenario")?;
        if schema != NETWORK_SCENARIO_SCHEMA_V1 {
            return Err(ScenarioError::new(format!(
                "scenario.schema: expected {NETWORK_SCENARIO_SCHEMA_V1:?}, got {schema:?}"
            )));
        }

        let name = required_nonempty_str(root, "name", "scenario")?.to_owned();
        let target = ScenarioTarget::parse(required_str(root, "target", "scenario")?)?;
        let (selector, transport, placement) =
            parse_device(required_value(root, "device", "scenario")?)?;
        let auxiliary = parse_auxiliary_list(root.get("auxiliary"))?;
        let backend = parse_backend(required_value(root, "backend", "scenario")?)?;
        let guest = parse_guest(required_value(root, "guest", "scenario")?)?;

        let scenario = Self {
            name,
            target,
            selector,
            transport,
            placement,
            auxiliary,
            backend,
            guest,
        };
        scenario.validate()?;
        Ok(scenario)
    }

    pub(crate) fn validate(&self) -> ScenarioResult<()> {
        let expected_transport = match self.target {
            ScenarioTarget::QemuRv64 => TransportKind::VirtioMmio,
            ScenarioTarget::QemuLa64 => TransportKind::VirtioPci,
            ScenarioTarget::BoardRv64 => TransportKind::Dwmac,
            ScenarioTarget::BoardLa64 => TransportKind::BoardHook,
        };
        if self.transport != expected_transport {
            return Err(ScenarioError::new(format!(
                "target {} requires transport {:?}, got {:?}",
                self.target.as_str(),
                expected_transport,
                self.transport
            )));
        }

        match (self.target, &self.placement) {
            (ScenarioTarget::QemuRv64, Some(NicPlacement::VirtioMmio { .. }))
            | (ScenarioTarget::QemuLa64, Some(NicPlacement::VirtioPci { .. })) => {}
            (target, None) if target.is_qemu() => {
                return Err(ScenarioError::new(format!(
                    "target {} requires explicit device.placement",
                    target.as_str()
                )));
            }
            (target, Some(_)) if !target.is_qemu() => {
                return Err(ScenarioError::new(format!(
                    "target {} must obtain placement from firmware, not a QEMU placement",
                    target.as_str()
                )));
            }
            (_, Some(_)) => {
                return Err(ScenarioError::new(
                    "device.placement kind does not match target transport",
                ));
            }
            (_, None) => {}
        }

        if !self.target.is_qemu() && !self.auxiliary.is_empty() {
            return Err(ScenarioError::new(
                "real-board scenarios cannot contain QEMU auxiliary devices",
            ));
        }
        let mut auxiliary_ids: Vec<&str> = Vec::new();
        let mut auxiliary_placements: Vec<&NicPlacement> = Vec::new();
        let mut backend_ids: Vec<&str> = network_backend_id(&self.backend).into_iter().collect();
        for (index, auxiliary) in self.auxiliary.iter().enumerate() {
            validate_qemu_token(&auxiliary.id, &format!("auxiliary[{index}].id"))?;
            if auxiliary_ids.contains(&auxiliary.id.as_str()) {
                return Err(ScenarioError::new(format!(
                    "auxiliary[{index}].id: duplicate QEMU device id {:?}",
                    auxiliary.id
                )));
            }
            auxiliary_ids.push(&auxiliary.id);

            let kind_matches = matches!(
                (self.target, auxiliary.kind, &auxiliary.placement),
                (
                    ScenarioTarget::QemuRv64,
                    AuxiliaryDeviceKind::VirtioRngMmio | AuxiliaryDeviceKind::VirtioNetMmio,
                    NicPlacement::VirtioMmio { .. }
                ) | (
                    ScenarioTarget::QemuLa64,
                    AuxiliaryDeviceKind::VirtioRngPci | AuxiliaryDeviceKind::VirtioNetPci,
                    NicPlacement::VirtioPci { .. }
                )
            );
            if !kind_matches {
                return Err(ScenarioError::new(format!(
                    "auxiliary[{index}]: kind and placement do not match target {}",
                    self.target.as_str()
                )));
            }
            if self.placement.as_ref() == Some(&auxiliary.placement)
                || auxiliary_placements.contains(&&auxiliary.placement)
            {
                return Err(ScenarioError::new(format!(
                    "auxiliary[{index}].placement: placement is already occupied"
                )));
            }
            auxiliary_placements.push(&auxiliary.placement);

            let is_network = matches!(
                auxiliary.kind,
                AuxiliaryDeviceKind::VirtioNetMmio | AuxiliaryDeviceKind::VirtioNetPci
            );
            match (is_network, &auxiliary.backend) {
                (false, None) => {}
                (false, Some(_)) => {
                    return Err(ScenarioError::new(format!(
                        "auxiliary[{index}].backend: only network devices accept a backend"
                    )));
                }
                (
                    true,
                    Some(
                        backend @ (NetworkBackend::QemuUser { .. }
                        | NetworkBackend::Tap { .. }
                        | NetworkBackend::Bridge { .. }),
                    ),
                ) => {
                    let id = network_backend_id(backend).expect("matched QEMU network backend");
                    if backend_ids.contains(&id) {
                        return Err(ScenarioError::new(format!(
                            "auxiliary[{index}].backend.id: duplicate QEMU netdev id {id:?}"
                        )));
                    }
                    backend_ids.push(id);
                    validate_qemu_user_backend(backend, &format!("auxiliary[{index}].backend"))?;
                }
                (true, None | Some(NetworkBackend::None | NetworkBackend::BoardLink)) => {
                    return Err(ScenarioError::new(format!(
                        "auxiliary[{index}].backend: network device requires a QEMU backend"
                    )));
                }
            }
        }

        match (&self.backend, self.target.is_qemu()) {
            (
                NetworkBackend::QemuUser { .. }
                | NetworkBackend::Tap { .. }
                | NetworkBackend::Bridge { .. },
                false,
            ) => {
                return Err(ScenarioError::new(
                    "QEMU network backend cannot be used by a real-board target",
                ));
            }
            (NetworkBackend::BoardLink, true) => {
                return Err(ScenarioError::new(
                    "board-link backend cannot be used by a QEMU target",
                ));
            }
            _ => {}
        }

        validate_qemu_user_backend(&self.backend, "backend")?;

        Ok(())
    }

    pub(crate) fn ensure_target(&self, requested_target: ScenarioTarget) -> ScenarioResult<()> {
        if self.target == requested_target {
            Ok(())
        } else {
            Err(ScenarioError::new(format!(
                "scenario target {} does not match requested target {}",
                self.target.as_str(),
                requested_target.as_str()
            )))
        }
    }

    pub(crate) fn with_backend(mut self, backend: NetworkBackend) -> ScenarioResult<Self> {
        self.backend = backend;
        self.validate()?;
        Ok(self)
    }

    pub(crate) fn backend(&self) -> &NetworkBackend {
        &self.backend
    }

    pub(crate) fn render_qemu_network_args(
        &self,
        requested_target: ScenarioTarget,
    ) -> ScenarioResult<Vec<String>> {
        self.ensure_target(requested_target)?;
        if !requested_target.is_qemu() {
            return Err(ScenarioError::new(
                "QEMU argument rendering requires a QEMU target",
            ));
        }

        let mut args = Vec::new();
        for auxiliary in self
            .auxiliary
            .iter()
            .filter(|device| device.order == AuxiliaryDeviceOrder::BeforeNetwork)
        {
            render_auxiliary_device(&mut args, auxiliary)?;
        }

        let backend_id = match &self.backend {
            NetworkBackend::None => None,
            NetworkBackend::QemuUser {
                id,
                network,
                dhcp_start,
                gateway,
                dns,
            } => {
                args.extend([
                    "-netdev".to_owned(),
                    format!(
                        "user,id={id},net={network},dhcpstart={dhcp_start},host={gateway},dns={dns}"
                    ),
                ]);
                Some(id)
            }
            NetworkBackend::Tap { id, ifname } => {
                args.extend([
                    "-netdev".to_owned(),
                    format!("tap,id={id},ifname={ifname},script=no,downscript=no"),
                ]);
                Some(id)
            }
            NetworkBackend::Bridge { id, bridge } => {
                args.extend(["-netdev".to_owned(), format!("bridge,id={id},br={bridge}")]);
                Some(id)
            }
            NetworkBackend::BoardLink => {
                return Err(ScenarioError::new(
                    "board-link backend cannot render QEMU arguments",
                ));
            }
        };

        if let Some(backend_id) = backend_id {
            args.push("-device".to_owned());
            match &self.placement {
                Some(NicPlacement::VirtioMmio { bus }) => {
                    args.push(format!("virtio-net-device,netdev={backend_id},bus={bus}"))
                }
                Some(NicPlacement::VirtioPci { slot, function }) => {
                    args.push(format!(
                        "virtio-net-pci,netdev={backend_id},addr={}",
                        format_pci_address(*slot, *function)
                    ));
                }
                None => {
                    return Err(ScenarioError::new(
                        "QEMU device rendering requires explicit placement",
                    ));
                }
            }
        }

        for auxiliary in self
            .auxiliary
            .iter()
            .filter(|device| device.order == AuxiliaryDeviceOrder::AfterNetwork)
        {
            render_auxiliary_device(&mut args, auxiliary)?;
        }
        Ok(args)
    }

    pub(crate) fn render_guest_cmdline(&self) -> Vec<String> {
        match self.guest {
            GuestConfig::KernelDefault => Vec::new(),
            GuestConfig::Dhcp => vec!["tx.net.mode=dhcp".to_owned()],
            GuestConfig::Static {
                address,
                prefix_len,
                gateway,
                dns,
            } => {
                let mut values = vec![
                    "tx.net.mode=static".to_owned(),
                    format!("tx.net.ipv4={address}/{prefix_len}"),
                ];
                if let Some(gateway) = gateway {
                    values.push(format!("tx.net.gateway={gateway}"));
                }
                if let Some(dns) = dns {
                    values.push(format!("tx.net.dns={dns}"));
                }
                values
            }
            GuestConfig::Userspace => vec!["tx.net.mode=userspace".to_owned()],
            GuestConfig::Unconfigured => vec!["tx.net.mode=none".to_owned()],
        }
    }

    pub(crate) fn expectations(&self) -> ScenarioExpectations {
        match (&self.guest, &self.backend) {
            (
                GuestConfig::Dhcp,
                NetworkBackend::QemuUser {
                    network,
                    dhcp_start,
                    gateway,
                    dns,
                    ..
                },
            ) => ScenarioExpectations {
                address: Some((*dhcp_start, network.prefix_len())),
                gateway: Some(*gateway),
                dns: Some(*dns),
            },
            (
                GuestConfig::Static {
                    address,
                    prefix_len,
                    gateway,
                    dns,
                },
                _,
            ) => ScenarioExpectations {
                address: Some((*address, *prefix_len)),
                gateway: *gateway,
                dns: *dns,
            },
            (
                GuestConfig::KernelDefault
                | GuestConfig::Dhcp
                | GuestConfig::Userspace
                | GuestConfig::Unconfigured,
                _,
            ) => ScenarioExpectations {
                address: None,
                gateway: None,
                dns: None,
            },
        }
    }
}

fn render_auxiliary_device(
    args: &mut Vec<String>,
    auxiliary: &AuxiliaryDevice,
) -> ScenarioResult<()> {
    match (auxiliary.kind, &auxiliary.placement) {
        (AuxiliaryDeviceKind::VirtioRngMmio, NicPlacement::VirtioMmio { bus }) => {
            let rng_source = format!("{}-source", auxiliary.id);
            args.extend([
                "-object".to_owned(),
                format!("rng-random,id={rng_source},filename=/dev/urandom"),
                "-device".to_owned(),
            ]);
            args.push(format!(
                "virtio-rng-device,id={},rng={rng_source},bus={bus}",
                auxiliary.id
            ));
        }
        (AuxiliaryDeviceKind::VirtioRngPci, NicPlacement::VirtioPci { slot, function }) => {
            let rng_source = format!("{}-source", auxiliary.id);
            args.extend([
                "-object".to_owned(),
                format!("rng-random,id={rng_source},filename=/dev/urandom"),
                "-device".to_owned(),
            ]);
            args.push(format!(
                "virtio-rng-pci,id={},rng={rng_source},addr={}",
                auxiliary.id,
                format_pci_address(*slot, *function)
            ));
        }
        (AuxiliaryDeviceKind::VirtioNetMmio, NicPlacement::VirtioMmio { bus }) => {
            let backend = auxiliary.backend.as_ref().ok_or_else(|| {
                ScenarioError::new(format!(
                    "auxiliary device {:?} is missing its network backend",
                    auxiliary.id
                ))
            })?;
            let backend_id = render_network_backend(args, backend)?;
            args.extend([
                "-device".to_owned(),
                format!(
                    "virtio-net-device,id={},netdev={backend_id},bus={bus}",
                    auxiliary.id
                ),
            ]);
        }
        (AuxiliaryDeviceKind::VirtioNetPci, NicPlacement::VirtioPci { slot, function }) => {
            let backend = auxiliary.backend.as_ref().ok_or_else(|| {
                ScenarioError::new(format!(
                    "auxiliary device {:?} is missing its network backend",
                    auxiliary.id
                ))
            })?;
            let backend_id = render_network_backend(args, backend)?;
            args.extend([
                "-device".to_owned(),
                format!(
                    "virtio-net-pci,id={},netdev={backend_id},addr={}",
                    auxiliary.id,
                    format_pci_address(*slot, *function)
                ),
            ]);
        }
        _ => {
            return Err(ScenarioError::new(format!(
                "auxiliary device {:?} has incompatible placement",
                auxiliary.id
            )));
        }
    }
    Ok(())
}

fn render_network_backend(
    args: &mut Vec<String>,
    backend: &NetworkBackend,
) -> ScenarioResult<String> {
    match backend {
        NetworkBackend::QemuUser {
            id,
            network,
            dhcp_start,
            gateway,
            dns,
        } => {
            args.extend([
                "-netdev".to_owned(),
                format!(
                    "user,id={id},net={network},dhcpstart={dhcp_start},host={gateway},dns={dns}"
                ),
            ]);
            Ok(id.clone())
        }
        NetworkBackend::Tap { id, ifname } => {
            args.extend([
                "-netdev".to_owned(),
                format!("tap,id={id},ifname={ifname},script=no,downscript=no"),
            ]);
            Ok(id.clone())
        }
        NetworkBackend::Bridge { id, bridge } => {
            args.extend(["-netdev".to_owned(), format!("bridge,id={id},br={bridge}")]);
            Ok(id.clone())
        }
        NetworkBackend::None | NetworkBackend::BoardLink => Err(ScenarioError::new(
            "auxiliary network device requires a QEMU network backend",
        )),
    }
}

fn network_backend_id(backend: &NetworkBackend) -> Option<&str> {
    match backend {
        NetworkBackend::QemuUser { id, .. }
        | NetworkBackend::Tap { id, .. }
        | NetworkBackend::Bridge { id, .. } => Some(id),
        NetworkBackend::None | NetworkBackend::BoardLink => None,
    }
}

fn validate_qemu_user_backend(backend: &NetworkBackend, path: &str) -> ScenarioResult<()> {
    let NetworkBackend::QemuUser {
        network,
        dhcp_start,
        gateway,
        dns,
        ..
    } = backend
    else {
        return Ok(());
    };
    for (field, address) in [
        ("dhcp_start", *dhcp_start),
        ("gateway", *gateway),
        ("dns", *dns),
    ] {
        if !network.contains(address) {
            return Err(ScenarioError::new(format!(
                "{path}.{field}: {address} is outside {network}"
            )));
        }
    }
    if dhcp_start == gateway || dhcp_start == dns {
        return Err(ScenarioError::new(format!(
            "{path}.dhcp_start must not equal gateway or dns"
        )));
    }
    Ok(())
}

fn format_pci_address(slot: u8, function: u8) -> String {
    if function == 0 {
        format!("0x{slot:02x}")
    } else {
        format!("0x{slot:02x}.{function}")
    }
}

pub(crate) type ScenarioResult<T> = Result<T, ScenarioError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScenarioError {
    message: String,
}

impl ScenarioError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ScenarioError {}

fn parse_device(
    value: &Value,
) -> ScenarioResult<(DeviceSelector, TransportKind, Option<NicPlacement>)> {
    let device = table(value, "device")?;
    reject_unknown(device, &["transport", "selector", "placement"], "device")?;
    let transport = TransportKind::parse(required_str(device, "transport", "device")?)?;
    let selector = parse_selector(required_value(device, "selector", "device")?)?;
    let placement = device
        .get("placement")
        .map(|value| parse_placement(value, "device.placement"))
        .transpose()?;
    Ok((selector, transport, placement))
}

fn parse_selector(value: &Value) -> ScenarioResult<DeviceSelector> {
    let selector = table(value, "device.selector")?;
    reject_unknown(selector, &["kind", "value"], "device.selector")?;
    let kind = required_str(selector, "kind", "device.selector")?;
    let value = required_nonempty_str(selector, "value", "device.selector")?;
    validate_token_or_path(value, "device.selector.value")?;
    match kind {
        "device-id" => Ok(DeviceSelector::DeviceId(value.to_owned())),
        "firmware-path" => Ok(DeviceSelector::FirmwarePath(value.to_owned())),
        "pci-bdf" => Ok(DeviceSelector::PciBdf(value.to_owned())),
        _ => Err(ScenarioError::new(format!(
            "device.selector.kind: unsupported value {kind:?}; first-device fallback is forbidden"
        ))),
    }
}

fn parse_placement(value: &Value, path: &str) -> ScenarioResult<NicPlacement> {
    let placement = table(value, path)?;
    let kind = required_str(placement, "kind", path)?;
    match kind {
        "virtio-mmio" => {
            reject_unknown(placement, &["kind", "bus"], path)?;
            let bus = required_nonempty_str(placement, "bus", path)?;
            validate_qemu_token(bus, &format!("{path}.bus"))?;
            Ok(NicPlacement::VirtioMmio {
                bus: bus.to_owned(),
            })
        }
        "virtio-pci" => {
            reject_unknown(placement, &["kind", "slot", "function"], path)?;
            let slot = required_u8(placement, "slot", path)?;
            let function = required_u8(placement, "function", path)?;
            if slot > 31 {
                return Err(ScenarioError::new(format!("{path}.slot must be <= 31")));
            }
            if function > 7 {
                return Err(ScenarioError::new(format!("{path}.function must be <= 7")));
            }
            Ok(NicPlacement::VirtioPci { slot, function })
        }
        _ => Err(ScenarioError::new(format!(
            "{path}.kind: unsupported value {kind:?}"
        ))),
    }
}

fn parse_auxiliary_list(value: Option<&Value>) -> ScenarioResult<Vec<AuxiliaryDevice>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| ScenarioError::new("scenario.auxiliary: expected array of tables"))?;
    let mut auxiliary = Vec::with_capacity(entries.len());
    for (index, value) in entries.iter().enumerate() {
        let path = format!("auxiliary[{index}]");
        let entry = table(value, &path)?;
        reject_unknown(
            entry,
            &["id", "kind", "order", "placement", "backend"],
            &path,
        )?;
        let id = required_nonempty_str(entry, "id", &path)?.to_owned();
        validate_qemu_token(&id, &format!("{path}.id"))?;
        let kind = match required_str(entry, "kind", &path)? {
            "virtio-rng-mmio" => AuxiliaryDeviceKind::VirtioRngMmio,
            "virtio-rng-pci" => AuxiliaryDeviceKind::VirtioRngPci,
            "virtio-net-mmio" => AuxiliaryDeviceKind::VirtioNetMmio,
            "virtio-net-pci" => AuxiliaryDeviceKind::VirtioNetPci,
            other => {
                return Err(ScenarioError::new(format!(
                    "{path}.kind: unsupported typed auxiliary device {other:?}"
                )));
            }
        };
        let order = match required_str(entry, "order", &path)? {
            "before-network" => AuxiliaryDeviceOrder::BeforeNetwork,
            "after-network" => AuxiliaryDeviceOrder::AfterNetwork,
            other => {
                return Err(ScenarioError::new(format!(
                    "{path}.order: unsupported value {other:?}"
                )));
            }
        };
        let placement = parse_placement(
            required_value(entry, "placement", &path)?,
            &format!("{path}.placement"),
        )?;
        let backend = entry.get("backend").map(parse_backend).transpose()?;
        auxiliary.push(AuxiliaryDevice {
            id,
            kind,
            order,
            placement,
            backend,
        });
    }
    Ok(auxiliary)
}

fn parse_backend(value: &Value) -> ScenarioResult<NetworkBackend> {
    let backend = table(value, "backend")?;
    let kind = required_str(backend, "kind", "backend")?;
    match kind {
        "none" => {
            reject_unknown(backend, &["kind"], "backend")?;
            Ok(NetworkBackend::None)
        }
        "qemu-user" => {
            reject_unknown(
                backend,
                &["kind", "id", "network", "dhcp_start", "gateway", "dns"],
                "backend",
            )?;
            let id = required_qemu_token(backend, "id", "backend")?;
            let network = parse_cidr(required_str(backend, "network", "backend")?)?;
            Ok(NetworkBackend::QemuUser {
                id,
                network,
                dhcp_start: required_ipv4(backend, "dhcp_start", "backend")?,
                gateway: required_ipv4(backend, "gateway", "backend")?,
                dns: required_ipv4(backend, "dns", "backend")?,
            })
        }
        "tap" => {
            reject_unknown(backend, &["kind", "id", "ifname"], "backend")?;
            Ok(NetworkBackend::Tap {
                id: required_qemu_token(backend, "id", "backend")?,
                ifname: required_qemu_token(backend, "ifname", "backend")?,
            })
        }
        "bridge" => {
            reject_unknown(backend, &["kind", "id", "bridge"], "backend")?;
            Ok(NetworkBackend::Bridge {
                id: required_qemu_token(backend, "id", "backend")?,
                bridge: required_qemu_token(backend, "bridge", "backend")?,
            })
        }
        "board-link" => {
            reject_unknown(backend, &["kind"], "backend")?;
            Ok(NetworkBackend::BoardLink)
        }
        _ => Err(ScenarioError::new(format!(
            "backend.kind: unsupported value {kind:?}"
        ))),
    }
}

fn parse_guest(value: &Value) -> ScenarioResult<GuestConfig> {
    let guest = table(value, "guest")?;
    let kind = required_str(guest, "kind", "guest")?;
    match kind {
        "kernel-default" => {
            reject_unknown(guest, &["kind"], "guest")?;
            Ok(GuestConfig::KernelDefault)
        }
        "dhcp" => {
            reject_unknown(guest, &["kind"], "guest")?;
            Ok(GuestConfig::Dhcp)
        }
        "userspace" => {
            reject_unknown(guest, &["kind"], "guest")?;
            Ok(GuestConfig::Userspace)
        }
        "unconfigured" => {
            reject_unknown(guest, &["kind"], "guest")?;
            Ok(GuestConfig::Unconfigured)
        }
        "static" => {
            reject_unknown(
                guest,
                &["kind", "address", "prefix", "gateway", "dns"],
                "guest",
            )?;
            let prefix_len = required_u8(guest, "prefix", "guest")?;
            if prefix_len > 32 {
                return Err(ScenarioError::new("guest.prefix must be <= 32"));
            }
            Ok(GuestConfig::Static {
                address: required_ipv4(guest, "address", "guest")?,
                prefix_len,
                gateway: optional_ipv4(guest, "gateway", "guest")?,
                dns: optional_ipv4(guest, "dns", "guest")?,
            })
        }
        _ => Err(ScenarioError::new(format!(
            "guest.kind: unsupported value {kind:?}"
        ))),
    }
}

fn parse_cidr(value: &str) -> ScenarioResult<Ipv4Cidr> {
    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| ScenarioError::new(format!("backend.network: invalid CIDR {value:?}")))?;
    let network = parse_ipv4(address, "backend.network")?;
    let prefix_len = prefix
        .parse::<u8>()
        .map_err(|_| ScenarioError::new(format!("backend.network: invalid prefix {prefix:?}")))?;
    if prefix_len > 32 {
        return Err(ScenarioError::new("backend.network: prefix must be <= 32"));
    }
    let mask = prefix_mask(prefix_len);
    if u32::from(network) & mask != u32::from(network) {
        return Err(ScenarioError::new(format!(
            "backend.network: {network} has host bits set for /{prefix_len}"
        )));
    }
    Ok(Ipv4Cidr {
        network,
        prefix_len,
    })
}

fn prefix_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    }
}

fn table<'a>(value: &'a Value, path: &str) -> ScenarioResult<&'a Table> {
    value
        .as_table()
        .ok_or_else(|| ScenarioError::new(format!("{path}: expected table")))
}

fn reject_unknown(table: &Table, allowed: &[&str], path: &str) -> ScenarioResult<()> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ScenarioError::new(format!("{path}.{key}: unknown field")));
        }
    }
    Ok(())
}

fn required_value<'a>(table: &'a Table, key: &str, path: &str) -> ScenarioResult<&'a Value> {
    table
        .get(key)
        .ok_or_else(|| ScenarioError::new(format!("{path}.{key}: missing required field")))
}

fn required_str<'a>(table: &'a Table, key: &str, path: &str) -> ScenarioResult<&'a str> {
    required_value(table, key, path)?
        .as_str()
        .ok_or_else(|| ScenarioError::new(format!("{path}.{key}: expected string")))
}

fn required_nonempty_str<'a>(table: &'a Table, key: &str, path: &str) -> ScenarioResult<&'a str> {
    let value = required_str(table, key, path)?;
    if value.is_empty() {
        Err(ScenarioError::new(format!(
            "{path}.{key}: must not be empty"
        )))
    } else {
        Ok(value)
    }
}

fn required_u8(table: &Table, key: &str, path: &str) -> ScenarioResult<u8> {
    let value = required_value(table, key, path)?
        .as_integer()
        .ok_or_else(|| ScenarioError::new(format!("{path}.{key}: expected integer")))?;
    u8::try_from(value).map_err(|_| ScenarioError::new(format!("{path}.{key}: outside u8 range")))
}

fn required_ipv4(table: &Table, key: &str, path: &str) -> ScenarioResult<Ipv4Addr> {
    parse_ipv4(required_str(table, key, path)?, &format!("{path}.{key}"))
}

fn optional_ipv4(table: &Table, key: &str, path: &str) -> ScenarioResult<Option<Ipv4Addr>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| ScenarioError::new(format!("{path}.{key}: expected string")))
                .and_then(|value| parse_ipv4(value, &format!("{path}.{key}")))
        })
        .transpose()
}

fn parse_ipv4(value: &str, path: &str) -> ScenarioResult<Ipv4Addr> {
    value
        .parse::<Ipv4Addr>()
        .map_err(|_| ScenarioError::new(format!("{path}: invalid IPv4 address {value:?}")))
}

fn required_qemu_token(table: &Table, key: &str, path: &str) -> ScenarioResult<String> {
    let value = required_nonempty_str(table, key, path)?;
    validate_qemu_token(value, &format!("{path}.{key}"))?;
    Ok(value.to_owned())
}

fn validate_qemu_token(value: &str, path: &str) -> ScenarioResult<()> {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(ScenarioError::new(format!(
            "{path}: contains characters unsafe for a QEMU option"
        )))
    }
}

fn validate_token_or_path(value: &str, path: &str) -> ScenarioResult<()> {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/' | b'@' | b',')
    }) {
        Ok(())
    } else {
        Err(ScenarioError::new(format!(
            "{path}: contains unsupported characters"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_RV64: &str =
        include_str!("../../tools/network-scenarios/default-qemu-user-rv64.toml");
    const DEFAULT_LA64: &str =
        include_str!("../../tools/network-scenarios/default-qemu-user-la64.toml");
    const MUTATED_RV64: &str =
        include_str!("../../tools/network-scenarios/mutations/rv64-relocated-changed-subnet.toml");
    const MUTATED_LA64: &str =
        include_str!("../../tools/network-scenarios/mutations/la64-relocated-changed-subnet.toml");
    const MULTI_RV64: &str =
        include_str!("../../tools/network-scenarios/mutations/rv64-multi-nic-reordered.toml");

    #[test]
    fn default_qemu_scenarios_render_target_specific_transports() {
        let rv64 = NetworkScenario::from_toml_str(DEFAULT_RV64).expect("parse RV64 scenario");
        let la64 = NetworkScenario::from_toml_str(DEFAULT_LA64).expect("parse LA64 scenario");

        let rv_args = rv64
            .render_qemu_network_args(ScenarioTarget::QemuRv64)
            .expect("render RV64");
        let la_args = la64
            .render_qemu_network_args(ScenarioTarget::QemuLa64)
            .expect("render LA64");
        assert!(rv_args
            .iter()
            .any(|arg| arg.contains("bus=virtio-mmio-bus.1")));
        assert!(la_args.iter().any(|arg| arg.contains("addr=0x02")));
        assert!(rv64.render_guest_cmdline().is_empty());
    }

    #[test]
    fn auxiliary_network_device_renders_its_own_typed_backend() {
        let scenario = NetworkScenario::from_toml_str(MULTI_RV64).expect("parse multi-NIC");

        let args = scenario
            .render_qemu_network_args(ScenarioTarget::QemuRv64)
            .expect("render multi-NIC")
            .join(" ");

        assert_eq!(args.matches("virtio-net-device").count(), 2);
        assert!(args.contains("id=primary-network,net=172.31.44.0/24"));
        assert!(args.contains("id=secondary-network,net=172.30.55.0/24"));
        assert!(args.contains("id=secondary-network-device"));
        assert!(args.contains("bus=virtio-mmio-bus.5"));
    }

    #[test]
    fn relocated_changed_subnet_is_entirely_fixture_driven() {
        let scenario = NetworkScenario::from_toml_str(MUTATED_RV64).expect("parse mutation");
        let args = scenario
            .render_qemu_network_args(ScenarioTarget::QemuRv64)
            .expect("render mutation");
        let rendered = args.join(" ");
        assert!(rendered.contains("bus=virtio-mmio-bus.7"));
        assert!(rendered.contains("old-location-decoy"));
        assert!(rendered.contains("old-location-decoy-source"));
        assert!(rendered.contains("bus=virtio-mmio-bus.1"));
        assert!(rendered.contains("post-network-rng"));
        assert!(rendered.contains("net=172.31.44.0/24"));
        assert!(rendered.contains("dhcpstart=172.31.44.77"));
        let before = rendered.find("old-location-decoy").expect("before decoy");
        let network = rendered
            .find("virtio-net-device")
            .expect("relocated network device");
        let after = rendered.find("post-network-rng").expect("after auxiliary");
        assert!(before < network && network < after);
        assert_eq!(
            scenario.expectations().address,
            Some((Ipv4Addr::new(172, 31, 44, 77), 24))
        );
    }

    #[test]
    fn la64_mutation_uses_typed_pci_decoy_and_order() {
        let scenario = NetworkScenario::from_toml_str(MUTATED_LA64).expect("parse LA mutation");
        let rendered = scenario
            .render_qemu_network_args(ScenarioTarget::QemuLa64)
            .expect("render LA mutation")
            .join(" ");

        assert!(rendered.contains("virtio-rng-pci,id=old-location-decoy"));
        assert!(rendered.contains("addr=0x02"));
        assert!(rendered.contains("virtio-net-pci"));
        assert!(rendered.contains("addr=0x05"));
        let before = rendered.find("old-location-decoy").expect("before decoy");
        let network = rendered.find("virtio-net-pci").expect("relocated NIC");
        let after = rendered.find("post-network-rng").expect("after auxiliary");
        assert!(before < network && network < after);
    }

    #[test]
    fn auxiliary_rejects_raw_arguments_and_target_mismatch() {
        let raw = MUTATED_RV64.replace(
            "order = \"before-network\"",
            "order = \"before-network\"\nraw_arg = \"-device arbitrary\"",
        );
        let raw_error =
            NetworkScenario::from_toml_str(&raw).expect_err("raw auxiliary arguments must fail");
        assert!(raw_error.to_string().contains("raw_arg: unknown field"));

        let mismatch =
            MUTATED_RV64.replacen("kind = \"virtio-rng-mmio\"", "kind = \"virtio-rng-pci\"", 1);
        let mismatch_error = NetworkScenario::from_toml_str(&mismatch)
            .expect_err("auxiliary target mismatch must fail");
        assert!(mismatch_error
            .to_string()
            .contains("kind and placement do not match target qemu-rv64"));
    }

    #[test]
    fn missing_required_field_is_not_defaulted() {
        let missing_placement = DEFAULT_RV64.replace(
            "\n[device.placement]\nkind = \"virtio-mmio\"\nbus = \"virtio-mmio-bus.1\"\n",
            "\n",
        );
        let error = NetworkScenario::from_toml_str(&missing_placement)
            .expect_err("missing placement must fail");
        assert!(error
            .to_string()
            .contains("requires explicit device.placement"));
    }

    #[test]
    fn target_transport_mismatch_is_rejected() {
        let mismatch = DEFAULT_RV64.replacen("target = \"qemu-rv64\"", "target = \"qemu-la64\"", 1);
        let error =
            NetworkScenario::from_toml_str(&mismatch).expect_err("target mismatch must fail");
        assert!(error.to_string().contains("requires transport VirtioPci"));
    }

    #[test]
    fn selector_never_falls_back_to_first_device() {
        let first = DEFAULT_RV64.replacen("kind = \"device-id\"", "kind = \"first\"", 1);
        let error = NetworkScenario::from_toml_str(&first).expect_err("first fallback must fail");
        assert!(error
            .to_string()
            .contains("first-device fallback is forbidden"));
    }

    #[test]
    fn parsing_and_rendering_are_deterministic() {
        let first = NetworkScenario::from_toml_str(MUTATED_RV64).expect("first parse");
        let second = NetworkScenario::from_toml_str(MUTATED_RV64).expect("second parse");
        assert_eq!(first, second);
        assert_eq!(
            first
                .render_qemu_network_args(ScenarioTarget::QemuRv64)
                .expect("first render"),
            second
                .render_qemu_network_args(ScenarioTarget::QemuRv64)
                .expect("second render")
        );
    }

    #[test]
    fn rendering_for_another_target_fails() {
        let scenario = NetworkScenario::from_toml_str(DEFAULT_RV64).expect("parse");
        let error = scenario
            .render_qemu_network_args(ScenarioTarget::QemuLa64)
            .expect_err("requested target mismatch must fail");
        assert!(error
            .to_string()
            .contains("does not match requested target"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let unknown = DEFAULT_RV64.replace(
            "name = \"default-qemu-user-rv64\"",
            "name = \"default-qemu-user-rv64\"\nfallback = \"eth0\"",
        );
        let error = NetworkScenario::from_toml_str(&unknown).expect_err("unknown field must fail");
        assert!(error
            .to_string()
            .contains("scenario.fallback: unknown field"));
    }
}
