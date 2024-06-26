use std::any::Any;
use std::fmt::Debug;
use std::ops::Not;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use config::Config;
use opentelemetry::propagation::text_map_propagator::TextMapPropagator;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio::time::sleep;
use tracing::{debug, error, info, Span, trace, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use opendut_carl_api::proto::services::peer_messaging_broker;
use opendut_carl_api::proto::services::peer_messaging_broker::{ApplyPeerConfiguration, TracingContext};
use opendut_carl_api::proto::services::peer_messaging_broker::downstream::Message;
use opendut_types::cluster::ClusterAssignment;
use opendut_types::peer::configuration::PeerConfiguration;
use opendut_types::peer::executor::{ContainerCommand, ContainerName, Engine, ExecutorDescriptor, ExecutorDescriptors};
use opendut_types::peer::PeerId;
use opendut_types::util::net::NetworkInterfaceName;
use opendut_util::logging;
use opendut_util::logging::LoggingConfig;
use opendut_util::settings::LoadedConfig;

use crate::common::{carl, settings};
use crate::service::{cluster_assignment, vpn};
use crate::service::can_manager::{CanManager, CanManagerRef};
use crate::service::network_interface::manager::{NetworkInterfaceManager, NetworkInterfaceManagerRef};

const BANNER: &str = r"
                         _____     _______
                        |  __ \   |__   __|
   ___  _ __   ___ _ __ | |  | |_   _| |
  / _ \| '_ \ / _ \ '_ \| |  | | | | | |
 | (_) | |_) |  __/ | | | |__| | |_| | |
  \___/| .__/ \___|_| |_|_____/ \__,_|_|
       | |  ______ _____   _____          _____
       |_| |  ____|  __ \ / ____|   /\   |  __ \
           | |__  | |  | | |  __   /  \  | |__) |
           |  __| | |  | | | |_ | / /\ \ |  _  /
           | |____| |__| | |__| |/ ____ \| | \ \
           |______|_____/ \_____/_/    \_\_|  \_\";

pub async fn launch(id_override: Option<PeerId>) -> anyhow::Result<()> {
    println!("{}", crate::app_info::formatted_with_banner(BANNER));

    let settings_override = Config::builder()
        .set_override_option(settings::key::peer::id, id_override.map(|id| id.to_string()))?
        .build()?;

    create_with_logging(settings_override).await
}

pub async fn create_with_logging(settings_override: config::Config) -> anyhow::Result<()> {
    let settings = settings::load_with_overrides(settings_override)?;

    let self_id = settings.config.get::<PeerId>(settings::key::peer::id)
        .context("Failed to read ID from configuration.\n\nRun `edgar setup` before launching the service.")?;

    let service_instance_id = self_id.to_string();

    let file_logging = None;
    let logging_config = LoggingConfig::load(&settings.config, service_instance_id)?;
    let mut shutdown = logging::initialize_with_config(logging_config.clone(), file_logging)?;

    if let logging::OpenTelemetryConfig::Enabled { cpu_collection_interval_ms, .. } = logging_config.opentelemetry {
        logging::initialize_metrics_collection(cpu_collection_interval_ms);   
    }

    create(self_id, settings).await?;

    shutdown.shutdown();

    Ok(())
}

pub async fn create(self_id: PeerId, settings: LoadedConfig) -> anyhow::Result<()> {

    info!("Started with ID <{self_id}> and configuration: {settings:?}");

    let network_interface_manager: NetworkInterfaceManagerRef = NetworkInterfaceManager::create()?;
    let can_manager: CanManagerRef = CanManager::create(Arc::clone(&network_interface_manager));

    let network_interface_management_enabled = settings.config.get::<bool>("network.interface.management.enabled")?;

    let bridge_name = crate::common::default_bridge_name();

    let remote_address = vpn::retrieve_remote_host(&settings).await?;

    let setup_cluster_info = SetupClusterInfo {
        self_id,
        bridge_name,
        network_interface_management_enabled,
        network_interface_manager,
        can_manager,
    };

    let timeout_duration = Duration::from_millis(settings.config.get::<u64>("carl.disconnect.timeout.ms")?);

    let mut carl = carl::connect(&settings.config).await?;

    let (mut rx_inbound, tx_outbound) = carl::open_stream(self_id, &remote_address, &mut carl).await?;

    loop {
        let received = tokio::time::timeout(timeout_duration, rx_inbound.message()).await;

        match received {
            Ok(received) => match received {
                Ok(Some(message)) => {
                    handle_stream_message(
                        message,
                        &setup_cluster_info,
                        &tx_outbound,
                    ).await?
                }
                Err(status) => {
                    warn!("CARL sent a gRPC error status: {status}");
                    //TODO exit?
                }
                Ok(None) => {
                    info!("CARL disconnected!");
                    break;
                }
            }
            Err(_) => {
                error!("No message from CARL within {} ms.", timeout_duration.as_millis());
                break;
            }
        }
    }

    Ok(())
}


async fn handle_stream_message(
    message: peer_messaging_broker::Downstream,
    setup_cluster_info: &SetupClusterInfo,
    tx_outbound: &Sender<peer_messaging_broker::Upstream>,
) -> anyhow::Result<()> {

    if let peer_messaging_broker::Downstream { message: Some(message), context } = message {
        if matches!(message, Message::Pong(_)).not() {
            trace!("Received message: {:?}", message);
        }

        match message {
            Message::Pong(_) => {
                sleep(Duration::from_secs(5)).await;
                let message = peer_messaging_broker::Upstream {
                    message: Some(peer_messaging_broker::upstream::Message::Ping(peer_messaging_broker::Ping {})),
                    context: None
                };
                let _ignore_error =
                    tx_outbound.send(message).await
                        .inspect_err(|cause| debug!("Failed to send ping to CARL: {cause}"));
            }
            Message::ApplyPeerConfiguration(message) => { apply_peer_configuration(message, context, setup_cluster_info).await }
        }
    } else {
        ignore(message)
    }

    Ok(())
}

#[tracing::instrument(skip(message, context, setup_cluster_info), level="trace")]
async fn apply_peer_configuration(message: ApplyPeerConfiguration, context: Option<TracingContext>, setup_cluster_info: &SetupClusterInfo) {
    match message.clone() {
        ApplyPeerConfiguration { configuration: Some(configuration) } => {

            let span = Span::current();
            set_parent_context(&span, context);
            let _span = span.enter();

            info!("Received configuration: {configuration:?}");
            match PeerConfiguration::try_from(configuration) {
                Err(error) => error!("Illegal PeerConfiguration: {error}"),
                Ok(configuration) => {
                    setup_executors(configuration.executors);
                    let _ = setup_cluster(
                        configuration.cluster_assignment,
                        setup_cluster_info,
                    ).await;
                }
            };
        }
        _ => ignore(message),
    }
}

#[tracing::instrument(skip(executors))]
fn setup_executors(executors: ExecutorDescriptors) { //TODO make idempotent
    for executor in executors.executors {
        match executor {
            ExecutorDescriptor::Executable => warn!("Executing Executable not yet implemented."),
            ExecutorDescriptor::Container {
                engine,
                name,
                image,
                volumes,
                devices,
                envs,
                ports,
                command,
                args
            } => {
                let engine = match engine {
                    Engine::Docker => { "docker" }
                    Engine::Podman => { "podman" }
                };
                let mut cmd = Command::new(engine);
                cmd.arg("run");
                cmd.arg("--restart=unless-stopped");
                if let ContainerName::Value(name) = name {
                    cmd.args(["--name", name.as_str()]);
                }
                for port in ports {
                    cmd.args(["--publish", port.value()]);
                }
                for volume in volumes {
                    cmd.args(["--volume", volume.value()]);
                }
                for device in devices {
                    cmd.args(["--devices", device.value()]);
                }
                for env in envs {
                    cmd.args(["--env", &format!("{}={}", env.name(), env.value())]);
                }
                cmd.arg(image.value());
                if let ContainerCommand::Value(command) = command {
                    cmd.arg(command.as_str());
                }
                for arg in args {
                    cmd.arg(arg.value());
                }
                debug!("Command: {:?}", cmd);
                match cmd.spawn() {
                    Ok(_) => { info!("Container started.") }
                    Err(_) => { error!("Failed to start container.") }
                };
            }
        }
    }
}

struct SetupClusterInfo {
    self_id: PeerId,
    bridge_name: NetworkInterfaceName,
    network_interface_management_enabled: bool,
    network_interface_manager: NetworkInterfaceManagerRef,
    can_manager: CanManagerRef,
}
#[tracing::instrument(skip(cluster_assignment, info))]
async fn setup_cluster(
    cluster_assignment: Option<ClusterAssignment>,
    info: &SetupClusterInfo,
) -> anyhow::Result<()> { //TODO make idempotent

    match cluster_assignment {
        Some(cluster_assignment) => {
            trace!("Received ClusterAssignment: {cluster_assignment:?}");
            info!("Was assigned to cluster <{}>", cluster_assignment.id);

            if info.network_interface_management_enabled {
                cluster_assignment::network_interfaces_setup(
                    cluster_assignment,
                    info.self_id,
                    &info.bridge_name,
                    Arc::clone(&info.network_interface_manager),
                    Arc::clone(&info.can_manager)
                ).await
                    .inspect_err(|error| {
                        error!("Failed to configure network interfaces: {error}")
                    })?;
            } else {
                debug!("Skipping changes to network interfaces after receiving ClusterAssignment, as this is disabled via configuration.");
            }
        }
        None => {
            debug!("No ClusterAssignment in peer configuration.");
            //TODO teardown cluster, if configuration changed
        }
    }
    Ok(())
}

fn set_parent_context(span: &Span, context: Option<TracingContext>) {
    if let Some(context) = context {
        let propagator = TraceContextPropagator::new();
        let parent_context = propagator.extract(&context.values);
        span.set_parent(parent_context);
    }
}
fn ignore(message: impl Any + Debug) {
    warn!("Ignoring illegal message: {message:?}");
}
