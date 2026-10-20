use super::*;
use culprit_core::Actionability;
pub fn cluster_health_info() -> CheckInfo {
    CheckInfo {id:"cluster-health".into(),title:"Galera node readiness and flow control".into(),category:Category::Configuration,engines:vec![Engine::Mysql],requires:vec![],description:"The current readiness, connectivity and primary-component verdict reported by a Galera node; its historical flow-control fraction is context, not database-specific pressure.".into()}
}
check!(ClusterHealthCheck: cluster_health_info, cluster_health);
pub fn cluster_health(s: &Snapshot) -> Vec<Finding> {
    let Some(c) = s.operations.cluster.as_ref() else { return vec![] };
    let unavailable = c.ready == Some(false) || c.connected == Some(false) || c.primary_component == Some(false);
    let paused = c.flow_control_paused.filter(|p| p.is_finite() && (0.0..=1.0).contains(p));
    if !unavailable && paused.is_none_or(|p| p < 0.1) {
        return vec![];
    }
    let mut f = finding(
        "cluster-health",
        &s.server.database,
        Category::Configuration,
        subject(SubjectKind::Database, &s.server.database),
    );
    f.title = if unavailable {
        "This Galera node reports that it is not ready, disconnected or outside the primary component".into()
    } else {
        "Galera recorded time paused for flow control (whole cluster context)".into()
    };
    f.summary = format!(
        "Measured node status: ready {}, connected {}, primary component {}. Local state: {}. {}",
        flag(c.ready),
        flag(c.connected),
        flag(c.primary_component),
        c.local_state.as_deref().unwrap_or("not recorded"),
        if unavailable {
            "This is the node's own availability verdict; it can reject application operations. Check whether it is intentionally being maintained before intervening."
        } else {
            "Donor or desynchronized states can be deliberate. The retained flow-control ratio does not establish pressure or delays in this database now."
        }
    );
    f.severity = if unavailable { Severity::Medium } else { Severity::Low };
    f.actionability = if unavailable { Actionability::NeedsAttention } else { Actionability::Information };
    f.confidence = Confidence::High;
    f.impact.score = base_score(f.severity);
    f.impact.label = if unavailable { "Node availability".into() } else { "Historical cluster context".into() };
    if let Some(p) = paused {
        f.metrics.push(Metric::new("Recorded flow-control paused share", p, Unit::Ratio));
    }
    f.fix=Some(Fix{summary:"Inspect cluster membership, node logs and state transfer with the cluster owner. Check network and capacity, and route traffic only to suitable nodes. Do not bootstrap a component, disable flow control or force quorum from this observation alone.".into(),sql:None,cost:None});
    f.verify=vec!["Verify the intended node rejoins the primary component, reports ready and connected, and serves application operations. Compare flow control over a fresh known interval before diagnosing sustained pressure.".into()];
    vec![f]
}
fn flag(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}
