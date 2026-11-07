//! Prometheus metrics served on `:8081/metrics`.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OutcomeLabels {
    pub outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct LoopLabels {
    pub controller: String,
}

pub struct Metrics {
    pub registry: Registry,
    pub builds: Family<OutcomeLabels, Counter>,
    pub reconcile_errors: Family<LoopLabels, Counter>,
    pub hooks: Counter,
    pub gc_deleted: Counter,
    pub leader: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        let mut registry = Registry::with_prefix("gitimage");
        let builds = Family::<OutcomeLabels, Counter>::default();
        let reconcile_errors = Family::<LoopLabels, Counter>::default();
        let hooks = Counter::default();
        let gc_deleted = Counter::default();
        let leader = Gauge::default();
        registry.register("builds", "Finished builds by outcome", builds.clone());
        registry.register(
            "reconcile_errors",
            "Reconcile errors by controller",
            reconcile_errors.clone(),
        );
        registry.register("hooks", "Accepted push hooks", hooks.clone());
        registry.register(
            "gc_deleted",
            "Manifests and blobs deleted by storage GC",
            gc_deleted.clone(),
        );
        registry.register(
            "leader",
            "1 when this replica holds the lease",
            leader.clone(),
        );
        Self {
            registry,
            builds,
            reconcile_errors,
            hooks,
            gc_deleted,
            leader,
        }
    }
}

impl Metrics {
    pub fn build(&self, outcome: &str) {
        self.builds
            .get_or_create(&OutcomeLabels {
                outcome: outcome.into(),
            })
            .inc();
    }

    pub fn error(&self, controller: &str) {
        self.reconcile_errors
            .get_or_create(&LoopLabels {
                controller: controller.into(),
            })
            .inc();
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        let _ = prometheus_client::encoding::text::encode(&mut out, &self.registry);
        out
    }
}
