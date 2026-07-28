use core::cell::{Cell, RefCell};
use core::error::Error;
use core::fmt;
use pipewire as pw;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// One `PipeWire` audio sink available for explicit playback routing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipeWireOutputTarget {
    target: String,
    display_name: String,
}

impl PipeWireOutputTarget {
    /// Returns the `node.name` accepted by
    /// [`crate::PipeWireOutputConfig::with_target`].
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    #[must_use]
    pub fn routing_id(&self) -> &str {
        self.target()
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

/// A validated, deterministic snapshot of `PipeWire` audio sinks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PipeWireOutputTargets {
    targets: Vec<PipeWireOutputTarget>,
}

impl PipeWireOutputTargets {
    #[must_use]
    pub fn targets(&self) -> &[PipeWireOutputTarget] {
        &self.targets
    }

    #[must_use]
    pub fn target_by_name(&self, target: &str) -> Option<&PipeWireOutputTarget> {
        self.targets
            .iter()
            .find(|candidate| candidate.target == target)
    }

    #[must_use]
    pub fn target_by_routing_id(&self, routing_id: &str) -> Option<&PipeWireOutputTarget> {
        self.target_by_name(routing_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeWireTargetDiscoveryStage {
    MainLoop,
    Context,
    Core,
    Registry,
    Sync,
    Timer,
    Allocation,
    Snapshot,
}

impl fmt::Display for PipeWireTargetDiscoveryStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MainLoop => "main loop creation",
            Self::Context => "context creation",
            Self::Core => "core connection",
            Self::Registry => "registry acquisition",
            Self::Sync => "registry synchronization",
            Self::Timer => "discovery timeout registration",
            Self::Allocation => "target snapshot allocation",
            Self::Snapshot => "target snapshot validation",
        })
    }
}

#[derive(Debug)]
pub enum PipeWireTargetDiscoveryError {
    Timeout,
    Backend {
        stage: PipeWireTargetDiscoveryStage,
        object_id: Option<u32>,
        detail: String,
    },
}

impl fmt::Display for PipeWireTargetDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("PipeWire target discovery timed out"),
            Self::Backend {
                stage,
                object_id: Some(object_id),
                detail,
            } => write!(
                formatter,
                "PipeWire {stage} failed for object {object_id}: {detail}"
            ),
            Self::Backend {
                stage,
                object_id: None,
                detail,
            } => write!(formatter, "PipeWire {stage} failed: {detail}"),
        }
    }
}

impl Error for PipeWireTargetDiscoveryError {}

/// Discovers the initial `PipeWire` `Audio/Sink` registry snapshot.
///
/// The matching core synchronization event is the completion barrier. Human
/// descriptions are presentation only; `node.name` is retained as the routing
/// identity accepted by `target.object`.
///
/// # Errors
///
/// Returns [`PipeWireTargetDiscoveryError`] if `PipeWire` setup, synchronization,
/// allocation, registry validation, or the bounded discovery interval fails.
pub fn discover_pipewire_output_targets()
-> Result<PipeWireOutputTargets, PipeWireTargetDiscoveryError> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|source| backend_error(PipeWireTargetDiscoveryStage::MainLoop, None, source))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|source| backend_error(PipeWireTargetDiscoveryStage::Context, None, source))?;
    let core = context
        .connect_rc(None)
        .map_err(|source| backend_error(PipeWireTargetDiscoveryStage::Core, None, source))?;
    let registry = core
        .get_registry()
        .map_err(|source| backend_error(PipeWireTargetDiscoveryStage::Registry, None, source))?;

    let records = Rc::new(RefCell::new(Vec::new()));
    let failure = Rc::new(RefCell::new(None));
    let registry_listener =
        register_target_listener(&registry, &main_loop, records.clone(), failure.clone());

    let pending = core
        .sync(0)
        .map_err(|source| backend_error(PipeWireTargetDiscoveryStage::Sync, None, source))?;
    let completed = Rc::new(Cell::new(false));
    let core_completed = completed.clone();
    let core_failure = failure.clone();
    let loop_for_done = main_loop.clone();
    let loop_for_error = main_loop.clone();
    let core_listener = core
        .add_listener_local()
        .done(move |id, sequence| {
            if id == pw::core::PW_ID_CORE && sequence == pending {
                core_completed.set(true);
                loop_for_done.quit();
            }
        })
        .error(move |id, _, result, message| {
            publish_failure(
                &core_failure,
                backend_error(
                    PipeWireTargetDiscoveryStage::Sync,
                    Some(id),
                    format!("server error {result}: {message}"),
                ),
                &loop_for_error,
            );
        })
        .register();

    let timed_out = Rc::new(Cell::new(false));
    let timer_expired = timed_out.clone();
    let loop_for_timer = main_loop.clone();
    let timer = main_loop.loop_().add_timer(move |_| {
        timer_expired.set(true);
        loop_for_timer.quit();
    });
    timer
        .update_timer(Some(DISCOVERY_TIMEOUT), None)
        .into_result()
        .map_err(|source| backend_error(PipeWireTargetDiscoveryStage::Timer, None, source))?;

    while !completed.get() && !timed_out.get() && failure.borrow().is_none() {
        main_loop.run();
    }
    drop(timer);
    drop(core_listener);
    drop(registry_listener);
    if let Some(source) = failure.borrow_mut().take() {
        return Err(source);
    }
    if timed_out.get() {
        return Err(PipeWireTargetDiscoveryError::Timeout);
    }

    let records = std::mem::take(&mut *records.borrow_mut());
    assemble_snapshot(records)
}

fn register_target_listener(
    registry: &pw::registry::Registry,
    main_loop: &pw::main_loop::MainLoopRc,
    records: Rc<RefCell<Vec<TargetRecord>>>,
    failure: Rc<RefCell<Option<PipeWireTargetDiscoveryError>>>,
) -> pw::registry::Listener {
    let main_loop = main_loop.clone();
    registry
        .add_listener_local()
        .global(move |global| {
            if failure.borrow().is_some() || global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(properties) = global.props else {
                return;
            };
            if properties.get("media.class") != Some("Audio/Sink") {
                return;
            }
            let Some(target) = properties.get("node.name") else {
                publish_failure(
                    &failure,
                    snapshot_error(Some(global.id), "audio sink has no UTF-8 node.name"),
                    &main_loop,
                );
                return;
            };
            if target.is_empty() {
                publish_failure(
                    &failure,
                    snapshot_error(Some(global.id), "audio sink node.name is empty"),
                    &main_loop,
                );
                return;
            }
            let display_name = properties
                .get("node.description")
                .or_else(|| properties.get("node.nick"))
                .unwrap_or(target);
            if display_name.is_empty() {
                publish_failure(
                    &failure,
                    snapshot_error(Some(global.id), "audio sink display name is empty"),
                    &main_loop,
                );
                return;
            }
            let Ok(record) = try_record(target, display_name) else {
                publish_failure(
                    &failure,
                    backend_error(
                        PipeWireTargetDiscoveryStage::Allocation,
                        Some(global.id),
                        "target text capacity is unavailable",
                    ),
                    &main_loop,
                );
                return;
            };

            let mut records = records.borrow_mut();
            if records.try_reserve(1).is_err() {
                drop(records);
                publish_failure(
                    &failure,
                    backend_error(
                        PipeWireTargetDiscoveryStage::Allocation,
                        Some(global.id),
                        "target record capacity is unavailable",
                    ),
                    &main_loop,
                );
                return;
            }
            records.push(record);
        })
        .register()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TargetRecord {
    target: String,
    display_name: String,
}

fn try_record(target: &str, display_name: &str) -> Result<TargetRecord, ()> {
    let mut owned_target = String::new();
    owned_target
        .try_reserve_exact(target.len())
        .map_err(|_| ())?;
    owned_target.push_str(target);
    let mut owned_display_name = String::new();
    owned_display_name
        .try_reserve_exact(display_name.len())
        .map_err(|_| ())?;
    owned_display_name.push_str(display_name);
    Ok(TargetRecord {
        target: owned_target,
        display_name: owned_display_name,
    })
}

fn assemble_snapshot(
    records: Vec<TargetRecord>,
) -> Result<PipeWireOutputTargets, PipeWireTargetDiscoveryError> {
    let mut identities = HashSet::new();
    identities.try_reserve(records.len()).map_err(|_| {
        backend_error(
            PipeWireTargetDiscoveryStage::Allocation,
            None,
            "target identity capacity is unavailable",
        )
    })?;
    for record in &records {
        if record.target.is_empty() {
            return Err(snapshot_error(None, "target node.name is empty"));
        }
        if record.display_name.is_empty() {
            return Err(snapshot_error(None, "target display name is empty"));
        }
        if !identities.insert(record.target.as_str()) {
            return Err(snapshot_error(None, "target node.name is duplicated"));
        }
    }

    let mut targets = Vec::new();
    targets.try_reserve_exact(records.len()).map_err(|_| {
        backend_error(
            PipeWireTargetDiscoveryStage::Allocation,
            None,
            "validated target capacity is unavailable",
        )
    })?;
    targets.extend(records.into_iter().map(|record| PipeWireOutputTarget {
        target: record.target,
        display_name: record.display_name,
    }));
    targets.sort_unstable_by(|left, right| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left.target.cmp(&right.target))
    });
    Ok(PipeWireOutputTargets { targets })
}

fn publish_failure(
    destination: &RefCell<Option<PipeWireTargetDiscoveryError>>,
    source: PipeWireTargetDiscoveryError,
    main_loop: &pw::main_loop::MainLoopRc,
) {
    if destination.borrow().is_none() {
        *destination.borrow_mut() = Some(source);
        main_loop.quit();
    }
}

fn backend_error(
    stage: PipeWireTargetDiscoveryStage,
    object_id: Option<u32>,
    source: impl fmt::Display,
) -> PipeWireTargetDiscoveryError {
    PipeWireTargetDiscoveryError::Backend {
        stage,
        object_id,
        detail: source.to_string(),
    }
}

fn snapshot_error(object_id: Option<u32>, detail: &str) -> PipeWireTargetDiscoveryError {
    backend_error(PipeWireTargetDiscoveryStage::Snapshot, object_id, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(target: &str, display_name: &str) -> TargetRecord {
        TargetRecord {
            target: target.to_owned(),
            display_name: display_name.to_owned(),
        }
    }

    #[test]
    fn empty_snapshot_has_no_targets() {
        assert_eq!(
            assemble_snapshot(Vec::new()).unwrap(),
            PipeWireOutputTargets::default()
        );
    }

    #[test]
    fn snapshot_keeps_routing_identity_separate_from_presentation() {
        let snapshot = assemble_snapshot(vec![
            record("alsa_output.usb-b", "USB Audio"),
            record("alsa_output.pci-a", "Built-in Audio"),
            record("alsa_output.usb-a", "USB Audio"),
        ])
        .unwrap();

        assert_eq!(
            snapshot
                .targets()
                .iter()
                .map(|target| (target.target(), target.display_name()))
                .collect::<Vec<_>>(),
            vec![
                ("alsa_output.pci-a", "Built-in Audio"),
                ("alsa_output.usb-a", "USB Audio"),
                ("alsa_output.usb-b", "USB Audio"),
            ]
        );
        assert_eq!(
            snapshot
                .target_by_name("alsa_output.usb-b")
                .map(PipeWireOutputTarget::display_name),
            Some("USB Audio")
        );
        let selected = snapshot.target_by_routing_id("alsa_output.usb-b").unwrap();
        assert_eq!(selected.routing_id(), "alsa_output.usb-b");
        let config = crate::PipeWireOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_output_target(selected)
            .unwrap();
        assert_eq!(config.target(), Some("alsa_output.usb-b"));
    }

    #[test]
    fn snapshot_rejects_missing_and_duplicate_routing_identity() {
        assert!(matches!(
            assemble_snapshot(vec![record("", "Sink")]),
            Err(PipeWireTargetDiscoveryError::Backend {
                stage: PipeWireTargetDiscoveryStage::Snapshot,
                ..
            })
        ));
        assert!(matches!(
            assemble_snapshot(vec![record("sink", "One"), record("sink", "Two")]),
            Err(PipeWireTargetDiscoveryError::Backend {
                stage: PipeWireTargetDiscoveryStage::Snapshot,
                ..
            })
        ));
    }
}
